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

/// Namespace owns a run's whole managed lifecycle: renewing the writer lease
/// through its own storage session, durably completing a finished run, and
/// leaving a failed one explicitly failed. Advertise only when `renew_writer`,
/// `finish_run` and `fail_run` are implemented, not merely routed.
pub const NAMESPACE_RUN_LIFECYCLE_V1: &str = "namespace-run-lifecycle-v1";

/// Storage applies [`crate::storage::MetadataUpdate::uid`]/`gid` on a
/// `SetMetadata` and reflects the result in [`crate::storage::BlobStat`].
///
/// The claim is "what the kernel permits is applied, and a refusal is reported",
/// not "every chown succeeds": a backend over a real filesystem cannot give an
/// object away to another uid without privilege it does not have, and the name
/// would be worthless if it promised otherwise. A consumer that carries
/// ownership onto a shadow object therefore requires this name to decide whether
/// to *try*, and still handles [`crate::ErrorKind::Denied`] on the attempt.
/// Advertise only after qualifying it against a live store, never from
/// configuration alone.
pub const STORAGE_OWNERSHIP_FIDELITY_V1: &str = "ownership-fidelity-v1";

/// A new object this backend creates takes an identity it inherits from its
/// prospective parent directory rather than from the store's own root, so a
/// consumer predicting the object's created identity may read it from that
/// parent. Exactly which components are inherited is per-backend and stated
/// below; it is never *less* than the parent's gid.
///
/// This is a *prediction* aid, not a promise about `SetMetadata`, and it is
/// consumed alongside [`STORAGE_OWNERSHIP_FIDELITY_V1`], never instead of it: a
/// consumer that carries base ownership onto a shadow object uses this name to
/// decide *which* identity a shadow `create` at a given path will produce --
/// the materialised shadow parent's, where one exists -- and still requires
/// ownership fidelity to carry at all. A backend that does not advertise it is
/// queried for the store's root identity instead, which is the conservative
/// answer that predates this name.
///
/// The claim is narrow and per-backend. tar advertises it unconditionally and
/// for the whole **pair**: a new node inherits its parent node's uid *and* gid,
/// because there is no kernel in the path to answer otherwise. A kernel-VFS
/// backend could qualify only where its platform inherits the parent's **gid**
/// unconditionally -- as BSD/macOS does, handing a new object its parent
/// directory's gid with no setgid bit required -- and even then only after a
/// live probe confirms it for the actual backing filesystem, because such a
/// backend may sit on a mount (a network filesystem, say) whose server assigns
/// identity instead. There the uid is *not* the parent's: `create` gives the
/// new object the creating process's euid. No kernel-VFS backend advertises
/// this name today -- `umbra-storage-local` leaves that qualification to a
/// follow-up -- so tar is currently its only advertiser.
///
/// A consumer that compares the whole `(uid, gid)` pair against the parent is
/// nonetheless exact on such a backend, and this is the premise a future
/// advertiser must preserve: a shadow parent can come to wear a uid other than
/// the creator's euid *only* through a successful ownership carry, which
/// requires privilege, and under that privilege the child's own carry succeeds
/// too. So on every input where the pair could disagree on uid, the carry the
/// comparison gates is guaranteed regardless -- the uid component never widens
/// admission unsoundly. Do **not** advertise this name for a backend whose
/// `create` can leave a child under a foreign-uid parent while that same carry
/// may be `Denied`: that is exactly where the pair comparison would fail open.
/// A backend whose kernel picks the process identity except under a setgid
/// parent (Linux), or whose server assigns identity the client never names
/// (NFS), does not advertise it. Advertise only after qualifying it against a
/// live store, never from configuration alone.
pub const STORAGE_PARENT_IDENTITY_V1: &str = "storage-parent-identity-v1";
