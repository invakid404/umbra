//! Error surfaces for `umbra run`, exercised through the real binary.
//!
//! Every case here runs with stdin closed. The point is not only that these
//! invocations fail, but that they fail *by themselves*: no prompt, no installer,
//! no attempt to read configuration from a terminal.

use std::io::Write;
use std::process::{Command, Stdio};

fn umbra() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbra"));
    // Closed stdin: anything that tried to ask a question would fail loudly here
    // instead of blocking a test run or a CI job.
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = umbra().args(args).output().expect("umbra binary runs");
    (
        output.status.code().expect("exited without a signal"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A registry whose descriptors are syntactically valid but advertise nothing.
fn registry(path: &std::path::Path, capabilities: &str) {
    registry_with_agent(path, capabilities, false)
}

fn registry_with_agent(path: &std::path::Path, capabilities: &str, agent: bool) {
    let executable: Vec<String> = b"/bin/true".iter().map(|b| b.to_string()).collect();
    let executable = executable.join(",");
    let descriptor = |id: &str, role: &str, caps: &str| {
        format!(
            r#""{role}":{{"id":"{id}","role":"{role}","protocol_version":1,"executable":[{executable}],"capabilities":[{caps}],"options":[]}}"#
        )
    };
    let mut roles = vec![
        descriptor("local", "storage", capabilities),
        descriptor("file", "journal", ""),
        descriptor("macos", "platform", ""),
    ];
    if agent {
        roles.push(descriptor("codex", "agent", ""));
    }
    let body = format!(
        r#"{{"timeout_ms":5000,"providers":{{{}}}}}"#,
        roles.join(",")
    );
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("umbra-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn contradictory_storage_modes_are_a_usage_error() {
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        "/nonexistent/registry.json",
        "--experimental",
        "--local-dev",
        "--strict-remote",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 2, "clap usage diagnostics exit 2: {stderr}");
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

#[test]
fn an_unreadable_registry_fails_without_asking_for_one() {
    let (code, stdout, stderr) = run(&[
        "run",
        "--registry",
        "/nonexistent/registry.json",
        "--experimental",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("registry.open"), "{stderr}");
    assert!(stdout.is_empty(), "diagnostics belong on stderr: {stdout}");
}

#[test]
fn the_experimental_acknowledgement_is_required_and_is_not_a_question() {
    let dir = scratch("experimental");
    let path = dir.join("registry.json");
    registry(
        &path,
        r#""local-development-v1","experimental-open-rewrite-v1""#,
    );
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--workspace",
        dir.to_str().unwrap(),
        "--local-dev",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--experimental"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unqualified_strict_remote_is_refused_rather_than_promised() {
    let dir = scratch("strict");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--workspace",
        dir.to_str().unwrap(),
        "--experimental",
        "--strict-remote",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("strict remote persistence"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_provider_that_does_not_advertise_the_mode_fails_before_connecting() {
    let dir = scratch("capability");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--workspace",
        dir.to_str().unwrap(),
        "--experimental",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    // Named capability and named remedy, with no offer to install anything.
    assert!(stderr.contains("mounted-nfsv4-v1"), "{stderr}");
    assert!(stderr.contains("registry"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_relative_command_is_rejected_because_path_is_not_searched() {
    let dir = scratch("relative");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--workspace",
        dir.to_str().unwrap(),
        "--experimental",
        "--",
        "true",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("absolute executable"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_command_names_the_expected_invocation() {
    let dir = scratch("nocommand");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--experimental",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("a command is required"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn storage_root_is_rejected_for_run_instead_of_silently_ignored() {
    let dir = scratch("storageroot");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "--storage-root",
        "/tmp/ignored",
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--experimental",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--storage-root"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn selecting_an_agent_requires_a_configured_agent_provider() {
    let dir = scratch("agent");
    let path = dir.join("registry.json");
    registry(&path, "");
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--experimental",
        "--agent",
        "codex",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("missing provider role: agent"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unset_inherited_variable_fails_rather_than_passing_an_empty_value() {
    let dir = scratch("inherit");
    let path = dir.join("registry.json");
    registry(
        &path,
        r#""local-development-v1","experimental-open-rewrite-v1""#,
    );
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--workspace",
        dir.to_str().unwrap(),
        "--experimental",
        "--local-dev",
        "--inherit-env",
        "UMBRA_DEFINITELY_UNSET_VARIABLE",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("UMBRA_DEFINITELY_UNSET_VARIABLE"),
        "{stderr}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_configured_agent_reports_that_adapters_are_unimplemented() {
    let dir = scratch("agent-configured");
    let path = dir.join("registry.json");
    registry_with_agent(&path, "", true);
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--experimental",
        "--agent",
        "codex",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not implemented"), "{stderr}");
    // A different ID than the registry declares is a naming error, not a silent
    // substitution of whichever adapter happens to be installed.
    let (code, _, stderr) = run(&[
        "run",
        "--registry",
        path.to_str().unwrap(),
        "--experimental",
        "--agent",
        "claude",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("is 'codex', not 'claude'"), "{stderr}");
    std::fs::remove_dir_all(dir).unwrap();
}
