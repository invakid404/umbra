use super::*;
use std::io::{BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};

fn command(root: &Path, target: &(String, u16), native: bool, mode: &str) -> Command {
    let mut c = super::stdio201::command_fixture(
        root,
        target,
        native,
        mode,
        "umbra-fix-284-signals",
        "UMBRA_F284_SIGNALS",
    );
    // Match a shell's default SIGPIPE, and avoid the runner's SID-1 lifecycle.
    unsafe {
        c.pre_exec(|| {
            if libc::setsid() < 0 || libc::getsid(0) == 1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }
    c.stdin(Stdio::null()).stdout(Stdio::piped());
    c
}

#[test]
fn f284_initial_sigpipe_default() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for native in [true, false] {
        let root = scratch.path().join(format!("initial-{native}"));
        let out = command(&root, &target, native, "initial").output().unwrap();
        eprintln!(
            "initial native={native}: {:?} stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success());
        assert_eq!(out.stdout, b"initial pipe=default blocked=0\n");
    }
}

#[test]
fn f284_handler_io() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for native in [true, false] {
        let root = scratch.path().join(format!("handler-{native}"));
        let mut cmd = command(&root, &target, native, "handler-io");
        let workspace = root.join("workspace");
        if native {
            std::fs::create_dir(&workspace).unwrap();
        }
        let destination = workspace.join("handler.txt");
        std::fs::write(&destination, b"host sentinel\n").unwrap();
        let out = cmd.arg(&destination).output().unwrap();
        assert!(
            out.status.success(),
            "{:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        // The fixture reopens through ordinary mediation and compares the bytes.
        assert_eq!(out.stdout, b"handler\n");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            if native {
                b"handler\n".as_slice()
            } else {
                b"host sentinel\n".as_slice()
            }
        );
    }
}

#[test]
fn f284_fork_exec_resume() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for (mode, expected) in [
        ("pending-exec", b"pending once\n".as_slice()),
        ("ignore-exec", b"ignored after exec\n".as_slice()),
    ] {
        for native in [true, false] {
            let root = scratch.path().join(format!("{mode}-{native}"));
            let out = command(&root, &target, native, mode).output().unwrap();
            eprintln!(
                "{mode} native={native} {:?}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success());
            assert_eq!(out.stdout, expected);
            if !native {
                let run = super::stdio201::run_record(&root, &out);
                assert!(journal_records_completion(&run));
                // A completed reopen must not require a platform or replay a signal.
                let mut registry: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&run.registry).unwrap()).unwrap();
                registry["providers"]
                    .as_object_mut()
                    .unwrap()
                    .remove("platform");
                std::fs::write(&run.registry, serde_json::to_vec(&registry).unwrap()).unwrap();
                let resumed = Command::new(binaries().join("umbra"))
                    .arg("resume")
                    .arg(run.run_id.0.to_string())
                    .arg("--registry")
                    .arg(&run.registry)
                    .arg("--workspace")
                    .arg(&run.workspace)
                    .arg("--state-dir")
                    .arg(&run.state)
                    .output()
                    .unwrap();
                assert!(
                    resumed.status.success(),
                    "{}",
                    String::from_utf8_lossy(&resumed.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&resumed.stderr).contains("requires no reconciliation")
                );
                assert!(resumed.stdout.is_empty());
            }
        }
    }
}

#[test]
fn f284_concurrency() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for native in [true, false] {
        let mut children = Vec::new();
        for index in 0..2 {
            let root = scratch.path().join(format!("concurrent-{native}-{index}"));
            let mut child = command(&root, &target, native, "concurrent")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "READY\n");
            children.push(child);
        }
        // Both independent sessions are alive before either generates a signal.
        for child in &mut children {
            assert!(child.try_wait().unwrap().is_none());
            child.stdin.take().unwrap().write_all(b"q").unwrap();
        }
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(out.stdout, b"ONE\n");
        }
    }
}

#[test]
fn f284_thread_delivery() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for native in [true, false] {
        let root = scratch.path().join(format!("directed-{native}"));
        let out = command(&root, &target, native, "directed")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"directed once\n");
    }
}

#[test]
fn f284_sources() {
    use std::os::fd::OwnedFd;
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for (mode, signal) in [
        ("timer", libc::SIGALRM),
        ("inherited-socket", libc::SIGPIPE),
    ] {
        for native in [true, false] {
            for disposition in ["default", "ignored", "handler"] {
                let root = scratch
                    .path()
                    .join(format!("{mode}-{native}-{disposition}"));
                let mut cmd = command(&root, &target, native, mode);
                cmd.arg(signal.to_string()).arg(disposition);
                if mode == "inherited-socket" {
                    let (writer, peer) = std::os::unix::net::UnixStream::pair().unwrap();
                    drop(peer);
                    cmd.stdout(Stdio::from(OwnedFd::from(writer)));
                }
                let out = cmd.output().unwrap();
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "{mode} native={native} {disposition}: {:?}\n{stderr}",
                    out.status
                );
                if disposition == "default" {
                    if native {
                        assert_eq!(out.status.signal(), Some(signal));
                    } else {
                        assert_eq!(out.status.code(), Some(1));
                        assert!(stderr.contains("ProcessFailed"));
                        assert!(stderr.contains(&format!("Signal({signal})")));
                    }
                } else {
                    assert!(out.status.success());
                    let count = if disposition == "handler" { 1 } else { 0 };
                    let expected = format!("continued count={count}\n");
                    if mode == "inherited-socket" {
                        assert!(stderr.contains(&expected));
                    } else {
                        assert_eq!(out.stdout, expected.as_bytes());
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "one rate reading per head; invoke explicitly with --ignored"]
fn f284_return_window_rate() {
    let Some(target) = fixture() else { return };
    let evidence = std::env::var_os("UMBRA_F284_RATE_EVIDENCE")
        .expect("rate reading needs a dedicated evidence file");
    let mut evidence = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evidence)
        .expect("never overwrite a rate reading");
    let scratch = tempfile::tempdir().unwrap();
    const TRIALS: usize = 6;
    let mut delivered = 0;
    let mut refused = 0;
    for native in [true, false] {
        for trial in 0..TRIALS {
            let root = scratch.path().join(format!("rate-{native}-{trial}"));
            let mut cmd = command(&root, &target, native, "rate");
            let workspace = root.join("workspace");
            if native {
                std::fs::create_dir(&workspace).unwrap();
            }
            for worker in 0..2 {
                std::fs::write(workspace.join(format!("thread-{worker}")), b"io\n").unwrap();
            }
            let mut child = cmd.arg(workspace).spawn().unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut ready)
                .unwrap();
            let Some(pid) = ready.trim().strip_prefix("READY ") else {
                let out = child.wait_with_output().unwrap();
                let record = serde_json::json!({"native": native, "trial": trial,
                    "threads": 2, "sent": 0, "outcome": "readiness-failure",
                    "ready": ready, "status": out.status.code(),
                    "stderr": String::from_utf8_lossy(&out.stderr)});
                writeln!(evidence, "{record}").unwrap();
                evidence.sync_all().unwrap();
                panic!("no rate reading without fixture readiness: {record}");
            };
            let pid: i32 = pid.parse().unwrap();
            assert!(pid > 1);
            // PID comes from this owned fixture's pipe, not process-name discovery.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGUSR1) }, 0);
            let out = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            let outcome = if out.status.success() && out.stdout == b"delivered=1\n" {
                if !native {
                    delivered += 1;
                }
                "delivered"
            } else if !native
                && out.status.code() == Some(1)
                && stderr.contains("signal-during-return-window")
            {
                refused += 1;
                "signal-during-return-window"
            } else {
                "other/no-reading"
            };
            let record = serde_json::json!({"native": native, "trial": trial, "threads": 2,
                "sent": 1, "pid": pid, "status": out.status.code(), "outcome": outcome,
                "stderr": stderr});
            writeln!(evidence, "{record}").unwrap();
            evidence.sync_all().unwrap();
            eprintln!("{record}");
            assert_ne!(
                outcome, "other/no-reading",
                "fixture did not produce a classified rate reading"
            );
        }
    }
    eprintln!("rate: N=2 K={TRIALS} delivered={delivered} refused={refused}");
}

fn unsafe_state(phase: &str, expected: &str) {
    let Some(target) = fixture() else { return };
    // CI builds the provider with signal-witness for this named command.
    // A missing witness is a failure, never an accepted refusal.
    let scratch = tempfile::tempdir().unwrap();
    {
        let native_root = scratch.path().join(format!("{phase}-native"));
        let mut native = command(&native_root, &target, true, "unsafe");
        native.args([phase, "native"]).stdin(Stdio::piped());
        let mut child = native.spawn().unwrap();
        if phase == "park" {
            child.stdin.take().unwrap().write_all(b"q").unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "native {phase}: {:?}", out.status);
        if phase != "exec" {
            assert_eq!(out.stdout, b"unsafe returned count=1\n");
        }

        let root = scratch.path().join(format!("{phase}-routed"));
        let mut cmd = command(&root, &target, false, "unsafe");
        let record = root.join("witness.json");
        cmd.args([phase, "routed"])
            .env("UMBRA_FIX284_PHASE", phase)
            .env("UMBRA_FIX284_RECORD", &record)
            .stdin(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if phase == "park" {
            let limit = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let ready = loop {
                if std::fs::read(&record)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .is_some()
                {
                    break true;
                }
                if child.try_wait().unwrap().is_some() {
                    break false;
                }
                if std::time::Instant::now() >= limit {
                    // Test instrumentation timeout, not a backend idle policy.
                    child.kill().unwrap();
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            if !ready {
                let out = child.wait_with_output().unwrap();
                panic!(
                    "no parked-wait reading: {:?}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            // Parent's debugger stop is still held; release its owned child.
            assert!(child.try_wait().unwrap().is_none());
            child.stdin.take().unwrap().write_all(b"q").unwrap();
        }
        let out = child.wait_with_output().unwrap();
        let witness: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).expect("missing state witness"))
                .unwrap();
        eprintln!(
            "phase={phase} witness={witness} status={:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        if matches!(phase, "identity" | "ambiguous") {
            let fault: serde_json::Value = serde_json::from_slice(
                &std::fs::read(format!("{}.stop.json", record.display())).unwrap(),
            )
            .unwrap();
            eprintln!("transport fault witness: {fault}");
            assert!(fault["original"].as_str().unwrap().starts_with("T1e"));
            assert_eq!(fault["fault"], phase);
            if phase == "identity" {
                assert!(!fault["dispatched"].as_str().unwrap().contains("thread:"));
            } else {
                assert_eq!(fault["interrupted"], true);
            }
        }
        assert_eq!(witness["phase"], phase);
        assert_eq!(witness["stopped"], true);
        match phase {
            "return-window" | "exec" => assert_eq!(witness["pending"], 1),
            "park" => assert!(witness["waiting"].as_u64().unwrap() > 0),
            "entry" => assert_eq!(witness["pending"], 0),
            _ => {}
        }
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains(expected));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("unsafe returned"));
    }
}

#[test]
fn f284_dispositions() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for native in [true, false] {
        for signal in [
            libc::SIGPIPE,
            libc::SIGTERM,
            libc::SIGUSR1,
            libc::SIGUSR2,
            libc::SIGALRM,
        ] {
            for disposition in ["default", "ignored", "handler"] {
                let root = scratch
                    .path()
                    .join(format!("{native}-{signal}-{disposition}"));
                let out = command(&root, &target, native, "disposition")
                    .arg(signal.to_string())
                    .arg(disposition)
                    .output()
                    .unwrap();
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "native={native} signal={signal} disposition={disposition}: {:?}\n{stderr}",
                    out.status
                );
                let valid = if disposition == "default" {
                    if native {
                        out.status.signal() == Some(signal)
                    } else {
                        out.status.code() == Some(1)
                            && stderr.contains("ProcessFailed")
                            && stderr.contains(&format!("Signal({signal})"))
                    }
                } else {
                    out.status.success()
                        && out.stdout
                            == if disposition == "handler" {
                                b"continued count=1\n".as_slice()
                            } else {
                                b"continued count=0\n".as_slice()
                            }
                };
                if !valid {
                    failures.push(format!("{native}-{signal}-{disposition}"));
                }
            }
        }
        assert!(
            native == false || failures.is_empty(),
            "native control: {failures:?}"
        );
    }
    assert!(failures.is_empty(), "signal mismatches: {failures:?}");
}

#[test]
fn f284_unsafe_entry() {
    unsafe_state("entry", "signal-at-syscall-entry");
}
#[test]
fn f284_unsafe_return_window() {
    unsafe_state("return-window", "signal-during-return-window");
}
#[test]
fn f284_unsafe_exec() {
    unsafe_state("exec", "signal-exec-transition");
}
#[test]
fn f284_unsafe_internal() {
    unsafe_state("internal", "signal-during-internal-operation");
}
#[test]
fn f284_unsafe_park() {
    unsafe_state("park", "signal-during-return-window");
}

#[test]
fn f284_unsafe_identity() {
    unsafe_state("identity", "signal-stop-identity");
}
#[test]
fn f284_unsafe_ambiguous() {
    unsafe_state("ambiguous", "ambiguous-interrupt-signal");
}
