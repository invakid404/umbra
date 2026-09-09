//! Userspace NFSv4.0 storage provider: frozen facade interfaces and scaffold.
//!
//! This crate carries the private facade contracts for Umbra's userspace NFS
//! backend and a [`Storage`](umbra_storage::Storage) scaffold that implements the
//! contract's shape without yet performing I/O. It exists so `raw_rpc` (transport)
//! and `raw_state` (protocol state) can be built against a fixed seam, and so
//! `authority_recovery` can build admission and replay wiring against the same
//! types with an in-memory implementation.
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
//! Unsafe code is denied crate-wide. The future libnfs binding is the only place
//! that may opt back in, and only with a documented safety contract, so the FFI
//! surface stays visible in review rather than diffused through the crate.
//!
//! # Current state
//!
//! The operations surface is wired against the frozen facades: anchoring, path
//! resolution, stat, bounded enumeration, read, exclusive and plain create, and
//! WRITE with `UNSTABLE`/COMMIT verifier accounting all run over whatever
//! [`RawTransport`](transport::RawTransport) is injected. Writer authority,
//! admission, epochs and durability receipts still answer `NotImplemented`
//! naming `authority_recovery`, and no live transport is bound here — that is
//! `m1_integrate`'s seam.
//!
//! Namespace mutation — REMOVE, RENAME, CREATE of a directory, SETATTR — is
//! typed and reachable through [`namespace::NamespaceDispatcher`] but has no
//! in-crate implementation, because the frozen [`transport::Nfs4Op`] carries no
//! argument variant for those NFSv4.0 operations. See [`capability`] for the
//! full matrix and [`namespace`] for why the seam is unbound rather than faked.

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
pub mod namespace;
pub mod ops;
pub mod pages;
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
