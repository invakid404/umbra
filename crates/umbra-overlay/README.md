# umbra-overlay

Storage-independent MVP overlay. **Runtime dependencies remain `umbra-core`,
`umbra-storage`, `umbra-journal`, and `serde` only.** `umbra-storage-local`,
`tempfile`, and `uuid` are dev-dependencies for real-filesystem unit tests. There
is no NFS dependency and no host filesystem I/O in the library.

`standard_namespace(storage, journal)` returns `Box<dyn NamespaceSession + Send>`;
`Overlay::new(Box<dyn Storage>, Box<dyn Journal>)` exposes the same engine directly.
Construction does no I/O. The owner must call `NamespaceSession::bind` with:

- `SessionConfig`: the opened shadow run's `RunBinding`, fenced `RequestContext`
  (with a unique session idempotency prefix), and the journal's `RecoveryState`.
- An approved, immutable `Box<dyn Base>`. `StorageBase` adapts another opened
  `Storage` run to this read-only contract. The owner freezes that base for the
  session and validates its fingerprint; the overlay never opens arbitrary host
  paths or chooses a base/storage implementation.

The owner opens the injected Journal against `binding.control` with the same run
and writer epoch. Binding validates matching identities and accepts only an empty,
intact journal with no checkpoint or pending transactions. Nonempty recovery is
explicitly unsupported until reconciliation is implemented. The existing
supervisor constructor does not yet supply this extra initialization; deployment
wiring is outside this track. Unbound engines return `InvalidState`.

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
- Rename materializes a regular-file source, creates shadow destination parents,
  and returns source/destination kernel rewrites for one native shadow rename.
  On observed success the source whiteout is set and destination whiteout cleared.
  A same-path rename is a validated no-op. The local backend does not implement
  `StorageOperation::Rename`; the test caller executes the prepared native rename.
  Kernel rewrites require trusted, qualified runtime bindings and caller execution.

`NamespaceSession` typed reads and directory pages also have provider IPC request/
response variants. Provider factories bind the engine and install any ABI encoder
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

**Persistence assumption / TODO:** `umbra-journal-file` still returns
`NotImplemented` for all operations. The actual Journal interface has
`append(JournalPayload::Commit)` and `flush`, not a `journal.commit` method. This
engine calls those real methods and propagates failures; it does not replace them
with a successful stub. Tests inject an in-memory Journal that models record
ordering and failure boundaries, with no production durability claim. A usable
persistent session awaits file-journal persistence and recovery reconciliation.

Abort never claims that copy-up, creation, unlink, or a kernel mutation was undone.
An aborted mutation requires recovery and leaves the session stopped. Checkpoint
flushes storage and journal, takes whiteouts from authoritative control markers,
and publishes a logical checkpoint with `clean: false`. Only the supervisor can
establish the broader clean-handoff conditions.

## Byte resolution and M1.5 deferrals

Resolution walks raw byte components from `ProcessContext::cwd`, logical root, or
tracked dirfd logical anchors. It checks directory prefixes, validates dirfd object
identity, ignores dirfd for absolute paths, preserves non-UTF-8 names, and rejects
`..` above the logical root. It does not use UTF-8 conversion or `canonicalize` to
establish containment. Runtime path composition happens only after anchored
storage/base validation.

Full first-class symlinks from handoff §4.3 are deferred to M1.5. Both base and
storage must reject symlink traversal; the overlay rejects logical symlink objects,
including a link before `..`. Logical target storage, readlink, bounded expansion,
and absolute-link handling are not implemented. Physical rewrites assume a trusted
immutable base and serialized namespace; race-proof native dirfd execution still
needs platform qualification.

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
bytes, symlink rejection, journal failures, transaction ordering and checkpoints.
APFS configurations that reject non-UTF-8 filenames still run byte resolver and
marker checks; actual raw-name filesystem I/O is conditional on native support.
