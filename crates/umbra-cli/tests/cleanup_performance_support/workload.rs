//! Disposable workload configuration, using only manifest-selected executables.
use super::{
    config::{Config, Workload},
    manifest::Manifest,
    schedule::Build,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn name(workload: &Workload) -> &'static str {
    match workload {
        Workload::NativeTrue => "native_true",
        Workload::RegistryTrue => "registry_true",
        Workload::NativeTree5 => "native_tree5",
        Workload::RegistryTree5 => "registry_tree5",
        Workload::AdmissionOnly => "admission_only",
    }
}

pub fn prepare(
    config: &Config,
    manifest: &Manifest,
    directory: &Path,
    build: Build,
) -> Result<Vec<OsString>, String> {
    std::fs::create_dir(directory).map_err(|e| e.to_string())?;
    if config.workload == Workload::AdmissionOnly {
        if !manifest.artifacts.contains_key("admission_driver") {
            return Err("missing frozen admission driver".into());
        }
        return Ok(vec![
            "r2_performance_admission_driver".into(),
            "--exact".into(),
            "--nocapture".into(),
        ]);
    }
    let workspace = directory.join("workspace");
    std::fs::create_dir(&workspace).map_err(|e| e.to_string())?;
    let store = directory.join("store");
    std::fs::create_dir(&store).map_err(|e| e.to_string())?;
    let selected_registry = match build {
        Build::Base => &config.registry,
        Build::Head => &config.head_registry,
    };
    let mut registry: Value = if let Some(path) = selected_registry {
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        let descriptor = |text: &str, role: &str| -> Result<Value, String> {
            let mut value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            value["executable"] = json!(manifest
                .artifacts
                .get(role)
                .ok_or("missing provider")?
                .as_os_str()
                .as_bytes());
            Ok(value)
        };
        json!({"timeout_ms": 5000, "providers": {
            "platform": descriptor(include_str!("../../../umbra-platform-macos/provider.json"), "platform")?,
            "storage": descriptor(include_str!("../../../umbra-storage-local/provider.json"), "storage")?,
            "journal": descriptor(include_str!("../../../umbra-journal-file/provider.json"), "journal")?,
        }})
    };
    for (role, descriptor) in registry["providers"]
        .as_object_mut()
        .ok_or("missing registry providers")?
    {
        let selected = manifest
            .artifacts
            .get(role)
            .ok_or("registry role absent from manifest")?;
        let bytes: Vec<u8> =
            serde_json::from_value(descriptor["executable"].clone()).map_err(|e| e.to_string())?;
        if bytes != selected.as_os_str().as_bytes() {
            return Err(format!(
                "registry executable outside selected manifest: {role}"
            ));
        }
        if descriptor["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|c| c == "mounted-nfsv4-v1"))
        {
            return Err("mounted storage is forbidden for this measurement".into());
        }
    }
    if config.registry.is_none() {
        registry["providers"]["platform"]["capabilities"] = json!([
            "sandboxed-stopped-launch-v1",
            "experimental-syscall-rewrite-v1",
            "bounded-event-poll-v1",
            "platform.shutdown-ledger.v1"
        ]);
        registry["providers"]["storage"]["capabilities"] =
            json!(["local-development-v1", "experimental-open-rewrite-v1"]);
        registry["providers"]["storage"]["options"] =
            json!(serde_json::to_vec(store.as_os_str().as_bytes()).unwrap());
    }
    let registry_path = directory.join("registry.json");
    std::fs::write(&registry_path, serde_json::to_vec(&registry).unwrap())
        .map_err(|e| e.to_string())?;
    let program = match config.workload {
        Workload::NativeTrue | Workload::RegistryTrue => Path::new("/usr/bin/true"),
        Workload::NativeTree5 | Workload::RegistryTree5 => manifest
            .artifacts
            .get("tree5")
            .ok_or("missing tree5 fixture")?
            .as_path(),
        Workload::AdmissionOnly => unreachable!("admission handled before CLI setup"),
    };
    let mut args: Vec<OsString> = vec![
        "run".into(),
        "--registry".into(),
        registry_path.into(),
        "--workspace".into(),
        workspace.clone().into(),
        "--state-dir".into(),
        directory.join("state").into(),
        "--experimental".into(),
    ];
    if config.registry.is_none() {
        args.push("--local-dev".into());
    }
    args.push("--".into());
    args.push(program.into());
    Ok(args)
}
