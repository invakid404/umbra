//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(unix)]
fn run() -> umbra_core::Result<()> {
    umbra_storage::provider::serve_provider("nfs", |options| {
        umbra_storage_nfs::NfsStorage::connect(umbra_storage_nfs::NfsStorageConfig::from_options(
            options,
        )?)
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
