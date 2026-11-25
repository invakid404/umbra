//! Validate producer identity and event order before deriving intervals.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub struct Events {
    pub rows: Vec<Value>,
}
impl Events {
    pub fn read(directory: &Path, digest: &str, nonce: &str) -> Result<Self, String> {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let mut producer = None;
            let mut last_time = 0;
            for (expected_sequence, line) in text.lines().enumerate() {
                let row: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
                if row["schema_version"] != 1
                    || row["manifest_digest"] != digest
                    || row["run_nonce"] != nonce
                {
                    return Err("event run/build identity mismatch".into());
                }
                let pid = row["pid"].as_u64().ok_or("missing producer PID")?;
                let birth = row["birth"].as_array().ok_or("missing producer birth")?;
                if birth.len() != 2 || birth.iter().any(|v| v.as_u64().is_none()) {
                    return Err("invalid producer birth".into());
                }
                let identity = (pid, birth[0].as_u64().unwrap(), birth[1].as_u64().unwrap());
                if producer.get_or_insert(identity) != &identity {
                    return Err("producer changed within file".into());
                }
                if row["sequence"].as_u64() != Some(expected_sequence as u64) {
                    return Err("event sequence gap or duplicate".into());
                }
                let timestamp = row["boot_nanos"].as_u64().ok_or("missing boot timestamp")?;
                if timestamp < last_time {
                    return Err("producer clock moved backwards".into());
                }
                last_time = timestamp;
                rows.push(row);
            }
        }
        if rows.is_empty() {
            return Err("no observation events".into());
        }
        Ok(Self { rows })
    }

    pub fn unique_time(&self, event: &str) -> Result<u64, String> {
        let matches: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row["event"] == event)
            .collect();
        if matches.len() != 1 {
            return Err(format!("expected one {event}, found {}", matches.len()));
        }
        matches[0]["boot_nanos"]
            .as_u64()
            .ok_or_else(|| "missing timestamp".into())
    }

    pub fn intervals(
        &self,
        invocation_start: u64,
        invocation_return: u64,
    ) -> Result<BTreeMap<&'static str, u64>, String> {
        let root_exit = self.unique_time("first_root_exit_observed")?;
        let acquisition_entry = self.unique_time("acquisition_entry")?;
        let handle_return = self.unique_time("process_handle_returned")?;
        if root_exit < invocation_start || handle_return > invocation_return {
            return Err("event outside invocation".into());
        }
        let difference = |end: u64, start: u64| {
            end.checked_sub(start)
                .ok_or_else(|| "reversed interval".to_string())
        };
        Ok(BTreeMap::from([
            ("teardown_ns", difference(invocation_return, root_exit)?),
            (
                "invocation_ns",
                difference(invocation_return, invocation_start)?,
            ),
            (
                "acquisition_ns",
                difference(handle_return, acquisition_entry)?,
            ),
        ]))
    }
}
