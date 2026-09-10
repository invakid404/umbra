//! The operations surface: one typed `Storage` primitive in, one typed response
//! out.
//!
//! This is what [`crate::storage`] routes `execute` to, and what the M1
//! operations tests drive directly over
//! [`StateSession`](crate::integration::StateSession). It owns path resolution,
//! bounded enumeration, CRUD over the frozen transport facade, and the honest
//! refusal of everything M1 does not offer.
//!
//! # Three answers, never a fourth
//!
//! Every request ends in exactly one of: the typed response, a refusal naming a
//! capability this provider will not offer, or a refusal naming the node that owes
//! the wiring. Nothing returns a success it did not perform, and nothing
//! substitutes a weaker operation for a stronger one that is unavailable.
//!
//! # Mutations are gated on proven authority
//!
//! A mutation needs a [`MutationContext`]: an open-owner registry from a confirmed
//! client incarnation, and a writer epoch on the request. Reads need neither, and
//! take the anonymous-stateid path so they mint no open-owner and consume no
//! seqid. That split is why the read surface is reachable from a provider that has
//! not yet acquired writer authority, while a write from the same provider is
//! [`AuthorityError::NoWriterEpoch`](crate::error::AuthorityError::NoWriterEpoch).

use umbra_core::{
    BlobStat, CreateKind, DirectoryPage, Durability, ErrorKind, Fencing, ListCursor,
    MetadataUpdate, ObjectResult, OpenRunRequest, RenameMode, Result, RunBinding, RunId,
    StorageAnchor, StorageCapabilities, StorageOperation, StoragePath, StoragePolicy,
    StorageRequest, StorageResponse, UmbraError, MAX_DIRECTORY_ENTRIES, MAX_IO_BYTES,
};

use crate::anchor::{Anchor, HandleMint, RunAnchors, Target};
use crate::capability::{operation_name, storage_support, Support};
use crate::crud::{read_anonymous, CreateDisposition, MutationIdentity, OpenObject, WriteAt};
use crate::identity::PinnedObject;
use crate::namespace::{
    apply as apply_namespace, NamespaceDispatcher, NamespaceMutation, NamespaceOutcome, RemoveKind,
};
use crate::pages::{page, NoiseFilter};
use crate::replay::ReplayLog;
use crate::state::open_owner::{CloseOutcome, OpenOwnerRegistry};
use crate::storage::NfsUserspaceConfig;
use crate::transport::{
    AttrMask, ComponentName, Deadline, RawTransport, ShareAccess, Stability, Verifier,
};

/// Everything one request needs that the run binding does not own.
pub struct OpsContext<'a> {
    /// The transport to dispatch on.
    pub transport: &'a mut dyn RawTransport,
    /// The replay ledger every mutation records its intent in.
    pub replay: &'a mut dyn ReplayLog,
    /// Present only when the caller holds proven writer authority.
    pub mutations: Option<MutationContext<'a>>,
    /// Deadline applied to every COMPOUND this request submits.
    pub deadline: Deadline,
}

/// The extra state a mutation needs, and a read does not.
pub struct MutationContext<'a> {
    /// Open owners minted from a confirmed client incarnation.
    pub owners: &'a mut OpenOwnerRegistry,
    /// Create verifier for this request's idempotency key, when the caller
    /// retained one.
    ///
    /// Supplied by value rather than as a ledger because the ledger lives beside
    /// the incarnation in [`ProtocolState`](crate::state::ProtocolState), and
    /// borrowing both at once is not expressible. Resolve it with
    /// [`CreateVerifierLedger::verifier_for`](crate::state::verifier::CreateVerifierLedger::verifier_for)
    /// before building this context.
    pub create_verifier: Option<Verifier>,
    /// Puts a namespace mutation on the wire, when one is bound.
    pub namespace: Option<&'a mut dyn NamespaceDispatcher>,
}

/// Finite limits this provider will honour, derived from the bound transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationLimits {
    /// Largest single read or write.
    pub max_io_bytes: u32,
    /// Largest directory page.
    pub max_directory_entries: u32,
}

impl OperationLimits {
    /// Derive limits from a transport's own bounds.
    ///
    /// Both are the minimum of what the contract permits and what the transport
    /// will actually carry, so an advertised limit is one the provider can meet
    /// rather than one it hopes to.
    pub fn from_transport(transport: &dyn RawTransport) -> Self {
        let reply_bytes = u32::try_from(transport.limits().max_reply_bytes).unwrap_or(u32::MAX);
        Self {
            max_io_bytes: reply_bytes.min(u32::try_from(MAX_IO_BYTES).unwrap_or(u32::MAX)),
            max_directory_entries: (reply_bytes / crate::pages::BYTES_PER_ENTRY)
                .clamp(1, MAX_DIRECTORY_ENTRIES),
        }
    }
}

/// One opened run's operations surface.
#[derive(Clone, Debug)]
pub struct Operations {
    run_id: RunId,
    anchors: RunAnchors,
    mint: HandleMint,
    limits: OperationLimits,
    /// The policy the run was opened under.
    ///
    /// **R1-003.** `open` used to validate `format_version` and drop the rest, so
    /// a read-only run mutated and an unsupported durability requirement was
    /// silently accepted. Keeping it is what lets `execute` answer for it.
    policy: StoragePolicy,
}

impl Operations {
    /// Open a run and resolve its anchors.
    ///
    /// `serial` distinguishes successive sessions over the same run so a
    /// [`StorageHandle`](umbra_core::StorageHandle) issued before `close_run`
    /// cannot be replayed after it.
    pub fn open(
        transport: &mut dyn RawTransport,
        config: &NfsUserspaceConfig,
        request: &OpenRunRequest,
        serial: u64,
        deadline: Deadline,
    ) -> Result<Self> {
        // R1-003: every unsupported requirement is refused *before* the run is
        // touched, so a caller that asked for a guarantee this provider does not
        // offer gets a refusal rather than a run that silently ignores it. The
        // mounted adapter refuses the same two at
        // `crates/umbra-storage-nfs/src/lib.rs`; a userspace run that accepted
        // them would be advertising a durability boundary it never qualified.
        if request.policy.require_kernel_shadow {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "open_run",
                "this provider has no kernel-visible shadow: it speaks NFSv4 itself and                  publishes no physical path",
            ));
        }
        if request.policy.require_strict_remote_persistence {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "open_run",
                "strict remote persistence is not qualified by this provider; it advertises                  Durability::None and must not accept a run that requires more",
            ));
        }
        if request.policy.format_version != crate::storage::FORMAT_VERSION {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "open_run",
                format!(
                    "run format version {} is not the {} this provider reads",
                    request.policy.format_version,
                    crate::storage::FORMAT_VERSION
                ),
            ));
        }
        if request.policy.read_only && request.intent == umbra_core::OpenRunIntent::CreateNew {
            return Err(UmbraError::new(
                ErrorKind::Denied,
                "open_run",
                "a read-only run cannot be created: creation is itself a mutation",
            ));
        }
        let anchors =
            RunAnchors::open(transport, config, request.run_id, request.intent, deadline)?;
        Ok(Self {
            run_id: request.run_id,
            anchors,
            mint: HandleMint::for_session(request.run_id, serial),
            limits: OperationLimits::from_transport(transport),
            policy: request.policy.clone(),
        })
    }

    /// The policy this run was opened under.
    pub fn policy(&self) -> &StoragePolicy {
        &self.policy
    }

    /// The run identity this surface is bound to.
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The resolved anchors.
    pub fn anchors(&self) -> &RunAnchors {
        &self.anchors
    }

    /// The token mint for this session.
    pub fn mint(&self) -> &HandleMint {
        &self.mint
    }

    /// Limits this surface honours.
    pub fn limits(&self) -> OperationLimits {
        self.limits
    }

    /// Capabilities advertised for this opened run.
    ///
    /// Only the finite I/O and page limits are advertised, and only because they
    /// are met. Durability stays [`Durability::None`] and fencing stays
    /// [`Fencing::ReadOnly`]: this node qualified no persistence boundary and owns
    /// no termination verifier, and neither the WRITE/COMMIT verifier flow nor a
    /// successful lease renewal changes that.
    pub fn capabilities(&self) -> StorageCapabilities {
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
            max_io_bytes: self.limits.max_io_bytes,
            max_directory_entries: self.limits.max_directory_entries,
        }
    }

    /// The binding published to a consumer.
    pub fn binding(&self) -> RunBinding {
        RunBinding {
            run_id: self.run_id,
            root: self.anchors.root().binding(&self.mint),
            control: self.anchors.control().binding(&self.mint),
            capabilities: self.capabilities(),
        }
    }

    /// Everything that must hold before a request may have *any* effect.
    ///
    /// **R2-002.** These checks used to live at the top of [`Self::execute`],
    /// which the provider reaches only *after* the durable retry journal has
    /// created and written its index and intent records. A request naming another
    /// run, a mutation on a read-only run, an unsupported operation and an
    /// oversized request therefore all left private records behind before being
    /// refused. Splitting them out lets the provider run them first, against the
    /// same surface, without duplicating the rules.
    ///
    /// Ordering inside is deliberate: bounds and the writer-epoch requirement,
    /// then "this run takes no mutations at all", then run identity, then
    /// capability support.
    pub fn preflight(&self, request: &StorageRequest) -> Result<()> {
        let operation = operation_name(&request.operation);
        // The same preflight direct callers and helper callers get: I/O bounds,
        // page limits and the writer-epoch requirement for mutations.
        umbra_storage::validate_request(&self.capabilities(), request)?;
        // R1-003: `validate_request` deliberately leaves run-policy enforcement to
        // the backend, so a read-only run mutated freely. Refused here, before any
        // effect and before the run identity check, because "this run does not
        // take mutations at all" is the broader answer.
        if self.policy.read_only && request.operation.is_mutation() {
            return Err(UmbraError::new(
                ErrorKind::Denied,
                operation,
                "this run was opened read-only; it does not accept mutations",
            ));
        }
        // R1-003: the request must name the run this surface is bound to. Nothing
        // upstream checks it: `validate_request` only requires that *some* writer
        // epoch is present, and `MutationIdentity::from_context` accepts whatever
        // run and epoch it is handed.
        if request.context.run_id != self.run_id {
            return Err(UmbraError::new(
                ErrorKind::InvalidInput,
                operation,
                format!(
                    "the request names run {}, but this surface is bound to run {}",
                    request.context.run_id.0, self.run_id.0
                ),
            ));
        }
        // Anything this provider will never offer stops here, before any effect.
        // A deferred operation falls through to its handler, which reports the
        // contracts gap by name when no dispatcher is bound.
        let support = storage_support(&request.operation);
        if matches!(support, Support::Unsupported { .. }) {
            let refused: Result<StorageResponse> = support.refuse(operation);
            return refused.map(|_| ());
        }
        Ok(())
    }

    /// Execute one contract primitive.
    pub fn execute(
        &self,
        context: &mut OpsContext<'_>,
        request: &StorageRequest,
    ) -> Result<StorageResponse> {
        let operation = operation_name(&request.operation);
        // Re-run even though `NfsUserspaceStorage::execute` already ran it before
        // touching the journal (R2-002). This is the seam a direct caller reaches
        // without a provider, so it validates for itself rather than trusting that
        // somebody upstream did.
        self.preflight(request)?;
        let support = storage_support(&request.operation);
        match &request.operation {
            StorageOperation::Lookup { path } => {
                let object = self.resolve(context, path)?;
                Ok(StorageResponse::Lookup(
                    self.object_result(context, &object)?,
                ))
            }
            StorageOperation::Stat { path } => {
                let object = self.resolve(context, path)?;
                Ok(StorageResponse::Stat(self.stat(context, &object)?))
            }
            StorageOperation::List {
                path,
                cursor,
                limit,
            } => {
                let directory = self.resolve(context, path)?;
                let listed = self.list(context, &directory, cursor.as_ref(), *limit)?;
                Ok(StorageResponse::List(listed))
            }
            StorageOperation::ReadAt { path, offset, len } => {
                let object = self.resolve(context, path)?;
                let reply =
                    read_anonymous(context.transport, &object, *offset, *len, context.deadline)
                        .map_err(|error| error.to_umbra(operation))?;
                Ok(StorageResponse::ReadAt(reply.data))
            }
            StorageOperation::WriteAt {
                path,
                offset,
                bytes,
            } => self.write_at(context, request, path, *offset, bytes, operation),
            StorageOperation::Create { path, options } => match &options.kind {
                CreateKind::File => {
                    self.create_file(context, request, path, options.mode, operation)
                }
                CreateKind::Directory => {
                    self.create_directory(context, path, options.mode, operation)
                }
                CreateKind::LogicalSymlink { .. } => support.refuse(operation),
            },
            StorageOperation::CreateParents { path, mode } => {
                self.create_parents(context, path, *mode, operation)
            }
            StorageOperation::CopyUp { .. } => support.refuse(operation),
            StorageOperation::Unlink { path } => {
                self.remove(context, path, RemoveKind::File, operation)?;
                Ok(StorageResponse::Unlinked)
            }
            StorageOperation::RemoveDirectory { path } => {
                self.remove(context, path, RemoveKind::Directory, operation)?;
                Ok(StorageResponse::DirectoryRemoved)
            }
            StorageOperation::Rename {
                source,
                destination,
                mode,
            } => self.rename(context, source, destination, *mode, operation),
            StorageOperation::SetMetadata { path, update } => {
                let object = self.resolve(context, path)?;
                self.attributes(
                    context,
                    NamespaceMutation::SetAttributes {
                        object: object.handle().clone(),
                        target: object.identity(),
                        update: update.clone(),
                    },
                    operation,
                )?;
                Ok(StorageResponse::MetadataSet(self.stat(context, &object)?))
            }
            StorageOperation::Truncate { path, len } => {
                let object = self.resolve(context, path)?;
                self.truncate(context, path, &object, *len, operation)?;
                Ok(StorageResponse::Truncated(self.stat(context, &object)?))
            }
            // Every remaining operation is `Support::Unsupported` and was refused
            // before this match. Reaching one would mean the capability table and
            // this dispatch had drifted apart, which `refuse` reports rather than
            // absorbing.
            other => storage_support(other).refuse(operation),
        }
    }

    /// List one bounded page of a directory, choosing the right noise filter.
    ///
    /// Exposed so a provider-internal caller can enumerate the run anchor, which
    /// no contract path can name.
    pub fn list(
        &self,
        context: &mut OpsContext<'_>,
        directory: &PinnedObject,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        let filter = self.filter_for(directory);
        page(
            context.transport,
            directory,
            cursor,
            limit,
            &filter,
            context.deadline,
        )
    }

    /// The noise filter appropriate to one directory.
    ///
    /// The run anchor holds this provider's own `.provider` state beside the two
    /// contract anchors, so enumerating it hides that name. Every other directory
    /// is the tracee's or the overlay's, and hiding anything there would be a
    /// false answer about its contents.
    pub fn filter_for(&self, directory: &PinnedObject) -> NoiseFilter {
        if directory.identity() == self.anchors.run().pin().identity() {
            NoiseFilter::with_private_state()
        } else {
            NoiseFilter::protocol_only()
        }
    }

    /// The anchor a contract anchor names.
    pub fn anchor(&self, anchor: StorageAnchor) -> &Anchor {
        self.anchors.for_contract(anchor)
    }

    fn resolve(&self, context: &mut OpsContext<'_>, path: &StoragePath) -> Result<PinnedObject> {
        self.anchors
            .resolve(context.transport, path, context.deadline)
    }

    fn parent_and_name(
        &self,
        context: &mut OpsContext<'_>,
        path: &StoragePath,
        operation: &str,
    ) -> Result<(PinnedObject, ComponentName)> {
        match self
            .anchors
            .resolve_parent(context.transport, path, context.deadline)?
        {
            Target::Named { parent, name } => Ok((parent, name)),
            Target::Anchor(_) => Err(UmbraError::new(
                ErrorKind::InvalidPath,
                operation,
                "an anchor is not a name this operation can act on",
            )),
        }
    }

    fn child(
        &self,
        context: &mut OpsContext<'_>,
        parent: &PinnedObject,
        name: &ComponentName,
        operation: &str,
    ) -> Result<PinnedObject> {
        let (handle, attributes) = context
            .transport
            .lookup(parent.handle(), name, AttrMask::STAT, context.deadline)
            .map_err(|error| error.to_umbra(operation))?;
        PinnedObject::adopt(handle, &attributes).map_err(|error| error.to_umbra(operation))
    }

    fn stat(&self, context: &mut OpsContext<'_>, object: &PinnedObject) -> Result<BlobStat> {
        object.stat(context.transport, context.deadline)
    }

    fn object_result(
        &self,
        context: &mut OpsContext<'_>,
        object: &PinnedObject,
    ) -> Result<ObjectResult> {
        Ok(ObjectResult {
            stat: self.stat(context, object)?,
            // No kernel-visible path exists for a userspace client, so there is no
            // operand a rewritten `openat` could use. This provider advertises no
            // open-rewrite capability and reports no target.
            rewrite_target: None,
        })
    }

    fn write_at(
        &self,
        context: &mut OpsContext<'_>,
        request: &StorageRequest,
        path: &StoragePath,
        offset: u64,
        bytes: &[u8],
        operation: &str,
    ) -> Result<StorageResponse> {
        let identity =
            MutationIdentity::from_context(&request.context).map_err(|e| e.to_umbra(operation))?;
        let (parent, name) = self.parent_and_name(context, path, operation)?;
        let OpsContext {
            transport,
            replay,
            mutations,
            deadline,
        } = context;
        let deadline = *deadline;
        let (owners, _) = require_owners(mutations, operation)?;
        let open = OpenObject::open(
            owners,
            &mut **transport,
            &parent,
            &name,
            CreateDisposition::OpenExisting,
            ShareAccess::WRITE,
            deadline,
        )
        .map_err(|error| error.to_umbra(operation))?;
        // `UNSTABLE`, because a durability receipt is the only thing entitled to
        // claim persistence and this provider issues none. The verifier is
        // recorded against the idempotency key, so whichever node eventually
        // COMMITs can find it and compare.
        let written = open.write(
            &mut **transport,
            &mut **replay,
            &identity,
            WriteAt {
                offset,
                stability: Stability::Unstable,
                data: bytes.to_vec(),
            },
            deadline,
        );
        let count = match written {
            Ok(ticket) => ticket.count(),
            Err(error) => {
                // The write failed; the open still has to go, but its own failure
                // must not replace the one the caller needs to see.
                let _ = open.close(&mut **transport, deadline);
                return Err(error.to_umbra(operation));
            }
        };
        release(open, &mut **transport, deadline, operation)?;
        Ok(StorageResponse::WriteAt(count))
    }

    fn create_file(
        &self,
        context: &mut OpsContext<'_>,
        request: &StorageRequest,
        path: &StoragePath,
        mode: u32,
        operation: &str,
    ) -> Result<StorageResponse> {
        MutationIdentity::from_context(&request.context).map_err(|e| e.to_umbra(operation))?;
        let (parent, name) = self.parent_and_name(context, path, operation)?;
        let (open, created, was_exclusive) = {
            let deadline = context.deadline;
            let transport = &mut context.transport;
            let (owners, create_verifier) = require_owners(&mut context.mutations, operation)?;
            // With a retained verifier the create is retry-safe: presenting the
            // same one again is a recognised replay rather than a false collision.
            // Without one, `GUARDED4` still gives the contract's semantics — an
            // existing name is `AlreadyExists` — but a lost reply cannot then be
            // told apart from a genuine collision.
            let disposition = match create_verifier {
                Some(verifier) => CreateDisposition::CreateExclusive { verifier },
                None => CreateDisposition::CreateNew { mode },
            };
            let was_exclusive = create_verifier.is_some();
            let open = OpenObject::open(
                owners,
                &mut **transport,
                &parent,
                &name,
                disposition,
                ShareAccess::BOTH,
                deadline,
            )
            .map_err(|error| error.to_umbra(operation))?;
            let created = PinnedObject::pin(&mut **transport, open.handle().clone(), deadline)
                .map_err(|error| error.to_umbra(operation))?;
            (open, created, was_exclusive)
        };

        // R1-007: `EXCLUSIVE4` carries a verifier in the field `GUARDED4` uses for
        // the initial attributes, so the requested mode was never sent — creating
        // an executable with mode 0755 quietly produced the server's default.
        // RFC 7530 §18.16.3 and `docs/design/managed-lifecycle-spike.md:33` both
        // say the attributes are applied afterwards, with SETATTR.
        //
        // The provider supplies a create verifier for every keyed operation, so
        // this is the ordinary path, not a corner case.
        if was_exclusive {
            let requested = MetadataUpdate {
                mode: Some(mode),
                uid: None,
                gid: None,
                accessed_nanos: None,
                modified_nanos: None,
            };
            let applied = dispatch_namespace(
                context,
                operation,
                &NamespaceMutation::SetAttributes {
                    object: created.handle().clone(),
                    target: created.identity(),
                    update: requested,
                },
            );
            if let Err(error) = applied {
                // The file exists and the mode is the server's default. There is
                // no rollback to invent — an EXCLUSIVE4 create is not undone by
                // removing the name, which could destroy a concurrent writer's
                // work — so the partial effect is reported rather than hidden.
                let _ = release(open, context.transport, context.deadline, operation);
                return Err(error);
            }
        }

        // Read the result after the attributes are applied, so a caller is told
        // the mode the object actually has.
        let result = self.object_result(context, &created)?;
        release(open, context.transport, context.deadline, operation)?;
        Ok(StorageResponse::Created(result))
    }

    fn create_directory(
        &self,
        context: &mut OpsContext<'_>,
        path: &StoragePath,
        mode: u32,
        operation: &str,
    ) -> Result<StorageResponse> {
        let (parent, name) = self.parent_and_name(context, path, operation)?;
        let effect = match dispatch_namespace(
            context,
            operation,
            &NamespaceMutation::CreateDirectory {
                parent: parent.handle().clone(),
                name,
                mode,
            },
        )? {
            NamespaceOutcome::Created(effect) => effect,
            _ => unreachable!("apply proves the outcome kind matches the mutation"),
        };
        let created = PinnedObject::pin(context.transport, effect.handle, context.deadline)
            .map_err(|error| error.to_umbra(operation))?;
        Ok(StorageResponse::Created(
            self.object_result(context, &created)?,
        ))
    }

    /// Create every missing component of `path`, like `mkdir -p`.
    ///
    /// A component that already exists as a directory is accepted and walked
    /// through; one that exists as anything else stops the walk, because
    /// continuing would mean treating a file as a directory. Nothing is rolled
    /// back on a later failure: the contract has no transactional create, and
    /// removing directories this call did not create would be worse than
    /// leaving a partial path a retry can complete.
    fn create_parents(
        &self,
        context: &mut OpsContext<'_>,
        path: &StoragePath,
        mode: u32,
        operation: &str,
    ) -> Result<StorageResponse> {
        let mut current = self.anchors.for_contract(path.anchor()).pin().clone();
        let mut created = None;
        for raw in path.as_bytes().split(|byte| *byte == b'/') {
            let name = crate::anchor::component(raw.to_vec())?;
            current = match self.existing_child(context, &current, &name)? {
                Some(existing) => {
                    if !existing.is_directory() {
                        return Err(UmbraError::new(
                            ErrorKind::AlreadyExists,
                            operation,
                            format!(
                                "{:?} exists and is not a directory",
                                String::from_utf8_lossy(raw)
                            ),
                        ));
                    }
                    existing
                }
                None => {
                    let effect = match dispatch_namespace(
                        context,
                        operation,
                        &NamespaceMutation::CreateDirectory {
                            parent: current.handle().clone(),
                            name,
                            mode,
                        },
                    )? {
                        NamespaceOutcome::Created(effect) => effect,
                        _ => unreachable!("apply proves the outcome kind matches the mutation"),
                    };
                    let pinned =
                        PinnedObject::pin(context.transport, effect.handle, context.deadline)
                            .map_err(|error| error.to_umbra(operation))?;
                    created = Some(pinned.clone());
                    pinned
                }
            };
        }
        // The response describes the leaf, whether this call made it or found it.
        let leaf = created.unwrap_or(current);
        Ok(StorageResponse::Created(
            self.object_result(context, &leaf)?,
        ))
    }

    /// Resolve one child, reporting absence as `None` rather than an error.
    fn existing_child(
        &self,
        context: &mut OpsContext<'_>,
        parent: &PinnedObject,
        name: &ComponentName,
    ) -> Result<Option<PinnedObject>> {
        match self.child(context, parent, name, "create_parents") {
            Ok(pinned) => Ok(Some(pinned)),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn rename(
        &self,
        context: &mut OpsContext<'_>,
        source: &StoragePath,
        destination: &StoragePath,
        mode: RenameMode,
        operation: &str,
    ) -> Result<StorageResponse> {
        if source.anchor() != destination.anchor() {
            // The syscall matrix rejects cross-anchor moves outright: the control
            // namespace is never tracee-visible, and an object must not cross that
            // boundary by being renamed across it.
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                operation,
                "a cross-anchor rename is rejected",
            ));
        }
        if mode == RenameMode::NoReplace {
            // R1-006: NFSv4.0 RENAME always replaces, and there is no v4.0
            // operation that makes "rename only if the destination is absent"
            // atomic. The previous emulation probed the destination and then
            // renamed: another client can create the destination between the two
            // round trips, and a probe that failed for any reason other than
            // NOENT — EIO, ACCESS, STALE — fell through to an ordinary replacing
            // RENAME. `docs/design/syscall-matrix.md` prohibits exactly that
            // check-then-rename, and `managed-lifecycle-spike.md` requires the
            // unsupported atomic no-replace to be refused unless it is proven.
            //
            // Refusing before any effect is the honest answer. Ordinary
            // `RenameMode::Replace` is unaffected.
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                operation,
                "atomic no-replace rename is not available over NFSv4.0: RENAME always \
                 replaces, and emulating the guarantee with a destination probe is not \
                 atomic. Use RenameMode::Replace, or check the destination yourself and \
                 accept the race.",
            ));
        }
        let (source_parent, source_name) = self.parent_and_name(context, source, operation)?;
        let (target_parent, target_name) = self.parent_and_name(context, destination, operation)?;
        // The source identity is proven *before* the rename, so `apply` can prove
        // afterwards that the very same object moved.
        let pinned = self.child(context, &source_parent, &source_name, operation)?;
        let effect = match dispatch_namespace(
            context,
            operation,
            &NamespaceMutation::Rename {
                source_parent: source_parent.handle().clone(),
                source_name,
                target_parent: target_parent.handle().clone(),
                target_name,
                source: pinned.identity(),
                mode,
            },
        )? {
            NamespaceOutcome::Renamed(effect) => effect,
            _ => unreachable!("apply proves the outcome kind matches the mutation"),
        };
        let moved = PinnedObject::pin(context.transport, effect.handle, context.deadline)
            .map_err(|error| error.to_umbra(operation))?;
        Ok(StorageResponse::Renamed(
            self.object_result(context, &moved)?,
        ))
    }

    fn attributes(
        &self,
        context: &mut OpsContext<'_>,
        mutation: NamespaceMutation,
        operation: &str,
    ) -> Result<()> {
        match dispatch_namespace(context, operation, &mutation)? {
            NamespaceOutcome::AttributesSet(_) => Ok(()),
            _ => unreachable!("apply proves the outcome kind matches the mutation"),
        }
    }

    /// Truncate through SETATTR, holding an open with WRITE access for it.
    ///
    /// The open exists solely to authorise the SETATTR: RFC 7530 §16.32 requires
    /// an open stateid with WRITE access to set `FATTR4_SIZE`. It is closed
    /// either way, and a CLOSE failure never replaces the truncation's own error.
    fn truncate(
        &self,
        context: &mut OpsContext<'_>,
        path: &StoragePath,
        object: &PinnedObject,
        len: u64,
        operation: &str,
    ) -> Result<()> {
        let (parent, name) = self.parent_and_name(context, path, operation)?;
        let deadline = context.deadline;
        let (owners, _) = require_owners(&mut context.mutations, operation)?;
        let open = OpenObject::open(
            owners,
            &mut *context.transport,
            &parent,
            &name,
            CreateDisposition::OpenExisting,
            ShareAccess::WRITE,
            deadline,
        )
        .map_err(|error| error.to_umbra(operation))?;
        let stateid = match open.stateid() {
            Ok(stateid) => stateid,
            Err(error) => {
                let _ = open.close(&mut *context.transport, deadline);
                return Err(error.to_umbra(operation));
            }
        };
        let outcome = self.attributes(
            context,
            NamespaceMutation::Truncate {
                object: object.handle().clone(),
                target: object.identity(),
                len,
                stateid,
            },
            operation,
        );
        if outcome.is_err() {
            let _ = open.close(&mut *context.transport, deadline);
            return outcome;
        }
        release(open, &mut *context.transport, deadline, operation)
    }

    fn remove(
        &self,
        context: &mut OpsContext<'_>,
        path: &StoragePath,
        kind: RemoveKind,
        operation: &str,
    ) -> Result<()> {
        let (parent, name) = self.parent_and_name(context, path, operation)?;
        let target = self.child(context, &parent, &name, operation)?;
        // The contract has two removal operations and the caller must use the one
        // matching the object. Absorbing the mismatch would hide a caller bug and
        // remove something it did not mean to.
        let wants_directory = kind == RemoveKind::Directory;
        if target.is_directory() != wants_directory {
            return Err(UmbraError::new(
                ErrorKind::InvalidInput,
                operation,
                if wants_directory {
                    "remove_directory was asked to remove a non-directory"
                } else {
                    "unlink was asked to remove a directory"
                },
            ));
        }
        match dispatch_namespace(
            context,
            operation,
            &NamespaceMutation::Remove {
                parent: parent.handle().clone(),
                name,
                target: target.identity(),
                kind,
            },
        )? {
            NamespaceOutcome::Removed => Ok(()),
            _ => unreachable!("apply proves the outcome kind matches the mutation"),
        }
    }
}

/// Borrow the open-owner registry and this request's create verifier.
///
/// Handed out as a tuple rather than as `&mut MutationContext` because that
/// reference is invariant in the context's own lifetime, which would pin the
/// whole [`OpsContext`] borrow to the request and make every later use of the
/// transport a conflict. Reborrowing the field shortens instead.
fn require_owners<'c, 'a: 'c>(
    slot: &'c mut Option<MutationContext<'a>>,
    operation: &str,
) -> Result<(&'c mut OpenOwnerRegistry, Option<Verifier>)> {
    let mutations = slot.as_mut().ok_or_else(|| no_authority(operation))?;
    Ok((&mut *mutations.owners, mutations.create_verifier))
}

/// Apply one namespace mutation through the context's dispatcher.
///
/// Control is inverted rather than handing the dispatcher back, because a
/// returned `&mut dyn NamespaceDispatcher` carries the context's own lifetime
/// inside a trait object and would pin the whole [`OpsContext`] borrow to the
/// request. Running the mutation here keeps the borrow to this call.
///
/// A missing [`MutationContext`] and a missing dispatcher inside one mean
/// different things: the first is "no writer authority is bound at all", the
/// second is "authority is bound but nothing can put this operation on the wire",
/// which [`apply`](crate::namespace::apply) turns into the named contracts gap.
fn dispatch_namespace(
    context: &mut OpsContext<'_>,
    operation: &str,
    mutation: &NamespaceMutation,
) -> Result<NamespaceOutcome> {
    // Disjoint field borrows: the dispatcher and the transport it dispatches on
    // come out of the same `&mut OpsContext` without aliasing, which is what lets
    // the mutation travel on this request's own connection.
    let transport = &mut *context.transport;
    let mutations = context
        .mutations
        .as_mut()
        .ok_or_else(|| no_authority(operation))?;
    apply_namespace(mutations.namespace.as_deref_mut(), transport, mutation)
}

fn no_authority(operation: &str) -> UmbraError {
    UmbraError::new(
        ErrorKind::LeaseLost,
        operation,
        "a mutation needs open owners from a confirmed client incarnation; \
         authority_recovery binds them",
    )
}

/// Close an open the request no longer needs.
///
/// A CLOSE the server refused leaves the open live, and a lost CLOSE reply leaves
/// its outcome unknown. Neither is reported as clean.
fn release(
    open: OpenObject,
    transport: &mut dyn RawTransport,
    deadline: Deadline,
    operation: &str,
) -> Result<()> {
    match open.close(transport, deadline) {
        CloseOutcome::Closed(_) => Ok(()),
        CloseOutcome::Rejected { error, .. } | CloseOutcome::Abandoned { error } => {
            Err(error.to_umbra(operation))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{CreateOptions, MetadataUpdate, OpenRunIntent, StorageAnchor};

    use crate::capability::CONTRACT_SURFACE;
    use crate::fixture;

    fn path(bytes: &[u8]) -> StoragePath {
        StoragePath::new(StorageAnchor::Root, bytes.to_vec()).expect("path")
    }

    fn request(operation: StorageOperation, epoch: Option<u64>) -> StorageRequest {
        StorageRequest {
            context: fixture::context("ops-key", epoch),
            operation,
        }
    }

    #[test]
    fn a_run_binding_publishes_session_tokens_and_no_physical_path() {
        let (mut fake, _) = fixture::server();
        let operations = fixture::operations(&mut fake);
        let binding = operations.binding();
        assert_eq!(binding.run_id, fixture::run_id());
        assert_eq!(binding.root.physical_path, None);
        assert_eq!(binding.control.physical_path, None);
        assert_ne!(binding.root.handle, binding.control.handle);
        // Finite limits are advertised because the transport can meet them.
        assert!(binding.capabilities.max_io_bytes > 0);
        assert!(binding.capabilities.max_directory_entries > 0);
        assert!(binding.capabilities.max_directory_entries <= umbra_core::MAX_DIRECTORY_ENTRIES);
        // Nothing qualified is claimed.
        assert_eq!(
            binding.capabilities.durability,
            umbra_core::Durability::None
        );
        assert_eq!(binding.capabilities.fencing, umbra_core::Fencing::ReadOnly);
        assert!(!binding.capabilities.xattrs);
        assert!(!binding.capabilities.hard_links);
        assert!(!binding.capabilities.atomic_swap);
        assert!(!binding.capabilities.kernel_shadow);
        assert!(binding.capabilities.features.is_empty());
    }

    #[test]
    fn opening_a_run_of_the_wrong_format_version_is_refused() {
        let (mut fake, _) = fixture::server();
        let mut ask = fixture::open_run_request();
        ask.policy.format_version = crate::storage::FORMAT_VERSION + 1;
        let error = Operations::open(
            &mut fake,
            &fixture::config(),
            &ask,
            0,
            Deadline { millis: 5_000 },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ProtocolMismatch);
    }

    #[test]
    fn creating_a_run_that_already_exists_is_refused_rather_than_reopened() {
        let (mut fake, _) = fixture::server();
        let mut ask = fixture::open_run_request();
        ask.intent = OpenRunIntent::CreateNew;
        let error = Operations::open(
            &mut fake,
            &fixture::config(),
            &ask,
            0,
            Deadline { millis: 5_000 },
        )
        .unwrap_err();
        // `CreateNew` over an existing run is a collision. Reopening it would
        // hand the caller someone else's run under the name it asked to create.
        assert_eq!(error.kind, ErrorKind::AlreadyExists);
    }

    #[test]
    fn the_read_surface_round_trips_without_writer_authority() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"hello".to_vec());
        fake.insert_directory(&layout.root, b"sub");
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut context = fixture::ops_context(&mut fake, &mut replay, None);

        let StorageResponse::Stat(stat) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::Stat {
                        path: path(b"note"),
                    },
                    None,
                ),
            )
            .expect("stat")
        else {
            panic!("stat answers with a stat");
        };
        assert_eq!(stat.len, 5);
        assert_eq!(stat.kind, umbra_core::ObjectKind::File);

        let StorageResponse::Lookup(found) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::Lookup {
                        path: path(b"note"),
                    },
                    None,
                ),
            )
            .expect("lookup")
        else {
            panic!("lookup answers with an object result");
        };
        assert_eq!(found.stat.object_id, stat.object_id);
        // No kernel-visible path exists, so no rewrite target is offered.
        assert!(found.rewrite_target.is_none());

        let StorageResponse::ReadAt(bytes) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::ReadAt {
                        path: path(b"note"),
                        offset: 0,
                        len: 16,
                    },
                    None,
                ),
            )
            .expect("read")
        else {
            panic!("read answers with bytes");
        };
        assert_eq!(bytes, b"hello");

        let StorageResponse::List(listed) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::List {
                        path: path(b""),
                        cursor: None,
                        limit: 8,
                    },
                    None,
                ),
            )
            .expect("list")
        else {
            panic!("list answers with a page");
        };
        let mut names: Vec<_> = listed
            .entries
            .iter()
            .map(|entry| entry.name.as_bytes().to_vec())
            .collect();
        names.sort();
        assert_eq!(names, vec![b"note".to_vec(), b"sub".to_vec()]);
    }

    #[test]
    fn the_write_surface_needs_both_a_writer_epoch_and_bound_open_owners() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"hello".to_vec());
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();

        // No epoch: the shared preflight refuses before anything is dispatched.
        {
            let mut context = fixture::ops_context(&mut fake, &mut replay, None);
            let error = operations
                .execute(
                    &mut context,
                    &request(
                        StorageOperation::WriteAt {
                            path: path(b"note"),
                            offset: 0,
                            bytes: b"x".to_vec(),
                        },
                        None,
                    ),
                )
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::LeaseLost);
        }

        // An epoch but no incarnation: still refused, and the error names who
        // binds the missing piece.
        {
            let mut context = fixture::ops_context(&mut fake, &mut replay, None);
            let error = operations
                .execute(
                    &mut context,
                    &request(
                        StorageOperation::WriteAt {
                            path: path(b"note"),
                            offset: 0,
                            bytes: b"x".to_vec(),
                        },
                        Some(4),
                    ),
                )
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::LeaseLost);
            assert!(error.context.contains("authority_recovery"));
        }

        // Both present: the write lands and reports what the server accepted.
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));
        let StorageResponse::WriteAt(count) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::WriteAt {
                        path: path(b"note"),
                        offset: 0,
                        bytes: b"HELLO".to_vec(),
                    },
                    Some(4),
                ),
            )
            .expect("write")
        else {
            panic!("a write answers with a count");
        };
        assert_eq!(count, 5);
        let StorageResponse::ReadAt(bytes) = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::ReadAt {
                        path: path(b"note"),
                        offset: 0,
                        len: 16,
                    },
                    None,
                ),
            )
            .expect("read back")
        else {
            panic!("read answers with bytes");
        };
        assert_eq!(bytes, b"HELLO");
    }

    #[test]
    fn creating_a_file_returns_its_identity_and_refuses_a_second_create() {
        let (mut fake, _) = fixture::server();
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));
        let create = || {
            request(
                StorageOperation::Create {
                    path: path(b"fresh"),
                    options: CreateOptions {
                        kind: CreateKind::File,
                        mode: 0o640,
                    },
                },
                Some(4),
            )
        };
        let StorageResponse::Created(created) =
            operations.execute(&mut context, &create()).expect("create")
        else {
            panic!("a create answers with an object result");
        };
        assert_eq!(created.stat.kind, umbra_core::ObjectKind::File);
        let error = operations.execute(&mut context, &create()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::AlreadyExists);
    }

    #[test]
    fn every_unsupported_operation_is_refused_with_unsupported_capability() {
        let (mut fake, _) = fixture::server();
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));

        let unsupported = [
            StorageOperation::Link {
                source: path(b"a"),
                destination: path(b"b"),
            },
            StorageOperation::ReadLink { path: path(b"a") },
            StorageOperation::Create {
                path: path(b"a"),
                options: CreateOptions {
                    kind: CreateKind::LogicalSymlink {
                        target: umbra_core::BytePath::new(b"t").expect("target"),
                    },
                    mode: 0o777,
                },
            },
            StorageOperation::GetXattr {
                path: path(b"a"),
                name: b"user.x".to_vec(),
                max_bytes: 8,
            },
            StorageOperation::SetXattr {
                path: path(b"a"),
                name: b"user.x".to_vec(),
                value: vec![1],
            },
            StorageOperation::RemoveXattr {
                path: path(b"a"),
                name: b"user.x".to_vec(),
            },
            StorageOperation::GetWhiteout { path: path(b"a") },
            StorageOperation::SetWhiteout {
                path: path(b"a"),
                present: true,
            },
            StorageOperation::AtomicSwap {
                left: path(b"a"),
                right: path(b"b"),
            },
        ];
        for operation in unsupported {
            let error = operations
                .execute(&mut context, &request(operation.clone(), Some(4)))
                .unwrap_err();
            assert_eq!(
                error.kind,
                ErrorKind::UnsupportedCapability,
                "{operation:?} must be refused honestly, never emulated"
            );
        }
    }

    #[test]
    fn every_deferred_operation_names_its_owner_instead_of_claiming_a_capability() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"x".to_vec());
        fake.insert_directory(&layout.root, b"dir");
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));

        let deferred = [
            StorageOperation::Create {
                path: path(b"newdir"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            },
            StorageOperation::CreateParents {
                path: path(b"a/b"),
                mode: 0o700,
            },
            StorageOperation::Unlink {
                path: path(b"note"),
            },
            StorageOperation::RemoveDirectory { path: path(b"dir") },
            StorageOperation::Rename {
                source: path(b"note"),
                destination: path(b"moved"),
                mode: umbra_core::RenameMode::Replace,
            },
            StorageOperation::SetMetadata {
                path: path(b"note"),
                update: MetadataUpdate {
                    mode: Some(0o600),
                    uid: None,
                    gid: None,
                    accessed_nanos: None,
                    modified_nanos: None,
                },
            },
            StorageOperation::Truncate {
                path: path(b"note"),
                len: 0,
            },
        ];
        for operation in deferred {
            let error = operations
                .execute(&mut context, &request(operation.clone(), Some(4)))
                .unwrap_err();
            assert_eq!(
                error.kind,
                ErrorKind::NotImplemented,
                "{operation:?} is authorised by the syscall matrix but unreachable"
            );
        }
    }

    #[test]
    fn the_dispatch_agrees_with_the_capability_table_on_every_row() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"x".to_vec());
        fake.insert_directory(&layout.root, b"dir");
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));

        // Every non-supported row must produce exactly the kind its verdict
        // promises, so the published matrix is the code path a caller meets.
        for row in CONTRACT_SURFACE {
            let Some(expected) = row.support.error_kind() else {
                continue;
            };
            let operation = match row.operation {
                "Create { kind: Directory }" => StorageOperation::Create {
                    path: path(b"newdir"),
                    options: CreateOptions {
                        kind: CreateKind::Directory,
                        mode: 0o700,
                    },
                },
                "Create { kind: LogicalSymlink }" => StorageOperation::Create {
                    path: path(b"link"),
                    options: CreateOptions {
                        kind: CreateKind::LogicalSymlink {
                            target: umbra_core::BytePath::new(b"t").expect("target"),
                        },
                        mode: 0o777,
                    },
                },
                "CreateParents" => StorageOperation::CreateParents {
                    path: path(b"a/b"),
                    mode: 0o700,
                },
                "CopyUp" => StorageOperation::CopyUp {
                    source: umbra_core::ApprovedBaseObject {
                        object_id: umbra_core::ObjectId(uuid::Uuid::nil()),
                        handle: umbra_core::StorageHandle(vec![1]),
                    },
                    destination: path(b"copied"),
                },
                "Unlink" => StorageOperation::Unlink {
                    path: path(b"note"),
                },
                "RemoveDirectory" => StorageOperation::RemoveDirectory { path: path(b"dir") },
                "Rename" => StorageOperation::Rename {
                    source: path(b"note"),
                    destination: path(b"moved"),
                    mode: umbra_core::RenameMode::Replace,
                },
                "Link" => StorageOperation::Link {
                    source: path(b"note"),
                    destination: path(b"other"),
                },
                "ReadLink" => StorageOperation::ReadLink {
                    path: path(b"note"),
                },
                "SetMetadata" => StorageOperation::SetMetadata {
                    path: path(b"note"),
                    update: MetadataUpdate {
                        mode: Some(0o600),
                        uid: None,
                        gid: None,
                        accessed_nanos: None,
                        modified_nanos: None,
                    },
                },
                "Truncate" => StorageOperation::Truncate {
                    path: path(b"note"),
                    len: 0,
                },
                "GetXattr" => StorageOperation::GetXattr {
                    path: path(b"note"),
                    name: b"user.x".to_vec(),
                    max_bytes: 8,
                },
                "SetXattr" => StorageOperation::SetXattr {
                    path: path(b"note"),
                    name: b"user.x".to_vec(),
                    value: vec![1],
                },
                "RemoveXattr" => StorageOperation::RemoveXattr {
                    path: path(b"note"),
                    name: b"user.x".to_vec(),
                },
                "GetWhiteout" => StorageOperation::GetWhiteout {
                    path: path(b"note"),
                },
                "SetWhiteout" => StorageOperation::SetWhiteout {
                    path: path(b"note"),
                    present: true,
                },
                "AtomicSwap" => StorageOperation::AtomicSwap {
                    left: path(b"note"),
                    right: path(b"other"),
                },
                other => panic!("no probe for capability row {other}"),
            };
            let error = operations
                .execute(&mut context, &request(operation, Some(4)))
                .unwrap_err();
            assert_eq!(error.kind, expected, "row {} drifted", row.operation);
        }
    }

    #[test]
    fn a_cross_anchor_rename_is_rejected_before_any_dispatcher_is_consulted() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"x".to_vec());
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));
        let error = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::Rename {
                        source: path(b"note"),
                        destination: StoragePath::new(StorageAnchor::Control, b"note")
                            .expect("path"),
                        mode: umbra_core::RenameMode::Replace,
                    },
                    Some(4),
                ),
            )
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidPath);
        assert!(error.context.contains("cross-anchor"));
    }

    #[test]
    fn a_removal_must_match_the_kind_of_object_it_names() {
        let (mut fake, layout) = fixture::server();
        fake.insert_directory(&layout.root, b"dir");
        fake.insert_file(&layout.root, b"file", b"x".to_vec());
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut owners = fixture::owners();
        let mut context = fixture::ops_context(&mut fake, &mut replay, Some(&mut owners));

        // The type check happens before the namespace seam, so these report the
        // caller's error rather than the contracts gap.
        let error = operations
            .execute(
                &mut context,
                &request(StorageOperation::Unlink { path: path(b"dir") }, Some(4)),
            )
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidInput);
        let error = operations
            .execute(
                &mut context,
                &request(
                    StorageOperation::RemoveDirectory {
                        path: path(b"file"),
                    },
                    Some(4),
                ),
            )
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn the_two_anchors_are_disjoint_namespaces_under_one_run() {
        let (mut fake, layout) = fixture::server();
        // The same byte name under each anchor names two different objects.
        fake.insert_file(&layout.root, b"same", b"tracee".to_vec());
        fake.insert_file(&layout.control, b"same", b"control".to_vec());
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut context = fixture::ops_context(&mut fake, &mut replay, None);

        let read = |context: &mut OpsContext<'_>, anchor| {
            let response = operations
                .execute(
                    context,
                    &request(
                        StorageOperation::ReadAt {
                            path: StoragePath::new(anchor, b"same").expect("path"),
                            offset: 0,
                            len: 16,
                        },
                        None,
                    ),
                )
                .expect("read");
            match response {
                StorageResponse::ReadAt(bytes) => bytes,
                other => panic!("a read answers with bytes, not {other:?}"),
            }
        };
        assert_eq!(read(&mut context, StorageAnchor::Root), b"tracee");
        assert_eq!(read(&mut context, StorageAnchor::Control), b"control");
        assert_ne!(
            operations.anchor(StorageAnchor::Root).pin().identity(),
            operations.anchor(StorageAnchor::Control).pin().identity()
        );
    }

    #[test]
    fn the_run_anchor_is_listed_with_provider_state_hidden() {
        let (mut fake, layout) = fixture::server();
        let operations = fixture::operations(&mut fake);
        let mut replay = fixture::replay();
        let mut context = fixture::ops_context(&mut fake, &mut replay, None);
        let run = crate::identity::PinnedObject::pin(
            context.transport,
            layout.run,
            Deadline { millis: 5_000 },
        )
        .expect("pin");
        let listed = operations.list(&mut context, &run, None, 16).expect("page");
        let names: Vec<_> = listed
            .entries
            .iter()
            .map(|entry| entry.name.as_bytes().to_vec())
            .collect();
        assert!(names.contains(&b"root".to_vec()));
        assert!(!names.contains(&crate::layout::PRIVATE_DIR.to_vec()));
    }
}
