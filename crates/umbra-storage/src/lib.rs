//! Object-safe storage primitives for a single run.
//!
//! This crate owns the storage contract and provider protocol. Namespace policy belongs to the overlay;
//! constructors, filesystem I/O and service clients belong to backend packages.
//! Shared DTOs live in core and are re-exported here for backend authors.
//!
//! ```
//! use umbra_storage::Storage;
//! fn inject(backend: Box<dyn Storage>) -> Box<dyn Storage> { backend }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use umbra_core::storage::*;
pub use umbra_core::{
    BytePath, ErrorKind, IdempotencyKey, LeaseEpoch, ObjectId, OperationId, Result, RunId,
    UmbraError, WriterId,
};

/// Synchronous storage backend, owning at most one active run binding.
///
/// All methods are dyn-compatible, with concrete shared request/result types.
/// Implementations must reject invalid lifecycle states, scope requests to the
/// active run, and validate capability and size limits before performing I/O.
/// Each mutation requires current writer authority and an idempotency key; reject
/// stale epochs and conflicting retries before mutation. Never follow a path out
/// of its anchored root or control directory, including through symlinks.
///
/// Methods report visibility, not durability, unless explicitly documented.
/// Implementations may not substitute host writes for unavailable shadow storage.
pub trait Storage: Send {
    /// Report only qualified behavior and finite limits, at most the contract maxima.
    fn capabilities(&self) -> StorageCapabilities;

    /// Open exactly one run, validating its immutable base, format and policy.
    /// An already-open object rejects another open; failure must not publish a
    /// partially usable binding. Physical roots are runtime information only.
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding>;

    /// Atomically acquire exclusive writer authority. Expiration is insufficient
    /// for takeover: fence old kernel handles/mappings or verify termination.
    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease>;

    /// Renew only the current lease. Failure or lost authority blocks mutations.
    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease>;

    /// Release the current lease after quiescence and durable completion.
    /// Reject stale leases; releasing authority does not flush data implicitly.
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()>;

    /// Execute one typed primitive with a matching typed response.
    ///
    /// Validate the active run, authority, paths, bounds and operation support.
    /// Reads/writes are capped by both `MAX_IO_BYTES` and advertised limits;
    /// directory pages by `MAX_DIRECTORY_ENTRIES` and advertised limits. Reject
    /// offset overflow. Direct callers receive the same checks as helper callers.
    /// Mutation retries with the same key/payload must not repeat effects; a
    /// conflicting payload is an error. Preserve hard-link object identity.
    /// An approved copy-up source is a validated base handle, never a host path.
    /// Unsupported semantics fail before mutation, including atomic operations
    /// that cannot be implemented atomically. A response-kind mismatch is a
    /// protocol error and cannot imply successful mutation or commit.
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse>;

    /// Persist the requested data/metadata scope under current writer authority.
    /// Return a receipt only after the promised persistence boundary is reached.
    /// Local persistence must never be reported as remote persistence.
    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt>;

    /// Close the active run and invalidate its runtime handles. Reject an active
    /// writer lease, surface pending persistence errors, and never mark a failed
    /// close as clean. After successful close this object can open another run.
    fn close_run(&mut self) -> Result<()>;

    /// Read at an absolute blob offset without changing any shared seek position.
    /// Short reads are allowed; zero on a nonempty buffer denotes EOF. The output
    /// remains untouched when a malformed response is rejected.
    fn read_at(
        &mut self,
        context: &RequestContext,
        path: &StoragePath,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize> {
        check_io(
            "read_at",
            offset,
            out.len(),
            self.capabilities().max_io_bytes,
        )?;
        let response = self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::ReadAt {
                path: path.clone(),
                offset,
                len: out.len() as u32,
            },
        })?;
        match response {
            StorageResponse::ReadAt(bytes) if bytes.len() <= out.len() => {
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            _ => Err(protocol_error("read_at")),
        }
    }

    /// Write at an absolute blob offset. Short writes are allowed; retry only the
    /// remaining bytes with a new operation/idempotency identity. No flush implied.
    fn write_at(
        &mut self,
        context: &RequestContext,
        path: &StoragePath,
        offset: u64,
        bytes: &[u8],
    ) -> Result<usize> {
        check_writer(context, "write_at")?;
        check_io(
            "write_at",
            offset,
            bytes.len(),
            self.capabilities().max_io_bytes,
        )?;
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::WriteAt {
                path: path.clone(),
                offset,
                bytes: bytes.to_vec(),
            },
        })? {
            StorageResponse::WriteAt(count) if count as usize <= bytes.len() => Ok(count as usize),
            _ => Err(protocol_error("write_at")),
        }
    }

    /// Exclusively create a new object; an existing name is `AlreadyExists`.
    fn create(
        &mut self,
        context: &RequestContext,
        path: &StoragePath,
        options: &CreateOptions,
    ) -> Result<ObjectResult> {
        check_writer(context, "create")?;
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::Create {
                path: path.clone(),
                options: options.clone(),
            },
        })? {
            StorageResponse::Created(object) => Ok(object),
            _ => Err(protocol_error("create")),
        }
    }

    /// Remove a file/logical-symlink name without following the final symlink.
    /// Preserve other hard links and open handles. Directories use RemoveDirectory.
    fn unlink(&mut self, context: &RequestContext, path: &StoragePath) -> Result<()> {
        check_writer(context, "unlink")?;
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::Unlink { path: path.clone() },
        })? {
            StorageResponse::Unlinked => Ok(()),
            _ => Err(protocol_error("unlink")),
        }
    }

    /// List one bounded page of direct children, excluding `.` and `..`.
    /// Cursors are opaque and scoped to the run/directory/session. Concurrent
    /// mutation may invalidate a cursor explicitly, never escape its directory.
    fn list(
        &mut self,
        context: &RequestContext,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        if limit == 0
            || limit > MAX_DIRECTORY_ENTRIES
            || limit > self.capabilities().max_directory_entries
        {
            return Err(UmbraError::new(
                ErrorKind::InvalidInput,
                "list",
                "invalid page limit",
            ));
        }
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::List {
                path: path.clone(),
                cursor: cursor.cloned(),
                limit,
            },
        })? {
            StorageResponse::List(page)
                if page.entries.len() <= limit as usize
                    && page.entries.iter().all(|entry| {
                        let name = entry.name.as_bytes();
                        !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/')
                    }) =>
            {
                Ok(page)
            }
            _ => Err(protocol_error("list")),
        }
    }

    /// Return stable object identity and metadata without following the final symlink.
    fn stat(&mut self, context: &RequestContext, path: &StoragePath) -> Result<BlobStat> {
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::Stat { path: path.clone() },
        })? {
            StorageResponse::Stat(stat) => Ok(stat),
            _ => Err(protocol_error("stat")),
        }
    }

    /// Atomically exchange two existing names within one anchor. Observers see
    /// either complete state, never an intermediate rename; this does not flush.
    fn atomic_swap(
        &mut self,
        context: &RequestContext,
        left: &StoragePath,
        right: &StoragePath,
    ) -> Result<()> {
        check_writer(context, "atomic_swap")?;
        if left.anchor() != right.anchor() {
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                "atomic_swap",
                "cross-anchor swap",
            ));
        }
        if !self.capabilities().atomic_swap {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "atomic_swap",
                "atomic exchange unavailable",
            ));
        }
        match self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::AtomicSwap {
                left: left.clone(),
                right: right.clone(),
            },
        })? {
            StorageResponse::Swapped => Ok(()),
            _ => Err(protocol_error("atomic_swap")),
        }
    }
}

fn protocol_error(operation: &str) -> UmbraError {
    UmbraError::new(
        ErrorKind::ProtocolMismatch,
        operation,
        "unexpected response kind or invalid response bounds",
    )
}

/// Shared preflight for direct provider requests, before invoking a backend.
/// Backends still validate run binding, current lease, path containment and retries.
pub fn validate_request(
    capabilities: &StorageCapabilities,
    request: &StorageRequest,
) -> Result<()> {
    if request.operation.is_mutation() {
        check_writer(&request.context, "execute")?;
    }
    match &request.operation {
        StorageOperation::ReadAt { offset, len, .. } => check_io(
            "execute.read_at",
            *offset,
            *len as usize,
            capabilities.max_io_bytes,
        ),
        StorageOperation::WriteAt { offset, bytes, .. } => check_io(
            "execute.write_at",
            *offset,
            bytes.len(),
            capabilities.max_io_bytes,
        ),
        StorageOperation::List { limit, .. }
            if *limit == 0
                || *limit > MAX_DIRECTORY_ENTRIES
                || *limit > capabilities.max_directory_entries =>
        {
            Err(UmbraError::new(
                ErrorKind::InvalidInput,
                "execute.list",
                "invalid page limit",
            ))
        }
        StorageOperation::AtomicSwap { .. } if !capabilities.atomic_swap => Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "execute.atomic_swap",
            "atomic swap unsupported",
        )),
        _ => Ok(()),
    }
}

fn check_writer(context: &RequestContext, operation: &str) -> Result<()> {
    if context.writer_epoch.is_none() {
        return Err(UmbraError::new(
            ErrorKind::LeaseLost,
            operation,
            "mutation requires writer epoch",
        ));
    }
    Ok(())
}

fn check_io(operation: &str, offset: u64, len: usize, backend_limit: u32) -> Result<()> {
    if len > MAX_IO_BYTES
        || len > backend_limit as usize
        || offset.checked_add(len as u64).is_none()
    {
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            operation,
            "I/O exceeds size or offset bounds",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        response: StorageResponse,
        calls: Vec<StorageRequest>,
    }

    fn unavailable<T>() -> Result<T> {
        Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "fake",
            "unused lifecycle method",
        ))
    }

    impl Storage for Fake {
        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                features: Default::default(),
                durability: Durability::None,
                strict_remote_persistence: false,
                fencing: Fencing::ReadOnly,
                kernel_shadow: false,
                complete_emulation: false,
                hard_links: false,
                logical_symlinks: false,
                xattrs: false,
                atomic_replace: false,
                atomic_swap: false,
                max_io_bytes: 16,
                max_directory_entries: 2,
            }
        }
        fn open_run(&mut self, _: &OpenRunRequest) -> Result<RunBinding> {
            unavailable()
        }
        fn acquire_writer(&mut self, _: &AcquireWriterRequest) -> Result<WriterLease> {
            unavailable()
        }
        fn renew_writer(&mut self, _: &WriterLease) -> Result<WriterLease> {
            unavailable()
        }
        fn release_writer(&mut self, _: &WriterLease) -> Result<()> {
            unavailable()
        }
        fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
            self.calls.push(request.clone());
            Ok(self.response.clone())
        }
        fn flush(&mut self, _: &FlushRequest) -> Result<DurabilityReceipt> {
            unavailable()
        }
        fn close_run(&mut self) -> Result<()> {
            unavailable()
        }
    }

    fn context() -> RequestContext {
        RequestContext {
            run_id: RunId(Default::default()),
            operation_id: OperationId(Default::default()),
            idempotency_key: IdempotencyKey("retry".into()),
            writer_epoch: Some(LeaseEpoch(7)),
        }
    }

    fn path() -> StoragePath {
        StoragePath::new(StorageAnchor::Root, b"dir/\xff".to_vec()).unwrap()
    }

    #[test]
    fn storage_paths_preserve_bytes_and_reject_unanchored_components() {
        assert_eq!(path().as_bytes(), b"dir/\xff");
        assert!(StoragePath::new(StorageAnchor::Control, Vec::new()).is_ok());
        for bytes in [
            b"/abs".as_slice(),
            b"..",
            b"a/../b",
            b"a/./b",
            b"a//b",
            b"a/",
            b"a\0b",
        ] {
            assert_eq!(
                StoragePath::new(StorageAnchor::Root, bytes.to_vec())
                    .unwrap_err()
                    .kind,
                ErrorKind::InvalidPath
            );
        }
    }

    #[test]
    fn dyn_storage_reads_short_data_and_preserves_request_identity() {
        let mut fake = Fake {
            response: StorageResponse::ReadAt(vec![1, 2]),
            calls: vec![],
        };
        let backend: &mut dyn Storage = &mut fake;
        let mut out = [9; 4];
        assert_eq!(
            backend.read_at(&context(), &path(), 11, &mut out).unwrap(),
            2
        );
        assert_eq!(out, [1, 2, 9, 9]);
        assert_eq!(
            fake.calls,
            vec![StorageRequest {
                context: context(),
                operation: StorageOperation::ReadAt {
                    path: path(),
                    offset: 11,
                    len: 4
                },
            }]
        );
    }

    #[test]
    fn malformed_read_response_does_not_touch_output() {
        for response in [
            StorageResponse::ReadAt(vec![1; 5]),
            StorageResponse::Unlinked,
        ] {
            let mut fake = Fake {
                response,
                calls: vec![],
            };
            let mut out = [9; 4];
            assert_eq!(
                fake.read_at(&context(), &path(), 0, &mut out)
                    .unwrap_err()
                    .kind,
                ErrorKind::ProtocolMismatch
            );
            assert_eq!(out, [9; 4]);
        }
    }

    #[test]
    fn invalid_io_and_missing_authority_never_reach_backend() {
        let mut fake = Fake {
            response: StorageResponse::WriteAt(1),
            calls: vec![],
        };
        let mut reader = context();
        reader.writer_epoch = None;
        assert_eq!(
            fake.write_at(&reader, &path(), 0, &[1]).unwrap_err().kind,
            ErrorKind::LeaseLost
        );
        assert_eq!(
            fake.write_at(&context(), &path(), u64::MAX, &[1])
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            fake.write_at(&context(), &path(), 0, &[1; 17])
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn write_count_cannot_exceed_input_and_wrong_stat_response_is_rejected() {
        let mut fake = Fake {
            response: StorageResponse::WriteAt(3),
            calls: vec![],
        };
        assert_eq!(
            fake.write_at(&context(), &path(), 0, &[1, 2])
                .unwrap_err()
                .kind,
            ErrorKind::ProtocolMismatch
        );
        assert_eq!(
            fake.stat(&context(), &path()).unwrap_err().kind,
            ErrorKind::ProtocolMismatch
        );
    }

    #[test]
    fn invalid_pages_and_unsupported_swap_never_reach_backend() {
        let mut fake = Fake {
            response: StorageResponse::Swapped,
            calls: vec![],
        };
        assert_eq!(
            fake.list(&context(), &path(), None, 0).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            fake.list(&context(), &path(), None, 3).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            fake.atomic_swap(&context(), &path(), &path())
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedCapability
        );
        let control = StoragePath::new(StorageAnchor::Control, b"entry".to_vec()).unwrap();
        assert_eq!(
            fake.atomic_swap(&context(), &path(), &control)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidPath
        );
        assert!(fake.calls.is_empty());
    }
}

const _: Option<&dyn Storage> = None;

/// Versioned provider protocol, server harness and trait proxy.
#[cfg(unix)]
pub mod provider;
