//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(unix)]
fn run() -> umbra_core::Result<()> {
    umbra_platform::provider::serve_provider("macos", |options| {
        let options = if options.is_empty() {
            umbra_platform_macos::Options::default()
        } else {
            serde_json::from_slice(options)
                .map_err(|e| umbra_core::provider::protocol_error(e.to_string()))?
        };
        Ok(umbra_platform::PlatformSession {
            control: Box::new(umbra_platform_macos::MacosTraceBackend::new(options)),
            abi: Box::new(umbra_platform_macos::DarwinArm64Abi),
        })
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
