//! Acceptance from retained records; never select a convenient subset.
use super::analysis;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn distribution(values: &[f64]) -> Result<Value, String> {
    let d = analysis::distribution(values)?;
    Ok(json!({"median": d.median, "p95": d.p95, "maximum": d.maximum}))
}

pub fn summarize(directory: &Path, paired: bool) -> Result<(), String> {
    let text =
        std::fs::read_to_string(directory.join("samples.jsonl")).map_err(|e| e.to_string())?;
    let mut measured = BTreeMap::new();
    let mut warmups = BTreeMap::new();
    for line in text.lines() {
        let row: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if !row["error"].is_null() || row["pair_expired"] != false {
            return Err("terminal sample failure".into());
        }
        let index = row["index"].as_u64().ok_or("missing sample index")?;
        let build = row["build"].as_str().ok_or("missing build")?.to_owned();
        if !matches!(build.as_str(), "base" | "head") || (!paired && build == "head") {
            return Err("unexpected build".into());
        }
        let warmup = row["warmup"]
            .as_bool()
            .ok_or("missing warmup classification")?;
        if index >= if warmup { 3 } else { 30 } {
            return Err("unexpected sample index".into());
        }
        let table = if warmup { &mut warmups } else { &mut measured };
        if table
            .insert((index, build), row["sample"].clone())
            .is_some()
        {
            return Err("duplicate sample".into());
        }
    }
    let builds = if paired { 2 } else { 1 };
    if warmups.len() != 3 * builds || measured.len() != 30 * builds {
        return Err("incomplete series".into());
    }
    let mut report = json!({"paired": paired, "warmups_retained": warmups.len(), "measured_retained": measured.len(), "metrics": {}});
    let mut accepted = true;
    for metric in ["teardown_ns", "invocation_ns", "acquisition_ns"] {
        let values = |build: &str| -> Result<Vec<f64>, String> {
            (0..30)
                .map(|index| {
                    measured
                        .get(&(index, build.to_owned()))
                        .ok_or("missing pair member")?["intervals"][metric]
                        .as_u64()
                        .map(|v| v as f64)
                        .ok_or_else(|| format!("missing {metric}"))
                })
                .collect()
        };
        let base = values("base")?;
        let mut entry = json!({"base_samples": base, "base": distribution(&base)?});
        if paired {
            let head = values("head")?;
            let comparison = analysis::compare(&base, &head)?;
            entry["head_samples"] = json!(head);
            entry["head"] = distribution(&head)?;
            entry["paired_differences"] = json!(comparison.differences);
            entry["paired"] = distribution(&comparison.differences)?;
            if metric == "teardown_ns" {
                accepted &= comparison.teardown_passes();
            }
        }
        report["metrics"][metric] = entry;
    }
    if paired {
        let mut admission = Vec::new();
        let mut helpers = Vec::new();
        let mut base_helpers = Vec::new();
        let mut absent_base_helpers = 0;
        for index in 0..30 {
            let baseline = &measured[&(index, "base".to_owned())];
            if let Some(value) = baseline["helper_start_ns"].as_u64() {
                base_helpers.push(value as f64);
            } else if baseline["helper_start_ns"].is_null() {
                absent_base_helpers += 1;
            } else {
                return Err("invalid baseline helper start".into());
            }
            let sample = &measured[&(index, "head".to_owned())];
            for field in ["guard_failures", "protocol_failures", "deadline_failures"] {
                if sample[field].as_u64() != Some(0) {
                    return Err(format!("missing or failed {field}"));
                }
            }
            helpers.push(
                sample["helper_start_ns"]
                    .as_u64()
                    .ok_or("missing helper start")? as f64,
            );
            let acquisitions = sample["admission_ns"]
                .as_array()
                .ok_or("missing admission components")?;
            if acquisitions.is_empty() {
                return Err("no admission samples".into());
            }
            for components in acquisitions {
                let total =
                    ["reserve", "entering", "completed"]
                        .iter()
                        .try_fold(0u64, |sum, field| {
                            sum.checked_add(
                                components[*field]
                                    .as_u64()
                                    .ok_or("missing admission component")?,
                            )
                            .ok_or("admission total overflow")
                        })?;
                admission.push(total as f64);
            }
        }
        accepted &= analysis::admission_passes(&admission)?;
        if absent_base_helpers != 0 && absent_base_helpers != 30 {
            return Err("mixed baseline helper presence".into());
        }
        if absent_base_helpers == 30 {
            let helper = analysis::distribution(&helpers)?;
            accepted &= helper.median <= 25_000_000.0 && helper.p95 <= 50_000_000.0;
            report["helper_start"] =
                json!({"baseline": null, "head_samples": helpers, "head": distribution(&helpers)?});
        } else {
            let comparison = analysis::compare(&base_helpers, &helpers)?;
            accepted &= comparison.helper_start_passes();
            report["helper_start"] = json!({"base_samples": base_helpers, "base": distribution(&base_helpers)?,
                "head_samples": helpers, "head": distribution(&helpers)?, "paired_differences": comparison.differences});
        }
        report["admission"] = json!({"baseline": null, "combined_samples": admission, "combined": distribution(&admission)?});
    }
    report["accepted"] = if paired { json!(accepted) } else { Value::Null };
    std::fs::write(
        directory.join("summary.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    if paired && !accepted {
        return Err("D6 threshold miss; owner ruling required".into());
    }
    Ok(())
}
