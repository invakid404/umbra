//! #274: an idle root must not retain resources for its 200 completed children.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{json, Value};

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use std::time::{Duration, Instant};

fn qualified() -> bool {
    ["UMBRA_INTEGRATION_REQUIRED", "UMBRA_TEST_ISOLATION"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()))
}

#[derive(Debug)]
struct Process {
    pid: u32,
    parent: u32,
    zombie: bool,
}

fn processes() -> Vec<Process> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,stat="])
        .output()
        .unwrap();
    assert!(out.status.success(), "process census unavailable: {out:?}");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 3, "malformed census: {line}");
            Process {
                pid: fields[0].parse().unwrap(),
                parent: fields[1].parse().unwrap(),
                zombie: fields[2].starts_with('Z'),
            }
        })
        .collect()
}

fn fds(pid: u32) -> usize {
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-Ff"])
        .output()
        .unwrap();
    assert!(out.status.success(), "fd census unavailable: {out:?}");
    let count = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|s| {
            s.strip_prefix('f')
                .is_some_and(|v| v.parse::<u32>().is_ok())
        })
        .count();
    assert!(count > 0, "empty fd census is unavailable");
    count
}

struct Run {
    child: Child,
    gates: [PathBuf; 2],
    gate: usize,
    stderr: PathBuf,
}
impl Run {
    fn start(mut command: Command, stderr: &Path) -> Self {
        let gates = [
            stderr.with_extension("start"),
            stderr.with_extension("release"),
        ];
        let child = command
            .args(&gates)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(stderr).unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            gates,
            gate: 0,
            stderr: stderr.to_owned(),
        }
    }
    fn marker(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let text = std::fs::read_to_string(&self.stderr).unwrap();
            if let Some(line) = text.lines().find(|line| line.starts_with(prefix)) {
                eprintln!("fixture: {line}");
                return line.to_owned();
            }
            assert!(
                self.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "fixture milestone unavailable; stderr: {text}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn release(&mut self) {
        std::fs::write(&self.gates[self.gate], b"release").unwrap();
        self.gate += 1;
    }
    fn finish(&mut self) {
        self.release();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "fixture/run failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "run cleanup exceeded 30s");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        // Only our directly spawned child is signalled. For umbra, owner loss
        // activates the existing provider cleanup; never kill by process name.
        self.child.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn registry(repo: &Path, root: &Path) -> PathBuf {
    let bin = Path::new(env!("CARGO_BIN_EXE_umbra")).parent().unwrap();
    let descriptor = |name: &str, caps: &[&str], options: Vec<u8>| {
        let mut d: Value = serde_json::from_slice(
            &std::fs::read(repo.join("crates").join(name).join("provider.json")).unwrap(),
        )
        .unwrap();
        assert!(bin.join(name).is_file(), "build provider {name} first");
        d["executable"] = json!(bin.join(name).as_os_str().as_bytes());
        d["capabilities"] = json!(caps);
        d["options"] = json!(options);
        d
    };
    let store = root.join("store");
    std::fs::create_dir(&store).unwrap();
    let r = json!({"timeout_ms": 25000, "providers": {
        "platform": descriptor("umbra-platform-macos", &["sandboxed-stopped-launch-v1", "experimental-syscall-rewrite-v1"], b"{}".to_vec()),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor("umbra-storage-local", &["local-development-v1", "experimental-open-rewrite-v1"], serde_json::to_vec(store.as_os_str().as_bytes()).unwrap())
    }});
    let path = root.join("registry.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&r).unwrap()).unwrap();
    path
}

#[test]
fn fix_274_completed_children_census() {
    if !qualified() {
        eprintln!("SKIP: child_retention requires debugger qualification");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = root.join("umbra-fix-274-children");
    assert!(Command::new("clang")
        .args(["-arch", "arm64", "-O1", "-Wall", "-Wextra", "-Werror"])
        .arg(repo.join("experiments/fixtures/umbra-fix-274-children.c"))
        .arg("-o")
        .arg(&fixture)
        .status()
        .unwrap()
        .success());
    let mut native = Run::start(Command::new(&fixture), &root.join("native.stderr"));
    native.marker("ready pid=");
    native.release();
    native.marker("completed=200");
    let native_children: Vec<_> = processes()
        .into_iter()
        .filter(|p| p.parent == native.child.id())
        .collect();
    native.finish();
    assert!(
        native_children.is_empty(),
        "native wait control: {native_children:?}"
    );
    eprintln!("native control: 200 successful waits; no children retained");

    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbra"));
    command
        .args(["run", "--registry"])
        .arg(registry(&repo, root))
        .arg("--workspace")
        .arg(workspace)
        .arg("--state-dir")
        .arg(root.join("state"))
        .args(["--experimental", "--local-dev", "--"])
        .arg(&fixture);
    let stderr = root.join("supervised.stderr");
    let mut run = Run::start(command, &stderr);
    let tracee: u32 = run
        .marker("ready pid=")
        .trim_start_matches("ready pid=")
        .parse()
        .unwrap();
    let table = processes();
    let parent = |pid| {
        table
            .iter()
            .find(|p| p.pid == pid)
            .expect("owned process missing")
            .parent
    };
    // The fixture's parent is its tracer, whose parent owns the RSP socket.
    let provider = parent(parent(tracee));
    let mut ancestor = provider;
    while ancestor != run.child.id() {
        assert!(ancestor > 1, "provider is not in this run's descendants");
        ancestor = parent(ancestor);
    }
    let baseline = fds(provider);
    run.release();
    run.marker("completed=200");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        run.child.try_wait().unwrap().is_none(),
        "root must remain alive"
    );
    let final_fds = fds(provider);
    let children: Vec<_> = processes()
        .into_iter()
        .filter(|p| p.parent == provider)
        .collect();
    let zombies = children.iter().filter(|p| p.zombie).count();
    let live = children.len() - zombies;
    eprintln!("provider={provider} warm_fds={baseline} final_fds={final_fds} zombies={zombies} live_children={live}");
    run.finish();
    let diagnostic = std::fs::read_to_string(stderr).unwrap();
    eprintln!("supervised stderr:\n{diagnostic}");
    assert!(
        zombies == 0 && final_fds <= baseline + 2,
        "completed-child retention: zombies={zombies} (expected 0), fds={final_fds} (maximum {})",
        baseline + 2
    );
    if std::env::var_os("UMBRA_RETIREMENT_WITNESS").is_some() {
        let readings: Vec<_> = diagnostic
            .lines()
            .filter(|line| line.starts_with("retirement-ownership pid="))
            .collect();
        assert_eq!(
            readings.len(),
            200,
            "every child must have an ownership reading"
        );
        assert!(
            readings.iter().all(|line| line.ends_with("remaining=[]")),
            "retained child owners: {readings:?}"
        );
    }
}
