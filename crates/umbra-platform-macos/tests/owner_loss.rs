#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::{
    fs,
    io::Read,
    net::Shutdown,
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use umbra_core::{
    provider::{self as wire, Client, Connection, Hello, Welcome},
    *,
};
use umbra_platform::provider::{Request, Response};

fn qualified() -> bool {
    std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_some()
}
fn call(c: &mut Client, r: Request) -> Response {
    c.call(&r).unwrap()
}

// Invoked in a separate process so this process is the provider's actual parent.
#[test]
fn owner_process() {
    let Some(dir) = std::env::var_os("UMBRA_FIX264_OWNER_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let listener = UnixListener::bind(dir.join("socket")).unwrap();
    let provider = Command::new(env!("CARGO_BIN_EXE_umbra-platform-macos"))
        .arg("--umbra-socket")
        .arg(dir.join("socket"))
        .args(["--umbra-timeout-ms", "10000"])
        .stderr(fs::File::create(dir.join("provider.log")).unwrap())
        .spawn()
        .unwrap();
    let (stream, _) = listener.accept().unwrap();
    let retained = stream.try_clone().unwrap();
    let mut conn = Connection::new(stream, Duration::from_secs(10));
    conn.send(&Hello {
        id: "macos".into(),
        role: "platform".into(),
        version: wire::PROTOCOL_VERSION,
        required_capabilities: Default::default(),
        options: br#"{"timeout_ms":20000}"#.to_vec(),
    })
    .unwrap();
    let welcome = conn.receive::<Result<Welcome>>().unwrap().unwrap();
    let mut client = Client::from_connection(conn, welcome);
    let path = |p: &str| BytePath::new(p.as_bytes().to_vec()).unwrap();
    let Response::Process(root) = call(
        &mut client,
        Request::Launch(LaunchSpec {
            executable: path("/bin/sleep"),
            argv: vec![b"sleep".to_vec(), b"20".to_vec()],
            environment: vec![],
            cwd: path("/tmp"),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        }),
    ) else {
        panic!("launch")
    };
    assert!(matches!(
        call(&mut client, Request::NextEvent),
        Response::Event(TraceEvent::ThreadStarted { .. })
    ));
    // Keep the launch stop unresumed. The second NextEvent cannot complete
    // normally, so a serve-loop EOF check cannot mask a missing monitor.
    client.start(&Request::NextEvent).unwrap();
    let helper = if std::env::var_os("UMBRA_FIX264_DUP").is_some() {
        Some(
            Command::new("/bin/sleep")
                .arg("20")
                .stdin(Stdio::from(std::os::fd::OwnedFd::from(
                    retained.try_clone().unwrap(),
                )))
                .spawn()
                .unwrap(),
        )
    } else {
        None
    };
    fs::write(
        dir.join("ready.tmp"),
        format!(
            "{} {} {}",
            provider.id(),
            root.0 .0.native_id,
            helper.as_ref().map_or(0, |c| c.id())
        ),
    )
    .unwrap();
    fs::rename(dir.join("ready.tmp"), dir.join("ready")).unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    retained.shutdown(Shutdown::Write).unwrap();
    fs::write(dir.join("eof"), b"sent").unwrap();
    // The parent test owns termination; keep this supervisor alive for EOF.
    let _ = std::io::stdin().read_exact(&mut byte);
    std::mem::forget(client);
}

#[derive(Debug)]
struct Owned {
    pid: i32,
    birth: (u64, u64),
}
fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut out = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: libproc writes at most the supplied initialized struct size.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            out.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if n == 0 {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "census unavailable"
        );
        return None;
    }
    assert_eq!(n as usize, std::mem::size_of::<libc::proc_bsdinfo>());
    Some(unsafe { out.assume_init() })
}
impl Owned {
    fn capture(pid: i32) -> Self {
        let i = info(pid).expect("owned process exists");
        Self {
            pid,
            birth: (i.pbi_start_tvsec, i.pbi_start_tvusec),
        }
    }
    fn live(&self) -> bool {
        info(self.pid).is_some_and(|i| {
            (i.pbi_start_tvsec, i.pbi_start_tvusec) == self.birth && i.pbi_status != 5
        })
    }
}
struct Recovery(Vec<Owned>);
impl Drop for Recovery {
    fn drop(&mut self) {
        for p in &self.0 {
            if p.live() {
                // SAFETY: identity was revalidated against the recorded owned process.
                unsafe {
                    libc::kill(p.pid, libc::SIGKILL);
                }
            }
        }
    }
}
fn loss(duplicate: bool) {
    if !qualified() {
        eprintln!("SKIP: owner loss requires UMBRA_INTEGRATION_REQUIRED");
        return;
    }
    use std::os::unix::process::ExitStatusExt;
    let mut native = Command::new("/bin/sleep").arg("20").spawn().unwrap();
    native.kill().unwrap();
    assert_eq!(
        native.wait().unwrap().signal(),
        Some(libc::SIGKILL),
        "native signal control"
    );
    let scratch = tempfile::tempdir().unwrap();
    let root = std::env::var_os("UMBRA_FIX264_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| scratch.path().to_owned());
    let dir = root.join(if duplicate { "retained-peer" } else { "eof" });
    fs::create_dir(&dir).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "owner_process", "--nocapture"])
        .env("UMBRA_FIX264_OWNER_DIR", dir.as_path())
        .stdin(Stdio::piped());
    if duplicate {
        command.env("UMBRA_FIX264_DUP", "1");
    }
    let mut owner = command.spawn().unwrap();
    let ready = dir.as_path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        assert!(owner.try_wait().unwrap().is_none(), "owner setup failed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "owner setup deadline");
    let ids: Vec<i32> = fs::read_to_string(ready)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let mut owned = Recovery(
        ids.iter()
            .copied()
            .filter(|p| *p > 0)
            .map(Owned::capture)
            .collect(),
    );
    let ps = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .unwrap();
    assert!(ps.status.success());
    for line in String::from_utf8(ps.stdout).unwrap().lines() {
        let v: Vec<i32> = line
            .split_whitespace()
            .map(|x| x.parse().unwrap())
            .collect();
        if v[1] == ids[0] && v[0] != ids[1] {
            owned.0.push(Owned::capture(v[0]));
        }
    }
    assert!(
        owned.0.len() >= if duplicate { 4 } else { 3 },
        "owned debugger identified"
    );
    eprintln!("owned before trigger: {:?}", owned.0);
    if duplicate {
        owner.kill().unwrap();
        owner.wait().unwrap();
    } else {
        use std::io::Write;
        owner.stdin.as_mut().unwrap().write_all(b"x").unwrap();
    }
    let until = Instant::now() + Duration::from_secs(4);
    let mut confirmed = false;
    while Instant::now() < until {
        let log = fs::read_to_string(dir.as_path().join("provider.log")).unwrap();
        if log.contains("tracee exit confirmed")
            && owned
                .0
                .iter()
                .filter(|p| p.pid != ids[2])
                .all(|p| !p.live())
        {
            confirmed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = fs::read_to_string(dir.as_path().join("provider.log")).unwrap();
    eprintln!("primary confirmation={confirmed}; provider log:\n{log}");
    let survivors: Vec<_> = owned
        .0
        .iter()
        .filter(|p| p.pid != ids[2] && p.live())
        .collect();
    eprintln!("primary survivors={survivors:?}");
    if duplicate {
        assert!(
            owned.0.iter().find(|p| p.pid == ids[2]).unwrap().live(),
            "duplicate still held"
        );
    } else {
        assert!(
            owner.try_wait().unwrap().is_none(),
            "supervisor stays alive"
        );
    }
    drop(owned); // Recovery is after the primary reading.
    let _ = owner.kill();
    let _ = owner.wait();
    assert!(confirmed, "owner-loss cleanup not confirmed within 4s");
}
#[test]
fn supervisor_exit_with_retained_socket() {
    loss(true);
}
#[test]
fn transport_eof_while_supervisor_lives() {
    loss(false);
}
