//! Successful invocation status alone is not a managed collection receipt.
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn validate(rows: &[Value], caller: u32, ordinary: bool) -> Result<Value, String> {
    let mut guards = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|r| r["event"] == "managed_guard_completed")
    {
        let details = &row["details"];
        let participant = &details["participant"];
        if participant["pid"] != row["pid"]
            || participant["birth"] != row["birth"]
            || row["resource"] != row["pid"]
        {
            return Err("guard producer identity differs from participant".into());
        }
        let role = participant["role"].as_str().ok_or("missing guard role")?;
        if guards.insert(role, details).is_some() {
            return Err("duplicate guard receipt".into());
        }
    }
    if ordinary {
        if !guards.is_empty() {
            return Err("unexpected managed guard in ordinary-child workload".into());
        }
        let resource = |event: &str| -> Result<u64, String> {
            let matching: Vec<_> = rows.iter().filter(|row| row["event"] == event).collect();
            if matching.len() != 1 {
                return Err(format!("expected one {event} receipt"));
            }
            matching[0]["resource"]
                .as_u64()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| format!("missing {event} identity"))
        };
        let program = resource("program_parent_collected")?;
        let owner = resource("admission_owner_parent_collected")?;
        if program == owner
            || program != resource("first_root_exit_observed")?
            || owner != resource("helpers_ready_go")?
        {
            return Err("ordinary-child parent receipts do not match acquisitions".into());
        }
        return Ok(json!({
            "kind": "ordinary-child-parent-receipts",
            "guard_failures": 0, "protocol_failures": 0, "deadline_failures": 0,
        }));
    }
    if guards.len() != 2 || !guards.contains_key("Relay") || !guards.contains_key("Guardian") {
        return Err("missing managed CLI guard receipts".into());
    }
    if guards["Relay"]["participant"]["pid"] != caller {
        return Err("relay is not the collected invocation root".into());
    }
    let mut failed = 0u64;
    let mut errors = 0u64;
    let mut deadlines = 0u64;
    for (role, details) in &guards {
        failed += u64::from(details["failed"].as_bool().ok_or("missing guard result")?);
        errors += details["errors"].as_u64().ok_or("missing guard errors")?;
        deadlines += u64::from(
            details["expired"]
                .as_bool()
                .ok_or("missing deadline result")?,
        );
        deadlines += details["opportunities_expired"]
            .as_u64()
            .ok_or("missing opportunity result")?;
        if details["unresolved"].as_u64() != Some(0) {
            return Err("unresolved guard child custody".into());
        }
        let expected = if *role == "Relay" {
            vec!["Guardian"]
        } else {
            vec!["CleanupOwner", "Worker"]
        };
        let receipts = details["collected"]
            .as_array()
            .ok_or("missing actual waits")?;
        let mut actual = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for receipt in receipts {
            let child = &receipt["participant"];
            let child_role = child["role"].as_str().ok_or("missing collected role")?;
            if receipt["status"].as_i64() != Some(0)
                || child["pid"].as_u64().is_none_or(|pid| pid == 0)
                || !identities.insert(child["pid"].as_u64().unwrap())
            {
                return Err("unsuccessful or duplicate actual wait".into());
            }
            if child_role == "Guardian" && child != &guards["Guardian"]["participant"] {
                return Err("collected guardian differs from its receipt producer".into());
            }
            actual.push(child_role);
        }
        actual.sort();
        if actual != expected {
            return Err("actual waits differ from the managed child tree".into());
        }
    }
    if failed != 0 || errors != 0 || deadlines != 0 {
        return Err("managed guard, protocol or deadline failure".into());
    }
    Ok(json!({
        "kind": "managed-parent-receipts", "guards_verified": guards.len(),
        "guard_failures": failed, "protocol_failures": errors, "deadline_failures": deadlines,
    }))
}

#[test]
fn d6_guard_receipts_require_actual_collection_and_success() {
    let participant = |role, pid| json!({"role": role, "pid": pid, "birth": [1, pid]});
    let guardian = participant("Guardian", 11);
    let make = |p: Value, children: Vec<Value>| {
        json!({
            "event": "managed_guard_completed", "pid": p["pid"], "birth": p["birth"],
            "resource": p["pid"], "details": {
                "participant": p, "failed": false, "expired": false,
                "unresolved": 0, "errors": 0, "opportunities_expired": 0,
                "collected": children.into_iter().map(|p| json!({"participant":p,"status":0})).collect::<Vec<_>>(),
            }
        })
    };
    let valid = vec![
        make(participant("Relay", 10), vec![guardian.clone()]),
        make(
            guardian,
            vec![participant("Worker", 12), participant("CleanupOwner", 13)],
        ),
    ];
    assert!(validate(&valid, 10, false).is_ok());
    assert!(validate(&valid[..1], 10, false).is_err());
    assert!(validate(&valid, 99, false).is_err());
    for field in ["unresolved", "errors", "opportunities_expired"] {
        let mut changed = valid.clone();
        changed[1]["details"][field] = json!(1);
        assert!(validate(&changed, 10, false).is_err());
    }
    for field in ["failed", "expired"] {
        let mut changed = valid.clone();
        changed[1]["details"][field] = json!(true);
        assert!(validate(&changed, 10, false).is_err());
    }
    let mut changed = valid.clone();
    changed[0]["details"]["collected"][0]["participant"]["birth"] = json!([2, 11]);
    assert!(validate(&changed, 10, false).is_err());
    let mut changed = valid;
    changed[1]["details"]["collected"][0]["status"] = json!(256);
    assert!(validate(&changed, 10, false).is_err());
}
