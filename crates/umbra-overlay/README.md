# umbra-overlay

Storage-independent MVP overlay. **Runtime dependencies remain `umbra-core`,
`umbra-storage`, `umbra-journal`, and `serde` only.** `umbra-storage-local`,
`tempfile`, and `uuid` are dev-dependencies for real-filesystem unit tests. There
is no NFS dependency and no host filesystem I/O in the library.

`standard_namespace(storage, journal)` returns `Box<dyn NamespaceSession + Send>`;
`Overlay::new(Box<dyn Storage>, Box<dyn Journal>)` exposes the same engine directly.
Construction does no I/O. The owner must call `NamespaceSession::bind` with:

- `SessionConfig`: the opened shadow run's `RunBinding`, fenced `RequestContext`
  (with a unique session idempotency prefix), the journal's `RecoveryState`, and
  the `WriterLease` those sessions mutate under. The lease lives here so that one
  owner renews, releases and mutates through the same storage session, rather than
  a second mutable session being opened just to hold authority.
- An approved, immutable `Box<dyn Base>`. `StorageBase` adapts another opened
  `Storage` run to this read-only contract. The owner freezes that base for the
  session and validates its fingerprint; the overlay never opens arbitrary host
  paths or chooses a base/storage implementation.

The owner opens the injected Journal against `binding.control` with the same run
and writer epoch. Binding validates matching identities and accepts only an empty,
intact journal with no checkpoint or pending transactions. Nonempty recovery is
explicitly unsupported until reconciliation is implemented. The supervisor's
`run` composition supplies this initialization. Unbound engines return `InvalidState`.

## MVP behavior

`resolve` performs lookups and plans actions without mutating storage or journal.
Only the most recently resolved plan can be prepared. Calls are serialized through
`resolve -> prepare -> observe_result -> commit` (or `abort`); logical reads and
new resolutions are blocked while a transaction is pending.

- Read/stat chooses the shadow first, rejects whiteouted base paths, and otherwise
  uses the approved immutable base. Path operations return validated runtime
  rewrites. `NamespaceSession::read_at` provides typed bounded reads; it propagates
  backend errors and falls through only on `NotFound`.
- `list` merges base and shadow into a byte-sorted snapshot, hides whiteouts and
  duplicate base names, and returns repeatable session/directory-bound cursors.
  Existing pages retain their snapshot across later mutations. Snapshot storage is
  in memory and retained for the session; bounded snapshot eviction is future work.
- `FsOp::ReadDir` validates its tracked directory descriptor and uses an injected
  `DirectoryEncoder` to translate merged entries into native records and writes to
  the stopped syscall's buffer. Install it with `set_directory_encoder`. The core
  `FsOp` has no buffer address or native directory layout, so this boundary must be
  supplied by the caller's ABI adapter. The overlay checks output bounds and whole
  entry progress, retains a snapshot per task/exec-generation/fd/object, and
  advances only after observed success and commit. Without an encoder, callers
  can use typed `list`; native ReadDir fails with `UnsupportedCapability`.
- Create/mkdir creates shadow parents through `Storage::create`. Exclusive create
  checks the merged namespace; the prepared open clears create/exclusive flags
  because creation has already happened through Storage.
- Writable opens copy existing base regular files through bounded reads/writes,
  including short I/O, then rewrite the open to shadow. Existing shadow bytes win.
  Truncation is performed by the prepared shadow open. Copy-up preserves file
  bytes and requests the base permission bits; exact metadata preservation across
  umask, owner/group, timestamps, ACLs and xattrs needs richer backend support.
- Unlink removes an existing shadow file through Storage and prepares a base
  whiteout, including for base-only files. Recreation clears its exact whiteout.
- Rename materializes a regular-file or logical-symlink source, creates shadow destination parents,
  and returns source/destination kernel rewrites for one native shadow rename.
  On observed success the source whiteout is set and destination whiteout cleared.
  A same-path rename is a validated no-op. The local backend does not implement
  `StorageOperation::Rename`; the test caller executes the prepared native rename.
  Kernel rewrites require trusted, qualified runtime bindings and caller execution.

`NamespaceSession` typed reads, readlink, stat, and directory pages also have provider IPC
request/response variants. `set_readlink_buffer` is available through IPC as well. Provider factories bind the engine and install any ABI encoder
before entering `serve_provider`; trait objects are not serialized over IPC.

## Journal and whiteouts

Each mutation appends and flushes a typed Prepare intent before effects, then an
ObservedResult, then a Commit. Record IDs and epochs are explicit; each storage
primitive gets a distinct idempotency key. Reusing a completed operation ID is
rejected. Physical mount paths never enter journal payloads.

Whiteout marker files are stored through injected Storage at
`<run_root>/control/whiteouts/`. Each logical byte component becomes `c/<hex>`
(with long hex encodings split into 120-byte chunks), followed by a terminal `.wh`.
This preserves arbitrary bytes, distinguishes ancestors from exact markers, and
keeps control entries outside tracee directory listings. An ancestor whiteout
suppresses base descendants; actual shadow entries still take precedence.

A rename's single journal Rename intent describes both source/destination changes;
its Commit is the logical publication boundary. Marker writes and native rename
are **not one filesystem transaction**. The engine blocks observers during these
steps, flushes storage before Commit, and poisons the session on an ambiguous
failure. Crash-atomic recovery requires replay/reconciliation of the intent and
both markers. This MVP refuses reopening nonempty journals rather than exposing
partially reconciled state.

The injected Journal owns persistence. The shipped `umbra-journal-file` backend
implements append and fsync-backed flush; the record payload carries
`JournalPayload::Commit`. This engine calls those methods and propagates failures.
Unit tests inject an in-memory Journal to model ordering and failure boundaries;
restart recovery still requires reconciliation that this engine does not implement.

Abort never claims that copy-up, creation, unlink, or a kernel mutation was undone.
An aborted mutation requires recovery and leaves the session stopped. Checkpoint
flushes storage and journal, takes whiteouts from authoritative control markers,
and publishes a logical checkpoint with `clean: false`. Only the supervisor can
establish the broader clean-handoff conditions.

## Byte resolution

Resolution walks raw byte components from `ProcessContext::cwd`, logical root, or
tracked dirfd logical anchors. It checks directory prefixes, validates dirfd object
identity, ignores dirfd for absolute paths, preserves non-UTF-8 names, and rejects
`..` above the logical root. It does not use UTF-8 conversion or `canonicalize` to
establish containment. Runtime path composition happens only after anchored
storage/base validation.

## Logical symlinks

Symlinks use the component-wise logical resolver from handoff §4.3. Creation is
emulated through injected Storage after a flushed `JournalIntent::Symlink` Prepare;
ObservedResult and Commit follow the existing transaction protocol. Shadow entries
are empty regular files requested with mode `0444`. They contain no physical
symlink, so unchecked kernel traversal cannot follow their target or descend
through them. Readlink and no-follow stat never expose the placeholder as a file.

Control metadata lives outside tracee listings:

- `control/symlinks/targets/<logical-object-uuid>` contains the exact raw target
  bytes, without UTF-8 conversion, escaping, or a NUL terminator. New symlinks use
  their journal operation UUID as logical object identity.
- `control/symlinks/objects/<backend-object-uuid>` contains the logical UUID as
  ASCII. This index identifies placeholders across rename and exposes the same
  logical identity in stat and merged directory entries. Unlink and successful
  replacement remove the retired index so backend inode reuse cannot inherit a
  deleted link. Immutable target records remain for future journal reconciliation.

The iterative resolver expands at most **40 symlinks per lookup**, including cwd
or dirfd anchor expansion, and returns structured `SymlinkLoop` on overflow —
a distinct `ErrorKind`, so a platform answers a link loop with its own native
errno without reading an error message, and containment failures stay
`InvalidPath`.
Absolute targets restart at `ProcessContext::root`; relative targets start at the
link's containing logical directory. Expansion precedes `..` processing, and a
parent above the logical root returns the existing containment error before any
physical operation is prepared. Non-UTF-8 bytes, repeated separators, and dots are
preserved in stored targets. Typed reads and directory pages use the same resolver
from the storage logical root; native calls honor their process root and dirfd.

Open and following stat resolve the final link; readlink, unlink, rename operands,
and symlink creation preserve the final object. No-follow open rejects a final
link; exclusive create detects even dangling links. Renaming a base logical link
copies its target and identity into a safe shadow placeholder. `Base::read_link`
provides logical target access; `StorageBase` understands this control format and
can also use a backend's typed logical ReadLink operation. Base/storage contracts
still prohibit unchecked physical symlink traversal.

`NamespaceSession::read_link` returns the complete logical `BytePath`.
`FsOp::ReadLink` covers both readlink and readlinkat: bind the next stopped syscall's
buffer with `set_readlink_buffer(address, len)`. Resolution consumes the binding,
returns up to `len` target bytes without a NUL, and reports the copied byte count.
The core FsOp has no output address, so a native call without a binding fails with
`UnsupportedCapability`. Typed `stat(path, follow)` exposes logical symlink kind,
identity, and target length. Native no-follow stat of a link requires an injected
`StatEncoder` via `set_stat_encoder`, analogous to the directory ABI boundary;
without it the operation fails explicitly. Provider factories install ABI encoders.

Physical rewrites assume a trusted immutable base and serialized namespace;
race-proof native dirfd execution still needs platform qualification. The existing
refusal to reopen nonempty journals also applies to symlink transactions.

Directory subtree rename/rmdir, hard links, metadata mutations, pathless native
read/write emulation, descriptor duplication/close/reuse bookkeeping, renamed or
unlinked directory handles, and restart recovery fail explicitly or require future
caller integration. Directory streams must not reuse a tracked fd/object identity
within one exec generation; descriptor lifecycle integration will remove that
restriction. This is an executable regular-file MVP, not complete POSIX coverage.

## Validation

Run `cargo test -p umbra-overlay`. Tests inject real `umbra-storage-local` base and
shadow runs, with no NFS or fixture environment variables. They exercise copy-up,
shadow/stat precedence, create parents, whiteouts and recreation, merged snapshot
pages, native-encoder continuation, rename journal grouping, containment, non-UTF-8
bytes, logical symlink creation/readlink/traversal, absolute and relative targets,
loop bounds, symlink escape and unchecked physical-link rejection, rename identity,
base symlink copy-up, journal failures, transaction ordering and checkpoints.
APFS configurations that reject non-UTF-8 filenames still run byte resolver and
marker checks; actual raw-name filesystem I/O is conditional on native support.

## Run lifecycle

`NamespaceSession` adds three lifecycle methods, each defaulting to a refusal so a
provider that does not implement them cannot be handed a run:

- `renew_writer` renews the injected lease through this session's own storage. A
  refused or epoch-advanced renewal is `LeaseLost`: authority is gone or unproven,
  and the caller must stop resuming tracees rather than retry into a mutation.
  Renewal is permitted while a transaction is pending. A failed renewal latches
  the session poisoned, and an already-poisoned session refuses renewal.
- `finish_run` durably completes a fresh command run: flush run data, append and
  flush a `RunCompleted` record, close the journal, release the writer, close
  storage. The storage flush receipt must match the run and writer epoch and
  report non-None durability before completion is recorded. Once completion is
  durable, the session is terminal: each cleanup
  stage is attempted once, errors retain stage context, and bindings are cleared.
  A subsequent `fail_run` does not repeat those closed stages. Failures before
  durable completion retain the binding for `fail_run` cleanup. A receipt is
  returned only when every step succeeded; it authorizes no takeover.
- `fail_run` leaves the run explicitly failed. It writes no completion record and
  publishes no checkpoint, and it releases the writer lease only when the request
  carries evidence that the supervised tree is gone. Without that evidence the
  writer marker is retained and the session is poisoned, so a later writer cannot
  take over a run whose effects are uncertain.

Each storage request derives its own `OperationId` from the pending transaction's
identity (or the session's) plus a serial. One logical transaction issues several
storage requests — a copy-up, its parent directories, the create itself — and a
backend may bind an operation ID to the single idempotency key it was first used
with, refusing a second, different request under it. Derivation is deterministic,
so requests stay attributable to the operation the journal recorded.

`umbra_supervisor` is the in-process owner that supplies all of the above; the
namespace provider protocol carries no lifecycle calls, so an alternative
namespace provider cannot yet own a run.
