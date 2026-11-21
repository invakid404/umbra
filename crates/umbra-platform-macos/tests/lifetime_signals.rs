#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! #205: the platform provider's lifetime is its owner's, not a terminal's.
//!
//! In a terminal the provider shares the job's process group, so a hangup, ^C,
//! ^\ or `kill -pgid` reaches it. Ending there, without its drop, leaves its
//! debugserver holding the program; ending that debugserver later hands the
//! program to launchd as a zombie nobody collects (#198, #205). The provider
//! therefore survives SIGHUP, SIGINT, SIGQUIT and SIGTERM where it inherited
//! their default action, and ends with its owner.
//!
//! These drive the real provider binary through the production sandboxed
//! launch path.
mod zombies;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use umbra_core::*;
use umbra_platform::PlatformSession;

const SIGNALS: [(&str, i32); 4] = [
    ("SIGHUP", libc::SIGHUP),
    ("SIGINT", libc::SIGINT),
    ("SIGQUIT", libc::SIGQUIT),
    ("SIGTERM", libc::SIGTERM),
];

fn path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}

/// The fixture, or `None` (skip) outside required integration.
fn fixture(case: &str) -> Option<PathBuf> {
    match std::env::var_os("UMBRA_TEST_FIXTURE_PATH") {
        Some(f) => Some(PathBuf::from(f)),
        None => {
            assert!(
                std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
                "required integration needs UMBRA_TEST_FIXTURE_PATH"
            );
            eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH");
            None
        }
    }
}

fn connect() -> PlatformSession {
    let descriptor = provider::ProviderDescriptor {
        id: "macos".into(),
        role: "platform".into(),
        protocol_version: provider::PROTOCOL_VERSION,
        executable: path(Path::new(env!("CARGO_BIN_EXE_umbra-platform-macos"))),
        capabilities: Default::default(),
        options: serde_json::to_vec(&umbra_platform_macos::Options::default()).unwrap(),
    };
    umbra_platform::provider::connect(&descriptor, 5000).unwrap()
}

/// The provider: this process's child running the provider image.
fn provider_pid() -> i32 {
    // SAFETY: getpid has no arguments.
    let me = unsafe { libc::getpid() };
    let mut pids = [0i32; 256];
    // SAFETY: `pids` is writable for its size in bytes.
    let n = unsafe {
        libc::proc_listchildpids(
            me,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as i32,
        )
    };
    assert!(n > 0, "no children of {me}");
    // libproc returns the number of pids it wrote.
    let found: Vec<i32> = pids[..(n as usize).min(pids.len())]
        .iter()
        .copied()
        .filter(|&pid| pid > 0)
        .filter(|&pid| {
            let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            // SAFETY: `buf` is writable for its length.
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            len > 0 && buf[..len as usize].ends_with(b"/umbra-platform-macos")
        })
        .collect();
    assert_eq!(found.len(), 1, "provider children: {found:?}");
    found[0]
}

/// Alive and not a zombie.
fn alive(pid: i32) -> bool {
    // SAFETY: plain old data, filled below.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is writable for `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    got == size && info.pbi_status != libc::SZOMB
}

fn sandbox(root: &Path) -> SandboxRequirement {
    let literal = format!("\"{}\"", root.display());
    let profile = include_str!("../../../experiments/seatbelt/umbra.sb")
        .replace("{{UMBRA_RUN_ROOT}}", &literal);
    SandboxRequirement::Required(
        SandboxProfile::new(SEATBELT_PROFILE_FORMAT, profile.into_bytes(), path(root)).unwrap(),
    )
}

/// Launch the fixture through the provider; it is held at its exec stop when
/// this returns. Returns the root handle and the program's pid.
fn launch_held(
    platform: &mut PlatformSession,
    fixture: &Path,
    shadow: &Path,
) -> (ProcessHandle, i32) {
    let root = platform
        .control
        .launch(LaunchSpec {
            executable: path(fixture),
            argv: vec![
                fixture.as_os_str().as_bytes().to_vec(),
                b"open-libc".to_vec(),
                shadow.join("out").as_os_str().as_bytes().to_vec(),
            ],
            environment: vec![],
            cwd: path(Path::new("/tmp")),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: sandbox(shadow),
        })
        .unwrap();
    let pid = root.0 .0.native_id as i32;
    (root, pid)
}

/// `kp_proc.p_sigignore` from `sysctl(KERN_PROC_PID)`: offset 232 of the
/// 648-byte arm64 `kinfo_proc` (measured with `offsetof`; the `libc` crate has
/// no Apple `kinfo_proc`). Validated on a known case before use.
fn sigignore_of(pid: i32) -> Option<u32> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; 648];
    let mut len = buf.len();
    // SAFETY: `buf` is writable for `len` bytes; the name has 4 components.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == buf.len()).then(|| u32::from_ne_bytes(buf[232..236].try_into().unwrap()))
}

fn bit(signal: i32) -> u32 {
    1 << (signal - 1)
}

/// The instrument on a known case: a native child with SIGUSR1 ignored.
fn check_instrument() {
    let mut known = std::process::Command::new("/bin/sleep");
    known.arg("30");
    // SAFETY: only async-signal-safe libc between fork and exec.
    unsafe {
        known.pre_exec(|| {
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut known = known.spawn().unwrap();
    let read = sigignore_of(known.id() as i32);
    let _ = known.kill();
    let _ = known.wait();
    let read = read.expect("p_sigignore of a known child");
    assert!(
        read & bit(libc::SIGUSR1) != 0 && read & bit(libc::SIGUSR2) == 0,
        "p_sigignore read {read:#x} does not show the known SIGUSR1 ignore"
    );
}

/// F10 (#205 A2a), in two ordered parts in one process.
///
/// (i) The provider, with a live owner and no launched program, is sent each
/// signal; it must stay alive and keep answering, then end when its owner goes.
/// Before the fix it dies of the first one.
///
/// (ii) Runs only if (i) showed the provider survives every signal; otherwise
/// it is skipped with the reason recorded. It sends the signals to the provider
/// while it tears a live session down, and the teardown must confirm with no
/// timeout: an interrupted call must never read as a closed tracer connection.
/// Gating (ii) on (i) keeps it from ever holding a session in a provider that
/// dies, which would leave its debugserver holding the program.
#[test]
fn provider_survives_terminal_signals_while_its_owner_lives() {
    let Some(fixture) = fixture("provider_survives_terminal_signals_while_its_owner_lives") else {
        return;
    };
    let since = zombies::now();
    let mut failures = Vec::new();
    // Part (i).
    let mut platform = connect();
    let provider = provider_pid();
    let unknown = ProcessHandle(TaskId(TaskIdentity {
        native_id: i64::from(i32::MAX - 1) as _,
        generation: 1,
    }));
    for (name, signal) in SIGNALS {
        // SAFETY: our own child.
        unsafe { libc::kill(provider, signal) };
        std::thread::sleep(Duration::from_millis(200));
        if !alive(provider) {
            failures.push(format!("provider did not survive {name}"));
            break;
        }
        match platform
            .control
            .terminate(unknown, TerminationPolicy::Immediate)
        {
            Err(e) if e.context.contains("unknown root") => {}
            other => {
                failures.push(format!(
                    "provider stopped answering after {name}: {other:?}"
                ));
                break;
            }
        }
    }
    drop(platform);
    let end = Instant::now() + Duration::from_secs(5);
    while alive(provider) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    if alive(provider) {
        failures.push("provider outlived its owner".into());
    }
    eprintln!("part (i): {failures:?}");
    // Part (ii).
    if failures.is_empty() {
        let shadow = tempfile::tempdir().unwrap();
        let mut platform = connect();
        let provider = provider_pid();
        let (root, pid) = launch_held(&mut platform, &fixture, shadow.path());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signaller = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut sent = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for (_, signal) in SIGNALS {
                        // SAFETY: our own child.
                        unsafe { libc::kill(provider, signal) };
                        sent += 1;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                sent
            })
        };
        let started = Instant::now();
        let result = platform
            .control
            .terminate(root, TerminationPolicy::Immediate);
        let elapsed = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let sent = signaller.join().unwrap();
        eprintln!("part (ii): terminate {result:?} in {elapsed:?} under {sent} signals");
        if let Err(e) = result {
            failures.push(format!("teardown under signals failed: {e}"));
        }
        if elapsed >= Duration::from_millis(1500) {
            failures.push(format!(
                "teardown under signals reached the bound: {elapsed:?}"
            ));
        }
        if !alive(provider) {
            failures.push("provider died during the teardown".into());
        }
        let closure = platform.shutdown();
        if closure.error.is_some() || !closure.tree_terminated {
            failures.push(format!("closure: {:?}", closure.error));
        }
        let left = zombies::launchd_zombies(&["umbra-test-child"], since, &[(pid, None)]);
        if !left.is_empty() {
            failures.push(format!("launchd-parented zombies left: {left:?}"));
        }
    } else {
        eprintln!("SKIP part (ii): {}", failures[0]);
    }
    assert!(failures.is_empty(), "{failures:?}");
}

/// F12 (#205 C-1), a regression guard: it passes before and after the fix.
///
/// A program launched through umbra starts with the signal dispositions it
/// would have natively. Where the caller left SIGHUP and SIGINT ignored
/// (`nohup`, or a background command of a non-interactive shell), the program
/// still has them ignored; where the caller left the default, so does the
/// program. The provider's own handlers (A2a) are installed only over the
/// default, and a caught handler resets to the default at exec.
///
/// Each case runs in a re-executed copy of this test binary started with the
/// case's dispositions, so this harness process is never changed.
#[test]
fn provider_keeps_inherited_ignored_signals() {
    if let Some(case) = std::env::var_os("UMBRA_FIX205_DISPOSITION_CASE") {
        return disposition_case(&case.to_string_lossy());
    }
    let Some(fixture) = fixture("provider_keeps_inherited_ignored_signals") else {
        return;
    };
    let since = zombies::now();
    check_instrument();
    // Native control: the fixture spawned directly with both ignored.
    let mut native = std::process::Command::new(&fixture);
    native.args(["--spin", "unused"]);
    // SAFETY: only async-signal-safe libc between fork and exec.
    unsafe {
        native.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut native = native.spawn().unwrap();
    let native_mask = sigignore_of(native.id() as i32);
    let _ = native.kill();
    let _ = native.wait();
    let native_mask = native_mask.expect("native p_sigignore");
    eprintln!("native control: p_sigignore {native_mask:#x}");
    assert!(
        native_mask & bit(libc::SIGHUP) != 0 && native_mask & bit(libc::SIGINT) != 0,
        "native control: {native_mask:#x}"
    );
    for (case, ignore) in [("ignored", true), ("default", false)] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "provider_keeps_inherited_ignored_signals",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("UMBRA_FIX205_DISPOSITION_CASE", case);
        if ignore {
            // SAFETY: only async-signal-safe libc between fork and exec.
            unsafe {
                child.pre_exec(|| {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let out = child.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        eprintln!("{case}:\n{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{case}: {}", out.status);
        let mask = text
            .lines()
            // libtest may print the test name on the same line first.
            .find_map(|l| l.split_once("PROGRAM SIGIGNORE ").map(|(_, hex)| hex))
            .and_then(|hex| u32::from_str_radix(hex.trim(), 16).ok())
            .unwrap_or_else(|| panic!("{case}: no reading"));
        let both = bit(libc::SIGHUP) | bit(libc::SIGINT);
        if ignore {
            assert_eq!(
                mask & both,
                both,
                "{case}: the program lost an inherited ignore"
            );
        } else {
            assert_eq!(mask & both, 0, "{case}: the program inherited an ignore");
        }
    }
    let left = zombies::launchd_zombies(&["umbra-test-child"], since, &[]);
    assert!(left.is_empty(), "launchd-parented zombies left: {left:?}");
}

/// One F12 case, in the re-executed child: launch through the provider, read
/// the held program's ignored set, end the run.
fn disposition_case(case: &str) {
    let fixture = PathBuf::from(std::env::var_os("UMBRA_TEST_FIXTURE_PATH").unwrap());
    let shadow = tempfile::tempdir().unwrap();
    let mut platform = connect();
    let (root, pid) = launch_held(&mut platform, &fixture, shadow.path());
    let mask = sigignore_of(pid);
    platform
        .control
        .terminate(root, TerminationPolicy::Immediate)
        .unwrap();
    let closure = platform.shutdown();
    assert!(closure.error.is_none(), "{case}: {:?}", closure.error);
    let mask = mask.unwrap_or_else(|| panic!("{case}: p_sigignore of {pid}"));
    println!("PROGRAM SIGIGNORE {mask:x}");
}
