//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run() -> umbra_core::Result<()> {
    use umbra_platform::TraceControl;
    let mut managed = umbra_isolation::managed::executable::entry()
        .map_err(|e| umbra_core::provider::protocol_error(e.to_string()))?;
    if managed.as_ref().is_some_and(|entry| {
        entry.role != umbra_core::managed::Role::Provider
            || entry.parent.role != umbra_core::managed::Role::CleanupOwner
    }) {
        return Err(umbra_core::provider::protocol_error(
            "invalid managed provider role",
        ));
    }
    // #205: end with the owner, not with the terminal's signals.
    umbra_platform_macos::keep_lifetime_with_owner()?;
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
    if let Some(entry) = managed.as_mut() {
        entry
            .complete_provider_startup()
            .map_err(|e| umbra_core::provider::protocol_error(e.to_string()))?;
        backend.install_managed_admission(
            entry,
            Box::new(|| {
                umbra_isolation::managed::boot_nanos()
                    .map(|now| now.saturating_add(5_000_000_000))
                    .map_err(|e| umbra_core::provider::protocol_error(e.to_string()))
            }),
        )?;
    }
    umbra_platform::provider::serve_session(
        connection,
        umbra_platform::PlatformSession {
            control: Box::new(backend),
            abi: Box::new(umbra_platform_macos::DarwinArm64Abi),
        },
    )
}
#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
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
