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
- `FsOp::Access` is a read-through probe of the merged namespace: it rewrites to
  the shadow object when one exists and to the base otherwise, and it never
  copies up, not even for a `W_OK` probe. Nothing is journaled, because a probe
  reports current permissions rather than authorizing a later mutation. The
  resolver now separates the two absence shapes at its own boundary, and one
  ownership limit remains invisible from its answer alone:
  - A **truly-absent** target resolves to `NotFound`. The supervisor turns a
    non-mutating `NotFound` into a plain resume, so the tracee's own unrewritten
    syscall runs against the host. This is correct because the base layer *is*
    the host filesystem, so the kernel's own `ENOENT` equals the namespace's
    answer. `FsOp::Stat`, a read-only `FsOp::Open` and `FsOp::ReadLink` share
    this branch.
  - A **whiteout-hidden** target — a base object a `.wh` marker deletes, whether
    at the final component or at any ancestor directory — resolves to
    `Deny(Errno::ENOENT)`. The supervisor emulates that denial: the backend steps
    the tracee past the trapped syscall, so the tracee observes `ENOENT` and the
    still-present base object is never revealed. This closes
    [#49](https://github.com/invakid404/umbra/issues/49). A backend whose
    `emulate_result` cannot skip the trap — the Linux stub — fails closed here
    rather than resuming into a host-visible read.
  - `W_OK` against a base-only object is answered from the **base** file's
    ownership, because that is the path the probe rewrites to. Copy-up carries
    `mode` but not uid/gid, so a base file owned by another user can fail
    `access(W_OK)` and still be writable through a later open, which copies it
    into a shadow object this run owns. The probe and the write disagree in that
    case.
- `FsOp::Fchownat` resolves to a shadow rewrite, then copies the target up after
  a flushed `JournalIntent::Chown` Prepare, so the kernel applies the ownership
  to the shadow object and never to the immutable base. Two shapes are refused at
  `resolve`, before any journal record exists, rather than half-performed:
  - A **base-only** target with an unchanged-ID sentinel in either position.
    Copy-up recreates the object through `CreateOptions`, which carries `mode`
    but not uid/gid, so the shadow belongs to whoever runs umbra; the kernel then
    sets only the IDs the tracee supplied, and the ID it asked to leave alone
    would silently become ours. The sentinel means "unchanged", and this path
    cannot honour it. Setting **both** IDs is allowed, because the copy
    contributes nothing to the result, and so is a sentinel against an object
    already in the shadow, where copy-up is a no-op. Lifting the restriction
    needs ownership-preserving copy-up, which needs `SetMetadata` in the storage
    backend and, for a base object owned by another user, privilege umbra does
    not have.
  - A **base-only directory**, because recursive directory copy-up is deferred.
    A directory already in the shadow needs no copy-up and chowns normally.

  Refusing at `resolve` keeps the journal and the overlay session clean, but it
  does **not** hand the tracee an errno: it ends the run. `Fchownat` is
  `Materialise`, so `mutation` is true, and the non-mutating outcomes above have
  no equivalent on the mutation path — every `resolve` error propagates and marks
  the run recovery-required, and the `!mutation` guard on the whiteout denial is
  what keeps it off this path. That covers both refusals above plus an absent
  target, and it is sharper than a non-mutating probe, which either resumes as
  host passthrough (truly absent) or is answered with a clean `ENOENT`
  (whiteout-hidden). The shapes ordinary
  tooling reaches are not exotic: `chown -R` over a base tree meets the directory
  refusal on its first directory, and `tar -x`, `cp -p`, `install -o` and `rsync`
  routinely issue `chown(-1, gid)` or `chown(uid, -1)`, which is exactly the
  sentinel shape. This is not a regression — before these operations were typed,
  every `fchownat` ended the run at decode — but it is the current ceiling on how
  useful mediated `fchownat` is, and lifting it needs ownership-preserving
  copy-up.

  The journaled `JournalIntent::Chown` carries the logical path and a `copy_up`
  flag, not just an object id: `object` is read before copy-up, so for a base-only
  target it names the pre-materialisation object. A copied-up file gets a new
  identity, so `object` no longer locates what the kernel chowned; a copied-up
  logical symlink keeps the base identity, so it still does. The path is what
  stays resolvable in both cases, for the same reason `CopyUp` carries one.

  A chown the **kernel** rejects is reconciled, not fatal: the `Abort` is
  journaled and `abort` returns `Ok`, so the supervisor resumes the tracee with
  the errno already in its return register
  ([#53](https://github.com/invakid404/umbra/issues/53)). That is the shared
  `Materialise` contract, not something specific to ownership, but `fchownat` is
  the first operation to route a routinely-failing syscall into it — an
  unprivileged tracee chowning to another uid gets `EPERM`, which most programs
  shrug off and which used to end the whole run. `FsOp::Chmod`, `FsOp::Fchmod`
  and `FsOp::Link` remain unimplemented at `resolve`; ownership is the only
  metadata mutation wired through today.
- Rename materializes a regular-file or logical-symlink source, creates shadow destination parents,
  and returns source/destination kernel rewrites for one native shadow rename.
  On observed success the source whiteout is set and destination whiteout cleared.
  A same-path rename is a validated no-op. The local backend does not implement
  `StorageOperation::Rename`; the test caller executes the prepared native rename.
  Kernel rewrites require trusted, qualified runtime bindings and caller execution.

`NamespaceSession` typed reads, readlink, stat, and directory pages also have provider IPC
request/response variants. `set_readlink_buffer` is available through IPC as well, and so are
the three run-lifecycle calls: `RenewWriter`, `FinishRun` and `FailRun`. Provider factories bind the engine and install any ABI encoder
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
An aborted mutation requires recovery and leaves the session stopped, with one
structurally distinct exception: `AbortReason::KernelRefused(errno)`. That reason
says the rewritten syscall reached the kernel and the kernel refused it, so the
effects are exactly the ones `prepare` journaled, the verdict is the one
`observe_result` journaled, and the tracee is owed the errno. Such an abort still
writes its `Abort` record and still rolls nothing back, but it returns `Ok` and
leaves the session usable, which is what lets an ordinary `EPERM`/`ENOSPC` reach
the tracee instead of ending the run
([#53](https://github.com/invakid404/umbra/issues/53)). The reason is a claim by
the caller, so it is honoured only when `Pending.outcome` — the failure this
session itself observed — carries the same errno; an unobserved or mismatched
claim is an interception inconsistency and takes the poison path. `Cancelled`,
`Failed` and `RecoveryRequired` are unchanged: they mean the interception broke
down, and neither mode widens to cover the other.

Reconciling does not undo what `prepare` already did, so it is confined to the
plans whose preparation created nothing. `Pending.created` records the
difference. A reconciled creating open would publish a path the tracee was just
told its open failed to make, and because the shadow object outranks even a stale
whiteout marker that `commit` would have cleared, every later `O_CREAT|O_EXCL` on
it would answer `AlreadyExists` permanently. Such a transaction therefore keeps
the pre-#53 poison behaviour: the run ends, loudly, instead of the namespace
diverging. The same applies to `Symlink`, `Mkdir` and a cross-path `rename` that
had to materialise destination parents. Lifting that — so a refused creating
operation is retryable rather than fatal — needs rollback of prepare-time
creations, which needs the reconciliation this MVP does not implement, and is
tracked in [#55](https://github.com/invakid404/umbra/issues/55).

Copy-up is deliberately not counted as a creation, so a refused `fchownat`, or a
refused write open on an object the base already holds, reconciles. The content
view is unchanged, and counting it would poison the run on the first write into
any not-yet-shadowed base subdirectory — a large share of the ordinary `EPERM`
cases this contract exists to survive. That is **not** a claim that copy-up is
invisible: it materialises shadow ancestors, and those are new objects in the
shadow whether or not the operation that needed them was allowed to stand. What
they are no longer is *wrong*: `parents` used to give every one of them a
hardcoded `0o755` rather than the base directory's mode, so a base directory at
`0700` was reported as `0755` afterwards, including after a syscall the tracee
was told had failed. That was a fidelity defect in `parents` rather than a
property of this contract, and it is fixed
([#56](https://github.com/invakid404/umbra/issues/56)): a shadow ancestor now
carries the **group and other bits of the base directory it shadows, exactly,
with its owner bits widened to at least `rwx`** — on the success path and the
reconciled-abort path alike.

The widening is not a rounding error, it is the point. umbra *owns* the shadow,
so POSIX judges umbra's own writes by the shadow's **owner** bits, and the engine
must be able to create inside every ancestor it materialises. A base directory
without owner-write is the ordinary case rather than an exotic one — read-only
artifact trees are exactly what an overlay exists to make writable — and copying
`0o555` verbatim yields an ancestor the next create cannot enter. Since `parents`
runs inside `prepare`, which poisons the run on any error, that would kill the
session for an operation POSIX itself permits: writing `d/f` needs write on the
file, not on `d`. Forcing the owner bits never widens group or other beyond what
the base granted, so against a hardcoded `0o755` this is strictly more faithful
there: a base at `0o500` yields `0o700` rather than leaking `g+rx,o+rx` the base
never gave. It is *less* restrictive in one direction only — a base at `0o077`
yields `0o777`, faithful to that base's own `o+rwx`.

Mode is not ownership, and this is a mode-only fix: `CreateOptions` carries no
owner field, so the shadow still belongs to whoever runs umbra.

Four ancestors keep the default `0o755` instead: a Control-anchored one — a
symlink blob, a whiteout marker — which shadows nothing; one the base does not
hold, or holds as something other than a directory; one whose logical path is
whiteouted **at any level**, since a whiteouted directory is logically deleted
and what replaces it is a different directory; and one whose base stat *fails*,
which is a live base directory whose bits are merely not legible, not an absent
one. The whiteout check scans every prefix, not just the ancestor being
materialised: a shadow directory and a whiteout marker for the same path coexist
by design — that is what `mkdir` keeps one for, as an opaque-base marker — and
`parents` skips ancestors already in the shadow, so a marker above the first
materialised ancestor would otherwise go unseen.

Note separately that `created` is shadow-shaped: `parents` decides it from the
shadow only, so a cross-path `rename` onto a destination parent that exists in
the base but has not been copied up yet is counted as a creation and poisons.
That is fail-closed and under-delivers for `rename`. #56 weakened one of the two
arguments for leaving it that way — a materialised ancestor no longer differs
from the base directory it shadows in its *group and other* bits — but only that
far: the owner bits are deliberately widened, mode is not ownership, and since
`CreateOptions` carries no owner field the shadow still belongs to whoever runs
umbra rather than to the base directory's owner. And it
does not touch the other argument at all: narrowing the predicate needs the
creation rollback [#55](https://github.com/invakid404/umbra/issues/55) tracks and
this MVP does not implement, so the over-approximation stays until that lands.

Commit-time effects are never applied by `abort`: `pending.whiteouts` and
`retired_index` are consumed in `commit` alone, so a reconciled `rename` sets no
whiteout and retires no symlink index. That is a property of this engine rather
than an end-to-end guarantee, because most of the class cannot reach a kernel
refusal at all yet. `FsOp::Unlink` — the whole `Whiteout` half — and also
`FsOp::Symlink`, `FsOp::Mkdir` and a same-path `rename` resolve to `Emulate`, and
`observe_result` refuses an outcome that differs from the emulated one; a
cross-path `rename` resolves to a two-path `Rewrite` that, as `apply_rewrite`
stands in `umbra-supervisor` today, is refused before the tracee ever reaches a
syscall exit — a fact owned by that crate, not this one. The reachable surface
today is a write-mode `Open` and `FsOp::Fchownat`. The behaviour is keyed on the
dispatch class, not on a variant list, so the rest of the class inherits it as
those paths are wired — with two caveats to settle first. `Unlink`'s `prepare`
*destroys* the shadow object outright and sets no `created`, so a `Whiteout`-class
refusal reaching `abort` would reconcile after discarding shadow-only data: the
mirror image of the creation case, and a real gap in the gate rather than a
question of wiring. `FsOp::Link` is a different shape — `resolve` refuses it as
beyond-MVP, so `prepare` is never entered for it and the `_ => {}` arm it would
fall to materialises nothing; when it *is* wired it will need an arm of its own
doing `copy_up` of the source and `parents` of the destination, exactly as
`rename` does, and that is where setting `created` is easy to forget. Whoever
wires either path has to revisit the gate rather than assume the inheritance;
noted on [#57](https://github.com/invakid404/umbra/issues/57).

Checkpoint flushes storage and journal, takes whiteouts from authoritative control
markers, and publishes a logical checkpoint with `clean: false`. Only the
supervisor can establish the broader clean-handoff conditions.

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
through them. Readlink, no-follow stat and a no-follow `FsOp::Access` never
expose the placeholder as a file: the probe is answered from the `0o777` the
overlay reports for every logical symlink, not from the placeholder's `0444`.
That is a resolver-layer property. Each of those answers is a
`ResolvedAction::Emulate`, and a caller that cannot execute emulated results —
the macOS supervisor among them — ends the run instead of answering, so it is
not yet an end-to-end behaviour. For the no-follow `FsOp::Access` case this is
no worse than before the operation was typed, when the same call was refused at
decode.

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
shadow/stat precedence, create parents, the mode a materialised shadow ancestor
takes from its base counterpart — including a read-only base directory, whose
owner bits must be widened or the run would be poisoned — whiteouts and
recreation, merged snapshot pages,
native-encoder continuation, rename journal grouping, containment, non-UTF-8
bytes, logical symlink creation/readlink/traversal, absolute and relative targets,
loop bounds, symlink escape and unchecked physical-link rejection, rename identity,
base symlink copy-up, journal failures, transaction ordering and checkpoints.
Mode assertions are made against the mode the engine *requests*, through a
recording `Storage` wrapper, because `mkdir(2)` applies the process umask to what
lands on disk; the on-disk checks alongside them account for the measured umask.
APFS configurations that reject non-UTF-8 filenames still run byte resolver and
marker checks; actual raw-name filesystem I/O is conditional on native support.

## Run lifecycle

`NamespaceSession` adds three lifecycle methods, each defaulting to a refusal so a
provider that does not implement them cannot be handed a run. Each has a provider
IPC request/response pair, so `Proxy` forwards them instead of inheriting the
refusal, and `serve_provider` dispatches them to the backend. A forwarded
`finish_run` is checked before it is believed: the receipt and the storage flush
receipt inside it must both name the run that was asked about, or the proxy
returns a protocol error rather than accepting another run's durability evidence.
`serve_provider`
still advertises an empty capability set, so nothing in this repo offers
`umbra_core::capabilities::NAMESPACE_RUN_LIFECYCLE_V1`. That name is the
forward-looking gate, not today's guard: the supervisor refuses every
namespace-role registry before anything is opened, because it still binds this
crate's `standard_namespace` and never opens a namespace descriptor.

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

`umbra_supervisor` is the in-process owner that supplies all of the above. The
protocol now carries the lifecycle calls, but `umbra_supervisor` still binds
`standard_namespace` and refuses a namespace-role registry, so an alternative
namespace provider cannot yet own a run.
