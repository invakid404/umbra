//! Experimental Darwin arm64 tracer; see README for qualification limits.
pub mod abi;
pub use abi::DarwinArm64Abi;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod cache;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod rsp;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use native::MacosTraceBackend;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub debugserver: Option<std::path::PathBuf>,
    pub twin_cache: Option<std::path::PathBuf>,
    pub timeout_ms: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            debugserver: None,
            twin_cache: None,
            timeout_ms: 30_000,
        }
    }
}
