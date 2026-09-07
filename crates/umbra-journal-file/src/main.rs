//! Provider executable; constructs only this package's backend after the handshake.
#![forbid(unsafe_code)]
#[cfg(unix)]
fn no_options(options: &[u8]) -> umbra_core::Result<()> {
    if !options.is_empty() {
        return Err(umbra_core::provider::protocol_error(
            "provider accepts no configuration options",
        ));
    }
    Ok(())
}
#[cfg(unix)]
fn run() -> umbra_core::Result<()> {
    umbra_journal::provider::serve_provider("file", |options| {
        no_options(options)?;
        Ok(umbra_journal_file::FileJournal::new())
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
