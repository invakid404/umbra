//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
fn run() -> umbra_core::Result<()> {
    umbra_storage::provider::serve_provider("nfs", |options| {
        let root: umbra_core::BytePath = umbra_core::provider::decode(options)?;
        Ok(umbra_storage_nfs::NfsStorage::new(
            umbra_storage_nfs::NfsStorageConfig::new(std::ffi::OsStr::from_bytes(root.as_bytes())),
        ))
    })
}
#[cfg(not(unix))]
fn run() -> umbra_core::Result<()> {
    Err(umbra_core::UmbraError::new(
        umbra_core::ErrorKind::UnsupportedCapability,
        "provider",
        "local IPC requires Unix",
    ))
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
