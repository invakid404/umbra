//! Validate all five identities and four actual parent waits.
use serde_json::Value;
use std::collections::BTreeSet;

pub fn validate(records: &[Value]) -> Result<(), String> {
    if records.len() != 5 {
        return Err("tree workload needs five identity records".into());
    }
    let root = records[0]["pid"].as_u64().ok_or("missing root PID")?;
    let mut identities = BTreeSet::new();
    for record in records {
        let pid = record["pid"].as_u64().ok_or("missing PID")?;
        if !identities.insert(pid) {
            return Err("duplicate program identity".into());
        }
        let birth = record["birth"].as_array().ok_or("missing birth identity")?;
        if birth.len() != 2 || birth.iter().any(|v| v.as_u64().is_none()) {
            return Err("invalid birth identity".into());
        }
    }
    let collected = records[0]["collected"]
        .as_array()
        .ok_or("missing collection receipts")?;
    if collected.len() != 4 {
        return Err("missing child collection".into());
    }
    let mut waits = BTreeSet::new();
    for receipt in collected {
        let pid = receipt["pid"].as_u64().ok_or("missing collected PID")?;
        if pid == root
            || !identities.contains(&pid)
            || !waits.insert(pid)
            || receipt["waited"].as_u64() != Some(pid)
            || receipt["status"].as_i64() != Some(0)
        {
            return Err("invalid parent wait receipt".into());
        }
    }
    // getppid() is retained as an observation, not collection authority:
    // debugger attachment can change the observed parent. Only the root's
    // actual waitpid return proves its ability to collect each recorded PID.
    Ok(())
}
