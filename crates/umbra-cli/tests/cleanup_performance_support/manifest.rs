//! Frozen artifact validation before any measured invocation.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Manifest {
    pub digest: String,
    pub artifacts: BTreeMap<String, PathBuf>,
}

pub fn sha256(path: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/shasum")
        .args(["-a", "256", "--"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("hash failed: {}", path.display()));
    }
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let digest = text.split_whitespace().next().ok_or("missing digest")?;
    if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid digest".into());
    }
    Ok(digest.to_owned())
}

pub fn load(path: &Path, frozen: bool) -> Result<Manifest, String> {
    if !path.is_absolute() {
        return Err("manifest path must be absolute".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value["schema_version"].as_u64() != Some(1) {
        return Err("manifest schema".into());
    }
    for field in [
        "source_commit",
        "source_tree",
        "build_profile",
        "rustc_version",
        "target_triple",
        "observation_patch_sha256",
    ] {
        if value[field].as_str().is_none_or(str::is_empty) {
            return Err(format!("missing {field}"));
        }
    }
    let mut artifacts = BTreeMap::new();
    for artifact in value["artifacts"].as_array().ok_or("missing artifacts")? {
        let role = artifact["role"].as_str().ok_or("missing role")?;
        let path = PathBuf::from(
            artifact["absolute_path"]
                .as_str()
                .ok_or("missing artifact path")?,
        );
        if !path.is_absolute() {
            return Err("artifact path must be absolute".into());
        }
        let resolved = path.canonicalize().map_err(|e| e.to_string())?;
        if frozen
            && resolved
                .components()
                .any(|part| part.as_os_str() == "target")
        {
            return Err("baseline artifact is in a mutable build directory".into());
        }
        if sha256(&resolved)? != artifact["sha256"].as_str().ok_or("missing hash")? {
            return Err(format!("artifact hash mismatch: {role}"));
        }
        if artifacts.insert(role.to_owned(), resolved).is_some() {
            return Err("duplicate artifact role".into());
        }
    }
    for role in ["cli", "platform", "storage", "journal", "namespace"] {
        if !artifacts.contains_key(role) {
            return Err(format!("missing artifact role: {role}"));
        }
    }
    if value["workloads"].as_array().is_none_or(Vec::is_empty) {
        return Err("missing workload definitions".into());
    }
    Ok(Manifest {
        digest: sha256(path)?,
        artifacts,
    })
}
