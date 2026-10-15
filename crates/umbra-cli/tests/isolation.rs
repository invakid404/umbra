//! #193: `umbra run` started in launchd's session (SID 1), on the production
//! path, and today's terminal semantics, pinned.
//!
//! **Where these run.** Every case starts real runs, so it needs the qualified
//! host (debugger permission). It runs when `UMBRA_INTEGRATION_REQUIRED` or
//! `UMBRA_TEST_ISOLATION` is set and is skipped otherwise. The `sid1_` cases
//! also need launchd's session: CI's self-hosted runner, or locally a
//! `launchctl submit` of this test binary. Elsewhere they skip, unless
//! `UMBRA_REQUIRE_SID1` is set, which makes a session other than 1 a failure.
//! Build the workspace binaries first. `UMBRA_ISOLATION_BIN_DIR` points the
//! cases at a frozen copy of `umbra` and the providers instead of this build's.
//!
//! **Topology** (the #193 design harness, `dg-56gcxqyw`): a launcher, spawned
//! into its own group G0 (`process_group(0)`, the harness shape), starts a
//! passive sentinel and `umbra run` in G0. A sampler in its own session records
//! G0's `pg_jobc`. Before #193, a SID-1 run's exits were counted against G0:
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

fn own_session() -> i32 {
    let me = std::process::id() as i32;
    procs()
        .into_iter()
        .find(|p| p.pid == me)
        .expect("this process in the table")
        .sid
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

#[derive(Debug)]
struct Outcome {
    /// The launcher's raw wait status of `umbra`, if it reported one.
    status: Option<i32>,
    /// Live processes of this run after it ended (recorded pids, or session S).
    survivors: Vec<Proc>,
    /// Signals the sentinel in the caller's group received: (signal, sender).
    sentinel: Vec<(i32, i32)>,
    /// The smallest `pg_jobc` of the caller's group seen while sampling.
    jobc_min: Option<i32>,
    /// The sampler's change-log lines with `pg_jobc` at or below 0.
    jobc_orphaned: Vec<String>,
    /// Observer deliveries: (image, signal, sender).
    delivered: Vec<(String, i32, i32)>,
    /// The isolation record of session S, if any.
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
        let mut command = Command::new(fixture());
        command
            .arg("launch")
            .arg(&dir)
            .arg(if fresh { "fresh" } else { "group" })
            .arg("--")
            .args(umbra_command(&dir, platform_timeout_ms, tracee))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if !fresh {
            command.process_group(0);
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
        match kids(umbra).into_iter().find(|r| r.comm == "umbra") {
            Some(guardian) => {
                tree.relay = Some(umbra);
                tree.guardian = Some(guardian.pid);
                tree.supervisor = kids(guardian.pid)
                    .into_iter()
                    .find(|r| r.comm == "umbra")
                    .expect("supervisor")
                    .pid;
                // The guardian is the relay's unreaped child now and was alive
                // at `before`: S is this run's session at that instant.
                assert!(guardian.start_us < before);
                self.session = Some((guardian.pid, before));
            }
            None => tree.supervisor = umbra,
        }
        for r in kids(tree.supervisor) {
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
        let sentinel = std::fs::read_to_string(self.dir.join("sentinel.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| Some((kv(l, "sig")?, kv(l, "si_pid")?)))
            .collect();
        let jobc_log = std::fs::read_to_string(self.dir.join("jobc.log")).unwrap_or_default();
        let jobc_min = jobc_log
            .lines()
            .find_map(|l| l.strip_prefix("MIN ").and_then(|v| v.parse().ok()));
        let jobc_orphaned = jobc_log
            .lines()
            .filter(|l| kv(l, "jobc").is_some_and(|j| j <= 0))
            .map(str::to_string)
            .collect();
        let delivered = observer_deliveries(&self.dir.join("signals"));
        let record = self
            .session
            .map(|(s, _)| isolation_record(s))
            .unwrap_or_default();
        let stderr = self.lines.lock().unwrap().join("\n");
        if let Ok(record_text) = serde_json::to_string_pretty(&record) {
            let _ = std::fs::write(self.dir.join("isolation-record.json"), record_text);
        }
        let outcome = Outcome {
            status,
            survivors,
            sentinel,
            jobc_min,
            jobc_orphaned,
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

/// The isolation record of session `sid` (`~/Library/Logs/umbra/isolation/`).
fn isolation_record(sid: i32) -> Vec<Value> {
    let out = Command::new(fixture()).arg("home").output().unwrap();
    let home = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let dir = home.join("Library/Logs/umbra/isolation");
    let suffix = format!("-s{sid}.jsonl");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    matches.retain(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&suffix))
    });
    matches.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let Some(path) = matches.last() else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every record line is one JSON object"))
        .collect()
}

/// The caller's group was never touched by the run: the sentinel received
/// nothing, and G0's `pg_jobc` never reached 0.
fn assert_caller_group_untouched(o: &Outcome) {
    assert!(
        o.sentinel.is_empty(),
        "the caller's group received signals {:?}",
        o.sentinel
    );
    assert!(
        o.jobc_min.is_some_and(|m| m >= 1),
        "pg_jobc of the caller's group reached {:?} (orphaned): {:?}",
        o.jobc_min,
        o.jobc_orphaned
    );
}

fn assert_nothing_survives(o: &Outcome) {
    assert!(
        o.survivors.is_empty(),
        "processes of the run survived: {:?}\n{}",
        o.survivors,
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
    assert_no_sighup_to_umbra(o);
    assert_caller_group_untouched(o);
    assert!(
        o.exit_code().is_some(),
        "umbra reports an exit status: {o:?}"
    );
    assert_nothing_survives(o);
}

/// P2: the held root is killed, then its debugserver 50 ms later (the tracer
/// dies with the tracee's exit pending; the tracee is handed to launchd).
#[test]
fn sid1_p2_tracer_killed_with_the_exit_pending() {
    if !qualified() || !in_launchd_session() {
        return;
    }
    let mut run = Run::start("P2", false, true, 20_000, &["park"]);
    run.line("ISO CHILD", Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(300));
    let tree = run.discover();
    signal(tree.root, "KILL");
    std::thread::sleep(Duration::from_millis(50));
    signal(tree.root_tracer, "KILL");
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
        ("platform", tree.platform),
        ("journal", tree.journal),
        ("storage", tree.storage),
        ("tracee", tree.root),
        ("debugserver", tree.root_tracer),
    ] {
        assert_eq!(tree.sid(pid), s, "{what} {pid} is in the run's session");
    }
    let o = run.finish(Duration::from_secs(30));
    let outcome = o
        .record
        .iter()
        .find(|v| v["event"] == "outcome")
        .expect("an outcome line");
    assert_eq!(outcome["cleanup"], "COMPLETE");
    let supervisor_status = outcome["supervisor_status"].as_i64().unwrap() as i32;
    assert_eq!(
        o.status,
        Some(supervisor_status),
        "the relay mirrors the supervisor's status"
    );
    assert_eq!(
        o.exit_code(),
        Some(1),
        "a failing program is umbra's exit 1, as before"
    );
    assert_eq!(o.record.first().unwrap()["event"], "relay-start");
    assert_eq!(o.record.last().unwrap()["event"], "guardian-reaped");
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
    // The guardian's bounds: relay loss is seen within ~10 ms, the sweep is
    // bounded by 3 s and the supervisor reap by 1 s.
    let left = run.drained(Duration::from_secs(8));
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
    if tree.guardian.is_some() {
        let death = o
            .record
            .iter()
            .find(|v| v["event"] == "relay-death")
            .expect("the guardian recorded relay death");
        assert_eq!(death["outcome"], "COMPLETE", "{death}");
    }
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
    // Relay loss is seen within ~10 ms; sweep 3 s; supervisor reap 1 s.
    let left = run.drained(Duration::from_secs(6));
    let o = run.finish(Duration::from_secs(5));
    assert!(left.is_empty(), "the run survived its relay: {left:?}");
    assert_eq!(o.term_signal(), Some(15), "{o:?}");
    assert_caller_group_untouched(o);
    let death = o
        .record
        .iter()
        .find(|v| v["event"] == "relay-death")
        .expect("the guardian recorded relay death");
    assert_eq!(death["outcome"], "COMPLETE", "{death}");
}

/// Outside launchd's session nothing changes: no relay, no guardian; the
/// supervisor is the launched process and the tracee is in the caller's
/// session.
#[test]
fn fresh_session_is_unchanged() {
    if !qualified() {
        return;
    }
    let mut run = Run::start("FRESH", true, false, 20_000, &["exit", "3"]);
    let launcher_sid = kv(&run.line("ISO LAUNCH", Duration::from_secs(10)), "sid").unwrap();
    let tree = run.discover();
    assert!(
        tree.guardian.is_none(),
        "no guardian outside SID 1: {:?}",
        tree.relay
    );
    assert!(!tree.debugservers.is_empty());
    for d in &tree.debugservers {
        let args = args_of(*d);
        assert!(
            !args.contains(PIN),
            "debugserver {d} has {PIN} outside an isolated run: {args}"
        );
    }
    assert_eq!(tree.sid(tree.supervisor), launcher_sid);
    assert_eq!(tree.sid(tree.root), launcher_sid);
    let o = run.finish(Duration::from_secs(30));
    assert_eq!(o.exit_code(), Some(1), "{o:?}");
    assert_nothing_survives(o);
}

// ------------------------------------------------- terminal semantics (pty)

/// What the mini-shell recorded for one pty case, and the terminal transcript.
struct Pty {
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
    Some(Pty {
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

/// The hangup's process leak is #198; this pins the status only.
#[test]
fn pty_t8_hangup() {
    let Some(p) = pty("T8", &["wait", "8"]) else {
        return;
    };
    inspected(&p);
    assert!(p.shell.lines().any(|l| l == "sighup"), "{}", p.shell);
    ended(&p, "signal:1");
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
