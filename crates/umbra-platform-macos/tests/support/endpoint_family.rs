//! Inspect native program boundaries while the tracer still holds each stop.
use super::{managed_admission, path};
use std::{
    os::unix::ffi::OsStrExt,
    time::{Duration, Instant},
};
use umbra_core::*;
use umbra_platform::{TraceBackend, TraceControl};
use umbra_platform_macos::{
    endpoint_probe::{inherited_sockets, Probe, Socket},
    MacosTraceBackend, Options,
};

fn inspect(pid: u64, expected: &[Socket], errors: &mut Vec<String>) {
    let result = u32::try_from(pid)
        .map_err(|e| e.to_string())
        .and_then(|pid| inherited_sockets(pid, expected).map_err(|e| e.to_string()));
    match result {
        Ok(fds) if fds.is_empty() => {}
        other => errors.push(format!("program {pid} endpoint inspection: {other:?}")),
    }
}

pub fn run() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("endpoint-family.c");
    let program = directory.path().join("endpoint-family");
    std::fs::write(
        &source,
        r#"
#include <spawn.h>
#include <errno.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static int collected(pid_t child) {
    int status;
    pid_t waited;
    do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != child) return 1;
    return !WIFEXITED(status) || WEXITSTATUS(status);
}
int main(void) {
    for (int exec_child = 0; exec_child != 2; ++exec_child) {
        pid_t child = fork();
        if (child < 0) return 10;
        if (!child) {
            if (exec_child) {
                execl("/usr/bin/true", "true", (char *)0);
                _exit(11);
            }
            _exit(0);
        }
        if (collected(child)) return 12;
    }
    posix_spawnattr_t attr;
    if (posix_spawnattr_init(&attr)) return 13;
    pid_t child;
    char *args[] = {"true", 0};
    int result = posix_spawn(&child, "/usr/bin/true", 0, &attr, args, environ);
    posix_spawnattr_destroy(&attr);
    if (result) return 14;
    return collected(child) ? 15 : 0;
}
"#,
    )
    .unwrap();
    assert!(std::process::Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .status()
        .unwrap()
        .success());
    let sentinel = std::os::unix::net::UnixStream::pair().unwrap();
    let probe = Probe::new().unwrap();
    let run_root = directory.path().canonicalize().unwrap();
    let profile = include_str!("../../../../experiments/seatbelt/umbra.sb")
        .replace("{{UMBRA_RUN_ROOT}}", &format!("\"{}\"", run_root.display()));
    let mut backend = MacosTraceBackend::new(Options {
        twin_cache: Some(directory.path().join("twins")),
        timeout_ms: Some(30_000),
        ..Options::default()
    });
    let (_count, service) = managed_admission::install(&mut backend);
    let root = backend
        .launch(LaunchSpec {
            executable: path(&program),
            argv: vec![program.as_os_str().as_bytes().to_vec()],
            environment: vec![],
            cwd: path(directory.path()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::Required(
                SandboxProfile::new(
                    SEATBELT_PROFILE_FORMAT,
                    profile.into_bytes(),
                    path(&run_root),
                )
                .unwrap(),
            ),
        })
        .unwrap();
    let mut errors = Vec::new();
    let mut expected = Vec::new();
    match probe.finish() {
        Ok(report) => {
            if report
                .sockets
                .iter()
                .any(|socket| socket.retained_before_exec)
            {
                errors.push("family debugger retained a pre-exec socket".into());
            }
            match inherited_sockets(std::process::id(), &report.sockets) {
                Ok(live) => {
                    // Keep only parent sockets still owned throughout the
                    // fixture, excluding the now-closed setup listener.
                    expected = report
                        .sockets
                        .into_iter()
                        .filter(|socket| live.contains(&socket.fd))
                        .collect();
                }
                Err(error) => errors.push(error.to_string()),
            }
            inspect(report.pid as u64, &expected, &mut errors);
        }
        Err(error) => errors.push(error.to_string()),
    }
    if expected.len() < 2 {
        errors.push("family parent endpoint identities missing".into());
    }
    inspect(root.0 .0.native_id, &expected, &mut errors);
    let (mut live, mut forks, mut spawns, mut child_execs) = (1usize, 0, 0, 0);
    let observation_bound = Instant::now() + Duration::from_secs(30);
    while live > 0 {
        if Instant::now() >= observation_bound {
            errors.push("family observation bound reached".into());
            break;
        }
        let event = match backend.poll_event(Duration::from_millis(10)) {
            Ok(umbra_platform::EventPoll::Idle) => continue,
            Ok(umbra_platform::EventPoll::Event(event)) => event,
            Err(error) => {
                errors.push(error.to_string());
                break;
            }
        };
        let resume = match event {
            TraceEvent::Child { child, kind, .. } => {
                if kind != ChildKind::Fork {
                    errors.push(format!("unexpected ordinary child kind {kind:?}"));
                }
                forks += 1;
                live += 1;
                inspect(child.0 .0.native_id, &expected, &mut errors);
                None
            }
            TraceEvent::SpawnPrepare { token, plan } => Some((
                token.thread,
                ResumeMode::SpawnAuthorize(SpawnAuthorization {
                    token,
                    digest: plan.digest,
                    table_generation: 1,
                }),
            )),
            TraceEvent::SpawnChild {
                authorization,
                child,
            } => {
                spawns += 1;
                live += 1;
                inspect(child.0 .0.native_id, &expected, &mut errors);
                Some((
                    authorization.token.thread,
                    ResumeMode::SpawnChildReady(authorization),
                ))
            }
            TraceEvent::Exec { task, thread, .. } => {
                if task != root.0 {
                    child_execs += 1;
                }
                inspect(task.0.native_id, &expected, &mut errors);
                Some((thread, ResumeMode::Syscall))
            }
            TraceEvent::ThreadStarted { thread, .. }
            | TraceEvent::SyscallEntry { thread, .. }
            | TraceEvent::SyscallExit { thread, .. } => Some((thread, ResumeMode::Syscall)),
            TraceEvent::Exit { status, .. } => {
                if status != ExitStatus::Code(0) {
                    errors.push(format!("family wait/exit failure: {status:?}"));
                }
                live -= 1;
                None
            }
            other => {
                errors.push(format!("unexpected family event {other:?}"));
                break;
            }
        };
        if let Some((thread, mode)) = resume {
            if let Err(error) = backend.resume(ResumeCommand {
                thread,
                mode,
                signal: None,
            }) {
                errors.push(error.to_string());
                break;
            }
        }
    }
    let start = backend.begin_shutdown(shutdown::Nonce::new_v4());
    drop(backend);
    drop(sentinel);
    let umbra_platform::shutdown::Start::Local { accepted, ledger } = start.unwrap() else {
        panic!("family native custody missing")
    };
    let confirmed = accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap();
    drop(ledger);
    let remaining = service.join().unwrap();
    eprintln!(
        "native family: forks={forks} spawns={spawns} child_execs={child_execs} live={live} confirmed={confirmed} remaining={remaining}"
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!((live, forks, spawns), (0, 2, 1));
    assert!(
        child_execs >= 1,
        "fork child exec boundary was not inspected"
    );
    assert!(confirmed, "family native collection unconfirmed");
    assert_eq!(
        remaining, 0,
        "family program charge remained after actual collection"
    );
}
