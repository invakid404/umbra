//! Generated libnfs raw-RPC ABI, plus the two `poll` declarations the event
//! pump needs.
//!
//! # Safety contract for this module
//!
//! This is the only module in the crate that enables `unsafe`. Everything it
//! exposes is a raw C declaration; every invariant below is upheld by
//! [`super::pump`] and [`super::args`], not here.
//!
//! 1. **The binding is allowlisted.** `build.rs` generates only the public raw
//!    RPC/XDR and task primitives, and fails the build if a managed-lifecycle
//!    symbol (`nfs_context`, any `nfs_*` entry point) reaches this file. The
//!    managed API is therefore not merely unused, it is absent.
//! 2. **C never receives a Rust pointer.** The only `private_data` handed to
//!    libnfs is a `u64` call id cast to `*mut c_void`. A completion for a call
//!    that has been retired resolves to no entry in the global registry and is
//!    discarded, so a late or duplicated callback cannot dereference anything.
//! 3. **Argument memory is heap-owned for the whole dispatch.** Every
//!    `*_val` pointer inside a `COMPOUND4args` points into an allocation owned
//!    by a [`super::args::CallArena`] that outlives the call. The arena is
//!    dropped only after the call completes or is proven withdrawn.
//! 4. **A PDU pointer is used at most once.** `rpc_cancel_pdu` dereferences the
//!    pointer it is given *before* checking whether libnfs still owns it, so a
//!    stale pointer is undefined behaviour. [`super::pump::Dispatch`] makes the
//!    pointer unreachable once a completion has been observed.
//! 5. **One pump per context.** `rpc_service` is driven only from
//!    `LibnfsRawTransport::submit`, `cancel` and `reconnect`, each of which
//!    takes `&mut self`.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
// Generated code keeps libnfs's own spelling; renaming it would break the ABI
// mapping a reader checks against the C headers.
#![allow(clippy::upper_case_acronyms)]

use core::ffi::{c_int, c_short};

include!(concat!(env!("OUT_DIR"), "/libnfs_raw.rs"));

/// `nfds_t` is `unsigned int` on the BSDs and `unsigned long` on Linux.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub type nfds_t = core::ffi::c_uint;
/// `nfds_t` is `unsigned long` on Linux.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
pub type nfds_t = core::ffi::c_ulong;

/// `POLLIN`: readable. Same value on every Unix this crate targets.
pub const POLLIN: c_short = 0x0001;
/// `POLLOUT`: writable.
pub const POLLOUT: c_short = 0x0004;

/// `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct pollfd {
    /// File descriptor to watch.
    pub fd: c_int,
    /// Requested events.
    pub events: c_short,
    /// Returned events.
    pub revents: c_short,
}

extern "C" {
    /// `poll(2)`. The pump passes exactly one descriptor and a bounded timeout,
    /// so there is no unbounded wait anywhere in the transport.
    pub fn poll(fds: *mut pollfd, nfds: nfds_t, timeout: c_int) -> c_int;
}

/// NFSv4 `NFS4_OK`.
pub const NFS4_OK: nfsstat4 = 0;
