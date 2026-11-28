//! #193: `umbra run` started in launchd's session (SID 1), on the production
//! path, and today's terminal semantics, pinned.
//!
//! **Where these run.** The two `jobc_` cases check the sampler's selection and
//! its log's line kinds without a run, and run anywhere. Every other case
//! starts real runs, so it needs the qualified host (debugger permission). It
//! runs when `UMBRA_INTEGRATION_REQUIRED` or `UMBRA_TEST_ISOLATION` is set and
//! is skipped otherwise. The `sid1_` cases
//! also need launchd's session: CI's self-hosted runner, or locally a
//! `launchctl submit` of this test binary. Elsewhere they skip, unless
//! `UMBRA_REQUIRE_SID1` is set, which makes a session other than 1 a failure.
//! Build the workspace binaries first. `UMBRA_ISOLATION_BIN_DIR` points the
//! cases at a frozen copy of `umbra` and the providers instead of this build's.
//!
//! **Topology** (the #193 design harness, `dg-56gcxqyw`): a launcher, spawned
//! into its own group G0 (`process_group(0)`, the harness shape), starts a
//! passive sentinel and `umbra run` in G0. A sampler in its own session records
//! G0's `pg_jobc` from each `KERN_PROC_PGRP` listing's entries whose own group
//! is G0. An entry the kernel listed in G0 but filled after it left, such as
//! the guardian just after its `setsid`, is logged as moved and never read
//! (#209). Before #193, a SID-1 run's exits were counted against G0:
//! P1/P2 SIGHUPed umbra, its providers and the sentinel; P3 drove G0's
//! `pg_jobc` to 0; P4 (a stopped provider, then the supervisor killed) SIGHUPed
//! the providers and the sentinel.
//!
//! **Ownership (L39).** The harness signals only pids it discovered by
//! parentage from its own launcher, and cleans up by the run's session S (the
//! guardian's), limited to processes that started before an instant at which
//! the guardian was seen as the relay's child (`kill-session`, fixture source).
//! Nothing is ever selected by name.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIGHUP: i32 = 1;
const SIGKILL: i32 = 9;
const EPERM: i32 = 1;
/// `p_stat` of a zombie (bsd/sys/proc.h `SZOMB`).
const SZOMB: i32 = 5;

fn set(name: &str) -> bool {
    std::env::var_os(name).filter(|v| !v.is_empty()).is_some()
}

fn qualified() -> bool {
    if set("UMBRA_INTEGRATION_REQUIRED") || set("UMBRA_TEST_ISOLATION") {
        return true;
    }
    eprintln!("SKIP: set UMBRA_TEST_ISOLATION on a host with debugger permission");
    false
}

fn in_launchd_session() -> bool {
    let sid = own_session();
    if sid == 1 {
        return true;
    }
    assert!(
        !set("UMBRA_REQUIRE_SID1"),
        "UMBRA_REQUIRE_SID1 is set, but this test runs in session {sid}, not launchd's (1)"
    );
    eprintln!(
        "SKIP: session {sid} is not launchd's; run in SID 1 (the CI runner, or launchctl submit)"
    );
    false
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Built {
    fixture: PathBuf,
    observer: PathBuf,
}

/// The C fixture and the signal observer, compiled once per test process.
fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("umbra-isolation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let compile = |source: &str, output: &Path, extra: &[&str]| {
            let source = repository().join("experiments/fixtures").join(source);
            let built = Command::new("clang")
                .args(["-arch", "arm64", "-O1", "-Wall", "-Wextra", "-Werror"])
                .args(extra)
                .arg(&source)
                .arg("-o")
                .arg(output)
                .output()
                .expect("clang is part of the Command Line Tools this host already requires");
            assert!(
                built.status.success(),
                "compiling {}: {}",
                source.display(),
                String::from_utf8_lossy(&built.stderr)
            );
        };
        let fixture = dir.join("umbra-isolation-fixture");
        compile("umbra-isolation-fixture.c", &fixture, &[]);
        let observer = dir.join("umbra-slate-signals.dylib");
        compile("umbra-slate-signals.c", &observer, &["-dynamiclib"]);
        Built { fixture, observer }
    })
}

fn fixture() -> &'static Path {
    &built().fixture
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct Proc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    sid: i32,
    stat: i32,
    start_us: i64,
    comm: String,
}

fn procs() -> Vec<Proc> {
    let out = Command::new(fixture()).arg("procs").output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(7, ' ').collect();
            (f.len() == 7).then(|| Proc {
                pid: f[0].parse().unwrap(),
                ppid: f[1].parse().unwrap(),
                pgid: f[2].parse().unwrap(),
                sid: f[3].parse().unwrap(),
                stat: f[4].parse().unwrap(),
                start_us: f[5].parse().unwrap(),
                comm: f[6].to_string(),
            })
        })
        .collect()
}

/// #205 per-test guard: launchd-parented zombies attributable to this test.
///
/// When a debugger process ends while its program has started exiting (or is
/// held at a breakpoint), the kernel hands the program to launchd as a zombie
/// nobody ever collects; it stays until a reboot and counts against the
/// user's process limit. Attribution follows the owner scope's rule: the
/// fixture's image name (the 16-character `ucomm` of `umbra-isolation-fixture`,
/// the traced program in these cases) and a start time inside the test's
/// window, plus any recorded (pid, start) of this run. Counting only: nothing
/// is signalled on this basis.
fn launchd_zombies(since_us: i64, recorded: &[(i32, i64)]) -> Vec<Proc> {
    procs()
        .into_iter()
        .filter(|p| {
            p.stat == SZOMB
                && p.ppid == 1
                && ((p.start_us >= since_us && p.comm.starts_with("umbra-isolation-"))
                    || recorded
                        .iter()
                        .any(|(pid, start)| *pid == p.pid && *start == p.start_us))
        })
        .collect()
}

fn own_session() -> i32 {
    let me = std::process::id() as i32;
    procs()
        .into_iter()
        .find(|p| p.pid == me)
        .expect("this process in the table")
        .sid
}

/// `(pid, start)` of a live (not zombie) process, from the census.
fn identity(pid: i32) -> Option<(i32, i64)> {
    procs()
        .into_iter()
        .find(|p| p.pid == pid && p.stat != SZOMB)
        .map(|p| (p.pid, p.start_us))
}

/// Whether this exact process (pid and start) is still live, not a zombie.
fn identity_live((pid, start): (i32, i64)) -> bool {
    procs()
        .iter()
        .any(|p| p.pid == pid && p.start_us == start && p.stat != SZOMB)
}

/// Signal one pid this harness discovered by parentage.
fn signal(pid: i32, sig: &str) {
    let ok = Command::new("/bin/kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(ok.success(), "kill -{sig} {pid}");
}

/// A process's argv as `ps` prints it (read-only).
fn args_of(pid: i32) -> String {
    let out = Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// debugserver's startup kill-on-error flag. umbra never passes it, in any
/// mode: the kill on a lost connection comes from the `QSetDetachOnError:0`
/// packet `umbra-platform-macos` sends and verifies on every connection
/// (#193's O4 re-ruling). These cases pin that the argv stays master's.
const PIN: &str = "--kill-on-error";

fn kv(line: &str, key: &str) -> Option<i32> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .and_then(|v| v.parse().ok())
}

/// `umbra` and the provider executables: this build's, or a frozen copy.
fn bin_dir() -> PathBuf {
    match std::env::var_os("UMBRA_ISOLATION_BIN_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_BIN_EXE_umbra"))
            .parent()
            .unwrap()
            .to_path_buf(),
    }
}

fn registry(dir: &Path, platform_timeout_ms: u64) -> PathBuf {
    let bin_dir = bin_dir();
    let descriptor = |package: &str, capabilities: &[&str], options: Vec<u8>| {
        let mut value: Value = serde_json::from_slice(
            &std::fs::read(
                repository()
                    .join("crates")
                    .join(package)
                    .join("provider.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let executable = bin_dir.join(package);
        assert!(
            executable.is_file(),
            "build workspace binaries first: {}",
            executable.display()
        );
        value["executable"] = json!(executable.as_os_str().as_bytes());
        value["capabilities"] = json!(capabilities);
        value["options"] = json!(options);
        value
    };
    let store = dir.join("store");
    std::fs::create_dir(&store).unwrap();
    let registry = json!({"timeout_ms": 25_000, "providers": {
        "platform": descriptor("umbra-platform-macos",
            &["sandboxed-stopped-launch-v1", "experimental-syscall-rewrite-v1"],
            serde_json::to_vec(&json!({"timeout_ms": platform_timeout_ms})).unwrap()),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor("umbra-storage-local", &["local-development-v1", "experimental-open-rewrite-v1"],
            serde_json::to_vec(store.as_os_str().as_bytes()).unwrap()),
    }});
    let path = dir.join("registry.json");
    std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
    path
}

fn umbra_command(dir: &Path, platform_timeout_ms: u64, tracee: &[&str]) -> Vec<std::ffi::OsString> {
    let workspace = dir.join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let mut argv: Vec<std::ffi::OsString> = vec![
        bin_dir().join("umbra").into(),
        "run".into(),
        "--registry".into(),
        registry(dir, platform_timeout_ms).into(),
        "--workspace".into(),
        workspace.into(),
        "--experimental".into(),
        "--local-dev".into(),
        "--".into(),
        fixture().into(),
    ];
    argv.extend(tracee.iter().map(Into::into));
    argv
}

/// A per-case directory; kept under `UMBRA_ISOLATION_EVIDENCE` when set.
fn case_dir(case: &str) -> (PathBuf, Option<tempfile::TempDir>) {
    match std::env::var_os("UMBRA_ISOLATION_EVIDENCE").filter(|v| !v.is_empty()) {
        Some(root) => {
            std::fs::create_dir_all(&root).unwrap();
            let dir = tempfile::Builder::new()
                .prefix(&format!("{case}-"))
                .tempdir_in(root)
                .unwrap()
                .keep();
            (dir, None)
        }
        None => {
            let dir = tempfile::Builder::new()
                .prefix(&format!("{case}-"))
                .tempdir()
                .unwrap();
            (dir.path().to_path_buf(), Some(dir))
        }
    }
}

/// The run tree, found by parentage from this harness's own launcher.
#[derive(Debug, Default)]
struct Tree {
    relay: Option<i32>,
    guardian: Option<i32>,
    supervisor: i32,
    cleanup_owner: i32,
    platform: i32,
    journal: i32,
    storage: i32,
    debugservers: Vec<i32>,
    root: i32,
    root_tracer: i32,
    rows: Vec<Proc>,
}

impl Tree {
    fn sid(&self, pid: i32) -> i32 {
        self.rows
            .iter()
            .find(|p| p.pid == pid)
            .map_or(-1, |p| p.sid)
    }
}

fn managed_worker(rows: &[Proc], guardian: i32) -> i32 {
    let workers: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.ppid == guardian
                && row.comm == "umbra"
                && ["umbra-journal", "umbra-storage"].iter().all(|prefix| {
                    rows.iter()
                        .filter(|child| child.ppid == row.pid && child.comm.starts_with(prefix))
                        .count()
                        == 1
                })
        })
        .collect();
    assert_eq!(
        workers.len(),
        1,
        "one worker must own journal and storage: {rows:?}"
    );
    workers[0].pid
}

#[test]
fn managed_worker_discovery_ignores_sibling_enumeration_order() {
    let process = |pid, ppid, comm: &str| Proc {
        pid,
        ppid,
        pgid: 20,
        sid: 20,
        stat: 2,
        start_us: i64::from(pid),
        comm: comm.into(),
    };
    let rows = vec![
        process(21, 20, "umbra"),
        process(22, 20, "umbra"),
        process(23, 21, "umbra-platform-macos"),
        process(24, 22, "umbra-journal-file"),
        process(25, 22, "umbra-storage-local"),
    ];
    assert_eq!(
        managed_worker(&rows, 20),
        22,
        "the cleanup owner is not the worker"
    );
}

#[derive(Debug)]
struct Outcome {
    /// The launcher's raw wait status of `umbra`, if it reported one.
    status: Option<i32>,
    /// Live processes of this run after it ended (recorded pids, or session S).
    survivors: Vec<Proc>,
    /// Launchd-parented zombies of this run after cleanup (#205).
    zombies: Vec<Proc>,
    /// Signals the sentinel in the caller's group received: (signal, sender).
    sentinel: Vec<(i32, i32)>,
    /// The sampler's readings of the caller's group's `pg_jobc`.
    jobc: Jobc,
    /// Observer deliveries: (image, signal, sender).
    delivered: Vec<(String, i32, i32)>,
    /// Managed parent collection observations from this run's private file.
    record: Vec<Value>,
    stderr: String,
}

impl Outcome {
    fn exit_code(&self) -> Option<i32> {
        self.status
            .filter(|s| s & 0x7f == 0)
            .map(|s| (s >> 8) & 0xff)
    }
    fn term_signal(&self) -> Option<i32> {
        self.status
            .filter(|s| s & 0x7f != 0 && s & 0x7f != 0x7f)
            .map(|s| s & 0x7f)
    }
}

struct Run {
    dir: PathBuf,
    _temp: Option<tempfile::TempDir>,
    launcher: Child,
    sampler: Option<Child>,
    g0: Option<i32>,
    lines: Arc<Mutex<Vec<String>>>,
    /// (pid, start time) of every process discovered for this run.
    recorded: Vec<(i32, i64)>,
    /// S and an instant when the guardian was seen as the relay's child.
    session: Option<(i32, i64)>,
    finished: Option<Outcome>,
    /// When the run started: the window for the zombie count (#205).
    since_us: i64,
    /// The supervisor and the debugservers, as (pid, start), from `discover`
    /// (#205: the cleanup ends the supervisor first and waits on these).
    supervisor: Option<(i32, i64)>,
    debuggers: Vec<(i32, i64)>,
}

impl Run {
    /// `fresh`: the launcher leads a new session instead of a new group.
    fn start(
        case: &str,
        fresh: bool,
        observe: bool,
        platform_timeout_ms: u64,
        tracee: &[&str],
    ) -> Run {
        let (dir, temp) = case_dir(case);
        let since_us = now_us();
        let mut command = Command::new(fixture());
        command
            .arg("launch")
            .arg(&dir)
            .arg(if fresh { "fresh" } else { "group" })
            .arg("--")
            .args(umbra_command(&dir, platform_timeout_ms, tracee))
            .env("UMBRA_FIX_271_EVIDENCE_FILE", dir.join("managed.jsonl"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if !fresh {
            command.process_group(0);
        }
        if case == "P1" {
            command.env("UMBRA_TEARDOWN_LOG", "1");
        }
        #[cfg(feature = "test-managed-session")]
        if case == "OWN-P4" {
            command.env("UMBRA_TEST_MANAGED_SESSION", "1");
        }
        if observe {
            std::fs::create_dir(dir.join("signals")).unwrap();
            command
                .env("DYLD_INSERT_LIBRARIES", &built().observer)
                .env("UMBRA_SLATE_SIGNAL_DIR", dir.join("signals"));
        }
        let mut launcher = command.spawn().unwrap();
        let g0 = (!fresh).then_some(launcher.id() as i32);
        let sampler = g0.map(|g| {
            Command::new(fixture())
                .arg("jobc")
                .arg(g.to_string())
                .arg(dir.join("jobc.log"))
                .arg(dir.join("jobc.stop"))
                .arg("120")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        });
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stderr = launcher.stderr.take().unwrap();
        let sink = lines.clone();
        let log = dir.join("stderr.log");
        std::thread::spawn(move || {
            let mut file = std::fs::File::create(log).ok();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(f) = file.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{} {line}", now_us());
                }
                sink.lock().unwrap().push(line);
            }
        });
        Run {
            dir,
            _temp: temp,
            launcher,
            sampler,
            g0,
            lines,
            recorded: Vec::new(),
            session: None,
            finished: None,
            since_us,
            supervisor: None,
            debuggers: Vec::new(),
        }
    }

    fn line(&self, needle: &str, timeout: Duration) -> String {
        let end = Instant::now() + timeout;
        loop {
            if let Some(l) = self
                .lines
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.contains(needle))
            {
                return l.clone();
            }
            assert!(
                Instant::now() < end,
                "no {needle:?} within {timeout:?}; stderr so far:\n{}",
                self.lines.lock().unwrap().join("\n")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn g0(&self) -> i32 {
        self.g0.expect("a group-mode run")
    }

    /// Find the run tree by parentage from the launcher's `umbra` child.
    fn discover(&mut self) -> Tree {
        let umbra = kv(&self.line("ISO LAUNCH", Duration::from_secs(10)), "umbra").unwrap();
        let ready = self.line("ISO READY", Duration::from_secs(30));
        let root = kv(&ready, "pid").unwrap();
        let before = now_us();
        let rows = procs();
        // Record the actual descendant tree before any topology assertion can fail.
        // Terminal runs share a session with their launcher, so session-wide
        // cleanup would include processes that this run does not own.
        let mut descendants = std::collections::BTreeSet::from([umbra]);
        loop {
            let count = descendants.len();
            for row in &rows {
                if descendants.contains(&row.ppid) {
                    descendants.insert(row.pid);
                }
            }
            if count == descendants.len() {
                break;
            }
        }
        self.recorded.extend(
            rows.iter()
                .filter(|row| descendants.contains(&row.pid))
                .map(|row| (row.pid, row.start_us)),
        );
        let kids = |p: i32| {
            rows.iter()
                .filter(move |r| r.ppid == p)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut tree = Tree {
            root,
            ..Tree::default()
        };
        let guardians: Vec<_> = kids(umbra)
            .into_iter()
            .filter(|r| r.comm == "umbra")
            .collect();
        assert_eq!(guardians.len(), 1, "relay must have exactly one guardian");
        match guardians.into_iter().next() {
            Some(guardian) => {
                tree.relay = Some(umbra);
                tree.guardian = Some(guardian.pid);
                tree.supervisor = managed_worker(&rows, guardian.pid);
                let owners: Vec<_> = kids(guardian.pid)
                    .into_iter()
                    .filter(|row| {
                        row.comm == "umbra"
                            && kids(row.pid)
                                .iter()
                                .filter(|child| child.comm.starts_with("umbra-platform"))
                                .count()
                                == 1
                    })
                    .collect();
                assert_eq!(owners.len(), 1, "one cleanup owner must own the platform");
                tree.cleanup_owner = owners[0].pid;
                assert_ne!(tree.cleanup_owner, tree.supervisor);
                assert_eq!(
                    kids(guardian.pid)
                        .iter()
                        .filter(|row| row.comm == "umbra")
                        .count(),
                    2,
                    "guardian must own exactly worker and cleanup owner"
                );
                // The guardian is the relay's unreaped child now and was alive
                // at `before`: S is this run's session at that instant.
                assert!(guardian.start_us < before);
                if guardian.sid == guardian.pid {
                    self.session = Some((guardian.pid, before));
                }
            }
            None => panic!("managed guardian missing from relay children"),
        }
        for r in kids(tree.supervisor)
            .into_iter()
            .chain(kids(tree.cleanup_owner))
        {
            if r.comm.starts_with("umbra-platform") {
                tree.platform = r.pid;
            } else if r.comm.starts_with("umbra-journal") {
                tree.journal = r.pid;
            } else if r.comm.starts_with("umbra-storage") {
                tree.storage = r.pid;
            }
        }
        tree.debugservers = kids(tree.platform)
            .into_iter()
            .filter(|r| r.comm == "debugserver")
            .map(|r| r.pid)
            .collect();
        tree.root_tracer = rows.iter().find(|r| r.pid == root).map_or(0, |r| r.ppid);
        let mut pids = vec![
            umbra,
            tree.supervisor,
            tree.cleanup_owner,
            tree.platform,
            tree.journal,
            tree.storage,
            root,
            tree.root_tracer,
        ];
        pids.extend(tree.guardian);
        pids.extend(&tree.debugservers);
        for p in pids.into_iter().filter(|p| *p > 1) {
            if let Some(r) = rows.iter().find(|r| r.pid == p) {
                self.recorded.push((p, r.start_us));
            }
        }
        let start_of = |pid: i32| {
            rows.iter()
                .find(|r| r.pid == pid)
                .map(|r| (pid, r.start_us))
        };
        self.supervisor = start_of(tree.supervisor);
        self.debuggers = tree
            .debugservers
            .iter()
            .filter_map(|d| start_of(*d))
            .collect();
        assert!(
            tree.platform > 0 && tree.journal > 0 && tree.storage > 0,
            "providers not found: {tree:?}"
        );
        eprintln!(
            "tree: relay={:?} guardian={:?} supervisor={} platform={} journal={} storage={} debugservers={:?} root={} root_tracer={}",
            tree.relay, tree.guardian, tree.supervisor, tree.platform, tree.journal, tree.storage,
            tree.debugservers, tree.root, tree.root_tracer
        );
        tree.rows = rows;
        tree
    }

    /// Wait up to `bound` until no process of this run is left; the survivors.
    fn drained(&self, bound: Duration) -> Vec<Proc> {
        let end = Instant::now() + bound;
        let mut left = self.live_of_this_run();
        while !left.is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            left = self.live_of_this_run();
        }
        left
    }

    /// Record one more pid of this run (found by parentage).
    fn record(&mut self, pid: i32) {
        if let Some(r) = procs().into_iter().find(|r| r.pid == pid) {
            self.recorded.push((pid, r.start_us));
        }
    }

    fn live_of_this_run(&self) -> Vec<Proc> {
        procs()
            .into_iter()
            .filter(|p| p.stat != SZOMB && p.pid != self.launcher.id() as i32)
            .filter(|p| {
                self.recorded
                    .iter()
                    .any(|(pid, start)| *pid == p.pid && *start == p.start_us)
                    || self
                        .session
                        .is_some_and(|(s, at)| p.sid == s && p.start_us < at)
            })
            .collect()
    }

    /// Wait for `umbra` to end (via the launcher), take the readings, clean up.
    fn finish(&mut self, timeout: Duration) -> &Outcome {
        let status_file = self.dir.join("umbra.status");
        let end = Instant::now() + timeout;
        let mut launcher_gone = false;
        while !status_file.exists() && Instant::now() < end {
            if self.launcher.try_wait().unwrap().is_some() {
                launcher_gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The supervisor's teardown and the guardian's sweep finish before the
        // relay reports; a short settle covers the kernel's exit bookkeeping.
        std::thread::sleep(Duration::from_millis(300));
        let status = std::fs::read_to_string(&status_file)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        if status.is_none() && !launcher_gone {
            eprintln!("umbra did not end within {timeout:?}");
        }
        let survivors = self.live_of_this_run();
        self.cleanup();
        // The kernel's exit bookkeeping for anything the cleanup ended.
        std::thread::sleep(Duration::from_millis(300));
        let zombies = launchd_zombies(self.since_us, &self.recorded);
        let sentinel = std::fs::read_to_string(self.dir.join("sentinel.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| Some((kv(l, "sig")?, kv(l, "si_pid")?)))
            .collect();
        let jobc =
            parse_jobc(&std::fs::read_to_string(self.dir.join("jobc.log")).unwrap_or_default());
        let delivered = observer_deliveries(&self.dir.join("signals"));
        let record = managed_record(&self.dir);
        let stderr = self.lines.lock().unwrap().join("\n");
        if let Ok(record_text) = serde_json::to_string_pretty(&record) {
            let _ = std::fs::write(self.dir.join("isolation-record.json"), record_text);
        }
        let outcome = Outcome {
            status,
            survivors,
            zombies,
            sentinel,
            jobc,
            delivered,
            record,
            stderr,
        };
        let _ = std::fs::write(self.dir.join("outcome.txt"), format!("{outcome:#?}\n"));
        eprintln!("outcome: {outcome:#?}");
        self.finished = Some(outcome);
        self.finished.as_ref().unwrap()
    }

    /// Kill what is left of this run (recorded pids; session S before the
    /// observed instant), then stop the launcher, sentinel and sampler.
    fn cleanup(&mut self) {
        // #205: end the run debugger-last. The supervisor goes first, so the
        // platform provider tears its sessions down in order; then wait, capped
        // at 3 s + 4 s per recorded debugserver, until none of them is live.
        // Only then are the survivors killed: killing a debugserver while its
        // program is held hands the program to launchd as a zombie nobody
        // collects.
        let live = self.live_of_this_run();
        if !live.is_empty() {
            let alive = |live: &[Proc], (pid, start): (i32, i64)| {
                live.iter().any(|p| p.pid == pid && p.start_us == start)
            };
            if let Some(supervisor) = self.supervisor.filter(|s| alive(&live, *s)) {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", &supervisor.0.to_string()])
                    .status();
            }
            let cap = Duration::from_secs(3)
                + Duration::from_secs(4) * self.debuggers.len().max(1) as u32;
            let end = Instant::now() + cap;
            while Instant::now() < end {
                let live = self.live_of_this_run();
                if !self.debuggers.iter().any(|d| alive(&live, *d)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        for p in self.live_of_this_run() {
            if self
                .recorded
                .iter()
                .any(|(pid, start)| *pid == p.pid && *start == p.start_us)
            {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", &p.pid.to_string()])
                    .status();
            }
        }
        if let Some((s, at)) = self.session {
            let out = Command::new(fixture())
                .args(["kill-session", &s.to_string(), &at.to_string()])
                .output();
            if let Ok(out) = out {
                let _ = std::fs::write(self.dir.join("cleanup.log"), &out.stdout);
            }
        }
        // The sampler stops, and is reaped, before the launcher is told to
        // exit: the launcher is G0's only member whose parent is outside G0,
        // so its own exit takes G0's `pg_jobc` to 0 while its zombie is still
        // a member. That is the harness ending G0, not the run; sampled, it
        // would read as an orphaned group (CI run 38776568893, P2).
        let _ = std::fs::write(self.dir.join("jobc.stop"), b"");
        if let Some(mut s) = self.sampler.take() {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end && s.try_wait().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            if s.try_wait().unwrap().is_none() {
                let _ = s.kill();
                let _ = s.wait();
            }
        }
        let _ = std::fs::write(self.dir.join("stop"), b"");
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && self.launcher.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.launcher.try_wait().unwrap().is_none() {
            let _ = self.launcher.kill();
            let _ = self.launcher.wait();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if self.finished.is_none() {
            self.cleanup();
        }
    }
}

/// Delivered records of the slate observer: (image, signal, sender).
fn observer_deliveries(directory: &Path) -> Vec<(String, i32, i32)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for file in entries.map(|e| e.unwrap().path()) {
        let bytes = std::fs::read(&file).unwrap();
        for chunk in bytes.chunks(104) {
            if chunk.len() != 104 || chunk[..4] != 0x534c4154u32.to_le_bytes() {
                rows.push(("PARTIAL_OR_FOREIGN".to_string(), -1, -1));
                continue;
            }
            let u32_at = |at: usize| u32::from_le_bytes(chunk[at..at + 4].try_into().unwrap());
            let i32_at = |at: usize| i32::from_le_bytes(chunk[at..at + 4].try_into().unwrap());
            if u32_at(8) != 1 {
                continue;
            }
            let name = &chunk[72..104];
            let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(32)];
            rows.push((
                String::from_utf8_lossy(name).into_owned(),
                i32_at(28),
                i32_at(36),
            ));
        }
    }
    rows
}

/// Read only the private run's actual parent collection observations.
fn managed_record(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("managed.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every record line is one JSON object"))
        .filter(|row: &Value| {
            matches!(
                row["event"].as_str(),
                Some("managed-guard-completed" | "provider-parent-wait")
            )
        })
        .collect()
}

fn assert_managed_collection(tree: &Tree, outcome: &Outcome) {
    let participant = |value: &Value, pid: i32, role: &str| {
        let process = tree.rows.iter().find(|row| row.pid == pid).unwrap();
        assert_eq!(value["pid"].as_i64(), Some(i64::from(pid)));
        assert_eq!(value["role"], role);
        let birth = value["birth"].as_array().unwrap();
        assert_eq!(
            birth[0].as_i64().unwrap() * 1_000_000 + birth[1].as_i64().unwrap(),
            process.start_us,
            "parent receipt must name the observed process instance"
        );
    };
    let guards: Vec<_> = outcome
        .record
        .iter()
        .filter(|row| row["event"] == "managed-guard-completed")
        .collect();
    assert_eq!(
        guards.len(),
        2,
        "relay and guardian must each report actual collection"
    );
    for (pid, role, children) in [
        (
            tree.relay.unwrap(),
            "Relay",
            vec![(tree.guardian.unwrap(), "Guardian")],
        ),
        (
            tree.guardian.unwrap(),
            "Guardian",
            vec![
                (tree.supervisor, "Worker"),
                (tree.cleanup_owner, "CleanupOwner"),
            ],
        ),
    ] {
        let records: Vec<_> = guards
            .iter()
            .filter(|row| row["writer_pid"].as_i64() == Some(i64::from(pid)))
            .collect();
        assert_eq!(records.len(), 1);
        let details = &records[0]["details"];
        participant(&details["participant"], pid, role);
        assert_eq!(details["expired"], false);
        assert_eq!(details["unresolved"], 0);
        assert_eq!(details["opportunities_expired"], 0);
        let collected = details["collected"].as_array().unwrap();
        assert_eq!(collected.len(), children.len());
        for (child, child_role) in children {
            let receipts: Vec<_> = collected
                .iter()
                .filter(|receipt| receipt["participant"]["pid"].as_i64() == Some(i64::from(child)))
                .collect();
            assert_eq!(receipts.len(), 1);
            participant(&receipts[0]["participant"], child, child_role);
            let status = receipts[0]["status"].as_i64().unwrap() as i32;
            assert_eq!(
                status & 0x7f,
                0,
                "normal run must collect a normally exited helper"
            );
            if role == "Relay" {
                assert_eq!(
                    outcome.status,
                    Some(status),
                    "relay mirrors its collected guardian status"
                );
            }
        }
    }
    let waits: Vec<_> = outcome
        .record
        .iter()
        .filter(|row| row["event"] == "provider-parent-wait")
        .collect();
    assert!(
        !waits.is_empty(),
        "provider must have an actual parent receipt"
    );
    for row in waits {
        assert_eq!(row["writer_pid"].as_i64(), Some(i64::from(tree.supervisor)));
        participant(&row["receipt"]["owner"], tree.cleanup_owner, "CleanupOwner");
        participant(&row["receipt"]["provider"], tree.platform, "Provider");
        assert_eq!(row["receipt"]["status"], 0);
    }
}

fn assert_guardian_collected_after_relay_loss(tree: &Tree, outcome: &Outcome) {
    let records: Vec<_> = outcome
        .record
        .iter()
        .filter(|row| {
            row["event"] == "managed-guard-completed"
                && row["writer_pid"].as_i64() == Some(i64::from(tree.guardian.unwrap()))
        })
        .collect();
    assert_eq!(
        records.len(),
        1,
        "guardian must record its real child waits"
    );
    let details = &records[0]["details"];
    assert_eq!(details["unresolved"], 0);
    assert_eq!(details["expired"], false);
    let same_process = |value: &Value, pid| {
        let row = tree.rows.iter().find(|row| row.pid == pid).unwrap();
        value["pid"].as_i64() == Some(i64::from(pid))
            && value["birth"][0].as_i64().unwrap() * 1_000_000 + value["birth"][1].as_i64().unwrap()
                == row.start_us
    };
    assert!(same_process(
        &details["participant"],
        tree.guardian.unwrap()
    ));
    assert!(
        details["observed_exits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer["role"] == "Relay" && same_process(peer, tree.relay.unwrap())),
        "guardian must observe the actual relay instance ending"
    );
    let collected = details["collected"].as_array().unwrap();
    assert_eq!(collected.len(), 2);
    for (pid, role) in [
        (tree.supervisor, "Worker"),
        (tree.cleanup_owner, "CleanupOwner"),
    ] {
        let receipts: Vec<_> = collected
            .iter()
            .filter(|receipt| {
                receipt["participant"]["role"] == role && same_process(&receipt["participant"], pid)
            })
            .collect();
        assert_eq!(receipts.len(), 1);
        let status = receipts[0]["status"].as_i64().unwrap() as i32;
        assert!(status & 0x7f != 0x7f, "a stopped child is not collected");
    }
}

/// The `jobc` sampler's log (fixture `jobc` mode), by line kind.
#[derive(Debug, Default)]
struct Jobc {
    /// `MIN`: the smallest reading of G0's own entries, or `None` when the run
    /// produced no valid reading ("MIN none", or no `MIN` line at all).
    min: Option<i32>,
    /// Readings (`jobc=` lines) at or below 0.
    orphaned: Vec<String>,
    /// Instrument errors, never readings: a failed listing (`ERR`), or own
    /// entries that disagree (`DISAGREE`).
    errors: Vec<String>,
    /// Entries listed for G0 but filled in another group (`moved`, #209): a
    /// process that left G0 while the kernel built the listing. Never readings.
    moved: Vec<String>,
}

fn parse_jobc(log: &str) -> Jobc {
    let mut jobc = Jobc::default();
    for line in log.lines() {
        if let Some(min) = line.strip_prefix("MIN ") {
            jobc.min = min.parse().ok();
        }
        match line.split_whitespace().nth(1) {
            Some("ERR" | "DISAGREE") => jobc.errors.push(line.to_string()),
            Some("moved") => jobc.moved.push(line.to_string()),
            _ if kv(line, "jobc").is_some_and(|j| j <= 0) => jobc.orphaned.push(line.to_string()),
            _ => {}
        }
    }
    jobc
}

/// G0's `pg_jobc` never reached 0: every reading of its own entries was at
/// least 1, there was at least one, and the sampler never failed.
fn jobc_verdict(jobc: &Jobc) -> Result<(), String> {
    if !jobc.min.is_some_and(|m| m >= 1) || !jobc.orphaned.is_empty() {
        if jobc.min.is_none() && jobc.orphaned.is_empty() {
            return Err(format!(
                "the pg_jobc sampler produced no valid reading of the caller's group; \
                 instrument errors: {:?}",
                jobc.errors
            ));
        }
        return Err(format!(
            "pg_jobc of the caller's group reached {:?} (orphaned): {:?}",
            jobc.min, jobc.orphaned
        ));
    }
    if !jobc.errors.is_empty() {
        return Err(format!(
            "the pg_jobc sampler failed (an instrument error, not a reading): {:?}",
            jobc.errors
        ));
    }
    Ok(())
}

/// The caller's group was never touched by the run: the sentinel received
/// nothing, and G0's `pg_jobc` never reached 0.
fn assert_caller_group_untouched(o: &Outcome) {
    assert!(
        o.sentinel.is_empty(),
        "the caller's group received signals {:?}",
        o.sentinel
    );
    if let Err(e) = jobc_verdict(&o.jobc) {
        panic!("{e}");
    }
}

/// #209: every line kind of the sampler's log, read and judged with no run,
/// so it runs anywhere (no launchd session, no debugger permission).
#[test]
fn jobc_log_line_kinds_are_judged_as_the_assertion_needs() {
    let judge = |log: &str| jobc_verdict(&parse_jobc(log));
    // A clean run: G0's own readings stay at 1.
    assert_eq!(judge("START pid=9 pgid=4\n1 jobc=1 n=3\nMIN 1\n"), Ok(()));
    // An own reading of 0 is the orphaning the assertion exists for.
    let zero = judge("1 jobc=1 n=3\n2 jobc=0 n=3\nMIN 0\n").unwrap_err();
    assert!(
        zero.contains("reached Some(0)") && zero.contains("2 jobc=0 n=3"),
        "{zero}"
    );
    // A moved entry, the guardian filled in its new group with pg_jobc 0, is
    // never a reading.
    let moved = parse_jobc("1 jobc=1 n=3\n2 moved pid=9 pgid=9 ej=0 n=4\n3 jobc=1 n=3\nMIN 1\n");
    assert_eq!(moved.moved.len(), 1);
    assert!(moved.orphaned.is_empty() && moved.errors.is_empty());
    assert_eq!(jobc_verdict(&moved), Ok(()));
    // No own entry across the whole run: no valid reading, which fails.
    let none = judge("1 none n=0\n2 moved pid=9 pgid=9 ej=0 n=1\n3 none n=1\nMIN none\n");
    assert!(none.unwrap_err().contains("no valid reading"));
    assert!(judge("START pid=9 pgid=4\n").is_err(), "no MIN line at all");
    // Own entries that disagree fail loudly, as an instrument error.
    let disagree = judge("1 jobc=1 n=3\n2 DISAGREE n=3 own=5:1,4:0\nMIN 1\n").unwrap_err();
    assert!(
        disagree.contains("instrument error") && disagree.contains("DISAGREE"),
        "{disagree}"
    );
    // A failed listing is never a successful sample.
    let err = judge("1 jobc=1 n=3\n2 ERR errno=12\nMIN 1\n").unwrap_err();
    assert!(
        err.contains("instrument error") && err.contains("ERR errno=12"),
        "{err}"
    );
    assert!(judge("1 ERR errno=12\nMIN none\n").is_err());
}

/// #209: the fixture's selection of G0's own entries, on canned listings
/// (`jobc-select`, the same code as the sampler's). Needs only clang.
#[test]
fn jobc_selection_reads_only_the_groups_own_entries() {
    let select = |listing: &str| {
        let mut child = Command::new(fixture())
            .args(["jobc-select", "4"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(listing.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        parse_jobc(&String::from_utf8_lossy(&out.stdout))
    };
    // kp[0] is the guardian, filled in its new group 9 with pg_jobc 0 (the
    // #209 listing): it is moved, and G0's own entries read 1.
    let guardian = select("9 9 0\n5 4 1\n4 4 1\n");
    assert_eq!(guardian.moved.len(), 1, "{guardian:?}");
    assert!(
        guardian.orphaned.is_empty() && guardian.errors.is_empty(),
        "{guardian:?}"
    );
    // G0's own entries at 0: a reading of 0.
    let orphaned = select("5 4 0\n4 4 0\n");
    assert_eq!(orphaned.orphaned.len(), 1, "{orphaned:?}");
    // Only moved entries: no reading.
    let none = select("9 9 0\n");
    assert!(
        none.orphaned.is_empty() && none.moved.len() == 1,
        "{none:?}"
    );
    // Own entries that disagree: an instrument error, not a reading.
    let disagree = select("5 4 1\n4 4 0\n");
    assert_eq!(disagree.errors.len(), 1, "{disagree:?}");
    assert!(disagree.orphaned.is_empty(), "{disagree:?}");
}

fn assert_nothing_survives(o: &Outcome) {
    assert!(
        o.survivors.is_empty(),
        "processes of the run survived: {:?}\n{}",
        o.survivors,
        o.stderr
    );
    assert!(
        o.zombies.is_empty(),
        "launchd-parented zombies of the run: {:?}\n{}",
        o.zombies,
        o.stderr
    );
}

fn assert_no_sighup_to_umbra(o: &Outcome) {
    let hups: Vec<_> = o
        .delivered
        .iter()
        .filter(|(_, sig, _)| *sig == SIGHUP)
        .collect();
    assert!(
        hups.is_empty(),
        "SIGHUP delivered to umbra or its providers: {hups:?}"
    );
    assert_ne!(o.term_signal(), Some(SIGHUP), "umbra was killed by SIGHUP");
}

// ------------------------------------------------------------ SID-1 paths

/// P1: the held root's debugserver is stopped; the platform deadline then
/// drives the ordered teardown. Before #193: SIGHUP to umbra, its providers
/// and the caller's group.
#[test]
fn sid1_p1_stopped_tracer_at_the_deadline() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("P1", false, true, 8_000, &["park"]);
    run.line("ISO CHILD", Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(300));
    let tree = run.discover();
    signal(tree.root_tracer, "STOP");
    let o = run.finish(Duration::from_secs(45));
    assert!(
        o.stderr.contains("Barrier(Confirmed(")
            && !o.stderr.contains("Barrier(Unconfirmed(")
            && !o.stderr.contains("timed_out: true")
            && !o.stderr.contains("unconfirmed: true"),
        "SID-1 stopped-tracer teardown must confirm before fallback: {}",
        o.stderr
    );
    assert_no_sighup_to_umbra(o);
    assert_caller_group_untouched(o);
    assert!(
        o.exit_code().is_some(),
        "umbra reports an exit status: {o:?}"
    );
    assert_nothing_survives(o);
}

/// P2, reshaped for #205 (OD2): the running root is killed, then its
/// debugserver once the root is dead.
///
/// It used to kill the root while it was **held** at a debugger stop and then
/// the debugserver, which is #193's uncounted-reparent route: the held exit
/// stays pending until the debugger dies, and the kernel then hands the
/// half-exited tracee to launchd as a zombie nobody collects. That route is
/// kernel behaviour with no software fix (#205 R1), and every run of it left a
/// zombie, so it no longer has a SID-1 regression here. Now the root is in an
/// ordinary blocking wait (`sleep`), so the kill ends it at once. Its
/// debugserver then reports the exit and usually ends by itself (CI showed it
/// gone 50 ms later), so the second step is exact rather than assumed: the
/// debugserver's (pid, start) is recorded before the root's kill, and after the
/// root's death it is either already gone, or killed by the test, or `kill`
/// fails with `ESRCH` and that same identity is proven gone. Anything
/// else fails. The oracle is the SID-1 kill sequence's: no SIGHUP to umbra,
/// the caller's group untouched, a status reported, nothing left, and no
/// launchd-parented zombie.
#[test]
fn sid1_p2_running_tracee_killed_then_its_tracer() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("P2", false, true, 20_000, &["sleep"]);
    std::thread::sleep(Duration::from_millis(500));
    let tree = run.discover();
    let root = identity(tree.root).expect("the root is live before its kill");
    let tracer =
        identity(tree.root_tracer).expect("its debugserver is live before the root's kill");
    signal(tree.root, "KILL");
    let end = Instant::now() + Duration::from_secs(5);
    while identity_live(root) {
        assert!(
            Instant::now() < end,
            "the root {root:?} survived its SIGKILL for 5 s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let ended = if !identity_live(tracer) {
        "ended by itself after its program's death"
    } else {
        // SAFETY: plain libSystem call on a pid this harness discovered by
        // parentage and whose (pid, start) was live just above.
        if unsafe { libc::kill(tracer.0, libc::SIGKILL) } == 0 {
            "ended by the test"
        } else {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => {
                    assert!(
                        !identity_live(tracer),
                        "kill reported ESRCH, but {tracer:?} is still live"
                    );
                    "ended by itself after its program's death"
                }
                _ => panic!("kill(SIGKILL) {}: {error}", tracer.0),
            }
        }
    };
    eprintln!("P2: debugserver {tracer:?} {ended}");
    let o = run.finish(Duration::from_secs(45));
    assert_no_sighup_to_umbra(o);
    assert_caller_group_untouched(o);
    assert!(
        o.exit_code().is_some(),
        "umbra reports an exit status: {o:?}"
    );
    assert_nothing_survives(o);
}

/// P3: the platform provider is killed while the tracee runs. Before #193 no
/// signal fired, but the caller's group's `pg_jobc` reached 0 (orphaned).
#[test]
fn sid1_p3_platform_killed() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("P3", false, false, 20_000, &["sleep"]);
    std::thread::sleep(Duration::from_millis(500));
    let tree = run.discover();
    signal(tree.platform, "KILL");
    let o = run.finish(Duration::from_secs(45));
    assert_caller_group_untouched(o);
    assert!(
        o.exit_code().is_some(),
        "umbra reports an exit status: {o:?}"
    );
    assert_nothing_survives(o);
}

/// P4: a provider is stopped, then the supervisor is killed. Before #193:
/// SIGHUP to the providers and to the caller's group. Now nothing of the run
/// survives the guardian's sweep, and the relay reports the supervisor's
/// SIGKILL.
#[test]
#[cfg(feature = "test-managed-session")]
fn own_session_worker_killed_with_a_stopped_provider() {
    if !qualified() {
        return;
    }
    let mut run = Run::start("OWN-P4", true, false, 20_000, &["sleep"]);
    std::thread::sleep(Duration::from_millis(500));
    let tree = run.discover();
    assert_ne!(tree.sid(tree.supervisor), 1);
    assert_eq!(tree.sid(tree.supervisor), tree.guardian.unwrap());
    signal(tree.journal, "STOP");
    std::thread::sleep(Duration::from_millis(100));
    signal(tree.supervisor, "KILL");
    let o = run.finish(Duration::from_secs(30));
    assert!(
        o.term_signal() == Some(SIGKILL) && o.survivors.is_empty(),
        "worker status must be mirrored and no provider left running: {o:?}"
    );
    assert_nothing_survives(o);
}

#[test]
fn sid1_p4_supervisor_killed_with_a_stopped_provider() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("P4", false, false, 20_000, &["sleep"]);
    std::thread::sleep(Duration::from_millis(500));
    let tree = run.discover();
    signal(tree.journal, "STOP");
    std::thread::sleep(Duration::from_millis(100));
    signal(tree.supervisor, "KILL");
    let o = run.finish(Duration::from_secs(30));
    assert_caller_group_untouched(o);
    assert_eq!(
        o.term_signal(),
        Some(SIGKILL),
        "the supervisor's status is mirrored: {o:?}"
    );
    assert_nothing_survives(o);
}

// ------------------------------------------------------- SID-1 topology

/// In SID 1 the run lives in the guardian's session S; only the relay stays in
/// the caller's group. The relay mirrors the supervisor's status, which the
/// isolation record holds next to a COMPLETE cleanup.
#[test]
fn sid1_topology_status_and_record() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("TOPOLOGY", false, false, 20_000, &["exit", "3"]);
    let tree = run.discover();
    let g0 = run.g0();
    let s = tree.guardian.expect("a guardian leads the run's session");
    assert_ne!(s, 1);
    assert_eq!(tree.sid(s), s, "the guardian leads its session");
    let relay = tree.relay.unwrap();
    assert_eq!(tree.sid(relay), 1, "the relay stays in launchd's session");
    assert_eq!(
        tree.rows.iter().find(|p| p.pid == relay).unwrap().pgid,
        g0,
        "the relay stays in the caller's group"
    );
    assert!(!tree.debugservers.is_empty());
    for d in &tree.debugservers {
        let args = args_of(*d);
        assert!(
            !args.contains(PIN),
            "debugserver {d} has {PIN} in an isolated run: {args}"
        );
    }
    for (what, pid) in [
        ("supervisor", tree.supervisor),
        ("cleanup owner", tree.cleanup_owner),
        ("platform", tree.platform),
        ("journal", tree.journal),
        ("storage", tree.storage),
        ("tracee", tree.root),
        ("debugserver", tree.root_tracer),
    ] {
        assert_eq!(tree.sid(pid), s, "{what} {pid} is in the run's session");
    }
    let o = run.finish(Duration::from_secs(30));
    assert_managed_collection(&tree, o);
    assert_eq!(
        o.exit_code(),
        Some(1),
        "a failing program is umbra's exit 1, as before"
    );
    assert_caller_group_untouched(o);
    assert_nothing_survives(o);
}

/// A descendant of the tracee cannot join the caller's group: it is in
/// another session now (`setpgid` across sessions is EPERM).
#[test]
fn sid1_descendant_cannot_join_the_callers_group() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("DESC", false, false, 20_000, &["desc", "@G0"]);
    let join = run.line("ISO DESC step=join", Duration::from_secs(30));
    let o = run.finish(Duration::from_secs(30));
    assert_eq!(kv(&join, "rc"), Some(-1), "{join}");
    assert_eq!(kv(&join, "errno"), Some(EPERM), "{join}");
    assert_eq!(o.exit_code(), Some(0), "{o:?}");
    assert_nothing_survives(o);
}

/// The tracee's own `setsid()` still succeeds in SID 1.
#[test]
fn sid1_tracee_setsid_succeeds() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("SETSID", false, false, 20_000, &["setsid"]);
    let line = run.line("ISO SETSID", Duration::from_secs(30));
    let o = run.finish(Duration::from_secs(30));
    assert_eq!(kv(&line, "rc"), kv(&line, "pid"), "{line}");
    assert_eq!(o.exit_code(), Some(0), "{o:?}");
}

/// The harness's own `killpg` of the caller's group, with a stopped tracer
/// and a three-generation tree: the relay dies, the guardian sweeps S, and
/// nothing with `sid == S` is left. The sweep is by session only: a process
/// named `debugserver` outside S is never touched.
#[test]
fn sid1_killpg_with_a_stopped_tracer_leaves_nothing() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let decoy_dir = tempfile::tempdir().unwrap();
    let decoy_path = decoy_dir.path().join("debugserver");
    std::fs::copy(fixture(), &decoy_path).unwrap();
    let mut decoy = Command::new(&decoy_path)
        .arg("sleep")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut run = Run::start("KILLPG", false, false, 20_000, &["tree"]);
    let child = kv(&run.line("ISO CHILD", Duration::from_secs(30)), "pid").unwrap();
    let grandchild = kv(&run.line("ISO GRANDCHILD", Duration::from_secs(30)), "pid").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let tree = run.discover();
    run.record(child);
    run.record(grandchild);
    signal(tree.root_tracer, "STOP");
    std::thread::sleep(Duration::from_millis(100));
    let g0 = run.g0();
    let ok = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{g0}")])
        .status()
        .unwrap();
    assert!(ok.success());
    // The guardian's bounds: relay loss is seen within ~10 ms; it ends the
    // supervisor first, drains for at most 3 s + 4 s per debugserver (three
    // sessions here, #205), sweeps within 3 s and reaps the supervisor within
    // 1 s: T(3) = 19 s.
    let left = run.drained(Duration::from_secs(20));
    let decoy_alive = decoy.try_wait().unwrap().is_none();
    let _ = decoy.kill();
    let _ = decoy.wait();
    let o = run.finish(Duration::from_secs(1));
    assert!(
        left.is_empty(),
        "the run survived its caller's group: {left:?}"
    );
    assert_nothing_survives(o);
    assert!(
        decoy_alive,
        "a same-named process outside the run's session was touched"
    );
    assert_guardian_collected_after_relay_loss(&tree, o);
}

/// O9: an external terminating signal to the relay cancels the run; it is
/// not a graceful stop. The relay dies of it (its status is that signal), and
/// the guardian, seeing the relay gone, sweeps the run's session.
#[test]
fn sid1_sigterm_to_the_relay_cancels_the_run() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("SIGTERM", false, false, 20_000, &["sleep"]);
    std::thread::sleep(Duration::from_millis(500));
    let tree = run.discover();
    signal(tree.relay.expect("a relay in SID 1"), "TERM");
    // Relay loss is seen within ~10 ms; #205's drain (at most 3 s + 4 s for
    // one debugserver), the 3 s sweep and the 1 s supervisor reap: T(1) = 11 s.
    let left = run.drained(Duration::from_secs(12));
    let o = run.finish(Duration::from_secs(5));
    assert!(left.is_empty(), "the run survived its relay: {left:?}");
    assert_eq!(o.term_signal(), Some(15), "{o:?}");
    assert_caller_group_untouched(o);
    assert_guardian_collected_after_relay_loss(&tree, o);
    // The guardian reports only members it enrolled; the finish census also
    // counts a launchd-parented zombie of the run that was never enrolled.
    assert!(
        o.zombies.is_empty(),
        "launchd-parented zombies of the run: {:?}",
        o.zombies
    );
}

/// Outside launchd's session every managed role and the tracee retain the
/// caller's session.
#[test]
fn fresh_session_is_unchanged() {
    if !qualified() {
        return;
    }
    let mut run = Run::start("FRESH", true, false, 20_000, &["exit", "3"]);
    let launcher_sid = kv(&run.line("ISO LAUNCH", Duration::from_secs(10)), "sid").unwrap();
    let tree = run.discover();
    let guardian = tree
        .guardian
        .expect("managed guardian in the caller session");
    assert!(!tree.debugservers.is_empty());
    for d in &tree.debugservers {
        let args = args_of(*d);
        assert!(
            !args.contains(PIN),
            "debugserver {d} has {PIN} outside an isolated run: {args}"
        );
    }
    for pid in [
        tree.relay.unwrap(),
        guardian,
        tree.supervisor,
        tree.cleanup_owner,
        tree.platform,
        tree.journal,
        tree.storage,
        tree.root,
        tree.root_tracer,
    ] {
        assert_eq!(
            tree.sid(pid),
            launcher_sid,
            "managed role {pid} changed the caller session"
        );
    }
    let o = run.finish(Duration::from_secs(30));
    assert_eq!(o.exit_code(), Some(1), "{o:?}");
    assert_managed_collection(&tree, o);
    assert_nothing_survives(o);
}

// ------------------------------------------------- terminal semantics (pty)

/// What the mini-shell recorded for one pty case, and the terminal transcript.
struct Pty {
    /// The fixture's one-line summary (`ISO PTY … survivors=N`).
    summary: String,
    shell: String,
    transcript: String,
    /// argv of each debugserver in the pty's session when the tracee was ready.
    debugservers: Vec<String>,
}

/// One pty case (fixture `pty`): a mini job-control shell on a new pty runs
/// `umbra run … -- fixture <tracee>` as its foreground job; the harness types
/// the case's input half a second after the tracee is ready. A terminal is
/// never launchd's session, so #193's isolation is inert here; these pin the
/// terminal semantics the #193 design measured on master (B-T1…B-T10).
fn pty(case: &str, tracee: &[&str]) -> Option<Pty> {
    if !qualified() {
        return None;
    }
    let (dir, _temp) = case_dir(&format!("PTY-{case}"));
    let since_us = now_us();
    let out = Command::new(fixture())
        .arg("pty")
        .arg(case)
        .arg(&dir)
        .arg("--")
        .args(umbra_command(&dir, 20_000, tracee))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let summary = String::from_utf8_lossy(&out.stdout).into_owned();
    let shell = std::fs::read_to_string(dir.join("mshell.txt")).unwrap_or_default();
    let transcript =
        String::from_utf8_lossy(&std::fs::read(dir.join("transcript.log")).unwrap_or_default())
            .into_owned();
    eprintln!("{case}: {summary}shell:\n{shell}transcript:\n{transcript}");
    assert!(out.status.success(), "pty harness: {:?}", out.status);
    assert!(summary.contains("timed_out=0"), "{summary}");
    assert!(
        summary.contains("ready=1"),
        "the tracee never became ready: {summary}"
    );
    let debugservers: Vec<String> = std::fs::read_to_string(dir.join("debugserver.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        debugservers.iter().all(|a| !a.contains(PIN)),
        "a terminal run's debugserver has {PIN}: {debugservers:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let zombies = launchd_zombies(since_us, &[]);
    assert!(
        zombies.is_empty(),
        "{case}: launchd-parented zombies of the run: {zombies:?}"
    );
    Some(Pty {
        summary,
        shell,
        transcript,
        debugservers,
    })
}

/// The tracee waits in these cases, so its debugserver was there to inspect.
fn inspected(p: &Pty) {
    assert!(
        !p.debugservers.is_empty(),
        "no debugserver captured in the pty session"
    );
}

fn ended(p: &Pty, expected: &str) {
    let line = p
        .shell
        .lines()
        .find(|l| l.starts_with("ended="))
        .unwrap_or_default();
    assert_eq!(
        line,
        format!("ended={expected} owner_is_job=1"),
        "shell record:\n{}",
        p.shell
    );
}

#[test]
fn pty_t1_terminal_read_is_refused() {
    let Some(p) = pty("T1", &["ttyread"]) else {
        return;
    };
    ended(&p, "exit:1");
}

#[test]
fn pty_t2_terminal_write() {
    let Some(p) = pty("T2", &["ttywrite"]) else {
        return;
    };
    ended(&p, "exit:0");
    assert!(p.transcript.contains("ISO TTYWRITE-PAYLOAD"));
}

#[test]
fn pty_t3_terminal_write_with_tostop() {
    let Some(p) = pty("T3", &["ttywrite"]) else {
        return;
    };
    ended(&p, "exit:0");
    assert!(p.transcript.contains("ISO TTYWRITE-PAYLOAD"));
}

#[test]
fn pty_t4_ctrl_c() {
    let Some(p) = pty("T4", &["wait", "8"]) else {
        return;
    };
    inspected(&p);
    ended(&p, "signal:2");
}

#[test]
fn pty_t5_ctrl_z_then_fg() {
    let Some(p) = pty("T5", &["wait", "6"]) else {
        return;
    };
    inspected(&p);
    assert!(p.shell.lines().any(|l| l == "stopped=18"), "{}", p.shell);
    ended(&p, "exit:1");
}

#[test]
fn pty_t6_ctrl_backslash() {
    let Some(p) = pty("T6", &["wait", "8"]) else {
        return;
    };
    inspected(&p);
    ended(&p, "signal:3");
}

#[test]
fn pty_t7_window_change_is_absorbed() {
    let Some(p) = pty("T7", &["wait", "3"]) else {
        return;
    };
    inspected(&p);
    ended(&p, "exit:0");
    assert!(
        p.transcript
            .contains("ISO WAIT-DONE winch=0 int=0 tstp=0 quit=0 hup=0 cont=0 rows=40 cols=100"),
        "{}",
        p.transcript
    );
}

/// The hangup (#198, #205). umbra and the shell's job end with SIGHUP, as
/// before. The platform provider no longer dies with the terminal: it ends
/// with its owner and tears its session down in order first, so the harness
/// finds nothing left to kill (`survivors=0`), and no launchd-parented zombie
/// is left (checked in `pty`).
#[test]
fn pty_t8_hangup() {
    let Some(p) = pty("T8", &["wait", "8"]) else {
        return;
    };
    inspected(&p);
    assert!(p.shell.lines().any(|l| l == "sighup"), "{}", p.shell);
    ended(&p, "signal:1");
    assert!(
        p.summary.contains("survivors=0"),
        "the harness had processes of the run to kill: {}",
        p.summary
    );
}

#[test]
fn pty_t9_exit_status() {
    let Some(p) = pty("T9", &["exit", "3"]) else {
        return;
    };
    ended(&p, "exit:1");
}

#[test]
fn pty_t10_terminal_ioctls() {
    let Some(p) = pty("T10", &["ioctl"]) else {
        return;
    };
    ended(&p, "exit:0");
    assert!(
        p.transcript
            .contains("ISO IOCTL tcgetattr=0/0 winsz=0/0 tcgetpgrp=-1/1 isatty2=1"),
        "{}",
        p.transcript
    );
}

/// Launchd relay/guardian retain stream copies until CLI exit (#201).
#[test]
fn f201_pipe_eof_isolated() {
    use std::io::Write;
    if !qualified() {
        return;
    }
    if own_session() != 1 {
        eprintln!(
            "SID-1 prerequisite: not runnable from a worker terminal session; qualified on CI"
        );
        assert!(!set("UMBRA_REQUIRE_SID1"), "SID-1 qualification required");
        return;
    }
    let (dir, _temp) = case_dir("f201_pipe_eof_isolated");
    let program = dir.join("stdio-control");
    let built = Command::new("clang")
        .args(["-arch", "arm64", "-O1", "-Wall", "-Wextra", "-Werror"])
        .arg(repository().join("experiments/fixtures/umbra-fix-201-stdio.c"))
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let payload = b"isolated stdin/stdout\0\xff\n";
    let mut argv = umbra_command(&dir, 20_000, &[]);
    *argv.last_mut().unwrap() = program.into_os_string();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .arg("copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(
        procs()
            .into_iter()
            .find(|p| p.pid == child.id() as i32)
            .unwrap()
            .sid,
        1
    );
    child.stdin.take().unwrap().write_all(payload).unwrap();
    // wait_with_output drains to EOF and collects CLI exit. Early EOF is not
    // promised here: relay and guardian may retain the writer until exit.
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, payload);
    eprintln!("f201_pipe_eof_isolated: exact bytes and stdout EOF at CLI exit");
}

/// Preserve the primary message and status measured on the frozen base CLI.
#[test]
fn missing_storage_preserves_base_cli_error() {
    use std::os::unix::process::CommandExt;
    if !qualified() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = registry(temp.path(), 25_000);
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["providers"]["storage"]["executable"] =
        json!(temp.path().join("missing-storage").as_os_str().as_bytes());
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let invoke = |binary: &Path| {
        let mut command = Command::new(binary);
        command
            .args(["run", "--registry"])
            .arg(&path)
            .arg("--workspace")
            .arg(&workspace)
            .args(["--experimental", "--local-dev", "--", "/usr/bin/true"])
            .stdin(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                assert_ne!(libc::getsid(0), 1);
                Ok(())
            });
        }
        command.output().unwrap()
    };
    // Frozen b052 CLI: exit 1 with this exact primary message. Helpers retain
    // their existing failure diagnostics; only the final primary is authoritative.
    const PRIMARY: &str = "StorageUnavailable during provider.transport: No such file or directory (os error 2) (errno: None)";
    let managed = invoke(&bin_dir().join("umbra"));
    let stderr = String::from_utf8(managed.stderr).unwrap();
    eprintln!("managed status={:?} stderr={stderr}", managed.status.code());
    assert_eq!(managed.status.code(), Some(1));
    let mut lines: Vec<_> = stderr.lines().collect();
    assert_eq!(lines.pop(), Some(PRIMARY), "original primary must be last");
    for diagnostic in lines {
        assert!(
            [
                PRIMARY,
                "peer activity interrupted namespace preparation",
                "unexpected end of file"
            ]
            .contains(&diagnostic),
            "unexpected managed role diagnostic: {diagnostic}"
        );
    }
    assert!(managed.stdout.is_empty());
}
