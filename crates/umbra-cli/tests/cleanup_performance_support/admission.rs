//! Pair measured exchanges by producer and obligation, never by stderr order.
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn components(rows: &[Value]) -> Result<Vec<Value>, String> {
    let mut groups: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|row| row["event"] == "admission_exchange")
    {
        let details = &row["details"];
        if details["accepted"] != true {
            return Err("admission measurement contains a refusal".into());
        }
        let operation = &details["operation"];
        if operation == "Freeze" {
            continue;
        }
        let operation = operation.as_object().ok_or("invalid admission operation")?;
        if operation.len() != 1 {
            return Err("ambiguous admission operation".into());
        }
        let (phase, value) = operation.iter().next().unwrap();
        let expected = match phase.as_str() {
            "Reserve" => 0,
            "Entering" => 1,
            "Retiring" => 2,
            "Release" => 3,
            _ => return Err("unknown admission phase".into()),
        };
        let producer = format!("{}:{}", row["pid"], row["birth"]);
        let obligation = value["obligation"]
            .as_str()
            .ok_or("missing measured obligation")?
            .to_owned();
        let values = groups.entry((producer, obligation)).or_default();
        if values.len() != expected {
            return Err("missing, reordered or duplicate admission phase".into());
        }
        values.push(
            details["nanos"]
                .as_u64()
                .ok_or("missing admission duration")?,
        );
    }
    groups
        .into_values()
        .map(|values| {
            if values.len() != 4 {
                return Err("admission obligation lacks completed exchange".into());
            }
            let completed = values[2]
                .checked_add(values[3])
                .ok_or("admission completion duration overflow")?;
            Ok(json!({"reserve": values[0], "entering": values[1], "completed": completed}))
        })
        .collect()
}
