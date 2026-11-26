//! Full registered library run, held in real native setup before session acceptance.
use std::{path::Path, time::Instant};

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

pub fn run() {
    let caller = std::env::var_os("UMBRA_NATIVE_MANAGED_CALLER")
        .expect("build managed_library with probe-native-setup-cancel and supply its executable");
    let evidence = tempfile::tempdir().unwrap().keep();
    eprintln!("native setup evidence: {}", evidence.display());
    let witness = evidence.join("witness.jsonl");
    let began = Instant::now();
    let status = std::process::Command::new(caller)
        .args(["r2_native_preaccepted_work_cancel", "64"])
        .env("UMBRA_NATIVE_SETUP_PROBE", &evidence)
        .env("UMBRA_FIX_271_EVIDENCE_FILE", &witness)
        .status()
        .unwrap();
    // The caller fixture checks failed durable custody and waits its complete
    // role tree before any assertion about native observations is evaluated.
    assert!(
        status.success(),
        "native cancellation caller fixture failed"
    );
    assert!(
        began.elapsed().as_secs_f64() <= 38.6,
        "native cancellation exceeded the retained 38.6-second test limit"
    );
    let setup = json(&evidence.join("setup.json"));
    let observed = json(&evidence.join("observed.json"));
    let triggered = json(&evidence.join("triggered.json")).as_u64().unwrap();
    let notification: umbra_isolation::managed::cancellation::Notification =
        serde_json::from_value(json(&evidence.join("ticket.json"))).unwrap();
    assert_eq!(setup["request"], "vCont?");
    assert_eq!(observed["accepted"], false);
    assert!(
        observed["error"]
            .as_str()
            .unwrap()
            .contains("ordinary work cancelled"),
        "native setup did not observe independent cancellation: {observed}"
    );
    let ended = observed["nanos"].as_u64().unwrap();
    assert!(setup["nanos"].as_u64().unwrap() <= triggered);
    assert!(triggered <= notification.ticket.started);
    assert!(notification.ticket.started <= ended);
    assert!(
        ended - triggered < 1_000_000_000,
        "native wakeup exceeded one second"
    );
    let cutoff = observed["cutoff"].as_u64().unwrap();
    assert!(
        ended < cutoff,
        "native cancellation waited for cutoff expiry"
    );
    assert!(cutoff <= notification.ticket.cutoffs(64).unwrap().fallback);
    let records: Vec<serde_json::Value> = std::fs::read_to_string(witness)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let final_record = records
        .iter()
        .find(|record| record["event"] == "final-to-exit")
        .expect("native provider collection witness missing");
    assert_eq!(final_record["proof"], true);
    assert_eq!(
        final_record["exit_ok"], false,
        "failed run must stay failed"
    );
    let receipt: umbra_core::managed::ProviderReceipt =
        serde_json::from_value(json(&evidence.join("collected.json")))
            .expect("native provider parent-wait receipt missing");
    assert_eq!(receipt.status, 0, "native provider did not exit cleanly");
    assert!(
        receipt.failed,
        "caller cancellation must retain failed outcome"
    );
    assert_eq!(receipt.run, notification.run);
    assert_eq!(
        receipt.provider.pid,
        setup["provider"].as_u64().unwrap() as u32
    );
    assert_eq!(receipt.provider.role, umbra_core::managed::Role::Provider);
    assert_eq!(receipt.owner.role, umbra_core::managed::Role::CleanupOwner);
    let accepted: umbra_core::shutdown::Accepted =
        serde_json::from_value(final_record["accepted"].clone()).unwrap();
    let final_state: umbra_core::shutdown::Final =
        serde_json::from_value(final_record["final"].clone()).unwrap();
    assert!(accepted.confirms(&final_state).unwrap());
    for kind in [
        umbra_core::shutdown::ResourceKind::Tracee,
        umbra_core::shutdown::ResourceKind::TaskRight,
        umbra_core::shutdown::ResourceKind::Tracer,
    ] {
        assert!(
            final_state.entries.iter().any(|entry| {
                entry.slot.kind == kind && entry.identity.is_some() && entry.confirms()
            }),
            "native setup lacked collected inventory for {kind:?}"
        );
    }
    eprintln!(
        "native preaccepted setup: wake={}ns, ticket={:?}, provider confirmed and parent waited",
        ended - triggered,
        notification.ticket
    );
}
