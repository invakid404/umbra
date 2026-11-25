//! Explicit workload selection; a missing setting is never a passing sample.
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Workload {
    NativeTrue,
    RegistryTrue,
    NativeTree5,
    RegistryTree5,
    AdmissionOnly,
}
#[derive(Debug, PartialEq)]
pub enum Session {
    OwnSession,
    Sid1,
}
#[derive(Debug)]
pub struct Config {
    pub base: PathBuf,
    pub head: Option<PathBuf>,
    pub evidence: PathBuf,
    pub registry: Option<PathBuf>,
    pub workload: Workload,
    pub session: Session,
}

pub fn read(mut get: impl FnMut(&str) -> Option<String>, paired: bool) -> Result<Config, String> {
    let mut required = |key: &str| {
        get(key)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing {key}"))
    };
    let workload = match required("UMBRA_F299_PERF_WORKLOAD")?.as_str() {
        "native_true" => Workload::NativeTrue,
        "registry_true" => Workload::RegistryTrue,
        "native_tree5" => Workload::NativeTree5,
        "registry_tree5" => Workload::RegistryTree5,
        "admission_only" => Workload::AdmissionOnly,
        _ => return Err("unknown workload".into()),
    };
    let session = match required("UMBRA_F299_PERF_SESSION")?.as_str() {
        "own_session" => Session::OwnSession,
        "sid1" => Session::Sid1,
        _ => return Err("unknown session".into()),
    };
    let mut absolute = |key| -> Result<PathBuf, String> {
        let path = PathBuf::from(required(key)?);
        if !path.is_absolute() {
            return Err(format!("{key} must be absolute"));
        }
        Ok(path)
    };
    let base = absolute("UMBRA_F299_PERF_BASE_MANIFEST")?;
    let head = if paired {
        Some(absolute("UMBRA_F299_PERF_HEAD_MANIFEST")?)
    } else {
        None
    };
    let evidence = absolute("UMBRA_F299_PERF_EVIDENCE")?;
    let registry = if matches!(workload, Workload::RegistryTrue | Workload::RegistryTree5) {
        Some(absolute("UMBRA_F299_PERF_REGISTRY")?)
    } else {
        None
    };
    if registry.is_some() && required("UMBRA_INTEGRATION_REQUIRED")? != "1" {
        return Err("registry measurement requires integration qualification".into());
    }
    if session == Session::Sid1 && required("UMBRA_REQUIRE_SID1")? != "1" {
        return Err("SID1 measurement requires the CI prerequisite".into());
    }
    Ok(Config {
        base,
        head,
        evidence,
        registry,
        workload,
        session,
    })
}
