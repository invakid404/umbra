//! The shipped `provider.json` must be a valid, installable storage descriptor.
//!
//! A template that fails to decode, names the wrong role, or carries options this
//! provider rejects is only discovered at install time otherwise. This runs no
//! provider process and opens no connection.

use umbra_core::provider::{ProviderDescriptor, PROTOCOL_VERSION};
use umbra_storage_nfs_userspace::{NfsUserspaceConfig, PROVIDER_ID};

fn descriptor() -> ProviderDescriptor {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/provider.json"))
        .expect("provider.json is shipped with the crate");
    serde_json::from_slice(&bytes).expect("provider.json decodes as a descriptor")
}

#[test]
fn the_template_registers_this_provider_for_the_storage_role() {
    let descriptor = descriptor();
    assert_eq!(descriptor.id, PROVIDER_ID);
    assert_eq!(descriptor.role, "storage");
    assert_eq!(descriptor.protocol_version, PROTOCOL_VERSION);
    assert!(descriptor.executable.is_absolute());
    // Stubs advertise nothing. Nothing here is qualified yet.
    assert!(descriptor.capabilities.is_empty());
    descriptor
        .validate("storage")
        .expect("the template validates for its own role");
}

#[test]
fn the_template_options_decode_into_a_valid_configuration() {
    let config = NfsUserspaceConfig::from_options(&descriptor().options)
        .expect("template options decode and validate");
    assert_eq!(config.host, b"127.0.0.1");
    assert_eq!(config.port, 2049);
    assert_eq!(config.root_anchor.as_bytes(), b"root");
    assert_eq!(config.control_anchor.as_bytes(), b"control");
    // Export and run parent are server-relative, never host paths.
    assert!(!config.export.is_absolute());
    assert!(!config.run_parent.is_absolute());
}
