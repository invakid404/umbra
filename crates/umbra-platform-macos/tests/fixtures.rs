#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod support;
use std::{
    collections::BTreeMap,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use umbra_core::*;
use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};
use umbra_platform_macos::{DarwinArm64Abi, MacosTraceBackend, Options};
fn byte_path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}
fn bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}
struct Memory<'a>(&'a mut MacosTraceBackend, TaskId);
impl TraceMemory for Memory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        self.0.read_memory(self.1, address, out)
    }
}
/// Bare-name cases: the child is launched as `<vendor path> <case> <output>`,
/// and this harness owns the `CAPTURED` verdict.
fn fixture(case: &str, expected: &[u8]) {
    fixture_argv(case, expected, true, |vendor, host| {
        vec![bytes(vendor), case.as_bytes().to_vec(), bytes(host)]
    })
}
/// Serializes every fixture launch in this binary, from the process snapshot
/// `StrayFixtureChildren` takes to the moment its cleanup finishes.
///
/// **Why a lock and not a tighter filter.** The stray reaper identifies a child
/// it never had a handle on by diffing two process snapshots, and that is only
/// sound if nothing else in this binary starts a fixture child in between. It is
/// not sound by itself: a matching child created *after* the before-snapshot by a
/// concurrently running test is indistinguishable from the stray one, and killing
/// it would fail an unrelated test non-deterministically. `cargo test` runs a
/// harness's tests on several threads unless told otherwise, so that is the
/// default configuration, not an exotic one. Trading a leaked process for a flaky
/// test is a bad trade, so the snapshot window is made exclusive instead.
///
/// **A process-level lock is deliberately not used.** The hazard is threads
/// within one test binary; `cargo` runs each test *target*'s executable in
/// sequence rather than concurrently, and this repository's CI additionally
/// passes `--test-threads=1`. Nothing in this workspace supports two fixture test
/// processes running at once — the tracer takes a debugger connection and a shared
/// resigned-twin cache, neither of which is contended for here — so an
/// interprocess lock would add a failure mode without removing one. If that ever
/// changes, this is the place that has to change with it.
///
/// Poisoning is absorbed rather than propagated: `mt_write` and `mt_spawn` panic
/// **by design** on every run, so a poisoned mutex is the normal state after them
/// and must not turn the other cases into secondary failures.
static FIXTURE_LAUNCH: Mutex<()> = Mutex::new(());

fn fixture_launch_lock() -> std::sync::MutexGuard<'static, ()> {
    FIXTURE_LAUNCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `argv` builds the launch argv from the absolute vendor executable and the
/// logical output path. `harness_verdict` is false for cases whose child prints
/// `CAPTURED <case>` itself; every assertion below still gates the test result.
///
/// Takes the launch lock and delegates. Callers that need the lock held across
/// more than one launch — or across a launch *and* their own stray cleanup, which
/// is what `mt_fixture` needs — hold it themselves and call
/// [`fixture_argv_locked`] instead; taking it twice on one thread would deadlock.
fn fixture_argv(
    case: &str,
    expected: &[u8],
    harness_verdict: bool,
    argv: impl FnOnce(&Path, &Path) -> Vec<Vec<u8>>,
) {
    let _serial = fixture_launch_lock();
    fixture_argv_locked(case, expected, harness_verdict, argv)
}

/// The body of [`fixture_argv`], for callers already holding the launch lock.
fn fixture_argv_locked(
    case: &str,
    expected: &[u8],
    harness_verdict: bool,
    argv: impl FnOnce(&Path, &Path) -> Vec<Vec<u8>>,
) {
    let (Some(fixture), Some(root)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        assert!(
            std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
            "required integration needs UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT"
        );
        eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return;
    };
    let fixture = PathBuf::from(fixture);
    let root = support::redirect_root(root);
    let host_dir =
        std::env::temp_dir().join(format!("umbra-rust-fixture-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&host_dir).unwrap();
    let host = host_dir.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    assert!(!host.exists(), "fixture host output already exists");
    assert!(!shadow.exists(), "fixture shadow output already exists");
    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 25_000,
        ..Options::default()
    });
    let process = tracer
        .launch_experimental(LaunchSpec {
            executable: byte_path(&fixture),
            argv: argv(&fixture, &host),
            environment: vec![],
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                // Not a routed launch: no interposer, no descriptor fence.
                interpose: false,
                descriptor_limit: None,
            },
            // Direct tracer coverage, deliberately without enforcement: these
            // cases measure interception, not the sandbox boundary. `umbra run`
            // cannot select this; it always renders and requires a profile.
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let mut live = 1;
    let mut opens = 0;
    let mut threads = BTreeMap::new();
    while live > 0 {
        let event = tracer.next_event().unwrap();
        eprintln!("{case}: {event:?}");
        let resume = match event {
            TraceEvent::ThreadStarted { task, thread } => {
                threads.insert(task, thread);
                Some(thread)
            }
            TraceEvent::Child { .. } => {
                live += 1;
                None
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let op = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .unwrap()
                    .unwrap();
                match op {
                    FsOp::Open {
                        ref path, flags, ..
                    } => {
                        assert!(path.is_absolute(), "relative paths are not qualified");
                        // Only rewrite write-intent opens to the shadow root; leave
                        // read-only opens (library/dyld loads, runtime resource reads)
                        // pointing at the host so the tracee can actually start.
                        let write_intent =
                            flags.write || flags.append || flags.create || flags.truncate;
                        if write_intent {
                            let physical = root.join(
                                Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
                                    .strip_prefix("/")
                                    .unwrap(),
                            );
                            std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
                            let plan = tracer
                                .prepare_rewrite(thread, &byte_path(&physical), op)
                                .unwrap();
                            for write in &plan.memory_writes {
                                tracer
                                    .write_memory(task, write.address, &write.bytes)
                                    .unwrap();
                            }
                            DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                            tracer.set_registers(thread, &registers).unwrap();
                            opens += 1;
                        }
                    }
                    // Let through to the kernel unmodified, which is what this
                    // harness has always done for a read-only open.
                    //
                    // **This arm is why the panic below is still meaningful.**
                    // `fstat` joined the breakpointed stub list, and libSystem
                    // issues it on a *kernel* descriptor before `main` in every
                    // process -- `_os_feature_table_once` on fd 3 -- so every
                    // case here now sees one before it sees an open. There is no
                    // namespace in this harness and the descriptor is real, so
                    // letting the kernel answer is the honest result, and it is
                    // the same disposition the supervisor's descriptor fence
                    // reaches for a sub-floor descriptor.
                    //
                    // Named rather than folded into a wildcard: this driver
                    // rewrites write-intent opens and nothing else, and an
                    // operation it has no plan for should still stop the case
                    // loudly rather than be silently resumed.
                    FsOp::Fstat { .. } => {}
                    // The same disposition as `fstat` above, for the same
                    // reason and one stub list later.
                    //
                    // `close`(6) and `__close_nocancel`(399) joined the
                    // breakpointed stubs so a *virtual* dirfd could be released
                    // -- `fts` closes its directory descriptor through 399,
                    // which is neither interposed nor previously breakpointed.
                    // The consequence here is the one `TRACED_STUBS`' own doc
                    // comment warns about: **routing one member of a refused set
                    // makes the next member reachable for the first time.**
                    // Every `close` in the process now traps, and dyld closes
                    // kernel descriptors before `main` in every case this file
                    // runs -- measured, fd 3, in all nine that failed when this
                    // arm was missing.
                    //
                    // There is no namespace in this harness and the descriptor
                    // is a real kernel one, so letting the kernel answer is the
                    // honest result -- identical to what the supervisor's
                    // descriptor fence does for a sub-floor descriptor.
                    //
                    // **`ReadDir`(461) and `Fchdir`(13) joined the same stub
                    // list and deliberately have no arm.** They cannot reach
                    // here: `umbra-test-child.c` issues no directory read and no
                    // `fchdir` (measured -- zero occurrences of
                    // `getattrlistbulk`, `fchdir`, `opendir`, `readdir` or
                    // `fts_` in its source), and neither is issued by libSystem
                    // before `main` the way `fstat` and `close` are. If one ever
                    // does arrive, the panic below is the right outcome: this
                    // driver has no plan for a directory read, and inventing an
                    // empty arm for it now would be the wildcard this comment
                    // exists to refuse.
                    FsOp::Close { .. } => {}
                    other => panic!("unexpected operation {other:?}"),
                }
                Some(thread)
            }
            TraceEvent::SyscallExit { thread, .. } => {
                // Non-rewritten (read-only) opens legitimately return errors
                // (files missing under this test harness); the target write
                // opens are asserted by the final shadow-content check.
                Some(thread)
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected event {other:?}"),
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    tracer
        .terminate(process, TerminationPolicy::Immediate)
        .unwrap();
    assert!(opens > 0);
    assert!(!host.exists(), "MISSED {case}: host output exists");
    assert_eq!(
        std::fs::read(&shadow).unwrap(),
        expected,
        "MISSED {case}: shadow content"
    );
    if harness_verdict {
        eprintln!("CAPTURED {case}");
    }
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(host_dir).unwrap();
}
#[test]
fn open_libc() {
    fixture("open-libc", b"libc\n")
}
#[test]
fn open_svc() {
    fixture("open-svc", b"libc\n")
}
#[test]
fn fork_write() {
    fixture("fork-write", b"fork\n")
}
#[test]
fn posix_spawn_write() {
    fixture("posix-spawn-write", b"libc\n")
}
#[test]
fn exec_write() {
    fixture("exec-write", b"libc\n")
}
#[test]
fn grandchild_write() {
    fixture("grandchild-write", b"grandchild\n")
}
#[test]
fn dup_inherit_write() {
    fixture("dup-inherit-write", b"dup\n")
}
/// WNOHANG must be honoured for both `__wait4` (syscall 7) and
/// `__wait4_nocancel` (400). The child is held on a pipe, so every poll runs
/// against a live, unreaped child: a poll that wrongly blocks deadlocks and
/// fails against the 25 s session deadline instead of passing on timing. Under
/// the tracer a native wait would report ECHILD — debugger attach reparents
/// children — so the zero returns the fixture asserts can only come from the
/// tracer's own virtualization. The fixture prints `CAPTURED wnohang-wait`.
#[test]
fn wnohang_wait() {
    fixture_argv("wnohang-wait", b"wnohang\n", false, |vendor, host| {
        vec![bytes(vendor), b"--wnohang-wait".to_vec(), bytes(host)]
    })
}
/// argv[0] must arrive as the absolute vendor path even though the tracer
/// launches a signed twin. The launcher is handed a deliberately different
/// argv[0], so the child's check — argv[0] equal to the vendor path and
/// different from the image actually running — can only hold if `launch_traced`
/// substituted it. The child prints `CAPTURED argv0-check` once that check
/// passes; the shadow content asserted here is written only after it.
#[test]
fn argv0_check() {
    fixture_argv("argv0-check", b"argv0\n", false, |vendor, host| {
        vec![
            b"umbra-decoy-argv0".to_vec(),
            b"--argv0-check".to_vec(),
            bytes(host),
            bytes(vendor),
        ]
    })
}

// ---------------------------------------------------------------------------
// Multithreaded cases, on the dispatch paths `single_thread()` does not gate.
//
// `single_thread()` (native.rs:498) is called from exactly two places --
// `Delivery::Fork` (:1922) and `WaitPlan::Park` (:1967). Four arms are ungated,
// and they do **not** all touch the slots. Per arm, read off `native.rs` rather
// than generalised -- a generalisation is what put a false claim in this
// crate's README on the first pass:
//
//   | arm            | gated | `Session::entry` | `Session::pending`          |
//   |----------------|-------|------------------|-----------------------------|
//   | `Namespace`    | no    | set (:1913)      | one hop later, via `resume`  |
//   | `Fork`         | YES   | --               | `return_stop` (:1931)       |
//   | `Wait`+`Poll`  | no    | --               | -- (neither slot)           |
//   | `Wait`+`Park`  | YES   | --               | -- (sets `s.waiting` only)  |
//   | `Wait`+`Native`| no    | --               | `return_stop` (:1970)       |
//   | `Exec`         | no    | --               | `return_stop` (:2002/:2008) |
//
// `Delivery::Namespace` does not call `return_stop` itself: it only records the
// entry PC and emits the event. The return gate is planted one hop later, when
// the caller resumes the thread -- `resume()` takes `s.entry` and calls
// `s.return_stop(ReturnKind::Syscall)` (native.rs:2387). `mt_write` reaches the
// window through that hop, not from `intercept`.
//
// So a multithreaded tracee doing ordinary file I/O or a `posix_spawn` is not
// refused: it is unmediated in the window between a return gate being planted
// and the return arriving. These two cases drive that window on purpose.
//
// **`WaitPlan::Native` is a third ungated `return_stop` caller and slice 0 does
// not measure it.** Stated rather than left to be inferred from the two cases
// below. It writes `pending` exactly as `Exec` does, so it is expected to
// collide the same way `mt_spawn` does, but "expected" is not "measured" and no
// fixture here drives it. A case for it belongs with the follow-up issue.
// ---------------------------------------------------------------------------

/// A second destination alongside the one `fixture_argv` owns.
///
/// `fixture_argv` is one-destination-per-case, and so is `matrix()` in
/// `umbra-cli/tests/run_fixtures.rs`; reshaping either is not what these cases
/// need. What they need is a second file with a different name and different
/// bytes, written by a different thread, and that can be an extra argv operand
/// the case builds for itself -- the `--dirfd-rename` shape, which already
/// hands the child a root of its own rather than a single output path.
///
/// Deliberately **not** inside the directory `fixture_argv` creates and removes.
/// A host-side leak there would surface as a failed `remove_dir` naming a
/// directory rather than as an assertion naming the defect, and this is the one
/// assertion the whole slice turns on.
///
/// It owns its own teardown through `Drop`, which is what makes the teardown
/// panic-safe: `mt_write`'s assertion and the tracer's own `debug_assert!` both
/// unwind past the end of this function, so anything cleaned up by statements
/// after the assertion is not cleaned up on the runs that matter.
/// **Nothing is removed that this test did not create, and the type is what
/// enforces it.** The value is constructed only after both absence checks have
/// passed, so arming cleanup and proving the outputs are absent are the same
/// event; and the directory is carried as an `Option` that is `Some` only when
/// exclusive creation succeeded. A test destructor that deletes a path it did not
/// make is worse than the leak this machinery exists to close, so the ownership is
/// tracked rather than assumed.
struct Second {
    /// `Some` only when `create_dir` — not `create_dir_all` — established that
    /// this test made the directory. A directory that already existed belongs to
    /// something else and is left alone however the run ends.
    owned_directory: Option<PathBuf>,
    /// The logical path handed to the tracee, which is also the host path a
    /// rewrite that never happened would write to.
    host: PathBuf,
    /// Where a rewritten open must land instead.
    shadow: PathBuf,
}
impl Drop for Second {
    /// Best-effort and infallible: this runs while a panic is already unwinding,
    /// so a failure here must not replace the assertion that is being reported.
    ///
    /// The escaped host file is removed rather than left as evidence, and the
    /// assertion message carries its bytes instead — a leaked file in `TMPDIR` is
    /// residue that accumulates across developer runs, while the bytes in the
    /// failure output are the part anybody reads.
    fn drop(&mut self) {
        // The two output paths this test is answerable for, and nothing else.
        // Both were proven absent before this value existed, so removing them
        // cannot remove somebody else's file.
        let _ = std::fs::remove_file(&self.host);
        let _ = std::fs::remove_file(&self.shadow);
        if let Some(directory) = &self.owned_directory {
            // `remove_dir`, never `remove_dir_all`: with the output above gone a
            // directory this test created is empty, and anything still in it is
            // something this test did not put there. The call refusing a
            // non-empty directory *is* the check.
            let _ = std::fs::remove_dir(directory);
        }
    }
}

/// Every process whose executable has the fixture's file name, by pid.
///
/// **Why a process scan rather than a handle.** `mt_spawn`'s tracee
/// `posix_spawn`s a child that the tracer has not attached yet — the return gate
/// has not fired — so no session, no watchdog and no `Drop` in the backend owns
/// it, and the harness never receives its pid either. It is created
/// `POSIX_SPAWN_START_SUSPENDED`, survives the tracee being killed, gets
/// reparented away from this process so `waitpid` cannot see it, and holds the
/// inherited descriptors 0/1/2 open until something kills it. Measured on every
/// `mt-spawn` run; both reviews of this change reaped such children by hand.
///
/// So the only way to own it is to notice it: snapshot before the run, snapshot
/// after, and kill the difference.
///
/// **The diff alone is not enough to make that safe, and it is worth being exact
/// about why.** It excludes processes that already existed when the before
/// snapshot was taken — it does *not* distinguish a child this run leaked from a
/// child some other test started afterwards. Both appear only in the "after" set.
/// What makes the difference attributable is `FIXTURE_LAUNCH`, which every
/// fixture entrypoint holds from before its snapshot until after this cleanup, so
/// no other launch in this binary can occur inside the window. The exclusivity is
/// the correctness argument; the diff is only the mechanism.
///
/// Matching is on the executable's file name because the tracer launches a
/// *resigned twin* out of its cache, at a path the harness does not know, which
/// keeps the vendor binary's name.
fn fixture_named_processes() -> Option<BTreeMap<i32, PathBuf>> {
    let name = Path::new(&std::env::var_os("UMBRA_TEST_FIXTURE_PATH")?)
        .file_name()?
        .to_owned();
    // **Units, because getting them wrong is not hypothetical — it shipped.** Both
    // of `proc_listallpids`' arguments and one of its results are counted
    // differently, and an earlier revision of this function divided the result by
    // `size_of::<i32>()` as if it were bytes:
    //
    //   * `buffersize` is in **bytes**;
    //   * the return is a **pid count** — libproc's wrapper divides the kernel's
    //     byte count by `sizeof(int)` before returning it.
    //
    // Measured on this host to settle it rather than reasoning from the header: the
    // null call answered 1094, the filled call answered 1075, the buffer held 1074
    // positive pids and `ps` saw 1077. Dividing again kept 268 — about a quarter —
    // so both snapshots were silently truncated and could disagree arbitrarily,
    // which is the reaper missing strays or diffing two inconsistent views.
    //
    // SAFETY, both calls: the documented two-call form. A null buffer with size 0
    // asks how many pids there are; the second call is handed a buffer and its true
    // length in bytes.
    const ATTEMPTS: usize = 4;
    let mut capacity = match unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) } {
        count if count > 0 => count as usize * 2,
        // Cannot enumerate, so cannot attribute anything. `None`, never an empty
        // map: an empty "before" would make every matching process look new.
        _ => return None,
    };
    for _ in 0..ATTEMPTS {
        let mut pids = vec![0i32; capacity];
        let size = (std::mem::size_of::<i32>() * pids.len()) as i32;
        let written =
            unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, size) };
        if written <= 0 {
            return None;
        }
        let written = written as usize;
        // A reply that exactly fills the buffer may have been cut off by it, and a
        // truncated snapshot is the dangerous kind of wrong: a process missing from
        // "before" but present in "after" is diffed as a stray and killed. Grow and
        // ask again rather than accept a list that might be short.
        if written >= pids.len() {
            capacity *= 2;
            continue;
        }
        // `min` with the buffer length as well, so a reply larger than the buffer
        // can never index past it even if the guard above is ever relaxed.
        pids.truncate(written.min(pids.len()));
        return Some(fixture_processes_named(&pids, &name));
    }
    // Never converged, so completeness is unproven. Refuse rather than guess.
    None
}

/// Resolve each pid's executable and keep the ones whose file name matches.
fn fixture_processes_named(pids: &[i32], name: &std::ffi::OsString) -> BTreeMap<i32, PathBuf> {
    let mut found = BTreeMap::new();
    for pid in pids.iter().copied().filter(|pid| *pid > 0) {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: owned buffer with its true length; the call writes at most that
        // many bytes and reports how many. A process this test cannot inspect
        // answers <= 0 and is skipped.
        let bytes = unsafe {
            libc::proc_pidpath(
                pid,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len() as u32,
            )
        };
        if bytes <= 0 {
            continue;
        }
        // `proc_pidpath` reports a **byte** length, unlike `proc_listallpids`
        // above, and it excludes the terminating NUL. Same convention as
        // `native.rs`'s own `image_path`.
        buffer.truncate(bytes as usize);
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(&buffer));
        if path.file_name() == Some(name.as_os_str()) {
            found.insert(pid, path);
        }
    }
    found
}

/// Run-scoped ownership of the fixture children nothing else owns.
///
/// Constructed before the traced run and dropped after it, including while a
/// panic unwinds, which is the only path that matters: on `mt-spawn` the tracer's
/// `debug_assert!` fires *inside* `next_event`, so every statement after the
/// harness's own assertions is skipped.
///
/// Correct only while `FIXTURE_LAUNCH` is held across the whole window — see
/// [`fixture_named_processes`] for why the snapshot diff does not stand on its own.
///
/// This fixes the **harness**, and only the harness. The leak it cleans up is a
/// property of master's error window — any error between the spawn's `svc` and
/// `finish_return` reaches it, measured 10/10 including the release runs where
/// the assertion is compiled out — and that defect is filed separately. Reaping
/// here does not close it and must not be read as closing it; what it closes is
/// this test suite accumulating suspended processes across repeated runs.
struct StrayFixtureChildren {
    case: String,
    /// `None` when the process list could not be enumerated completely, which
    /// disables reaping entirely — see `Drop`.
    before: Option<BTreeMap<i32, PathBuf>>,
}
impl Drop for StrayFixtureChildren {
    fn drop(&mut self) {
        // **Uncertainty means reap nothing.** A snapshot that could be incomplete
        // is worse than no snapshot: a process missing from `before` but present
        // now is indistinguishable from a stray, and killing it is the failure mode
        // this guard exists to avoid. Both sides must be known-complete.
        let (Some(before), Some(after)) = (self.before.as_ref(), fixture_named_processes()) else {
            eprintln!(
                "REAP SKIPPED {}: the process list could not be enumerated completely,                  so nothing was killed",
                self.case
            );
            return;
        };
        for (pid, path) in after {
            if before.contains_key(&pid) {
                continue;
            }
            // SIGKILL rather than SIGTERM: the child is held suspended before its
            // first instruction, so it has no handler and will never run one.
            // SAFETY: a pid this scan just observed; a stale pid answers ESRCH.
            let killed = unsafe { libc::kill(pid, libc::SIGKILL) } == 0;
            // Never silent. A reaped child is a defect of the run that produced
            // it, and the count is what the follow-up issue is about.
            eprintln!(
                "REAPED {}: stray fixture child pid {pid} ({}){}",
                self.case,
                path.display(),
                if killed { "" } else { " — kill failed" }
            );
        }
    }
}

/// Drive one multithreaded case and check both destinations.
///
/// `first` is asserted by `fixture_argv`, which owns that destination, the
/// host-absence check on it and the tracer event loop. `second` is asserted here.
/// Both differ in name and in bytes, so a rewrite that reached the wrong thread
/// shows up three ways: wrong content in a shadow file, a shadow file that is
/// not there at all, or bytes on the host under the unrewritten name.
///
/// **The journal is not an oracle for any of that.** `JournalRecord` carries no
/// task or thread field, so a cross-thread mix-up still journals
/// `Prepare`/`ObservedResult`/`Commit` triples that pair correctly by
/// `OperationId`. Tracee-visible entry names and bytes are the only witness,
/// which is the #121/#117 pattern.
///
/// `harness_verdict` is false and `CAPTURED` is printed here instead, after the
/// second destination has been checked too. A verdict printed while an assertion
/// is still outstanding is lesson 23's skip-as-pass wearing a different hat.
fn mt_fixture(case: &str, option: &str, first: &[u8], second: &[u8]) {
    let Some(root) = std::env::var_os("UMBRA_TEST_REDIRECT_ROOT")
        .filter(|_| std::env::var_os("UMBRA_TEST_FIXTURE_PATH").is_some())
    else {
        // `fixture_argv` owns the `SKIP` line and the `UMBRA_INTEGRATION_REQUIRED`
        // assertion that makes a skip fatal in CI; reach it rather than
        // re-deciding here what a missing environment means.
        return fixture_argv(case, first, false, |vendor, host| {
            vec![bytes(vendor), option.as_bytes().to_vec(), bytes(host)]
        });
    };
    // Declared first so it is dropped **last**, and acquired before *setup* rather
    // than just before the launch: the exclusive `create_dir` below is what proves
    // this test owns the directory, and a concurrent case observing or creating the
    // same path would make that proof worthless. One region covers setup, the
    // before-snapshot, the traced run and the stray cleanup. `fixture_argv_locked`
    // is called below rather than `fixture_argv` because taking this same lock
    // twice on one thread would deadlock.
    let _serial = fixture_launch_lock();
    let root = support::redirect_root(root);
    let directory = std::env::temp_dir().join(format!(
        "umbra-rust-fixture-{}-{case}-b",
        std::process::id()
    ));
    // `create_dir`, not `create_dir_all`: success means this test made the
    // directory and may remove it, `AlreadyExists` means something else owns it and
    // it must survive the run. Process ids are recycled, so a directory left by an
    // older binary at this path is a real possibility rather than a hypothetical.
    let owned_directory = match std::fs::create_dir(&directory) {
        Ok(()) => Some(directory.clone()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
        Err(e) => panic!("second destination directory {}: {e}", directory.display()),
    };
    let host = directory.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    // Both checks precede the construction below, so cleanup is armed only for
    // paths just proven absent. A failure here leaves an empty directory behind
    // rather than deleting a file the test cannot account for.
    assert!(!host.exists(), "second host output already exists");
    assert!(!shadow.exists(), "second shadow output already exists");
    let extra = Second {
        owned_directory,
        host,
        shadow,
    };
    // Dropped in reverse declaration order after the run, and on an unwinding
    // panic too, which is the case that actually happens here: strays are reaped
    // first, then the destinations are cleared, then the lock is released.
    let _strays = StrayFixtureChildren {
        case: case.to_owned(),
        before: fixture_named_processes(),
    };
    let operand = bytes(&extra.host);
    fixture_argv_locked(case, first, false, |vendor, host| {
        vec![
            bytes(vendor),
            option.as_bytes().to_vec(),
            bytes(host),
            operand,
        ]
    });
    // Read before asserting: `Second::drop` removes this file, so the bytes have
    // to travel in the failure message rather than being left on disk.
    let escaped = std::fs::read(&extra.host).ok();
    assert!(
        escaped.is_none(),
        "MISSED {case}: the second thread's output reached the host at {}, holding {:?}",
        extra.host.display(),
        escaped.as_deref().map(String::from_utf8_lossy)
    );
    assert_eq!(
        std::fs::read(&extra.shadow).unwrap(),
        second,
        "MISSED {case}: second destination content"
    );
    eprintln!("CAPTURED {case}");
}

/// Two threads opening and writing distinct files inside one window, entirely on
/// the ungated `Delivery::Namespace` path.
///
/// **Measured against master (`e44d0db8`), three runs out of three:**
///
/// ```text
/// MISSED mt-write: the second thread's output reached the host at
/// /var/folders/.../umbra-rust-fixture-<pid>-mt-write-b/output
/// ```
///
/// The host file holds `two\n`, and no shadow file was ever created for it. The
/// event stream names exactly one worker thread: the other issues its `open`,
/// `write` and `close` without producing a single `SyscallEntry`. It is not
/// misattributed, it is **not intercepted at all**, so the rewrite never happens
/// and the tracee's own path is what reaches the kernel.
///
/// The mechanism is `return_stop` (native.rs:539-...), and it is not the
/// single-slot overwrite its comment describes. To let the stopped thread execute
/// the `svc` it sits on, `return_stop` *releases the entry breakpoint* --
/// `remove_breakpoint(pc)`, a `z0` -- plants the return gate at `pc + 4`, and
/// resumes. `install_breakpoint(pending.entry, ..)` in `finish_return` is what
/// puts it back. Between those two points the stub carries no breakpoint **for
/// the whole process**, because a debugserver `Z0` is per-process and
/// `continue_run` resumes every thread. A sibling thread reaching the same stub
/// inside that window walks straight through it.
///
/// **Why this case and `mt_spawn` fail differently, in one sentence.** Both of
/// this case's threads call `open`, so they share *one stub at one address*, and
/// the `z0` window on that address un-arms it for the sibling -- an escape.
/// `mt_spawn`'s two threads use *different* stubs (`__posix_spawn` and
/// `__open_nocancel`), so neither un-arms the other and the collision lands on
/// the shared `pending` slot instead -- an assertion. That is exactly why
/// per-thread slots close one and not the other.
///
/// So making `Session::pending` and `Session::entry` per-thread does **not**
/// close this: the hole is in the shared breakpoint registry, not in the slot.
/// Closing it needs the entry site to stay armed while another thread could
/// reach it. Three shapes, not two: single-step the trapping thread over the
/// `svc`; hold the siblings stopped over the Mach task port this backend
/// already owns (`task_threads` + `thread_suspend`, no new RSP surface); or
/// per-thread RSP resume (`vCont`).
///
/// **On failure the evidence is in the message, not on disk.** `Second::drop` runs
/// on the unwinding path and removes the escaped host file, plus the directory
/// holding it when this test created that directory; what survives is the
/// assertion's own text, which carries the bytes it found -- `holding
/// Some("two\n")`. That is the deliberate trade: a leaked file accumulates across
/// developer runs, while the failure output is the part anybody reads. Two earlier
/// revisions of this comment were wrong about this -- one describing the opposite
/// before the guard existed, one before the guard tracked what it owned.
#[test]
#[ignore = "slice 0 measurement: fails on master by design; the unmediated \
            entry-breakpoint window it names is not closed by per-thread slots \
            alone. Run with `--ignored`; un-ignore in the PR that fixes it."]
fn mt_write() {
    mt_fixture("mt-write", "--mt-write", b"one\n", b"two\n")
}

/// A `posix_spawn` with an unrelated namespace call in flight on another thread:
/// the ungated `Delivery::Exec` path, which is where the audit measured Node and
/// Tokio's process creation.
///
/// **Measured against master (`e44d0db8`), three runs out of three:**
///
/// ```text
/// thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
/// a second intercepted syscall entered while one was still in flight: this
/// session's pending entry breakpoint, return gate and exec candidate would all
/// be overwritten
/// ```
///
/// This is the `debug_assert!` `return_stop`'s comment planted for this arc,
/// reached exactly the way it predicts: the spawn's `ReturnKind::Spawn` is in
/// flight on one thread while the other's `open` return opens a second
/// transaction on the same slot. It is a `debug_assert!`, so **the backend** takes
/// the overwrite in a release build -- losing the spawn's gate, its entry
/// breakpoint, and the pid pointer the child would have been attached by.
///
/// **The run does not fail silently in release, though, and saying it did was
/// wrong.** For *this* case what ends it is the path decode refusing the
/// overwritten operand -- `Io during path: null or overflowing pointer` (`EFAULT`),
/// 5 of 5 enforced release runs. Measured: 10 enforced `umbra run` executions of
/// this case, 5 of them release, exited non-zero every time.
///
/// The supervisor also holds a second tripwire that ships -- `syscall_entry`
/// refuses a second entry for a thread whose operation is still awaiting its exit
/// (`umbra-supervisor/src/events.rs`, `operations: BTreeMap<ThreadId,
/// OperationId>`) with a real `Err(InvalidState)`, not an assertion -- and it is
/// keyed by thread, which says that layer already models per-thread operations
/// correctly while this backend's slots do not. That asymmetry is the part worth
/// keeping. **It is not what makes this case non-silent**: it is a
/// contention-dependent race that fired in 0 of 10 runs of `mt_spawn`, and an
/// earlier revision of this comment wrongly leaned on it.
///
/// Unlike `mt_write`, this one *is* the slot. Per-thread slots are **expected**
/// to close it and that expectation is **not verified**: slice 1 is not
/// implemented, so unlike every other claim in this comment it is a prediction,
/// not a measurement. It is recorded as such so the next reader does not inherit
/// it as a result.
///
/// The case does not reap its child. A blocking wait over a live child is
/// `WaitPlan::Park`, which `single_thread()` does gate, so reaping would refuse
/// the run before the spawn had been measured; the harness's own loop sees the
/// child exit instead.
///
/// **One measured side effect, and the harness now contains it.** The tripwire
/// fires after the `posix_spawn` syscall has already run, so the child exists, is
/// held by `POSIX_SPAWN_START_SUSPENDED`, and has not been attached to any session
/// yet -- nothing in the backend owns it, so neither `Session::drop` nor the
/// watchdog kills it, and it survives the panic still holding the inherited
/// descriptors 0/1/2.
///
/// `StrayFixtureChildren` reaps it: this test runs under a guard whose `Drop`
/// snapshots the fixture's processes before the run and kills whatever appeared
/// by the time it unwinds, printing a `REAPED` line naming the pid. What makes
/// "whatever appeared" attributable to *this* run is `FIXTURE_LAUNCH`, held across
/// the whole window so no other launch in this binary can occur inside it. So a
/// developer running this repeatedly no longer accumulates suspended processes or
/// keeps a captured pipe open.
///
/// **That fixes the harness and not the defect.** The leak is a property of
/// master's **error window**, not of the tripwire, and it is measured rather than
/// argued: under an enforced `umbra run` -- which does not go through this harness
/// at all -- the case leaked exactly one suspended orphan in 10 of 10 runs,
/// including the 5 release runs where the `debug_assert!` is compiled out and the
/// run instead fails on an unrelated `Io during path` refusal. Any error between
/// the spawn's `svc` and `finish_return` reaches it. Reaping here does not close
/// that, and a `REAPED` line is evidence of it rather than of its absence.
#[test]
#[ignore = "slice 0 measurement: fails on master by design, on the \
            `debug_assert!` at native.rs:571. Run with `--ignored`; un-ignore \
            in the PR that fixes it."]
fn mt_spawn() {
    mt_fixture("mt-spawn", "--mt-spawn", b"libc\n", b"two\n")
}

// ---------------------------------------------------------------------------
// dirfd-rename runs the same tracer through the real overlay transaction flow
// -- resolve, prepare, apply the rewrite, observe_result, commit -- instead of
// the prefix redirect the cases above use. A prefix redirect is not copy-up or
// whiteout semantics, so it cannot qualify a two-operand rename.
// ---------------------------------------------------------------------------
use std::sync::{Arc, Mutex};
use umbra_journal::Journal;
use umbra_overlay::{
    NamespaceResolver, NamespaceSession, Overlay, SessionConfig, StatEncoder, StorageBase,
};
use umbra_platform_macos::abi;
use umbra_storage::Storage;
use umbra_storage_local::LocalStorage;
use uuid::Uuid;

/// Logical roots the overlay-backed cases work under. These are namespace
/// paths, not host paths: nothing exists there on the host, so an operand the
/// tracer fails to rewrite lands somewhere absent and fails loudly instead of
/// quietly working.
const DIRFD_ROOT: &[u8] = b"/umbra-dirfd";
const SYMLINK_ROOT: &[u8] = b"/umbra-symlink";

/// Output-buffer address for the stat currently stopped at its entry. The
/// overlay's encoder contract carries logical metadata only, so the harness
/// binds the tracee's buffer alongside it.
#[derive(Clone, Default)]
struct StatBuffer(Arc<Mutex<Option<u64>>>);
/// Encodes overlay metadata into Darwin's `struct stat`. A logical symlink is
/// reported as a link with its target length, never as the placeholder object
/// the overlay stores for it.
struct DarwinStat(StatBuffer);
impl StatEncoder for DarwinStat {
    fn encode(
        &mut self,
        _context: &ProcessContext,
        _operation: &FsOp,
        stat: &BlobStat,
    ) -> Result<EmulatedResult> {
        let address = self.0 .0.lock().unwrap().take().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                "stat encoder",
                "no bound output buffer",
            )
        })?;
        Ok(EmulatedResult {
            outcome: OperationOutcome::Success { return_value: 0 },
            memory_writes: vec![MemoryWrite {
                address,
                bytes: abi::encode_stat(stat)?,
            }],
        })
    }
}

/// Records every journal record: the overlay's Prepare/ObservedResult/Commit
/// ordering is evidence this test asserts, not noise to discard.
struct RecordingJournal {
    records: Arc<Mutex<Vec<JournalRecord>>>,
    run: RunId,
    epoch: LeaseEpoch,
}
impl Journal for RecordingJournal {
    fn open(&mut self, _: &JournalOpenRequest) -> Result<RecoveryState> {
        Ok(recovery(self.run))
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        let mut records = self.records.lock().unwrap();
        let sequence = Sequence(records.len() as u64 + 1);
        let mut record = record.clone();
        record.sequence = sequence;
        records.push(record);
        Ok(sequence)
    }
    fn flush(&mut self, sequence: Sequence) -> Result<DurableSequence> {
        Ok(DurableSequence {
            run_id: self.run,
            writer_epoch: self.epoch,
            sequence,
        })
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let records = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.sequence.0 > after.0)
            .cloned()
            .map(Ok)
            .collect::<Vec<_>>();
        Ok(Box::new(records.into_iter()))
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        Ok(checkpoint.id)
    }
    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
fn recovery(run_id: RunId) -> RecoveryState {
    RecoveryState {
        run_id,
        checkpoint: None,
        last_valid_sequence: Sequence(0),
        durable: None,
        pending: vec![],
        tail: JournalTailRecovery::Intact,
        // Nothing declared this run unrecoverable; see the field.
        recovery_required: false,
        clean: true,
    }
}
fn request(run_id: RunId, epoch: Option<LeaseEpoch>) -> RequestContext {
    RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: epoch,
    }
}
fn open_run(dir: &Path) -> (LocalStorage, RunBinding, WriterLease) {
    let mut storage = LocalStorage::new(dir).unwrap();
    let run_id = RunId(Uuid::new_v4());
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "umbra-dirfd-base".into(),
                fingerprint: vec![1],
            },
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        })
        .unwrap();
    let lease = storage
        .acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: WriterId("umbra-dirfd".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .unwrap();
    (storage, binding, lease)
}
/// Every (anchor, path) pair an operation names, so the harness can tell the
/// case's own traffic from dyld and library startup reads.
fn operands(op: &FsOp) -> Vec<(DirRef, &BytePath)> {
    match op {
        FsOp::Open { dir, path, .. }
        | FsOp::Stat { dir, path, .. }
        | FsOp::Access { dir, path, .. }
        | FsOp::Unlink { dir, path, .. }
        | FsOp::ReadLink { dir, path }
        | FsOp::Mkdir { dir, path, .. }
        | FsOp::Chmod { dir, path, .. }
        | FsOp::Fchownat { dir, path, .. }
        // `SetTimes` carries exactly this shape and was missing, so
        // `case_traffic` answered `false` for it and the driver below resumed
        // the syscall **unrewritten** -- the kernel would have applied the times
        // to the host path, or Seatbelt refused, and either way the case would
        // have passed or failed for a reason that has nothing to do with the
        // overlay. Latent when it was introduced (no case issues `utimensat`)
        // and named rather than left to be discovered, because a harness that
        // silently stops mediating an operation class it appears to mediate is
        // worse than one that never claimed to.
        //
        // The wildcard below is what made it silent. It is kept, because the
        // descriptor-relative operations genuinely name no path operand and
        // adding them here would be wrong -- but every *path-carrying* variant
        // must be listed, and that is the rule a future `FsOp` addition has to
        // check against rather than a shape this match happens to have today.
        | FsOp::SetTimes { dir, path, .. } => vec![(*dir, path)],
        FsOp::Rename {
            from_dir,
            from,
            to_dir,
            to,
        } => vec![(*from_dir, from), (*to_dir, to)],
        FsOp::Link {
            from_dir,
            from,
            to_dir,
            to,
            ..
        } => vec![(*from_dir, from), (*to_dir, to)],
        FsOp::Symlink {
            link_dir,
            link_name,
            ..
        } => vec![(*link_dir, link_name)],
        _ => vec![],
    }
}
/// True when the operation belongs to the case rather than to startup traffic.
///
/// This harness binds an empty immutable base, so dyld and the shared cache
/// could not resolve through it; those reads are left pointing at the host,
/// unrewritten. Everything the case itself does is either under the logical
/// root or anchored on a descriptor this harness tracked.
fn case_traffic(op: &FsOp, root: &[u8], fds: &BTreeMap<TracedFd, FdState>) -> bool {
    operands(op).iter().any(|(dir, path)| {
        path.as_bytes().starts_with(root) || matches!(dir, DirRef::Fd(fd) if fds.contains_key(fd))
    })
}
/// Errno for a refused resolution. A refusal is a real syscall result for the
/// tracee, not a harness failure: this case deliberately looks up a name the
/// rename removed. Structured kinds are translated; anything else is a genuine
/// harness failure and stays loud.
fn denial(error: &UmbraError) -> Errno {
    error.errno.unwrap_or_else(|| match error.kind {
        ErrorKind::NotFound => Errno(2),
        ErrorKind::Denied => Errno(13),
        ErrorKind::AlreadyExists => Errno(17),
        ErrorKind::InvalidPath => Errno(22),
        // Loop exhaustion is a distinct kind precisely so this mapping does
        // not have to read an error message. ELOOP is 62 on Darwin.
        ErrorKind::SymlinkLoop => Errno(62),
        _ => panic!("MISSED dirfd-rename: {error:?}"),
    })
}
/// Absolute logical path of an operand, for descriptor bookkeeping.
fn logical(dir: DirRef, path: &BytePath, fds: &BTreeMap<TracedFd, FdState>) -> BytePath {
    if path.is_absolute() {
        return path.clone();
    }
    let anchor = match dir {
        DirRef::Fd(fd) => fds[&fd].logical_path.clone().unwrap(),
        DirRef::Cwd => panic!("relative cwd anchor is not used by this case"),
    };
    let mut bytes = anchor.as_bytes().to_vec();
    bytes.push(b'/');
    bytes.extend(path.as_bytes());
    BytePath::new(bytes).unwrap()
}

/// Drives one fixture case through the real overlay transaction flow --
/// `resolve` -> `prepare` -> apply the rewrite or the emulated result ->
/// `observe_result` -> `commit`/`abort` -- against a real local storage shadow,
/// an approved empty immutable base and a recording journal. A prefix redirect
/// is not copy-up or whiteout semantics, so it cannot qualify these cases.
///
/// dyld and library startup reads are left pointing at the host, unrewritten:
/// the bound base is empty, so the tracee could not otherwise start. That
/// carve-out is a property of this harness, not of the tracer.
fn overlay_fixture(
    case: &str,
    option: &str,
    root: &[u8],
    verify: impl FnOnce(&mut Overlay, &ProcessContext, &[JournalRecord], &Path, &[FsOp]),
) {
    let (Some(fixture), Some(_)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        assert!(
            std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
            "required integration needs UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT"
        );
        eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return;
    };
    // This entrypoint launches a tracee too, so it belongs inside the same
    // serialized region -- otherwise a concurrent `mt_fixture` would see this
    // case's child appear after its before-snapshot and reap it.
    let _serial = fixture_launch_lock();
    let fixture = PathBuf::from(fixture);
    let host_root = Path::new(std::ffi::OsStr::from_bytes(root));
    assert!(
        !host_root.exists(),
        "the logical root must not exist on the host"
    );

    let base_dir = tempfile::tempdir().unwrap();
    let shadow_dir = tempfile::tempdir().unwrap();
    let (mut base_storage, base_binding, base_lease) = open_run(base_dir.path());
    let base_root = PathBuf::from(std::ffi::OsStr::from_bytes(
        base_binding.root.physical_path.as_ref().unwrap().as_bytes(),
    ));
    base_storage.release_writer(&base_lease).unwrap();
    let base = StorageBase::new(
        Box::new(base_storage),
        base_binding.clone(),
        request(base_binding.run_id, None),
    )
    .unwrap();
    let (shadow, binding, lease) = open_run(shadow_dir.path());
    let records = Arc::new(Mutex::new(Vec::new()));
    let journal = RecordingJournal {
        records: records.clone(),
        run: binding.run_id,
        epoch: lease.epoch,
    };
    let control_root = PathBuf::from(std::ffi::OsStr::from_bytes(
        binding.control.physical_path.as_ref().unwrap().as_bytes(),
    ));
    let config = SessionConfig {
        context: request(binding.run_id, Some(lease.epoch)),
        recovery: recovery(binding.run_id),
        binding,
        lease,
    };
    let mut overlay = Overlay::new(Box::new(shadow), Box::new(journal));
    overlay.bind(config, Box::new(base)).unwrap();
    let stat_buffer = StatBuffer::default();
    overlay
        .set_stat_encoder(Box::new(DarwinStat(stat_buffer.clone())))
        .unwrap();

    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 25_000,
        ..Options::default()
    });
    let handle = tracer
        .launch_experimental(LaunchSpec {
            executable: byte_path(&fixture),
            argv: vec![bytes(&fixture), option.as_bytes().to_vec(), root.to_vec()],
            environment: vec![],
            sandbox: SandboxRequirement::UnsandboxedExperiment,
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                // Not a routed launch: no interposer, no descriptor fence.
                interpose: false,
                descriptor_limit: None,
            },
        })
        .unwrap();
    let mut process = ProcessContext {
        task: TaskId(TaskIdentity {
            native_id: 0,
            generation: 0,
        }),
        parent: None,
        architecture: Architecture::Aarch64,
        abi: "darwin-arm64-abi-v1".into(),
        exec_generation: 0,
        root: byte_path(Path::new("/")),
        cwd: byte_path(&std::env::current_dir().unwrap()),
        fds: BTreeMap::new(),
    };
    let mut pending: Option<(PreparedAction, FsOp)> = None;
    let mut observed = Vec::new();
    let mut live = 1;
    while live > 0 {
        let event = tracer.next_event().unwrap();
        let resume = match event {
            TraceEvent::ThreadStarted { task, thread } => {
                process.task = task;
                Some(thread)
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let op = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .unwrap();
                match op {
                    Some(op) if case_traffic(&op, root, &process.fds) => {
                        eprintln!("{case}: {op:?}");
                        observed.push(op.clone());
                        // The overlay owns logical metadata, so the tracee's
                        // own output buffers are bound here, from the stopped
                        // call's registers, before resolution consumes them.
                        match &op {
                            FsOp::ReadLink { .. } => {
                                let (address, len) = abi::readlink_buffer(&registers).unwrap();
                                overlay.set_readlink_buffer(address, len).unwrap();
                            }
                            FsOp::Stat { .. } => {
                                *stat_buffer.0.lock().unwrap() =
                                    Some(abi::stat_buffer(&registers).unwrap());
                            }
                            _ => {}
                        }
                        let action = match overlay.resolve(&process, &op) {
                            Ok(action) => action,
                            Err(refused) => {
                                // A truly-absent, non-mutating NotFound is not a
                                // refusal the overlay owns: with the host as base
                                // the kernel produces the same ENOENT, so the
                                // supervisor resumes the tracee's own unrewritten
                                // syscall (events.rs, gated on `!mutation`) instead
                                // of emulating. Model that gate here so the two
                                // NotFound shapes stay distinct: a regression that
                                // turned genuine absence into an emulated denial
                                // would change this path rather than be masked by
                                // the harness emulating the same errno, and a
                                // whiteout-hidden path already arrives above as
                                // `Deny`, not here.
                                //
                                // The `!mutation` gate is load-bearing: resuming a
                                // *mutating* NotFound unrewritten would run the real
                                // syscall against the host path — an `O_CREAT` open
                                // could create a file under `host_root` and trip the
                                // end-of-run "touched the host" invariant — so a
                                // mutating NotFound, and every non-NotFound refusal,
                                // is still emulated as a denial via `denial()`.
                                let mutation = matches!(
                                    umbra_overlay::dispatch(&op),
                                    umbra_overlay::Dispatch::Materialise
                                        | umbra_overlay::Dispatch::Whiteout
                                );
                                if refused.kind == ErrorKind::NotFound && !mutation {
                                    tracer
                                        .resume(ResumeCommand {
                                            thread,
                                            mode: ResumeMode::Syscall,
                                            signal: None,
                                        })
                                        .unwrap();
                                    continue;
                                }
                                let result = EmulatedResult {
                                    outcome: OperationOutcome::Failure(denial(&refused)),
                                    memory_writes: vec![],
                                };
                                DarwinArm64Abi
                                    .emulate_result(&mut registers, &result)
                                    .unwrap();
                                tracer.set_registers(thread, &registers).unwrap();
                                tracer
                                    .resume(ResumeCommand {
                                        thread,
                                        mode: ResumeMode::Syscall,
                                        signal: None,
                                    })
                                    .unwrap();
                                continue;
                            }
                        };
                        // A denial mints no operation: the overlay returns it
                        // before planning (`self.planned` stays `None`), so
                        // `prepare` would refuse with "resolve must precede
                        // prepare". Mirror the supervisor
                        // (crates/umbra-supervisor/src/events.rs) and answer the
                        // tracee directly — emulate the errno, install the
                        // registers, resume, mint no operation. A whiteout-hidden
                        // path (e.g. dirfd_rename looking up the removed name)
                        // now arrives here as `Deny`; genuinely-absent and
                        // non-NotFound refusals still arrive as `Err` above.
                        if let ResolvedAction::Deny(errno) = action {
                            let result = EmulatedResult {
                                outcome: OperationOutcome::Failure(errno),
                                memory_writes: vec![],
                            };
                            DarwinArm64Abi
                                .emulate_result(&mut registers, &result)
                                .unwrap();
                            tracer.set_registers(thread, &registers).unwrap();
                            tracer
                                .resume(ResumeCommand {
                                    thread,
                                    mode: ResumeMode::Syscall,
                                    signal: None,
                                })
                                .unwrap();
                            continue;
                        }
                        let prepared = overlay
                            .prepare(OperationId(Uuid::new_v4()), &action)
                            .unwrap();
                        // Emulated results never reach the kernel: the tracer
                        // steps the PC past the `svc` and synthesises the exit,
                        // so the same transaction bookkeeping still applies.
                        let physical = match &prepared.action {
                            ResolvedAction::Rewrite(physical) => physical,
                            ResolvedAction::Emulate(result) => {
                                for write in &result.memory_writes {
                                    tracer
                                        .write_memory(task, write.address, &write.bytes)
                                        .unwrap();
                                }
                                DarwinArm64Abi
                                    .emulate_result(&mut registers, result)
                                    .unwrap();
                                tracer.set_registers(thread, &registers).unwrap();
                                pending = Some((prepared, op));
                                tracer
                                    .resume(ResumeCommand {
                                        thread,
                                        mode: ResumeMode::Syscall,
                                        signal: None,
                                    })
                                    .unwrap();
                                continue;
                            }
                            other => panic!("MISSED {case}: {other:?}"),
                        };
                        let plan = tracer.prepare_physical(thread, physical.clone()).unwrap();
                        // Any two-operand syscall must rewrite both endpoints
                        // into separate buffers; one rewritten endpoint would
                        // mutate outside the shadow.
                        if plan.arguments.len() == 2 {
                            let slots = plan.arguments.iter().map(|a| a.index).collect::<Vec<_>>();
                            assert_eq!(
                                slots,
                                vec![1, 3],
                                "MISSED {case}: two-operand rewrite must use x1 and x3"
                            );
                            assert_ne!(
                                plan.arguments[0].value, plan.arguments[1].value,
                                "MISSED {case}: operands share a scratch buffer"
                            );
                            assert_eq!(plan.memory_writes.len(), 2);
                            for write in &plan.memory_writes {
                                assert!(
                                    write.bytes.starts_with(b"/") && write.bytes.ends_with(&[0]),
                                    "MISSED {case}: operand is not an absolute C string"
                                );
                            }
                        }
                        for write in &plan.memory_writes {
                            tracer
                                .write_memory(task, write.address, &write.bytes)
                                .unwrap();
                        }
                        DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                        tracer.set_registers(thread, &registers).unwrap();
                        pending = Some((prepared, op));
                    }
                    // Startup traffic: left pointing at the host, unrewritten.
                    _ => {}
                }
                Some(thread)
            }
            TraceEvent::SyscallExit {
                thread, outcome, ..
            } => {
                if let Some((prepared, op)) = pending.take() {
                    overlay
                        .observe_result(prepared.operation_id, &outcome)
                        .unwrap();
                    match &outcome {
                        OperationOutcome::Success { return_value } => {
                            overlay.commit(prepared.operation_id).unwrap();
                            if let FsOp::Open {
                                dir, path, flags, ..
                            } = &op
                            {
                                if flags.directory {
                                    let path = logical(*dir, path, &process.fds);
                                    let anchored = overlay
                                        .resolve_path(&process, DirRef::Cwd, &path, false)
                                        .unwrap();
                                    let stat = overlay.stat(&anchored, true).unwrap();
                                    process.fds.insert(
                                        TracedFd(*return_value as i32),
                                        FdState {
                                            object: stat.object_id,
                                            logical_path: Some(path),
                                            directory: true,
                                            flags: *flags,
                                            offset: 0,
                                        },
                                    );
                                }
                            }
                        }
                        OperationOutcome::Failure(errno) => {
                            // A refused lookup is a real outcome for this case:
                            // the fixture proves the renamed-away source is gone.
                            overlay
                                .abort(
                                    prepared.operation_id,
                                    &AbortReason::Failed(UmbraError::new(
                                        ErrorKind::NotFound,
                                        "dirfd-rename",
                                        format!("errno {}", errno.0),
                                    )),
                                )
                                .unwrap();
                        }
                    }
                }
                Some(thread)
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected event {other:?}"),
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    tracer
        .terminate(handle, TerminationPolicy::Immediate)
        .unwrap();

    verify(
        &mut overlay,
        &process,
        &records.lock().unwrap(),
        &control_root,
        &observed,
    );
    assert!(
        !host_root.exists(),
        "MISSED {case}: the case touched the host"
    );
    assert_eq!(
        std::fs::read_dir(&base_root).unwrap().count(),
        0,
        "MISSED {case}: the immutable base changed"
    );
}

/// `renameat` across two directory descriptors, resolved and executed through
/// the overlay. Both path operands must be rewritten to distinct scratch
/// buffers holding shadow paths; the dirfd registers stay as the tracee set
/// them, because a qualified physical path is absolute and the kernel ignores
/// the anchor for an absolute path. The fixture prints `CAPTURED dirfd-rename`
/// once its own end-to-end reads agree.
#[test]
fn dirfd_rename() {
    overlay_fixture(
        "dirfd-rename",
        "--dirfd-rename",
        DIRFD_ROOT,
        |overlay, process, records, _control, observed| {
            assert_eq!(
                observed
                    .iter()
                    .filter(|op| matches!(op, FsOp::Rename { .. }))
                    .count(),
                2,
                "MISSED dirfd-rename: both rename legs must run"
            );
            let destination = BytePath::new([DIRFD_ROOT, b"/db/c"].concat()).unwrap();
            let anchored = overlay
                .resolve_path(process, DirRef::Cwd, &destination, false)
                .unwrap();
            let stat = overlay.stat(&anchored, true).unwrap();
            assert_eq!(stat.kind, ObjectKind::File);
            assert_eq!(stat.len, 6, "MISSED dirfd-rename: destination bytes");
            let source = BytePath::new([DIRFD_ROOT, b"/da/a"].concat()).unwrap();
            let absent = overlay
                .resolve_path(process, DirRef::Cwd, &source, false)
                .and_then(|p| overlay.stat(&p, true));
            assert!(
                absent.is_err(),
                "MISSED dirfd-rename: renamed source is still visible"
            );
            assert!(
                records.windows(3).any(|w| matches!(
                    w[0].payload,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Rename { .. }
                    }
                ) && matches!(
                    w[1].payload,
                    JournalPayload::ObservedResult { .. }
                ) && matches!(w[2].payload, JournalPayload::Commit)),
                "MISSED dirfd-rename: no journaled rename transaction"
            );
        },
    )
}

/// Logical symlinks end to end: `symlink`/`symlinkat` create control metadata
/// rather than a filesystem symlink, `readlink`/`readlinkat` are answered from
/// that metadata with exact bytes and no NUL, a no-follow stat reports a link
/// rather than the stored placeholder, and a two-link loop exhausts the
/// expansion bound as ELOOP. The fixture prints `CAPTURED symlink-cycle` once
/// all of that agrees from inside the tracee.
#[test]
fn symlink_cycle() {
    overlay_fixture(
        "symlink-cycle",
        "--symlink-cycle",
        SYMLINK_ROOT,
        |_overlay, _process, records, control, observed| {
            let kinds =
                |mut f: Box<dyn FnMut(&FsOp) -> bool>| observed.iter().filter(|op| f(op)).count();
            // symlink(57) and symlinkat(474); readlink(58) and readlinkat(473).
            assert_eq!(
                kinds(Box::new(|op| matches!(op, FsOp::Symlink { .. }))),
                5,
                "MISSED symlink-cycle: both create forms must run"
            );
            assert_eq!(
                kinds(Box::new(|op| matches!(op, FsOp::ReadLink { .. }))),
                3,
                "MISSED symlink-cycle: both read forms must run"
            );
            assert_eq!(
                kinds(Box::new(|op| matches!(
                    op,
                    FsOp::Stat { follow: false, .. }
                ))),
                1,
                "MISSED symlink-cycle: the no-follow stat must run"
            );
            // Target metadata holds literal tracee-visible bytes. A
            // physicalized target would name the temporary shadow root, which
            // is not in this set; the absolute one is a logical path.
            let mut stored = std::fs::read_dir(control.join("symlinks/targets"))
                .unwrap()
                .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>();
            stored.sort();
            let mut expected = vec![
                b"data-\xff".to_vec(),
                b"data".to_vec(),
                [SYMLINK_ROOT, b"/sl/data"].concat(),
                b"loop1".to_vec(),
                b"loop2".to_vec(),
            ];
            expected.sort();
            assert_eq!(
                stored, expected,
                "MISSED symlink-cycle: stored targets are not the literal bytes"
            );
            assert!(
                records.windows(3).any(|w| matches!(
                    w[0].payload,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Symlink { .. }
                    }
                ) && matches!(
                    w[1].payload,
                    JournalPayload::ObservedResult { .. }
                ) && matches!(w[2].payload, JournalPayload::Commit)),
                "MISSED symlink-cycle: no journaled symlink transaction"
            );
        },
    )
}
