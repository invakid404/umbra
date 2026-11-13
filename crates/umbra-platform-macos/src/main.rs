//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run() -> umbra_core::Result<()> {
    use umbra_platform::TraceControl;
    let owner = umbra_platform_macos::OwnerIdentity::parent()?;
    let (connection, mut backend) = umbra_core::provider::accept("macos", "platform", |options| {
        let options = if options.is_empty() {
            umbra_platform_macos::Options::default()
        } else {
            serde_json::from_slice(options)
                .map_err(|e| umbra_core::provider::protocol_error(e.to_string()))?
        };
        let backend = umbra_platform_macos::MacosTraceBackend::new(options);
        let caps = backend.capabilities().capabilities;
        Ok((backend, caps))
    })?;
    backend.observe_owner(owner, connection.observation_stream()?)?;
    umbra_platform::provider::serve_session(
        connection,
        umbra_platform::PlatformSession {
            control: Box::new(backend),
            abi: Box::new(umbra_platform_macos::DarwinArm64Abi),
        },
    )
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
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
