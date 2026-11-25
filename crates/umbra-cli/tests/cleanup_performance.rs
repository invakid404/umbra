//! Cleanup performance qualification and retained-sample analysis.
#[path = "cleanup_performance_support/analysis.rs"]
mod analysis;

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
fn qualify(paired: bool, sid1: bool) {
    if std::env::var_os("UMBRA_F299_PERF_BASE_MANIFEST").is_none() {
        assert!(
            std::env::var_os("UMBRA_F299_PERF_WORKLOAD").is_none(),
            "missing performance base manifest"
        );
        eprintln!("SKIP: performance measurement requires frozen artifact configuration");
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
        let args = workload::prepare(&c, selected, &setup)?;
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
#[test]
fn r2_performance_admission_driver() {
    if std::env::var_os("UMBRA_F299_PERF_EVENTS").is_none() {
        return;
    }
    observation::record("acquisition_entry", None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
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
