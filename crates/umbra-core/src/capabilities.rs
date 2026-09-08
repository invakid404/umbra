//! Open capability names negotiated through the provider handshake.
//!
//! These are configuration strings shared so that every crate spells the same
//! requirement the same way. They are deliberately *not* a backend enum: a
//! consumer requires a capability name, the registry descriptor declares it, and
//! `umbra_core::provider` fails the handshake unless the provider advertises it.
//! Nothing here maps a name to an implementation, and a declared name is a claim
//! to be qualified by measurement, never evidence on its own.

/// Storage has validated an existing, exact NFSv4 mount and negotiated v4.
/// Advertise only after that validation succeeds, never from configuration alone.
pub const STORAGE_MOUNTED_NFSV4_V1: &str = "mounted-nfsv4-v1";

/// Storage is an explicitly selected local development store with no remote
/// durability claim. Selecting it always requires `--local-dev` on the CLI.
pub const STORAGE_LOCAL_DEVELOPMENT_V1: &str = "local-development-v1";

/// Storage issues runtime rewrite targets usable as kernel `open`/`openat`
/// operands for the narrow experimental open-redirection surface.
pub const STORAGE_OPEN_REWRITE_V1: &str = "experimental-open-rewrite-v1";

/// Platform applies a supplied [`crate::SandboxProfile`] and returns only after
/// the target image is stopped before its first instruction with that policy
/// already in force.
pub const PLATFORM_SANDBOXED_LAUNCH_V1: &str = "sandboxed-stopped-launch-v1";

/// Platform can prepare a physical path rewrite for a stopped thread, including
/// scratch memory and prepared open flags, through the platform contract rather
/// than a backend-private method.
pub const PLATFORM_SYSCALL_REWRITE_V1: &str = "experimental-syscall-rewrite-v1";
