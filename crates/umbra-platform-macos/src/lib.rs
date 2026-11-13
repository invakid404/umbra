//! Experimental Darwin arm64 tracer; see README for qualification limits.
pub mod abi;
#[cfg(any(test, all(target_os = "macos", target_arch = "aarch64")))]
mod spawn;
pub use abi::DarwinArm64Abi;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod cache;
/// Publishing the embedded userspace-routing interposer, and its launch contract.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod interpose;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod rsp;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod spawn_host;
#[cfg(all(
    target_os = "macos",
    target_arch = "aarch64",
    feature = "mutation-probe-spawn-lifecycle"
))]
mod spawn_probe;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use native::{
    BarrierEvidence, BarrierReading, MacosTraceBackend, ReportedExitLookup, TeardownReport,
    TeardownStep,
};
// Compile the refusal backend in native-host unit tests as well.
#[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]
mod unsupported;
use umbra_core::{ErrorKind, UmbraError};
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub use unsupported::MacosTraceBackend;
pub(crate) fn error(operation: &str, context: impl ToString) -> UmbraError {
    UmbraError::new(ErrorKind::Io, operation, context.to_string())
}
pub(crate) fn unsupported(context: impl ToString) -> UmbraError {
    UmbraError::new(
        ErrorKind::UnsupportedCapability,
        "macos",
        context.to_string(),
    )
}
/// Runtime configuration; redirect policy belongs to the caller.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub debugserver: Option<std::path::PathBuf>,
    pub twin_cache: Option<std::path::PathBuf>,
    /// Optional absolute lifetime in awake milliseconds; disabled by default.
    pub timeout_ms: Option<u64>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod owner;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use owner::OwnerIdentity;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod clock;

#[cfg(test)]
mod option_tests {
    use super::*;
    #[test]
    fn absent_or_null_lifetime_is_unbounded_numeric_is_explicit() {
        for input in ["{}", r#"{"timeout_ms":null}"#] {
            assert_eq!(
                serde_json::from_str::<Options>(input).unwrap().timeout_ms,
                None
            );
        }
        assert_eq!(
            serde_json::from_str::<Options>(r#"{"timeout_ms":8000}"#)
                .unwrap()
                .timeout_ms,
            Some(8000)
        );
    }
}
