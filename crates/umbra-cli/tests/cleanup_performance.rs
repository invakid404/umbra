//! Cleanup performance qualification and retained-sample analysis.
#[path = "cleanup_performance_support/admission.rs"]
mod admission;
#[path = "cleanup_performance_support/analysis.rs"]
mod analysis;

#[test]
fn d6_admission_requires_every_acknowledged_phase_from_one_producer() {
    use serde_json::json;
    let rows = ["Reserve", "Entering", "Retiring", "Release"]
        .iter()
        .map(|phase| {
            json!({"event": "admission_exchange", "pid": 10, "birth": [1, 2],
                "details": {"accepted": true, "nanos": 7,
                    "operation": {(*phase): {"obligation": "one"}}}})
        })
        .collect::<Vec<_>>();
    assert_eq!(
        admission::components(&rows).unwrap(),
        vec![json!({"reserve": 7, "entering": 7, "completed": 14})]
    );
    assert!(admission::components(&rows[..3]).is_err());
    let mut changed = rows.clone();
    changed[3]["pid"] = json!(11);
    assert!(admission::components(&changed).is_err());
    changed = rows.clone();
    changed[2]["details"]["accepted"] = json!(false);
    assert!(admission::components(&changed).is_err());
    changed = rows.clone();
    changed.push(rows[3].clone());
    assert!(admission::components(&changed).is_err());
    changed = rows;
    changed.swap(1, 2);
    assert!(admission::components(&changed).is_err());
}

#[test]
fn d6_uses_distribution_differences_not_paired_quantiles() {
    let base: Vec<f64> = (0..30).map(|n| n as f64 * 1_000_000.0).collect();
    let head: Vec<f64> = base.iter().rev().copied().collect();
    let result = analysis::compare(&base, &head).unwrap();
    assert_eq!(result.base.median, 14_500_000.0);
    assert_eq!(result.base.p95, 28_000_000.0);
    assert_eq!(result.base.maximum, 29_000_000.0);
    assert_eq!(result.head.p95, result.base.p95);
    assert!(result.paired.p95 > 10_000_000.0);
    assert_eq!(result.differences.len(), 30);
    assert!(result.teardown_passes());
    assert!(result.helper_start_passes());
}

#[test]
fn d6_keeps_outliers_and_rejects_incomplete_samples() {
    let base = vec![0.0; 30];
    let mut head = base.clone();
    head[28] = 11_000_000.0;
    head[29] = 100_000_000.0;
    assert!(!analysis::compare(&base, &head).unwrap().teardown_passes());
    head[28] = 51_000_000.0;
    assert!(!analysis::compare(&base, &head)
        .unwrap()
        .helper_start_passes());
    assert!(analysis::compare(&base[..29], &head).is_err());
    assert!(analysis::distribution(&[f64::NAN]).is_err());
    assert!(analysis::admission_passes(&[1_000_000.0; 30]).unwrap());
    assert!(!analysis::admission_passes(&[1_000_001.0; 30]).unwrap());
}

#[path = "cleanup_performance_support/manifest.rs"]
mod manifest;

#[test]
fn d6_manifest_rejects_changed_artifacts() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("frozen-program");
    std::fs::write(&artifact, b"frozen artifact bytes").unwrap();
    let digest = manifest::sha256(&artifact).unwrap();
    let roles = ["cli", "platform", "storage", "journal", "namespace"];
    let value = json!({
        "schema_version": 1,
        "source_commit": "a".repeat(40), "source_tree": "b".repeat(40),
        "build_profile": "debug", "rustc_version": "test", "target_triple": "test",
        "observation_patch_sha256": "c".repeat(64),
        "workloads": [{"name": "native_true", "sha256": digest}],
        "artifacts": roles.iter().map(|role| json!({
            "role": role, "absolute_path": artifact, "sha256": digest
        })).collect::<Vec<_>>()
    });
    let path = dir.path().join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let loaded = manifest::load(&path, true).unwrap();
    assert_eq!(loaded.artifacts.len(), 5);
    assert_eq!(loaded.digest, manifest::sha256(&path).unwrap());
    std::fs::write(&artifact, b"replaced artifact").unwrap();
    assert!(manifest::load(&path, true)
        .err()
        .unwrap()
        .contains("hash mismatch"));
}

#[path = "cleanup_performance_support/config.rs"]
mod config;

#[path = "cleanup_performance_support/guards.rs"]
mod guards;

#[test]
fn d6_requires_explicit_configuration() {
    use std::collections::BTreeMap;
    let mut env = BTreeMap::from([
        ("UMBRA_F299_PERF_WORKLOAD", "native_true"),
        ("UMBRA_F299_PERF_SESSION", "own_session"),
        ("UMBRA_F299_PERF_BASE_MANIFEST", "/frozen/base.json"),
        ("UMBRA_F299_PERF_EVIDENCE", "/fresh/measurement"),
    ]);
    let read = |env: &BTreeMap<&str, &str>, paired| {
        config::read(|key| env.get(key).map(|v| v.to_string()), paired)
    };
    let c = read(&env, false).unwrap();
    assert!(c.base.is_absolute());
    assert!(c.evidence.is_absolute());
    assert!(c.head.is_none());
    assert!(c.registry.is_none());
    assert_eq!(c.workload, config::Workload::NativeTrue);
    assert_eq!(c.session, config::Session::OwnSession);
    assert!(read(&env, true).is_err());
    env.insert("UMBRA_F299_PERF_WORKLOAD", "registry_true");
    assert!(read(&env, false).is_err());
    env.insert("UMBRA_F299_PERF_REGISTRY", "/frozen/base-registry.json");
    env.insert("UMBRA_INTEGRATION_REQUIRED", "1");
    env.insert("UMBRA_F299_PERF_HEAD_MANIFEST", "/frozen/head.json");
    assert!(read(&env, false).is_ok());
    assert!(read(&env, true).is_err());
    env.insert(
        "UMBRA_F299_PERF_HEAD_REGISTRY",
        "/frozen/head-registry.json",
    );
    let paired = read(&env, true).unwrap();
    assert_ne!(paired.registry, paired.head_registry);
    env.insert("UMBRA_F299_PERF_WORKLOAD", "misspelled");
    assert!(read(&env, false).is_err());
}

#[path = "cleanup_performance_support/schedule.rs"]
mod schedule;

#[test]
fn d6_terminal_failure_cancels_second_member_and_retains_first() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("failed-series");
    let mut calls = Vec::new();
    let result = schedule::run(&path, true, |member| {
        calls.push((member.warmup, member.index, member.order, member.build));
        assert!(member.deadline > std::time::Instant::now());
        Err("controlled guard failure".into())
    });
    assert_eq!(result.unwrap_err(), "controlled guard failure");
    assert_eq!(calls, [(true, 0, 0, schedule::Build::Base)]);
    let records = std::fs::read_to_string(path.join("samples.jsonl")).unwrap();
    assert_eq!(records.lines().count(), 1);
    assert!(records.contains("controlled guard failure"));
    assert!(schedule::run(&path, true, |_| unreachable!()).is_err());
}

#[test]
fn d6_schedule_retains_three_warmups_and_thirty_alternating_pairs() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("series");
    let mut measured = 0;
    let mut warmups = 0;
    schedule::run(&path, true, |member| {
        if member.warmup {
            warmups += 1;
        } else {
            measured += 1;
        }
        let expected = if (member.index + member.order) % 2 == 0 {
            schedule::Build::Base
        } else {
            schedule::Build::Head
        };
        assert_eq!(member.build, expected);
        Ok(serde_json::json!({"retained": true}))
    })
    .unwrap();
    assert_eq!((warmups, measured), (6, 60));
    assert_eq!(
        std::fs::read_to_string(path.join("samples.jsonl"))
            .unwrap()
            .lines()
            .count(),
        66
    );
}

#[path = "cleanup_performance_support/events.rs"]
mod events;

#[test]
fn d6_events_reject_reused_identity_and_missing_root_exit() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let row = json!({"schema_version": 1, "manifest_digest": "digest", "run_nonce": "nonce",
        "pid": 12, "birth": [1, 2], "sequence": 0, "boot_nanos": 10, "event": "acquisition_entry"});
    let path = dir.path().join("12.jsonl");
    std::fs::write(&path, format!("{row}\n")).unwrap();
    let e = events::Events::read(dir.path(), "digest", "nonce").unwrap();
    assert!(e.intervals(1, 100).is_err());
    assert!(events::Events::read(dir.path(), "different", "nonce").is_err());
    let mut changed = row.clone();
    changed["birth"] = json!([3, 4]);
    changed["sequence"] = json!(1);
    std::fs::write(&path, format!("{row}\n{changed}\n")).unwrap();
    assert!(events::Events::read(dir.path(), "digest", "nonce").is_err());
}

#[cfg(target_os = "macos")]
#[path = "cleanup_performance_support/invocation.rs"]
mod invocation;
#[cfg(target_os = "macos")]
#[path = "../../../experiments/fixtures/umbra-fix-299b-performance/observation.rs"]
#[allow(dead_code)] // The external parent uses clock/identity; the provider uses record.
mod observation;
#[cfg(target_os = "macos")]
#[path = "cleanup_performance_support/workload.rs"]
mod workload;

#[cfg(target_os = "macos")]
#[path = "cleanup_performance_support/report.rs"]
mod report;

#[cfg(target_os = "macos")]
fn configured() -> bool {
    if std::env::var_os("UMBRA_F299_PERF_BASE_MANIFEST").is_none() {
        assert!(
            std::env::var_os("UMBRA_F299_PERF_WORKLOAD").is_none(),
            "missing performance base manifest"
        );
        eprintln!("SKIP: performance measurement requires frozen artifact configuration");
        return false;
    }
    true
}

#[cfg(target_os = "macos")]
fn qualify(paired: bool, sid1: bool) {
    if !configured() {
        return;
    }
    let c = config::read(|key| std::env::var(key).ok(), paired).unwrap();
    assert_eq!(
        c.session == config::Session::Sid1,
        sid1,
        "wrong session entrypoint"
    );
    if sid1 {
        assert_eq!(unsafe { libc::getsid(0) }, 1, "SID-1 prerequisite: CI only");
    }
    let base = manifest::load(&c.base, true).unwrap();
    let head = c
        .head
        .as_ref()
        .map(|path| manifest::load(path, true).unwrap());
    schedule::run(&c.evidence, paired, |member| {
        let selected = match member.build {
            schedule::Build::Base => &base,
            schedule::Build::Head => head.as_ref().unwrap(),
        };
        let prefix = format!(
            "{}-{}-{}",
            if member.warmup { "warmup" } else { "sample" },
            member.index,
            member.order
        );
        let setup = c.evidence.join(format!("{prefix}-setup"));
        let args = workload::prepare(&c, selected, &setup, member.build)?;
        invocation::measure(
            selected,
            &c.evidence.join(prefix),
            &args,
            workload::name(&c.workload),
            !sid1,
            member,
        )
    })
    .unwrap();
    report::summarize(&c.evidence, paired).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn baseline() {
    qualify(false, false);
}
#[cfg(target_os = "macos")]
#[test]
fn paired() {
    qualify(true, false);
}
#[cfg(target_os = "macos")]
#[test]
fn baseline_sid1() {
    qualify(false, true);
}
#[cfg(target_os = "macos")]
#[test]
fn paired_sid1() {
    qualify(true, true);
}

#[cfg(target_os = "macos")]
#[path = "cleanup_performance_support/child.rs"]
mod child;

#[cfg(target_os = "macos")]
#[path = "cleanup_performance_support/admission_owner.rs"]
mod admission_owner;

#[cfg(target_os = "macos")]
#[test]
fn r2_performance_admission_owner() {
    admission_owner::serve();
}

#[cfg(target_os = "macos")]
#[test]
fn r10_admission_owner_ready_exchange_budget() {
    use std::time::{Duration, Instant};
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        return;
    }
    let mut samples = Vec::new();
    for _ in 0..30 {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (admission, mut owner, cutoff) = admission_owner::start(deadline);
        let began = Instant::now();
        let mut grant = admission.reserve(cutoff).unwrap();
        grant.enter(cutoff).unwrap();
        let before_child = began.elapsed();
        let mut child = child::OwnedChild::new(
            std::process::Command::new("/usr/bin/true").spawn().unwrap(),
            deadline,
        );
        assert!(child.collect_until(deadline).unwrap().unwrap().success());
        let began = Instant::now();
        grant.collected(cutoff).unwrap();
        samples.push(before_child + began.elapsed());
        assert!(owner.collect_until(deadline).unwrap().unwrap().success());
    }
    eprintln!("admission owner exchange samples: {samples:?}");
    samples.sort();
    assert!(
        samples[15] <= Duration::from_millis(1),
        "median {:?}",
        samples[15]
    );
    assert!(
        samples[28] <= Duration::from_millis(5),
        "p95 {:?}",
        samples[28]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn r2_performance_admission_driver() {
    if std::env::var_os("UMBRA_F299_PERF_EVENTS").is_none() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let (admission, mut owner, cutoff) = admission_owner::start(deadline);
    observation::record("acquisition_entry", None);
    let mut grant = admission.reserve(cutoff).unwrap();
    grant.enter(cutoff).unwrap();
    let mut child = child::OwnedChild::new(
        std::process::Command::new("/usr/bin/true").spawn().unwrap(),
        deadline + std::time::Duration::from_secs(1),
    );
    observation::record("process_handle_returned", Some(child.id() as u64));
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let status = child
                .collect_until(deadline + std::time::Duration::from_secs(1))
                .unwrap();
            panic!("ordinary-child workload exceeded test limit; parent collection {status:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    observation::record("first_root_exit_observed", Some(child.id() as u64));
    observation::record("program_parent_collected", Some(child.id() as u64));
    assert!(status.success(), "ordinary child: {status}");
    grant.collected(cutoff).unwrap();
    let owner_status = owner
        .collect_until(deadline)
        .unwrap()
        .expect("owner collection");
    assert!(owner_status.success(), "admission owner: {owner_status}");
    observation::record(
        "admission_owner_parent_collected",
        Some(u64::from(owner.id())),
    );
}

#[path = "cleanup_performance_support/tree.rs"]
mod tree;

#[test]
fn d6_tree_receipts_require_every_actual_parent_wait() {
    use serde_json::json;
    let mut records = vec![json!({"pid": 100, "birth": [1, 0], "collected": [
        {"pid":101,"waited":101,"status":0}, {"pid":102,"waited":102,"status":0},
        {"pid":103,"waited":103,"status":0}, {"pid":104,"waited":104,"status":0}
    ]})];
    records.extend((101..105).map(|pid| json!({"pid":pid,"parent":100,"birth":[1,pid]})));
    tree::validate(&records).unwrap();
    records[0]["collected"][2]["waited"] = json!(-1);
    assert!(tree::validate(&records).is_err());
}

/// One diagnostic invocation, not a D6 series or a successful-product assertion.
#[cfg(target_os = "macos")]
#[test]
fn startup_stage_reading() {
    if !configured() {
        return;
    }
    let c = config::read(|key| std::env::var(key).ok(), true).unwrap();
    assert!(matches!(
        c.workload,
        config::Workload::NativeTrue | config::Workload::RegistryTrue
    ));
    assert_eq!(c.session, config::Session::OwnSession);
    let selected =
        manifest::load(c.head.as_deref().expect("head manifest required"), false).unwrap();
    std::fs::create_dir(&c.evidence).unwrap();
    let args = workload::prepare(
        &c,
        &selected,
        &c.evidence.join("setup"),
        schedule::Build::Head,
    )
    .unwrap();
    let result = invocation::measure(
        &selected,
        &c.evidence.join("invocation"),
        &args,
        workload::name(&c.workload),
        true,
        schedule::Member {
            warmup: false,
            index: 0,
            order: 0,
            build: schedule::Build::Head,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(900),
        },
    );
    eprintln!("startup stages (not D6 qualification): {result:?}");
    assert!(result.is_ok());
}

/// One diagnostic invocation, not a D6 series or a successful-product assertion.
#[cfg(target_os = "macos")]
#[test]
fn r11_head_tree_reproduction() {
    if !configured() {
        return;
    }
    let c = config::read(|key| std::env::var(key).ok(), true).unwrap();
    assert!(matches!(c.workload, config::Workload::RegistryTree5));
    assert_eq!(c.session, config::Session::OwnSession);
    let selected =
        manifest::load(c.head.as_deref().expect("head manifest required"), false).unwrap();
    std::fs::create_dir(&c.evidence).unwrap();
    let args = workload::prepare(
        &c,
        &selected,
        &c.evidence.join("setup"),
        schedule::Build::Head,
    )
    .unwrap();
    let result = invocation::measure(
        &selected,
        &c.evidence.join("invocation"),
        &args,
        workload::name(&c.workload),
        true,
        schedule::Member {
            warmup: false,
            index: 0,
            order: 0,
            build: schedule::Build::Head,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(900),
        },
    );
    eprintln!("head tree reproduction (not D6 qualification): {result:?}");
    assert!(result.is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn r11_startup_descriptor_inventory() {
    use std::os::fd::AsRawFd;
    let before = observation::own_descriptors().unwrap();
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let during = observation::own_descriptors().unwrap();
    assert_eq!(during.len(), before.len() + 2);
    assert!(during.iter().any(|entry| entry.0 == left.as_raw_fd()));
    assert!(during.iter().any(|entry| entry.0 == right.as_raw_fd()));
    drop((left, right));
    assert_eq!(observation::own_descriptors().unwrap(), before);
}

/// One diagnostic invocation, not a D6 series or a successful-product assertion.
#[cfg(target_os = "macos")]
#[test]
fn baseline_tree_diagnosis() {
    if !configured() {
        return;
    }
    let c = config::read(|key| std::env::var(key).ok(), false).unwrap();
    assert!(matches!(
        c.workload,
        config::Workload::NativeTree5 | config::Workload::RegistryTree5
    ));
    assert_eq!(c.session, config::Session::OwnSession);
    let selected = manifest::load(&c.base, true).unwrap();
    std::fs::create_dir(&c.evidence).unwrap();
    let args = workload::prepare(
        &c,
        &selected,
        &c.evidence.join("setup"),
        schedule::Build::Base,
    )
    .unwrap();
    let result = invocation::measure(
        &selected,
        &c.evidence.join("invocation"),
        &args,
        workload::name(&c.workload),
        true,
        schedule::Member {
            warmup: false,
            index: 0,
            order: 0,
            build: schedule::Build::Base,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(900),
        },
    );
    eprintln!("DIAGNOSIS (not qualification): {result:?}");
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(c.evidence.join("invocation/parent-receipt.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["cancelled"], false);
    assert_eq!(receipt["forced"], false);
    eprintln!("actual CLI parent receipt: {receipt}");
}

#[cfg(target_os = "macos")]
#[test]
fn r2_tree_native_control_and_recorded_registry_receipts() {
    if !configured() {
        return;
    }
    use std::process::{Command, Stdio};
    let manifest_path = std::env::var("UMBRA_F299_PERF_BASE_MANIFEST").unwrap();
    let frozen = manifest::load(std::path::Path::new(&manifest_path), true).unwrap();
    let output = std::env::var("UMBRA_F299_PARENT_EVIDENCE").unwrap();
    let directory = std::path::Path::new(&output);
    std::fs::create_dir(directory).unwrap();
    let mut child = child::OwnedChild::new(
        Command::new(&frozen.artifacts["tree5"])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(directory.join("native.jsonl")).unwrap())
            .stderr(std::fs::File::create(directory.join("native.stderr")).unwrap())
            .spawn()
            .unwrap(),
        std::time::Instant::now() + std::time::Duration::from_secs(65),
    );
    let status = child
        .collect_until(std::time::Instant::now() + std::time::Duration::from_secs(64))
        .unwrap()
        .expect("native fixture collection");
    assert!(status.success());
    let parse = |text: &str| {
        let mut rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        rows.sort_by_key(|row| row["root"] != true);
        rows
    };
    let native = parse(&std::fs::read_to_string(directory.join("native.jsonl")).unwrap());
    tree::validate(&native).unwrap();
    assert!(native[1..]
        .iter()
        .all(|row| row["parent"] == native[0]["pid"]));
    let retained = std::env::var("UMBRA_F299_PARENT_REPLAY").unwrap();
    let routed = parse(&std::fs::read_to_string(retained).unwrap());
    assert!(routed[1..]
        .iter()
        .any(|row| row["parent"] != routed[0]["pid"]));
    tree::validate(&routed).unwrap();
    std::fs::write(directory.join("replay-result.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "native_direct_edges": true, "old_registry_parent_check": false, "registry_wait_receipts": true
    })).unwrap()).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn r2_parent_edge_middle() {
    let Ok(path) = std::env::var("UMBRA_F299_GRANDCHILD_RECORD") else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut child = child::OwnedChild::new(
        std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap(),
        deadline,
    );
    let staged = std::path::Path::new(&path).with_extension("pending");
    std::fs::write(&staged, child.id().to_string()).unwrap();
    std::fs::rename(staged, path).unwrap();
    assert!(child.collect_until(deadline).unwrap().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn r2_parent_edge_rejects_actual_nonchild() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grandchild");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut middle = child::OwnedChild::new(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["r2_parent_edge_middle", "--exact", "--nocapture"])
            .env("UMBRA_F299_GRANDCHILD_RECORD", &path)
            .spawn()
            .unwrap(),
        deadline,
    );
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let grandchild: i32 = std::fs::read_to_string(path).unwrap().parse().unwrap();
    let mut status = 0;
    let waited = unsafe { libc::waitpid(grandchild, &mut status, libc::WNOHANG) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(waited, -1);
    assert_eq!(errno, Some(libc::ECHILD));
    let root = std::process::id();
    let mut receipts = Vec::new();
    let mut records = vec![json!({"pid":root,"birth":[1,0]})];
    for _ in 0..3 {
        let mut direct = child::OwnedChild::new(
            std::process::Command::new("/usr/bin/true").spawn().unwrap(),
            deadline,
        );
        assert!(direct.collect_until(deadline).unwrap().unwrap().success());
        records.push(json!({"pid":direct.id(),"parent":root,"birth":[1,0]}));
        receipts.push(json!({"pid":direct.id(),"waited":direct.id(),"status":0}));
    }
    records.push(json!({"pid":grandchild,"parent":middle.id(),"birth":[1,0]}));
    receipts.push(json!({"pid":grandchild,"waited":waited,"status":status}));
    records[0]["collected"] = json!(receipts);
    assert_eq!(
        tree::validate(&records).unwrap_err(),
        "invalid parent wait receipt"
    );
    assert!(middle.collect_until(deadline).unwrap().unwrap().success());
    eprintln!(
        "NONCHILD DISCRIMINATION: waitpid={waited}, errno={errno:?}; corrected oracle rejected"
    );
}
