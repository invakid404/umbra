//! Ruled stdio delivery controls. Reuses the suite's registry, binary discovery
//! and fixture compiler; no mount and no copied slate launch implementation.
use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;

const PAYLOAD: &[u8] = b"stdio-201 exact bytes\n";

fn command(root: &Path, target: &(String, u16), native: bool, mode: &str) -> Command {
    command_fixture(root, target, native, mode, "umbra-fix-201-stdio", "UMBRA_F201_STDIO")
}

pub(super) fn command_fixture(
    root: &Path,
    target: &(String, u16),
    native: bool,
    mode: &str,
    fixture_name: &str,
    fixture_env: &str,
) -> Command {
    std::fs::create_dir_all(root).unwrap();
    let program = fixture_binary(root, fixture_name, fixture_env);
    let mut command = if native {
        Command::new(program)
    } else {
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let registry = registry(root, &target.0, target.1);
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
        config["providers"]["platform"]["options"] = serde_json::json!(serde_json::to_vec(
            &serde_json::json!({"timeout_ms": 20_000})
        )
        .unwrap());
        std::fs::write(&registry, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut c = Command::new(binaries().join("umbra"));
        c.args(["run", "--registry"])
            .arg(registry)
            .arg("--workspace")
            .arg(workspace)
            .arg("--state-dir")
            .arg(root.join("state"))
            .args(["--experimental", "--"])
            .arg(program);
        c
    };
    command.arg(mode).stderr(Stdio::piped());
    command
}

fn pairs(mut run: impl FnMut(&Path, &(String, u16), bool)) {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    for native in [true, false] {
        let root = scratch
            .path()
            .join(if native { "native" } else { "routed" });
        eprintln!("f201 leg native={native}");
        run(&root, &target, native);
    }
}

fn success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    for fd in fds {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn f201_pipe_bytes_eof() {
    pairs(|root, target, native| {
        let mut child = command(root, target, native, "copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(PAYLOAD).unwrap();
        let out = child.wait_with_output().unwrap();
        success(&out);
        assert_eq!(out.stdout, PAYLOAD, "native={native}");
    });
}

#[test]
fn f201_socketpair_stdio() {
    pairs(|root, target, native| {
        let mut cmd = command(root, target, native, "copy");
        let (mut input, child_input) = UnixStream::pair().unwrap();
        let (mut output, child_output) = UnixStream::pair().unwrap();
        output
            .set_read_timeout(Some(std::time::Duration::from_secs(25)))
            .unwrap();
        let child = cmd
            .stdin(Stdio::from(OwnedFd::from(child_input)))
            .stdout(Stdio::from(OwnedFd::from(child_output)))
            .spawn()
            .unwrap();
        drop(cmd);
        input.write_all(PAYLOAD).unwrap();
        input.shutdown(std::net::Shutdown::Write).unwrap();
        let out = child.wait_with_output().unwrap();
        success(&out);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, PAYLOAD, "native={native}");
    });
}

#[test]
fn f201_regular_offset_append() {
    pairs(|root, target, native| {
        let mut cmd = command(root, target, native, "copy");
        let input_path = root.join("input");
        let output_path = root.join("output");
        std::fs::write(&input_path, [b"prefix:".as_slice(), PAYLOAD].concat()).unwrap();
        std::fs::write(&output_path, b"existing:").unwrap();
        let mut input = File::open(input_path).unwrap();
        input.seek(SeekFrom::Start(7)).unwrap();
        let output = OpenOptions::new().append(true).open(&output_path).unwrap();
        let out = cmd.stdin(input).stdout(output).output().unwrap();
        success(&out);
        assert_eq!(
            std::fs::read(output_path).unwrap(),
            [b"existing:".as_slice(), PAYLOAD].concat(),
            "native={native}"
        );
    });
}

#[test]
fn f201_null_stdio() {
    pairs(|root, target, native| {
        let out = command(root, target, native, "copy")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .unwrap();
        success(&out);
    });
}

#[test]
fn f201_pipe_sigpipe() {
    pairs(|root, target, native| {
        for ignored in [false, true] {
            let case = root.join(if ignored { "ignored" } else { "default" });
            let mut cmd = command(&case, target, native, "sigpipe");
            if ignored {
                cmd.arg("ignore");
            }
            let (reader, writer) = pipe();
            drop(reader);
            let out = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::from(writer))
                .output()
                .unwrap();
            eprintln!(
                "sigpipe native={native} ignored={ignored}: {:?}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            if native && ignored {
                success(&out);
            } else if native {
                assert_eq!(out.status.signal(), Some(libc::SIGPIPE));
            } else {
                assert_eq!(out.status.code(), Some(1));
                fatal_sigpipe(&case, &out);
            }
        }
    });
}

fn fatal_sigpipe(root: &Path, out: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("tracee stop"), "{stderr}");
    assert!(stderr.contains("fatal signal/exception: T0d"), "{stderr}");
    let state = root.join("state");
    let ids: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|name| uuid::Uuid::parse_str(&name).ok())
        .collect();
    assert_eq!(ids.len(), 1);
    let run = Run {
        run_id: RunId(ids[0]),
        status: out.status.code(),
        stderr: stderr.into_owned(),
        destination: root.join("unused"),
        state,
        workspace: root.join("workspace"),
        registry: root.join("unused-registry"),
    };
    assert!(
        !journal_records_completion(&run),
        "fatal stop recorded completion"
    );
}

#[test]
fn f201_self_pipe_sigpipe() {
    pairs(|root, target, native| {
        let out = command(root, target, native, "self-pipe")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .unwrap();
        eprintln!(
            "self-pipe native={native}: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        if native {
            assert_eq!(out.status.signal(), Some(libc::SIGPIPE));
        } else {
            fatal_sigpipe(root, &out);
        }
    });
}

#[test]
fn f201_pipe_eof_control_alive() {
    pairs(|root, target, native| {
        let mut command = command(root, target, native, "close-output");
        // This assertion concerns non-isolated ownership even on a SID-1 runner.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let sid = unsafe { libc::getsid(child.id() as libc::pid_t) };
        assert_eq!(sid, child.id() as libc::pid_t);
        assert_ne!(sid, 1, "early EOF is a non-isolated guarantee");
        let mut output = child.stdout.take().unwrap();
        let mut ready = libc::pollfd {
            fd: output.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut ready, 1, 10_000) };
        let alive = child.try_wait().unwrap().is_none();
        // Release and reap even on a failing EOF assertion.
        let release = child.stdin.take().unwrap().write_all(b"q");
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            polled, 1,
            "EOF absent while control process was alive; native={native}"
        );
        assert!(alive);
        release.unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
        success(&out);
    });
}

#[test]
fn f201_fork_exec_stdio() {
    pairs(|root, target, native| {
        let mut child = command(root, target, native, "fork-exec")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(PAYLOAD).unwrap();
        let out = child.wait_with_output().unwrap();
        success(&out);
        assert_eq!(out.stdout, PAYLOAD);
    });
}

#[test]
fn f201_rust_closed_slots() {
    let scratch = tempfile::tempdir().unwrap();
    let binary = rust_fixture_binary(
        scratch.path(),
        "umbra-fix-201-sanitize",
        "UMBRA_F201_SANITIZE",
    );
    let mut command = Command::new(binary);
    unsafe {
        command.pre_exec(|| {
            for fd in 0..3 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    assert!(
        command.status().unwrap().success(),
        "closed-slot branch of the gate ruling needs re-evaluation"
    );
}

#[test]
fn f201_fifo_stdio() {
    pairs(|root, target, native| {
        let mut cmd = command(root, target, native, "copy");
        let path = root.join("fifo");
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let reader = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        assert!(reader >= 0);
        let reader = unsafe { OwnedFd::from_raw_fd(reader) };
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        writer.write_all(PAYLOAD).unwrap();
        drop(writer);
        let out = cmd
            .stdin(Stdio::from(reader))
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        success(&out);
        assert_eq!(out.stdout, PAYLOAD, "native={native}");
    });
}

#[test]
fn f201_mixed_null_pipe() {
    pairs(|root, target, native| {
        let out = command(root, target, native, "write")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        success(&out);
        assert_eq!(out.stdout, b"f201-output\n");
    });
}

fn refused_endpoint(path: &Path) {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    // Native control demonstrates that the inherited descriptor itself is valid;
    // the fixture does not operate on it, so a directory/device read is unnecessary.
    for native in [true, false] {
        let root = scratch
            .path()
            .join(if native { "native" } else { "routed" });
        let out = command(&root, &target, native, "write")
            .stdin(File::open(path).unwrap())
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        if native {
            success(&out);
            assert_eq!(out.stdout, b"f201-output\n");
        } else {
            assert_eq!(out.status.code(), Some(1));
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(
                error.contains("UnsupportedCapability") && error.contains("stdio-endpoint-kind"),
                "{error}"
            );
            assert!(
                !error.contains(" prepared"),
                "refusal must precede provider launch"
            );
        }
    }
}

#[test]
fn f201_directory_refused() {
    let directory = tempfile::tempdir().unwrap();
    refused_endpoint(directory.path());
}

#[test]
fn f201_other_device_refused() {
    refused_endpoint(Path::new("/dev/zero"));
}

fn pty_case(root: &Path, target: &(String, u16), native: bool, fd: &str, background: bool) {
    let mut cmd = command(root, target, native, "pty-read");
    cmd.arg(fd);
    let harness = fixture_binary(root, "umbra-isolation-fixture", "UMBRA_F201_PTY_HARNESS");
    let output = Command::new(harness)
        .args(["pty", if background { "F201B" } else { "T1" }])
        .arg(root)
        .arg("--")
        .arg(cmd.get_program())
        .args(cmd.get_args())
        .output()
        .unwrap();
    success(&output);
    let summary = String::from_utf8_lossy(&output.stdout);
    let shell = std::fs::read_to_string(root.join("mshell.txt")).unwrap();
    let transcript = std::fs::read_to_string(root.join("transcript.log")).unwrap();
    eprintln!("native={native} background={background} fd={fd}\n{summary}\n{shell}\n{transcript}");
    assert!(
        summary.contains("ready=1") && summary.contains("timed_out=0"),
        "{summary}"
    );
    if background {
        assert!(shell.contains("stopped=21"), "{shell}");
    }
    if background && !native {
        assert!(shell.contains("ended=exit:1"), "{shell}");
        assert!(
            transcript.contains("fatal signal/exception: T15"),
            "{transcript}"
        );
    } else {
        assert!(shell.contains("ended=exit:0"), "{shell}");
        assert!(
            transcript.contains("F201 input matched exactly six bytes"),
            "{transcript}"
        );
        if fd == "0" {
            assert!(transcript.contains("f201-read-ok"), "{transcript}");
        }
    }
}

#[test]
fn f201_pty_stdin_stdout() {
    pairs(|root, target, native| pty_case(root, target, native, "0", false));
}

#[test]
fn f201_fd2_foreground() {
    pairs(|root, target, native| pty_case(root, target, native, "2", false));
}

#[test]
fn f201_fd2_background() {
    pairs(|root, target, native| pty_case(root, target, native, "2", true));
}

#[test]
fn f201_stdin_background() {
    pairs(|root, target, native| pty_case(root, target, native, "0", true));
}

#[test]
fn f201_parallel_ptys() {
    pairs(|root, target, native| {
        std::thread::scope(|scope| {
            let a = scope.spawn(|| pty_case(&root.join("a"), target, native, "0", false));
            let b = scope.spawn(|| pty_case(&root.join("b"), target, native, "0", false));
            a.join().unwrap();
            b.join().unwrap();
        });
    });
}

#[test]
fn f201_cloexec_sentinel() {
    pairs(|root, target, native| {
        let sentinel = File::open("/dev/null").unwrap();
        let out = command(root, target, native, "sentinel")
            .arg(sentinel.as_raw_fd().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .unwrap();
        success(&out);
    });
}

#[test]
fn f201_spawn_failure_rollback() {
    let Some(target) = fixture() else { return };
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("routed");
    let mut cmd = command(&root, &target, false, "write");
    let registry = root.join("registry.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
    let absent = root.join("absent-provider");
    assert_eq!(
        Command::new(&absent).spawn().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    config["providers"]["platform"]["executable"] =
        serde_json::json!(absent.as_os_str().as_bytes());
    std::fs::write(registry, serde_json::to_vec(&config).unwrap()).unwrap();
    let out = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No such file"));
}
