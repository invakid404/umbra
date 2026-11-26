//! Explicit D5 readings. Counts are observed separately from D6 latency runs.
#[cfg(target_os = "macos")]
#[test]
fn r2_startup_cost_reading() {
    use serde::Deserialize;
    use serde_json::Value;
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::File,
        path::PathBuf,
        process::Command,
        time::{Duration, Instant},
    };

    #[derive(Deserialize)]
    struct Case {
        executable: PathBuf,
        arguments: Vec<String>,
        environment: BTreeMap<String, String>,
        expected_exit: i32,
        timeout_seconds: u64,
        required_events: Vec<String>,
        #[serde(default)]
        own_session: bool,
    }
    let Some(case) = std::env::var_os("UMBRA_COST_CASE") else {
        eprintln!("D5 requires an explicit case, cost-witness build, and observer");
        return;
    };
    let value: Value = serde_json::from_slice(&std::fs::read(case).unwrap()).unwrap();
    let cases: Vec<Case> = if value.is_array() {
        serde_json::from_value(value).unwrap()
    } else {
        vec![serde_json::from_value(value).unwrap()]
    };
    assert!(!cases.is_empty());
    let single = cases.len() == 1;
    let directory = PathBuf::from(std::env::var_os("UMBRA_COST_DIRECTORY").unwrap());
    assert!(!directory.exists(), "never overwrite a cost reading");
    std::fs::create_dir_all(&directory).unwrap();
    let mut previous_rows = 0;
    for (index, case) in cases.into_iter().enumerate() {
        eprintln!("D5 case {index}: {}", case.executable.display());
        umbra_core::cost_snapshot!("driver-before-spawn", None, &[]);
        let began = Instant::now();
        let mut command = Command::new(&case.executable);
        command
            .args(&case.arguments)
            .envs(&case.environment)
            .stdout(
                File::create(directory.join(if single {
                    "stdout".into()
                } else {
                    format!("{index:02}.stdout")
                }))
                .unwrap(),
            )
            .stderr(
                File::create(directory.join(if single {
                    "stderr".into()
                } else {
                    format!("{index:02}.stderr")
                }))
                .unwrap(),
            );
        if case.own_session {
            use std::os::unix::process::CommandExt;
            // SAFETY: only async-signal-safe session syscalls run before exec.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 || libc::getsid(0) == 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() >= Duration::from_secs(case.timeout_seconds) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("D5 driver exceeded its declared timeout");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        umbra_core::cost_snapshot!("driver-after-collection", Some(child.id()), &[]);
        assert_eq!(status.code(), Some(case.expected_exit));
        assert!(!directory.join("errors").exists(), "cost observer failed");
        let all_rows: Vec<Value> = std::fs::read_to_string(directory.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let rows = &all_rows[previous_rows..];
        previous_rows = all_rows.len();
        let first = rows.first().unwrap();
        let last = rows.last().unwrap();
        assert_eq!(first["event"], "driver-before-spawn");
        assert_eq!(last["event"], "driver-after-collection");
        let caller = |row: &Value| {
            row["processes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|process| process["pid"] == std::process::id())
                .unwrap()
                .clone()
        };
        let descriptors = |process: &Value| {
            process["fds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|fd| (fd["fd"].as_i64().unwrap(), fd["kind"].as_u64().unwrap()))
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(descriptors(&caller(first)), descriptors(&caller(last)));
        assert_eq!(last["processes"].as_array().unwrap().len(), 1);
        let identity = |process: &Value| {
            (
                process["pid"].as_u64().unwrap(),
                process["birth"][0].as_u64().unwrap(),
                process["birth"][1].as_u64().unwrap(),
            )
        };
        let observed: BTreeSet<_> = rows
            .iter()
            .flat_map(|row| row["processes"].as_array().unwrap())
            .filter(|process| process["pid"] != std::process::id())
            .map(identity)
            .collect();
        if case.own_session {
            let selected: Vec<_> = rows
                .iter()
                .flat_map(|row| row["processes"].as_array().unwrap())
                .filter(|process| process["pid"] == child.id())
                .collect();
            assert!(!selected.is_empty(), "selected caller was never observed");
            assert!(selected
                .iter()
                .all(|process| process["session"] == child.id()));
        }
        let observers: BTreeSet<_> = std::fs::read_to_string(directory.join("observers"))
            .unwrap()
            .lines()
            .map(|line| {
                let fields: Vec<u64> = line
                    .split_whitespace()
                    .map(|field| field.parse().unwrap())
                    .collect();
                assert_eq!(fields.len(), 3);
                (fields[0], fields[1], fields[2])
            })
            .collect();
        assert!(
            observed.is_disjoint(&observers),
            "observer process counted as a product process"
        );
        for process in last["host_processes"].as_array().unwrap() {
            assert!(
                !observed.contains(&identity(process)),
                "owned process or zombie survived collection: {process}"
            );
        }
        assert!(!case.required_events.is_empty());
        for event in &case.required_events {
            assert!(
                rows.iter().any(|row| row["event"] == *event),
                "missing required cost boundary: {event}"
            );
        }
        if case
            .required_events
            .iter()
            .any(|event| event == "control-open")
        {
            let own = |event: &str| {
                let row = rows.iter().find(|row| row["event"] == event).unwrap();
                let actor = row["actor"].as_u64().unwrap();
                row["processes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|process| process["pid"] == actor)
                    .unwrap()
                    .clone()
            };
            let before = descriptors(&own("control-before"));
            let opened = descriptors(&own("control-open"));
            assert!(before.is_subset(&opened));
            assert_eq!(opened.len(), before.len() + 2);
            assert_eq!(descriptors(&own("control-closed")), before);
            let row = rows
                .iter()
                .find(|row| row["event"] == "control-child")
                .unwrap();
            let child = row["child"].as_u64().unwrap();
            assert!(row["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|process| { process["pid"] == child && process["parent"] == row["actor"] }));
        }
    }
}

/// An ordinary owned child and two sockets calibrate the observer independently
/// of managed startup. The reading driver must see both changes and collection.
#[cfg(target_os = "macos")]
#[test]
fn r2_startup_cost_control() {
    if std::env::var_os("UMBRA_COST_CONTROL").is_none() {
        return;
    }
    umbra_core::cost_snapshot!("control-before", None, &[]);
    let sockets = std::os::unix::net::UnixStream::pair().unwrap();
    umbra_core::cost_snapshot!("control-open", None, &[]);
    drop(sockets);
    umbra_core::cost_snapshot!("control-closed", None, &[]);
    // A pipe, rather than a sleep, keeps the child alive through the census.
    use std::process::{Command, Stdio};
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    umbra_core::cost_snapshot!("control-child", Some(child.id()), &[]);
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let mut peers: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "r2_startup_cost_peer", "--nocapture"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for _ in 0..8 {
        umbra_core::cost_snapshot!("control-concurrent", None, &[]);
    }
    for peer in &mut peers {
        assert!(peer.wait().unwrap().success());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn r2_startup_cost_peer() {
    if std::env::var_os("UMBRA_COST_CONTROL").is_some() {
        for _ in 0..8 {
            umbra_core::cost_snapshot!("control-peer", None, &[]);
        }
    }
}
