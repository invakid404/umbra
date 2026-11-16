//! Userspace NFSv4.0 storage provider.
//!
//! Implements [`Storage`](umbra_storage::Storage) over the transport and protocol
//! state facades. With `transport-raw`, [`NfsUserspaceStorage::connect`] binds the
//! live libnfs transport; without it, the provider remains unbound.
//!
//! # Wire profile
//!
//! NFSv4.0 over TCP with AUTH_SYS, and nothing else. Umbra owns client ids,
//! open/lock owners, stateids, seqids, renewal, reconnect, grace, v4.0
//! `CLAIM_PREVIOUS` reclaim, replay buffers and verifier accounting. libnfs is
//! expected to supply public raw RPC/XDR transport and task primitives only. The
//! NFSv4.1 session model — `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`,
//! `RECLAIM_COMPLETE` — is out of scope and has no representation in
//! [`transport::OpCode`].
//!
//! # Protocol ownership is not writer authority
//!
//! Renewing an NFS lease proves the lease is alive. It never authorises taking
//! over an abandoned session. One-session-one-Umbra admission stays product-wide,
//! and [`error::AuthorityError`] is a domain of its own for exactly that reason.
//!
//! # Safe-wrapper posture
//!
//! Unsafe code is denied crate-wide; the raw transport binding opts in locally
//! for its FFI implementation.
//!
//! # Current state
//!
//! Data operations use [`RawTransport`](transport::RawTransport), and namespace
//! mutations use [`namespace::dispatch::TransportDispatcher`]. Writes settle
//! their COMMIT evidence before returning. `flush` checks the settlement ledger
//! and returns a receipt at the run's qualified durability level.
//! Durable checkpoint support remains unimplemented.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod anchor;
pub mod authority;
pub mod capability;
pub mod crud;
pub mod error;
pub mod fake;
#[cfg(test)]
mod fixture;
pub mod handle;
pub mod identity;
pub mod integration;
pub mod journal;
pub mod namespace;
pub mod ops;
pub mod pages;
pub mod pdu;
/// The per-run persistence-boundary probe. Private: its only output is the
/// boolean `open_run` reads, and nothing outside this crate may assert it.
mod probe;
pub mod replay;
pub mod session;
pub mod state;
pub mod storage;
pub mod transport;

pub use storage::{NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION, PROVIDER_ID};

/// Golden fixture layout the mounted `nfs` adapter writes, and that this provider
/// must read to open an existing run sequentially.
///
/// These names are duplicated deliberately: the operations node asserts the
/// userspace provider produces byte-identical layout, and a silent divergence in
/// either adapter should fail a test rather than be discovered on a live run.
pub mod layout {
    /// Private per-run subdirectory holding provider state.
    pub const PRIVATE_DIR: &[u8] = b".provider";
    /// Directory holding retry records, one file per idempotency key.
    pub const RETRIES_DIR: &[u8] = b"retries";
    /// Little-endian `u64` writer epoch, created as zero.
    pub const EPOCH_FILE: &[u8] = b"epoch";
    /// JSON `[run_id, immutable_base, format_version]` identifying the run.
    pub const MANIFEST_FILE: &[u8] = b"manifest";
    /// Exclusively created file holding the current writer's opaque token.
    pub const WRITER_LOCK_FILE: &[u8] = b"writer.lock";
    /// Prefix of a retry record file, followed by the hex idempotency key.
    pub const RETRY_KEY_PREFIX: &str = "key-";
    /// Prefix of an operation-id index file, followed by the hyphenated UUID.
    pub const RETRY_OP_PREFIX: &str = "op-";
    /// Mode applied to every directory the run layout creates.
    pub const DIRECTORY_MODE: u32 = 0o700;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_id_is_distinct_from_the_mounted_adapter() {
        assert_eq!(PROVIDER_ID, "nfs-userspace");
        assert_ne!(PROVIDER_ID, "nfs");
    }

    #[test]
    fn the_wire_profile_is_locked_to_v4_0() {
        assert_eq!(transport::MINOR_VERSION, 0);
        assert!(transport::WireProfile::V40_TCP_SYS.check().is_ok());
    }
}
