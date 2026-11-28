//! Experimental Darwin arm64 tracer; see README for qualification limits.
pub mod abi;
#[cfg(any(test, all(target_os = "macos", target_arch = "aarch64")))]
mod spawn;
pub use abi::DarwinArm64Abi;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod cache;
#[cfg(all(target_os = "macos", feature = "test-native-cleanup-order"))]
mod cleanup_order_probe;
#[cfg(all(
    feature = "managed-endpoint-probe",
    target_os = "macos",
    target_arch = "aarch64"
))]
pub mod endpoint_probe;
/// Publishing the embedded userspace-routing interposer, and its launch contract.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod interpose;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod ownership;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod retirement;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod rsp;
#[cfg(all(
    feature = "managed-cancel-probe",
    target_os = "macos",
    target_arch = "aarch64"
))]
mod setup_probe;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod signals;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use retirement::FinishOutcome;
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
pub use owner::{keep_lifetime_with_owner, OwnerIdentity};
#[cfg(all(
    feature = "managed-watch-probe",
    target_os = "macos",
    target_arch = "aarch64"
))]
#[doc(hidden)]
pub use owner::{probe_owner_watch, probe_watch_registration_error, WatchCheckpoint};

/// Whether this host's packed-spawn ABI is the qualified profile: exactly the
/// predicate `capture_spawn` refuses on (`spawn_host::qualify`), read-only.
/// A seam for the userspace suite's profile-conditional cases (#205, #247);
/// the backend never calls it.
#[doc(hidden)]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn spawn_profile_qualified() -> bool {
    spawn_host::qualify().is_ok()
}

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

#[cfg(all(
    feature = "shutdown-witness",
    target_os = "macos",
    target_arch = "aarch64"
))]
#[path = "../../../experiments/fixtures/umbra-fix-299b-performance/observation.rs"]
mod performance_observation;
