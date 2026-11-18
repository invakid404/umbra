use super::*;
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
