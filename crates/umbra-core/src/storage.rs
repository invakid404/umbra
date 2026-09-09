//! Shared storage DTOs; no backend implementation or namespace policy.

use serde::{Deserialize, Serialize};

use crate::{
    BytePath, ErrorKind, IdempotencyKey, LeaseEpoch, ObjectId, OperationId, Result, RunId,
    UmbraError, WriterId,
};

/// Maximum bytes allowed in one storage or trace-memory operation.
pub const MAX_IO_BYTES: usize = 1024 * 1024;
/// Maximum directory entries requested in one page.
pub const MAX_DIRECTORY_ENTRIES: u32 = 4096;

/// The two disjoint namespaces within an opened run. Control is never tracee-visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StorageAnchor {
    /// Root.
    Root,
    /// Control.
    Control,
}

/// Byte-preserving path relative to an opened run's anchor, independent of mount roots.
/// Empty bytes denote the anchor itself. Normal components only; no normalization.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "StoragePathWire", into = "StoragePathWire")]
pub struct StoragePath {
    anchor: StorageAnchor,
    relative: Option<BytePath>,
}

#[derive(Serialize, Deserialize)]
struct StoragePathWire {
    anchor: StorageAnchor,
    bytes: Vec<u8>,
}

impl TryFrom<StoragePathWire> for StoragePath {
    type Error = UmbraError;
    fn try_from(wire: StoragePathWire) -> Result<Self> {
        Self::new(wire.anchor, wire.bytes)
    }
}

impl From<StoragePath> for StoragePathWire {
    fn from(path: StoragePath) -> Self {
        Self {
            anchor: path.anchor(),
            bytes: path.as_bytes().to_vec(),
        }
    }
}

impl StoragePath {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(anchor: StorageAnchor, bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if !bytes.is_empty()
            && bytes
                .split(|b| *b == b'/')
                .any(|c| c.is_empty() || c == b"." || c == b"..")
        {
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                "storage_path",
                "expected relative normal components without empty, dot or parent components",
            ));
        }
        let relative = if bytes.is_empty() {
            None
        } else {
            Some(BytePath::new(bytes)?)
        };
        Ok(Self { anchor, relative })
    }

    /// Anchor.
    pub fn anchor(&self) -> StorageAnchor {
        self.anchor
    }
    /// As bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.relative.as_ref().map_or(b"", BytePath::as_bytes)
    }
}

/// Opaque token owned by a provider session, invalid after that session closes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StorageHandle(pub Vec<u8>);

/// Runtime-only binding. Never put this in a manifest, checkpoint or journal record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDirectoryBinding {
    /// Handle.
    pub handle: StorageHandle,
    /// Runtime-only host path; never persistent run identity.
    pub physical_path: Option<BytePath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Persistence boundary acknowledged by a provider for a receipt scope.
/// Neither level establishes fencing, an atomic snapshot, or continuous persistence.
pub enum Durability {
    /// No persistence claim.
    None,
    /// Provider-documented local/OS synchronization, without qualified remote persistence.
    /// On a mounted network filesystem this does not promise a durable client replica.
    Local,
    /// Acknowledged persistence at a qualified remote stable-storage boundary.
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Fencing.
pub enum Fencing {
    /// Writable operation is unavailable.
    ReadOnly,
    /// Takeover requires independently confirmed former-writer termination/quiescence.
    ConfirmedTermination,
    /// Fencing revokes existing kernel FDs/mappings as well as API authority.
    KernelAndApi,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Storage capabilities.
pub struct StorageCapabilities {
    /// Durability.
    pub durability: Durability,
    /// Strict remote persistence.
    pub strict_remote_persistence: bool,
    /// Fencing.
    pub fencing: Fencing,
    /// Kernel shadow.
    pub kernel_shadow: bool,
    /// Complete emulation.
    pub complete_emulation: bool,
    /// Hard links.
    pub hard_links: bool,
    /// Logical symlinks.
    pub logical_symlinks: bool,
    /// Xattrs.
    pub xattrs: bool,
    /// Atomic replace.
    pub atomic_replace: bool,
    /// Atomic swap.
    pub atomic_swap: bool,
    /// Max io bytes.
    pub max_io_bytes: u32,
    /// Max directory entries.
    pub max_directory_entries: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Open run intent.
pub enum OpenRunIntent {
    /// Create new.
    CreateNew,
    /// Open existing.
    OpenExisting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Immutable base contract.
pub struct ImmutableBaseContract {
    /// Identity.
    pub identity: String,
    /// Fingerprint.
    pub fingerprint: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Storage policy.
pub struct StoragePolicy {
    /// Read only.
    pub read_only: bool,
    /// Require strict remote persistence.
    pub require_strict_remote_persistence: bool,
    /// Require kernel shadow.
    pub require_kernel_shadow: bool,
    /// Format version.
    pub format_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Open run request.
pub struct OpenRunRequest {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Intent.
    pub intent: OpenRunIntent,
    /// Immutable base.
    pub immutable_base: ImmutableBaseContract,
    /// Policy.
    pub policy: StoragePolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Run binding.
pub struct RunBinding {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Root.
    pub root: RuntimeDirectoryBinding,
    /// Control.
    pub control: RuntimeDirectoryBinding,
    /// Supported behavior advertised by the provider; qualification is required.
    pub capabilities: StorageCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Takeover policy.
pub enum TakeoverPolicy {
    /// Refuse.
    Refuse,
    /// Backend verifies evidence; opaque bytes alone are not proof of termination.
    ConfirmedTermination {
        /// Evidence.
        evidence: Vec<u8>,
    },
    /// Fence previous writer.
    FencePreviousWriter,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Acquire writer request.
pub struct AcquireWriterRequest {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Writer id.
    pub writer_id: WriterId,
    /// Takeover.
    pub takeover: TakeoverPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Writer lease.
pub struct WriterLease {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Writer id.
    pub writer_id: WriterId,
    /// Epoch.
    pub epoch: LeaseEpoch,
    /// Renewal token.
    pub renewal_token: Vec<u8>,
    /// Renew within this interval; this is not permission for another writer to take over.
    pub renew_after_millis: u64,
}

/// Every request is scoped to one run and idempotency key. Mutations require an epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Caller-supplied identity for retry deduplication.
    pub idempotency_key: IdempotencyKey,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: Option<LeaseEpoch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Storage request.
pub struct StorageRequest {
    /// Context associated with this value or operation.
    pub context: RequestContext,
    /// Operation.
    pub operation: StorageOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Object kind.
pub enum ObjectKind {
    /// File.
    File,
    /// Directory.
    Directory,
    /// Logical symlink.
    LogicalSymlink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Blob stat.
pub struct BlobStat {
    /// Object id.
    pub object_id: ObjectId,
    /// Kind.
    pub kind: ObjectKind,
    /// Length in bytes.
    pub len: u64,
    /// Link count.
    pub link_count: u64,
    /// Mode.
    pub mode: u32,
    /// Uid.
    pub uid: u32,
    /// Gid.
    pub gid: u32,
    /// Modified nanos.
    pub modified_nanos: i128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Create kind.
pub enum CreateKind {
    /// File.
    File,
    /// Directory.
    Directory,
    /// Logical bytes, never an unchecked physical host symlink target.
    LogicalSymlink {
        /// Target.
        target: BytePath,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Create options.
pub struct CreateOptions {
    /// Kind.
    pub kind: CreateKind,
    /// Mode.
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Directory entry.
pub struct DirectoryEntry {
    /// One nonempty byte component, never `.` or `..`.
    pub name: BytePath,
    /// Stat.
    pub stat: BlobStat,
}

/// Opaque, session-bound continuation; concurrent mutation may invalidate it explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListCursor(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Directory page.
pub struct DirectoryPage {
    /// Entries.
    pub entries: Vec<DirectoryEntry>,
    /// Next.
    pub next: Option<ListCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Rename mode.
pub enum RenameMode {
    /// No replace.
    NoReplace,
    /// Replace.
    Replace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Metadata update.
pub struct MetadataUpdate {
    /// Mode.
    pub mode: Option<u32>,
    /// Uid.
    pub uid: Option<u32>,
    /// Gid.
    pub gid: Option<u32>,
    /// Accessed nanos.
    pub accessed_nanos: Option<i128>,
    /// Modified nanos.
    pub modified_nanos: Option<i128>,
}

/// Handle issued after validating the immutable base contract; no arbitrary host paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedBaseObject {
    /// Object id.
    pub object_id: ObjectId,
    /// Handle.
    pub handle: StorageHandle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Storage operation.
pub enum StorageOperation {
    /// Lookup.
    Lookup {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// Stat.
    Stat {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// List.
    List {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Opaque continuation identity owned by this provider session.
        cursor: Option<ListCursor>,
        /// Limit.
        limit: u32,
    },
    /// Read at.
    ReadAt {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Absolute byte offset from the beginning of the object.
        offset: u64,
        /// Length in bytes.
        len: u32,
    },
    /// Write at.
    WriteAt {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Absolute byte offset from the beginning of the object.
        offset: u64,
        #[serde(deserialize_with = "bounded_write_bytes")]
        /// Owned bytes; no UTF-8 conversion is implied.
        bytes: Vec<u8>,
    },
    /// Create.
    Create {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Configuration or behavior options defined by the enclosing contract.
        options: CreateOptions,
    },
    /// Create parents.
    CreateParents {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Mode.
        mode: u32,
    },
    /// Copy up.
    CopyUp {
        /// Source.
        source: ApprovedBaseObject,
        /// Destination.
        destination: StoragePath,
    },
    /// Unlink.
    Unlink {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// Remove directory.
    RemoveDirectory {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// Rename.
    Rename {
        /// Source.
        source: StoragePath,
        /// Destination.
        destination: StoragePath,
        /// Mode.
        mode: RenameMode,
    },
    /// Link.
    Link {
        /// Source.
        source: StoragePath,
        /// Destination.
        destination: StoragePath,
    },
    /// Read link.
    ReadLink {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// Set metadata.
    SetMetadata {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Update.
        update: MetadataUpdate,
    },
    /// Truncate.
    Truncate {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Length in bytes.
        len: u64,
    },
    /// Get xattr.
    GetXattr {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Name.
        name: Vec<u8>,
        /// Max bytes.
        max_bytes: u32,
    },
    /// Set xattr.
    SetXattr {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Name.
        name: Vec<u8>,
        /// Value.
        value: Vec<u8>,
    },
    /// Remove xattr.
    RemoveXattr {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Name.
        name: Vec<u8>,
    },
    /// Get whiteout.
    GetWhiteout {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
    },
    /// Set whiteout.
    SetWhiteout {
        /// Path interpreted according to the enclosing operation and path type.
        path: StoragePath,
        /// Present.
        present: bool,
    },
    /// Exchange both existing names atomically; never a three-rename fallback.
    AtomicSwap {
        /// Left.
        left: StoragePath,
        /// Right.
        right: StoragePath,
    },
}

impl StorageOperation {
    /// Is mutation.
    pub fn is_mutation(&self) -> bool {
        !matches!(
            self,
            Self::Lookup { .. }
                | Self::Stat { .. }
                | Self::List { .. }
                | Self::ReadAt { .. }
                | Self::ReadLink { .. }
                | Self::GetXattr { .. }
                | Self::GetWhiteout { .. }
        )
    }
}

/// Runtime-only result for a validated kernel rewrite target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRewriteTarget {
    /// Parent.
    pub parent: RuntimeDirectoryBinding,
    /// Name.
    pub name: BytePath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Object result.
pub struct ObjectResult {
    /// Stat.
    pub stat: BlobStat,
    /// Rewrite target.
    pub rewrite_target: Option<RuntimeRewriteTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Storage response.
pub enum StorageResponse {
    /// Lookup.
    Lookup(ObjectResult),
    /// Stat.
    Stat(BlobStat),
    /// List.
    List(DirectoryPage),
    /// Read at.
    ReadAt(Vec<u8>),
    /// Write at.
    WriteAt(u32),
    /// Created.
    Created(ObjectResult),
    /// Parents created.
    ParentsCreated,
    /// Copied up.
    CopiedUp(ObjectResult),
    /// Unlinked.
    Unlinked,
    /// Directory removed.
    DirectoryRemoved,
    /// Renamed.
    Renamed(ObjectResult),
    /// Linked.
    Linked(ObjectResult),
    /// Read link.
    ReadLink(BytePath),
    /// Metadata set.
    MetadataSet(BlobStat),
    /// Truncated.
    Truncated(BlobStat),
    /// Xattr.
    Xattr(Option<Vec<u8>>),
    /// Xattr set.
    XattrSet,
    /// Xattr removed.
    XattrRemoved,
    /// Whiteout.
    Whiteout(bool),
    /// Whiteout set.
    WhiteoutSet,
    /// Swapped.
    Swapped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Flush scope.
pub enum FlushScope {
    /// Data.
    Data {
        /// Objects.
        objects: Vec<ObjectId>,
    },
    /// Data and metadata.
    DataAndMetadata {
        /// Objects.
        objects: Vec<ObjectId>,
    },
    /// Entire run.
    EntireRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Flush request.
pub struct FlushRequest {
    /// Context associated with this value or operation.
    pub context: RequestContext,
    /// Scope.
    pub scope: FlushScope,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Durability receipt.
pub struct DurabilityReceipt {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: LeaseEpoch,
    /// Scope.
    pub scope: FlushScope,
    /// Durability.
    pub durability: Durability,
    /// Provider evidence identifying the synchronization method and any qualification.
    /// This is not an independent attestation of storage hardware or writer fencing.
    pub evidence: Vec<u8>,
}

fn bounded_write_bytes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<u8>, D::Error> {
    struct BoundedBytes;
    impl<'de> serde::de::Visitor<'de> for BoundedBytes {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX_IO_BYTES} write bytes")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Vec<u8>, A::Error> {
            let mut bytes = Vec::new();
            while let Some(byte) = seq.next_element::<u8>()? {
                if bytes.len() == MAX_IO_BYTES {
                    return Err(serde::de::Error::custom("write exceeds MAX_IO_BYTES"));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
    deserializer.deserialize_seq(BoundedBytes)
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    #[test]
    fn storage_paths_roundtrip_bytes_and_reject_forged_components() {
        let path = StoragePath::new(StorageAnchor::Root, b"a/\xff".to_vec()).unwrap();
        let encoded = serde_json::to_vec(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<StoragePath>(&encoded).unwrap(),
            path
        );
        for bytes in [
            b"/absolute".as_slice(),
            b"../escape",
            b"a//b",
            b"a/./b",
            b"a\0b",
        ] {
            let encoded = serde_json::to_vec(&StoragePathWire {
                anchor: StorageAnchor::Root,
                bytes: bytes.to_vec(),
            })
            .unwrap();
            assert!(serde_json::from_slice::<StoragePath>(&encoded).is_err());
        }
    }
    #[test]
    fn write_payload_is_bounded_at_the_deserialization_boundary() {
        let mut op = StorageOperation::WriteAt {
            path: StoragePath::new(StorageAnchor::Root, b"file").unwrap(),
            offset: 0,
            bytes: vec![255; MAX_IO_BYTES],
        };
        let encoded = serde_json::to_vec(&op).unwrap();
        assert_eq!(
            serde_json::from_slice::<StorageOperation>(&encoded).unwrap(),
            op
        );
        if let StorageOperation::WriteAt { bytes, .. } = &mut op {
            bytes.push(0);
        }
        assert!(
            serde_json::from_slice::<StorageOperation>(&serde_json::to_vec(&op).unwrap()).is_err()
        );
    }
    #[test]
    fn storage_context_and_response_preserve_wire_identity() {
        let context = RequestContext {
            run_id: RunId(uuid::Uuid::new_v4()),
            operation_id: OperationId(uuid::Uuid::new_v4()),
            idempotency_key: IdempotencyKey("retry".into()),
            writer_epoch: Some(LeaseEpoch(9)),
        };
        assert_eq!(
            serde_json::from_slice::<RequestContext>(&serde_json::to_vec(&context).unwrap())
                .unwrap(),
            context
        );
        let response = StorageResponse::ReadAt(vec![0, 255, 3]);
        assert_eq!(
            serde_json::from_slice::<StorageResponse>(&serde_json::to_vec(&response).unwrap())
                .unwrap(),
            response
        );
    }
}
