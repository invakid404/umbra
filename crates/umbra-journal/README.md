# umbra-journal

Pure synchronous journal contract, depending only on `umbra-core`. Shared request,
record, recovery and checkpoint types are owned by core. This crate contains no
file I/O, CRC implementation, storage backend, lease service or provider transport.
The workspace uses edition 2021 and pins Rust 1.93.0 for build validation.

## Public contract

```rust
use umbra_core::{
    Checkpoint, CheckpointId, DurableSequence, JournalOpenRequest, JournalRecord,
    RecoveryState, Result, Sequence,
};

pub trait Journal: Send {
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState>;
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence>;
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence>;
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>>;
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId>;
    fn close(&mut self) -> Result<()>;
}
```

Construct an implementation outside the trait and inject it as `Box<dyn Journal>`.
One owner orders calls. An iterator borrows the session; dropping it must release
its resources. Worker queues and replay buffers must be bounded.

`open` validates format compatibility, run identity, read-only/writer intent and
injected writer authority before writes. It returns recovery state, including
unfinished operations. `append` assigns increasing sequences but does not promise
persistence. Records carry version, sequence, operation ID, epoch and typed prepare,
observed-result, commit, abort or lifecycle payloads. File backends add bounded
length framing and CRC32 checksums. `flush(through)` returns a durability receipt
only after that sequence reaches the backing store's promised persistence boundary.
It does not flush tracee-owned files. `close` surfaces pending flush failures.

`replay(after)` yields only validated records strictly after the cursor, in sequence
order, with errors delivered either at iterator creation or during iteration. It
must not silently truncate interior corruption or a complete bad-checksum frame.
A demonstrably torn final frame can be excluded and reported during recovery;
physical repair requires current writer authority. Read-only inspection never
repairs the log. Prepared operations without a commit remain subject to explicit
reconciliation using stable IDs and idempotent storage operations. An abort cannot
promise to undo a kernel write that already happened.

`write_checkpoint` requires an already flushed last committed sequence and durably
publishes both an immutable snapshot and its reference, in that order. Checkpoints
contain logical state and version/base/toolchain fingerprints, independent of mount
roots and process memory. Recovery selects the last valid checkpoint and validated
subsequent frames; it must not select a partially published checkpoint.

## Recovery, leases and clean handoff

The supervisor supplies the runtime control-directory binding obtained from storage.
The journal uses `control/journal` and checkpoint files on that selected backing
store; the tracee namespace `root/` must never expose `control/`. Persistent records
retain logical byte paths and stable identity, including fencing epochs, rather than
machine-specific mount roots.

One writer owns each run. Lease evidence identifies the run, writer instance, host
fingerprint, supervisor version, acquisition/renewal times and epoch. The journal
uses supplied authority and never acquires a competing lease. Storage owns atomic
acquisition/renewal/release and must qualify those operations on its actual service.
An expired lease alone does not justify takeover: fencing must disable the old
writer, including existing writable kernel handles, or prior termination and
quiescence must be confirmed. Otherwise refuse writable recovery. Stale authority
must fail journal mutation, tail repair and checkpoint/manifest publication alike.
Lease loss makes the supervisor block mutations and quiesce the tracee tree.

For clean handoff, the supervisor must reject new children and mutations, stop all
threads at known boundaries, request orderly agent exit and handle remaining
descendants, then finish or reconcile prepared transactions. Flush tracee files,
backing storage, journal and manifest before publishing the checkpoint and clean
marker. Only then release the fenced lease. Journal success alone cannot establish
clean handoff. Resume validates base/toolchain/agent/supervisor compatibility,
obtains safe exclusive authority, reconstructs logical state at the new runtime
mount root, and launches a new agent process; there is no live process migration.

## Adding a new journal backend

1. Create `crates/umbra-journal-example`, add its path to the root workspace member
   list, and create a manifest with inherited metadata:

   ```toml
   [package]
   name = "umbra-journal-example"
   version.workspace = true
   authors.workspace = true
   license.workspace = true
   edition.workspace = true
   publish = false

   [dependencies]
   umbra-journal = { path = "../umbra-journal" }
   umbra-core = { path = "../umbra-core" }
   ```

   These are its only direct Umbra dependencies, including dev/build/optional and
   target-specific dependencies. External implementation libraries may be inherited
   from the workspace; keep CRC and native I/O dependencies in the backend.

2. Implement the six signatures above for your concrete session type and provide a
   constructor outside the trait. Track open mode, supplied authority, accepted and
   durable sequences, replay position and checkpoint publication state. Reject
   calls in invalid lifecycle states with structured core errors. Document frame
   encoding, payload limit, version compatibility, sequence assignment, torn-tail
   recovery and duplicate-request policy. Never reopen an incompatible format for
   writing or convert an interrupted append into an assumed successful retry.

3. Accept the injected runtime control binding and writer authority. Open files on
   that backing store directly; do not construct a storage backend. Apply the same
   authority checks to append, flush, recovery repairs and checkpoint publication.
   Store logical byte paths and fencing evidence, excluding physical mount roots.
   Persist immutable snapshot contents before publishing their durable reference.

4. For in-process consumers, construct your type and pass `Box::new(session)` as
   `Box<dyn Journal>`. For runtime integration, supply a provider binary and install
   a descriptor with an open provider ID (for example `example.journal`), journal
   role, protocol version, absolute executable path and qualified capabilities.
   Select that descriptor in runtime role configuration without adding supervisor
   imports, CLI features, factory match arms or a backend enum. Use `provider::serve_provider` and `provider::Proxy` from this crate.
   The protocol supplies exclusive replay cursors with one bounded record per page,
   including iterator errors and early-drop cursor release. Private transport frames
   include versioned handshakes, IDs, explicit errors and deadlines. Provider loss
   never means successful empty replay. Runtime selection can be checked with
   `umbra providers --registry PATH --role journal`.


5. Add backend conformance tests using injected `dyn Journal` sessions. The shared
   backend conformance suite is a buildout obligation, not supplied by this trait
   declaration. Exercise truncated headers/payloads/checksums at the final frame,
   complete checksum failures, interior corruption, incompatible versions,
   prepared-but-uncommitted recovery, interrupted append/flush, duplicate requests,
   checkpoint crashes before/after snapshot and reference publication, and replay
   from the last durable sequence. Also cover read-only repair denial, stale epochs,
   unsafe takeover, sequence monotonicity, bounded replay, early iterator drop and
   close flush failures. Verify shared contract compilation with
   `cargo check -p umbra-journal`, then run `cargo test -p umbra-journal-example`.
   Qualify claimed durability against the actual backing service: local tests do
   not establish NFS locking, fencing or remote persistence.
