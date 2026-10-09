# umbra-storage-nfs-userspace

**Current state: a working userspace NFSv4.0 storage provider — the Umbra-owned
protocol state machine, a raw-RPC transport behind an off-by-default feature, the
storage operations surface, and product admission acquired before any run
binding is published.** This crate holds the private facade contracts for
Umbra's userspace NFSv4.0 backend, the client state machine in `src/state/`, the
operations surface in `src/ops.rs` and the modules beside it, the admission,
journal and outage machine in `src/authority/`, the admission binding in
`src/session.rs`, and a `Storage` implementation that creates and opens runs,
resolves paths, stats, enumerates, reads, writes, creates, renames, removes,
sets metadata and truncates. Semantics this provider will not offer answer
`UnsupportedCapability`; `flush` certifies a run-scoped completeness barrier over
writes it has already committed and returns a receipt at the durability the run
qualified for — a zero-I/O bookkeeping barrier, not a new persistence mechanism,
that re-COMMITs nothing. `Durability::Remote` is claimed only when the boundary
was qualified for that `open_run`; otherwise the receipt is `Durability::Local`.
What `Remote` does and does not assert, and what qualifies it, is stated under
Durability below.

`open_run` acquires admission before it returns a binding, so a denied session
receives an error and nothing to use. Admission is granted only by a release the
previous holder recorded — never by elapsed time — and every takeover policy
other than `Refuse` is refused before the marker is touched.

A default build needs no native toolchain, and its tests contact no server:
they run against the in-memory fake in `src/fake.rs`. Building with
`--features transport-raw` adds `transport::raw::LibnfsRawTransport`, which
speaks NFSv4.0 to a real server; `tests/m1_conformance.rs` and
`tests/golden_compat.rs` then drive the same provider against an isolated
NFS-Ganesha container when `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names one. No
suite in this crate mounts anything.

**Ownership boundary.** This README owns `crates/umbra-storage-nfs-userspace/`:
`src/`, `tests/`, `Cargo.toml`, `build.rs`, `libnfs.pin` and `provider.json`. The
mounted adapter is a separate boundary at
[umbra-storage-nfs](../umbra-storage-nfs/README.md); the contract this crate
implements is [umbra-storage](../umbra-storage/README.md).

## Why a second NFS provider

[umbra-storage-nfs](../umbra-storage-nfs/README.md) needs an NFSv4 mount that
already exists in the OS namespace. This provider speaks NFSv4.0 from user space,
so a host does not need a mount. The mounted adapter stays the qualified fallback
where a mount is already present.

The two are sequential, not concurrent: a run created by one can later be opened
by the other, and one-session-one-Umbra admission remains product-wide.

## Scope

- **Wire profile:** NFSv4.0 over TCP with AUTH_SYS. Nothing else. The NFSv4.1
  session model — `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`, `RECLAIM_COMPLETE`
  — is out of scope and has no variant in `transport::OpCode`, so it is
  unrepresentable rather than merely discouraged.
- **Umbra owns protocol state:** client ids, open and lock owners, stateids,
  seqids, renewal, reconnect, grace, NFSv4.0 `CLAIM_PREVIOUS` reclaim, replay
  buffers and verifier accounting. A transport supplies raw RPC/XDR and task
  primitives only. `LibnfsRawTransport` holds none of that state: stateids and
  open-owner bytes pass through it as opaque owned values inside `Nfs4Op`.
- **Protocol ownership is not writer authority.** A successful `RENEW` proves the
  NFS lease is alive. It never authorises taking over an abandoned session.
  `error::AuthorityError` is a separate domain for exactly that reason.
- **Locking:** the M2 syscall contract (design snapshot `docs/design/syscall-matrix.md`,
  not tracked in this repository) defers `flock` to an M2 gate and admits only
  `fcntl` record locks meanwhile, so the only locks in scope are NFSv4.0
  **advisory** byte-range record locks. `LOCK` and `LOCKU` are frozen in the
  transport facade so the M2 gate does not reopen the seam; no consumer path in
  this crate calls them.

## The five facades

| Facade | File | What it fixes in place |
| --- | --- | --- |
| Transport | `src/transport.rs` | COMPOUND submission, the v4.0 operation set, deadlines, bounded queues, connection epochs, fault-injection points |
| Handle | `src/handle.rs` | Byte-owned filehandles, stable object identity, open-owner sequencing |
| Error | `src/error.rs` | Four disjoint failure domains and retained-error tracking |
| Replay | `src/replay.rs` | Durable intent identity, verifier accounting, bounded buffer, backpressure |
| Fake | `src/fake.rs` | In-memory transport and replay log behind the same traits |

`src/transport/raw/` implements the transport facade against libnfs and is
compiled only under `transport-raw`; see [Raw transport](#raw-transport) below.

`src/storage.rs` holds the provider configuration and the `Storage`
implementation; `src/lib.rs` exports the provider id and the run-layout
constants. `src/state/` and the operations modules consume the facades rather
than adding to them.

## Protocol state

`src/state/` is the client state machine: everything that makes raw RPC an NFSv4.0
*client*. It binds no transport of its own.

| Module | Owns |
| --- | --- |
| `state::seqid` | Owner seqids for the window before an `OpenFile` exists |
| `state::client_id` | `SETCLIENTID` then `SETCLIENTID_CONFIRM`, and the retained client verifier |
| `state::open_owner` | Owner allocation, `OPEN`, `OPEN_CONFIRM`, `CLOSE`, `OPEN_DOWNGRADE` |
| `state::lease` | `RENEW` while idle, and the connection-epoch rule |
| `state::reclaim` | `CLAIM_PREVIOUS` reclaim within a bounded grace |
| `state::verifier` | `EXCLUSIVE4` create verifiers, and WRITE/COMMIT matching |
| `state::retained_errors` | Which failures are settled, and for how long |

`state::retained_errors` means one thing by *settled*: `ErrorClass::Permanent`,
a failure that retrying the identical request cannot change. Only a settled
failure may be retained durably; a transient one — a server asking for a retry,
a lost connection, a changed write verifier, a safe stop whose whole meaning is
that the outcome is unknown — is retained volatile or not at all, and asking for
a durable record of one is refused before anything is written. A deadline is in
that group: `Retirement` proves this pump withdrew the call and says nothing
about whether the server acted on it, so `DeadlineExpired` classifies
`NeedsRecovery`, unlike the pre-dispatch `QueueFull` refusal beside it.

Three rules shape it.

1. **Illegal transitions do not compile.** A `ConfirmedClient` is reachable only
   through a `SETCLIENTID_CONFIRM` the server accepted; an `OwnerLease` is consumed
   by its OPEN attempt; `close` consumes the `OpenFile`; an unconfirmed open's
   `stateid()` keeps refusing. None of these is a runtime guard. Seqid authority
   is not duplicable either: `OwnerSequence`, `OwnerLease` and `LockOwnerLease`
   implement neither `Clone` nor `Copy`, because a copy is a second claim on one
   owner's counter — two copies would issue the same seqid, and poisoning one
   would leave the other usable.
2. **Nothing advances further than the server proved.** A server answer resolves
   the seqid by the RFC 7530 section 9.1.7 rule and leaves the owner reusable. An
   answer that never arrived poisons the owner instead, because the seqid the
   server observed cannot be inferred from silence. The cost is one retired owner
   name per lost reply.
3. **Renewal is not authority.** `LeaseClock::takeover_by_timeout` always returns
   `AuthorityError::TakeoverRefused`. An expired lease means this client's own
   state may be gone; it is never evidence that another session terminated.

An OPEN that the server committed is released rather than dropped, on both paths
that can decide it is unusable: an identity lookup that fails after the OPEN, and
a `CLAIM_PREVIOUS` reclaim that comes back naming a different object. Dropping an
`OpenFile` sends nothing — CLOSE is a wire operation `state::open_owner::close`
dispatches — and a client that goes on renewing its lease is the reason that
state would otherwise survive. `OPEN_CONFIRM` runs first where the server asked
for it, because its reply carries the stateid the CLOSE must present. Neither
cleanup changes the answer: an unproven identity is still abandoned with the
owner burned, and a reclaim mismatch is still surrendered as `IdentityChanged`
with any cleanup failure retained beside it.

`OPEN_DOWNGRADE` is a partial seam. `state::open_owner::downgrade_with` owns the
seqid and the share-bit narrowing and takes the wire step as a closure, because
`transport::Nfs4Op` has no `OpenDowngrade` variant to submit. The state transition
is implemented and tested; the operation cannot yet be put on the wire.

### Safe-wrapper invariants

The crate denies `unsafe_code`. `src/transport/raw/` is the single module that
lifts the lint, under a `#![allow(unsafe_code)]` carrying the reason, so the FFI
stays visible in review rather than diffused through the crate.

Two hazards are closed by types rather than by convention:

1. **Nothing borrows across the seam.** A `FileHandle` owns its bytes and a
   `CompoundReply` owns its result bytes, so no reply can alias a caller's freed
   buffer. Ordering that a borrow lifetime would have expressed is carried by
   ownership instead: an `OpenFile` holds an `Arc<SessionCore>`, so a session core
   cannot be dropped while open state derived from it is alive. Closing a session
   makes surviving handles report `IdentityUnproven`, not undefined behaviour.
2. **A deadline cannot be reported without a cancellation.**
   `TransportError::DeadlineExpired` holds a `Retirement`, whose constructor is
   crate-private and reachable only through `RawTransport::cancel`. An
   implementation cannot report a timeout for a call it did not first withdraw
   from the pump. Calls are registered under an integer `CallToken` resolved
   through a pump-owned registry, so a completion for a retired call finds nothing
   instead of dereferencing freed memory.

`OpenFile::sequence` returns a `SequencedOp` guard holding the open-owner's lock,
which makes two concurrent sequenced operations for one owner impossible. The
guard must be resolved: `commit` advances the seqid, `abort` applies the RFC 7530
section 9.1.7 rule about which failures hold it, and both an explicit `abandon`
and an unresolved drop poison the sequence so the next caller is told to recover
rather than silently desynchronising. `OpenFile::next_seqid` reads the same
counter without creating a guard, so inspecting an owner cannot poison it.

## Operations

`src/ops.rs` is the operations surface: one typed `Storage` primitive in, one
typed response out. Every request ends in the typed response, a refusal naming a
capability this provider will not offer, or a refusal naming the node that owes
the wiring — never a success it did not perform.

| Module | Owns |
| --- | --- |
| `anchor` | The `run`, `root`, `control` and `.provider` anchors, containment-checked path resolution, and the session-bound `StorageHandle` mint |
| `identity` | `(fsid, fileid)` as stable object identity, its mapping to `ObjectId`, and the `PinnedObject` every operation addresses |
| `pages` | Bounded `READDIR` paging, cookie-verifier continuity, and the noise filter |
| `crud` | `OPEN` dispositions, reads, writes, and the WRITE/COMMIT verifier flow |
| `namespace` | The typed seam for `REMOVE`, `RENAME`, `CREATE` and `SETATTR` |
| `capability` | The capability matrix, and the error each verdict produces |

Four rules shape it.

1. **An anchor is a handle, never a path.** A userspace client has no mount root
   and no host path, so every `RuntimeDirectoryBinding::physical_path` is `None`.
   A fabricated one would name a path no `open` could use.
2. **Identity is the `(fsid, fileid)` pair, never the name and never the
   filehandle bytes.** An `OpenObject` records the name it was opened under for
   diagnostics and never re-resolves it. A rename therefore cannot retarget it, a
   pathname replacement leaves it on the original object while a fresh lookup
   finds the replacement, and an object whose last name is gone stays readable
   through the open until `CLOSE`.
3. **Escaping an anchor is unrepresentable, not merely checked.** `StoragePath`
   refuses empty, `.` and `..` components; `ComponentName` refuses those again
   along with `/` and NUL; and every intermediate component must resolve to a
   directory, so a server-side symlink stops the walk instead of being followed.
   Path bytes are not containment on their own: the walk also pins the server
   filesystem. `RunAnchors::open` records the `fsid` of the resolved export — the
   pseudo-root to configured-export transition is the one deliberate crossing —
   and every descent below it, including the final component of a resolved path,
   must report that same `fsid`. A `LOOKUP` into a nested exported filesystem
   returns an ordinary directory that no byte check and no symlink check can see,
   so it is refused on its own terms.
4. **A page is bounded at every level.** One `List` issues at most
   `pages::MAX_SERVER_PAGES` `READDIR` operations, each capped by `max_count`,
   and returns at most the requested limit; nothing accumulates a whole
   directory. The *allocation* is bounded independently of the caller's limit: a
   limit is a request, not a measurement of the directory, so reserving for it
   made a three-entry listing cost whatever number was asked for. The initial
   reservation is the smallest of the limit, what those server pages could
   physically carry, and a fixed ceiling; the vector still grows if more arrives. A changed `cookieverf` invalidates outstanding cursors explicitly
   rather than restarting silently or answering from a cached snapshot. The
   invalidation keeps the server's own diagnosis: a `NFS4ERR_BAD_COOKIE` is
   reported as an invalidated cursor *and* carries the original status number and
   failing operation, so a caller can tell why enumeration restarted. Absence and
   failure stay distinct throughout — a `.provider` lookup answered `NFS4ERR_IO`
   is that failure, not a run without a private directory.

Creating a file applies the mode that was requested. The provider supplies a
create verifier for every keyed operation, so a create is `EXCLUSIVE4`, which
carries that verifier in the field `GUARDED4` uses for initial attributes. The
attributes therefore follow in a `SETATTR`, whose returned attrset is checked
before the create reports success; a `SETATTR` failure is surfaced with its
partial effect rather than undone by removing a name another writer may own.

The verifier is derived from the operation id by mixing both halves of it through
MurmurHash3's `fmix64`, with fixed published constants rather than a hasher whose
algorithm may change between compiler releases. It is deterministic, which is
what lets a replayed create be recognised without consulting storage, and it is
**not** unique: 2^128 identities cannot map injectively onto 2^64 verifiers, so
what the module states is a collision probability of about 2^-64 for two random
ids. The earlier fold XORed paired bytes of the two halves together, which made
whole families of ids collide by construction — operation ids `1` and `1 << 64`
derived the same verifier, and an `EXCLUSIVE4` replay treats an equal verifier as
the same create. `CreateVerifierLedger` retains a key's verifier once it has been
used and `authority_recovery` persists that ledger, so a create whose verifier
reached durable storage replays exactly; one interrupted before that presents the
new derivation, is answered `NFS4ERR_EXIST`, and stops as a safe give-up rather
than adopting an object.

A `WriteAt` records a durable intent through the replay facade before dispatch,
reports the count and stability the server actually reached rather than the ones
requested, and writes `UNSTABLE`. An unstable write is durable only once a
`COMMIT` returns the verifier the `WRITE` did; a changed verifier is
`ReplayError::VerifierChanged`, meaning the bytes must be rewritten from the
retained payload. The write itself issues no receipt and makes no aggregate
claim; `flush` is what reads this settled evidence and certifies the run-scoped
barrier its receipt carries.

### Capabilities

`capability::CONTRACT_SURFACE` is the single source of truth for what each
`Storage` operation does, and `capability::OUT_OF_SURFACE` records a verdict for
the syscall-matrix entries no contract operation maps to. Three verdicts:

- **Supported**, implemented here and exercised by this crate's tests: `Lookup`,
  `Stat`, `List`, `ReadAt`, `WriteAt`, `Create` of a file or a directory,
  `CreateParents`, `Unlink`, `RemoveDirectory`, `Rename`, `SetMetadata` and
  `Truncate`.
- **Unsupported**, answered with `ErrorKind::UnsupportedCapability`: hard links;
  logical symlinks and `ReadLink`, which are overlay-owned; extended attributes;
  whiteouts; `AtomicSwap`; and `RenameMode::NoReplace`, because NFSv4.0 `RENAME`
  always replaces and no v4.0 operation makes "only if the destination is absent"
  atomic. Probing the destination first and renaming second is the
  check-then-rename the syscall matrix prohibits: another client can create the
  destination between the two round trips, and a probe that fails for any reason
  other than `NFS4ERR_NOENT` says nothing about what is there. It is refused in
  preflight, with the other unsupported operations, so it leaves no journal
  record behind; the capability table classifies by operation and cannot express
  an unsupported submode on its own. `RenameMode::Replace` is supported and
  unaffected. `OUT_OF_SURFACE` adds file-backed `mmap`, ACLs,
  `flock`, and — out of scope by the syscall matrix's notifications decision —
  `kqueue`/`kevent` with `EVFILT_VNODE` and FSEvents. Nothing in this crate
  registers, delivers or emulates a file-change notification.
- **Deferred**, answered with `ErrorKind::NotImplemented` naming the owner:
  `CopyUp`, which needs a base-materialisation seam that lives above storage.

`flush` is wired. It is a zero-I/O completeness barrier over the run's
already-committed writes: as of the call, under the current writer epoch, every
mutation in scope is settled and verifier-matched and none is outstanding,
indeterminate or latched-failed. It re-COMMITs nothing — a matched-verifier
`COMMIT` already placed those bytes on the server's stable storage — so it adds
the aggregate the per-write path cannot, not a new persistence mechanism, and it
returns a receipt at the run's qualified durability. The receipt never outruns
its evidence: it inherits `capabilities().durability` rather than asserting a
constant, so a run whose boundary was not qualified receives `Durability::Local`;
a
latched write failure returns the original error verbatim, an unsettled call is
indeterminate, a `Data` or `DataAndMetadata` scope is certified only when every
named object carries settled ledger evidence and is refused when any of them is
missing or still outstanding — an object the run never recorded is refused rather
than certified as a vacuous `Ok` — and no run open is `InvalidState`. A request naming a run other than the one this provider
holds is refused as `InvalidInput`, exactly as `execute` refuses it — a receipt
is issued only to the run that asked for it. An `UNSTABLE` write whose `COMMIT`
failed leaves an unresolved-recovery obligation whatever the server's status
mapped to, so the barrier reports it as indeterminate rather than certifying
bytes no `COMMIT` ever proved.

Namespace mutation — `Unlink`, `RemoveDirectory`, `Rename`, `Create` of a
directory, `CreateParents`, `SetMetadata`, `Truncate` — was deferred while the
frozen `transport::Nfs4Op` carried no argument variant for `REMOVE`, `RENAME`,
`CREATE` or `SETATTR`, even though `transport::OpCode` enumerated all four. The
authorised contracts hotfix at `m1_integrate` added those variants plus the
`SAVEFH` a RENAME needs to name its source directory, and
`namespace::dispatch::TransportDispatcher` binds them. A request that carries no
dispatcher — because it holds no writer authority — still receives
`NotImplemented` naming the missing binding, never a fabricated success.

`namespace::apply` enforces one rule whoever dispatches: a rename moves a name,
not an object, so an outcome reporting an identity other than the one the caller
pinned is refused rather than believed. `TransportDispatcher` takes the transport
per call rather than owning one, so a mutation provably travels on the session's
own connection instead of a second one whose epoch authorised nothing.

Three things the dispatcher refuses rather than absorbs:

- **An unlink whose target type the server did not report.** The probe exists to
  prove what a name resolves to, and a reply with no `FATTR4_TYPE` proves
  nothing. `RemoveKind::File` still covers a regular file *and* a logical-symlink
  name, so the refusal is about absent evidence and not about narrowing the kinds
  the contract supports.
- **A removal it cannot show removed the object the caller pinned.** REMOVE is a
  pathname operation and a name is not an object: another client can replace the
  name between the probe and the unlink. No COMPOUND can close that window —
  [RFC 7530 §14.2](https://www.rfc-editor.org/rfc/rfc7530.html#section-14.2)
  guarantees order but not atomicity, so a VERIFY guard would be an atomicity
  claim v4.0 cannot make — but the server can say whether its directory moved.
  The parent's `FATTR4_CHANGE` is captured before the probe and compared against
  the REMOVE's own atomic `cinfo.before`; equal means nothing happened in that
  directory across the window. Anything else stops the run
  `BLOCKED_RECOVERABLE`, because the REMOVE *succeeded* — a name really is gone,
  re-running would unlink whatever holds it next, and there is nothing to roll
  back. A server that never reports atomic change info therefore makes every
  pinned unlink uncertain, which is the conservative direction.
- **A metadata update whose timestamp cannot be represented.** An out-of-range
  time used to be dropped, which left a mixed mode-and-time update applying the
  mode and reporting `AttributesSet`: a partial effect reported as a whole one.
  The conversion is fallible and refuses before any SETATTR reaches the wire.
  Pre-epoch times still floor their seconds and carry a positive remainder,
  because `nfstime4.nseconds` is unsigned.

## Raw transport

`transport::raw::LibnfsRawTransport` implements `RawTransport` over
[libnfs](https://github.com/sahlberg/libnfs), pinned in `libnfs.pin`. It is
transport only: it holds no client id, seqid, lease or reclaim state.

`build.rs` does three things, all of them scope enforcement rather than
convenience, and it does nothing at all unless the feature is enabled:

1. **Pins the dependency.** The libnfs checkout is rejected unless it is at the
   commit `libnfs.pin` names, so the generated ABI cannot drift silently. The
   checkout is expected at `third_party/libnfs`, or wherever `UMBRA_LIBNFS_SRC`
   points.
2. **Allowlists the ABI.** Only the public raw RPC, XDR and task primitives are
   generated. `libnfs.h` is parsed because `libnfs-raw.h` needs its `rpc_cb`
   typedef, but no managed-lifecycle symbol is emitted.
3. **Proves the allowlist held.** The generated file is scanned afterwards and
   the build fails if any `nfs_*` entry point or `nfs_context` reached it. An
   allowlist passed to a generator is a statement of intent; the scan is a
   statement about the artefact.

Two hazards the audited spike carried are closed by construction:

- **libnfs never receives the address of a Rust value.** `private_data` is a
  `u64` call id resolved against a registry, so a late or duplicated completion
  for a retired call finds no entry instead of dereferencing freed memory.
- **Argument memory is heap-owned for the whole dispatch.** `rpc_nfs4_write_task`
  references the WRITE payload from the PDU's iovector rather than copying it, so
  a `CallArena` owns every buffer C can reach until the call completes or is
  proven withdrawn. `rpc_cancel_pdu` dereferences the PDU it is given before
  checking that libnfs still owns it, so the pointer is reachable exactly once,
  through `Dispatch::take_for_cancel`.
- **A connection failure frees every outstanding PDU, not just one.** Reachable
  once is not enough on its own: when `rpc_service` returns a negative value,
  `rpc_reconnect_requeue` has already errored every outstanding call and freed
  each PDU, so every pointer the wrapper holds is dangling at once. `pdu`
  counts those connection-wide disposals, each `CallSlot` records the generation
  it was queued in, and retirement makes a stale slot's pointer unreachable
  instead of cancelling it. The rule lives in `src/pdu.rs` rather than behind the
  `transport-raw` feature so it is compiled and tested in a default build.
- **Every service return is reconciled before any cancellation.** The same
  requeue path calls each outstanding completion on its way out, so a connection
  failure can carry a settled answer. The pump consults the completion registry
  on every return, including the failure paths, rather than reporting
  `Disconnected` over a completion that had already arrived.
- **The lifetime rule is asserted, not just stated.** `CallArena` counts its live
  instances, so a harness can observe the *ordering* of C-side disposal against
  Rust-side release — the ordering a use-after-free inverts. The `r3_004_*` tests
  in `src/transport/raw/mod.rs` queue a real specialized READ against a live
  server and drive it through a local poll failure and a real `rpc_disconnect`,
  asserting the arena is still alive when C is told to dispose, that retirement
  never cancels the freed PDU, that no stale registration remains, and that the
  arena is released exactly once afterwards. A third case pins that an ordinary
  completed call does release its arena, so the others cannot pass by arenas
  never dropping. ASan was not used: the workspace pins stable Rust and
  `-Zsanitizer=address` is nightly-only.
- **Retirement happens on every return path.** A reply that fails to decode
  retires its registration, slot and arena before the error propagates. Leaving
  them in flight would accumulate retained slots until `max_inflight` was spent
  and every later submission was refused `QueueFull`.

Replies are copied out of libnfs's buffers inside the completion callback, so no
reply borrows C memory. They are copied with `read_unaligned`: libnfs decodes
into a ZDR bump allocator with four-byte granularity, while `nfs_resop4` and
`entry4` contain `uint64_t` fields, and a misaligned reference is undefined
behaviour in Rust even where C tolerates the same address.

## Joining the two

`integration::StateSession` is the seam where the protocol state machine and a
wire transport meet. It owns one `Box<dyn RawTransport>` and one `ProtocolState`
driven over it, and it is constructed either way:

- `StateSession::over_fake` — the in-memory shape fake, no I/O.
- `StateSession::over_libnfs` — the live libnfs transport, behind the
  `transport-raw` feature.

The state machine already consumed `&mut dyn RawTransport`, so joining the two
implementations changed no state-machine code and moved no public surface; the
session only makes the choice explicit and reports which backend answered through
`StateSession::backend`. `split` hands out both halves at once because the driver
methods need `&mut ProtocolState` and `&mut dyn RawTransport` in one call, and
`observe` expresses the epoch comparison that needs both by shared reference.

**No live transport is constructed for `Storage` in this seam.** `NfsUserspaceStorage`
runs its operations over whatever `with_facades` is given — the fake in a default
build. Where a fixture is configured, `m1_conformance.rs`, `live_state.rs` and
`fault_matrix.rs` give the provider a live `LibnfsRawTransport` instead; that live
provider acceptance — the persistence, server-restart and failure-handling cases
that qualify `Durability::Remote` — is done rather than deferred.

## Authority and recovery

`src/authority/` owns the two questions the protocol state machine deliberately
refuses: whether this session may mutate at all, and what happens when it is
interrupted. `state::lease::LeaseClock::takeover_by_timeout` and
`state::ProtocolState::takeover` both answer `TakeoverRefused` rather than
owning the question; this is where it is owned.

| Module | Owns |
| --- | --- |
| `authority::marker` | The durable ownership record, its fixed-width codec, and the `MarkerStore` seam |
| `authority::server_marker` | That store on the server, over the frozen transport facade |
| `authority::admission` | Acquire, deny, cooperatively release; the epoch ladder |
| `authority::journal` | Durable intent, payload and committed result over the frozen `ReplayLog` |
| `authority::outage` | The finite state machine over the failure model's five states |

Four rules shape it.

1. **A held marker denies every acquirer.** Not until a lease elapses, and not
   unless the token matches — denies. No code path leads from elapsed time to
   admission, and a restarted process presenting its predecessor's token has
   proven only that it can read a file, not that the predecessor is dead. A
   crashed owner's own restart is therefore denied exactly like a stranger's and
   the run stops as `BLOCKED_RECOVERABLE` with the marker, the replay data and
   the diagnostic all retained. Every `TakeoverPolicy` other than `Refuse` is
   answered with `TakeoverRefused` before the store is touched.
2. **Release is recorded, never deleted.** The store has no `remove`, so no
   recovery path can reach for one under pressure. A graceful shutdown writes
   `AdmissionPhase::Released` in place, which is the evidence a follow-on process
   reads before acquiring at exactly one higher epoch. A release is withheld
   entirely while outstanding I/O cannot be excluded: `ReleaseOutcome::Retained`
   hands the proof back rather than publishing a handover the session cannot
   stand behind. `OutstandingIo` is derived from the calls the provider actually
   observed — a lost connection or an elapsed deadline leaves a request that may
   already have been applied — not asserted from the shape of the dispatch loop.

   A release whose write fails is reconciled against the marker rather than
   assumed not to have landed. If the marker reads `Released` at this session's
   epoch the write did land and only its reply was lost; if it still reads `Held`
   by this session the release provably did not happen and the proof is handed
   back; anything else is `ReleaseOutcome::Uncertain`, which consumes the proof.
   Reviving authority over evidence the session cannot account for is how one
   failed round trip becomes two live owners.
3. **Succession is decided by the server, not by a read.** Reading a released
   marker and overwriting it is two round trips, so two contenders can both read
   the release and both write themselves in. `MarkerStore::claim_succession`
   serialises it: a `GUARDED4` create of the epoch's claim name means the loser is
   told `NFS4ERR_EXIST` by the server rather than by a local guess. Claims are
   durable evidence of which contender took which epoch and are never deleted, so
   a run that has been succeeded carries one `writer.lock.claim.<epoch>` per
   handover alongside the untouched predecessor evidence.
4. **Nothing dispatches before its intent is durable.** `DispatchTicket` has no
   public constructor; the only way to get one is `MutationJournal::begin`
   returning `Acknowledged::Dispatch`, which happens after the intent, the
   payload and the authorising epoch have reached the log. Backpressure is
   applied before that admit, so an exhausted buffer refuses the mutation instead
   of letting it reach the wire with nowhere to record its outcome.
5. **A stop is terminal, and the first error is latched.** `OutageMachine`
   answers with its existing state as soon as that state `is_terminal` —
   `BlockedRecoverable`, `Corrupted` or `FailedTracee` — before a later window
   can decide anything. Each window is decided on its own evidence with no memory
   of where the run stopped, so without that guard a `Corrupted` run went back to
   `Running` on the next `StaleFilehandle` whose identity happened to prove. A
   stopped machine counts no further attempts and records no later window's
   deferral; only building a new machine reopens it, which is the operator
   intervention the state is for. Within a window that is still live, the failure
   that opened it is kept in a frozen `RetainedError` and later attempts are
   counted without replacing it, so an `NFS4ERR_NOSPC` is still 28 after three
   reconnects. A
   digest-only payload is `PayloadMissing` rather than reconstructed bytes, and a
   namespace intent whose reply was lost is `Indeterminate` rather than a guess:
   its record carries a name, not the before/after proof the failure model
   requires.

`AdmissionMarker` decodes two encodings and produces one. The 16-byte legacy
lock the mounted adapter writes — the one `tests/goldens/writer-lock.bin` pins —
reads as *held* by an unnamed writer, never as released, because absence of a
phase is not evidence of a release. Records this provider writes are a
fixed-width extended encoding, so every overwrite is total: a shorter record
written over a longer one would leave the old tail readable and the next reader
would decode a chimera. `SETATTR` exists on the wire now, so truncation is
available in principle, but a fixed width needs no truncation to be correct and
narrowing the record would reopen a case that is currently unrepresentable.

`ServerMarkerStore` is the one place an operation is expressed here rather than
left to the operations node, because the failure model requires *server-atomic*
admission and that atomicity is an authority requirement. It uses `OPEN` with
`GUARDED4`, not `EXCLUSIVE4`: an exclusive create is designed to let a replay
with the same verifier succeed again, which is right for a retried create and
wrong for admission, where the second session must be told the name exists. It
is four calls on two names and nothing else — the marker, and the per-epoch
succession claim that serialises a cooperative handover — with no path
resolution, no anchoring and no capability. `session::Session` binds it into
`Storage`.

**Split brain and hard partition are not implemented here.** Two hosts holding
conflicting valid ownership evidence needs a fence receipt and a cutoff before a
winner may be selected. `CrashWindow::SplitBrain` and `CrashWindow::HardPartition`
stop *both* sides and record `Deferral::M3Fencing`; `CrashWindow::ServerPowerLoss`
records `Deferral::M3Qualification`. An increasing epoch is not a fence and
nothing here pretends otherwise.

## Configuration and registration

`NfsUserspaceConfig` carries the server host and port, a server-relative `export`
and `run_parent`, the two anchor components, and a per-COMPOUND deadline. Export
and run parent are validated with the storage contract's own path rule, so neither
can be absolute or contain `.` or `..`. There is no mount root and no host path.

Options are the JSON encoding of that struct, carried as descriptor option bytes.
`provider.json` is an installation template; `tests/provider_template.rs` checks it
decodes as a storage descriptor for id `nfs-userspace` and that its options
validate. A userspace client has no kernel-visible run root, so this provider
supplies opaque session-bound handles and no `physical_path`. `umbra run` selects
it through the **routed** path described below, not by rewriting syscall
operands: there is no path for an operand to be rewritten *to*.

`open_run` establishes a client incarnation, resolves or creates the run's
anchors over the bound transport, acquires product admission, and only then
publishes a binding whose advertised `max_io_bytes` and `max_directory_entries`
come from that transport's own limits. `durability` is derived per run, and a
satisfied `flush` returns that same level, so the provider never advertises less
than it delivers and never more than it proved. It is `Durability::Remote` only
when both gates held for that `open_run` — the bound transport declared
`PersistenceBoundary::RemoteServer`, and a synthetic `WRITE`/`COMMIT`
verifier-compare probe succeeded against the run's own `.provider` — and
`Durability::Local` otherwise, including on a read-only run, on a transport that
declares nothing, and when the probe did not succeed. `Fencing::ReadOnly` stays: no independent
termination verifier exists, and the barrier says nothing about fencing. The
binding advertises exactly two `features` names,
`ownership-fidelity-v1` and `timestamp-fidelity-v1`, both corroborated by the
same two places: `SetMetadata` is `Support::Supported` in the capability table,
and `NamespaceMutation::SetAttributes` maps `update.uid`/`update.gid` onto
`FATTR4_OWNER`/`FATTR4_OWNER_GROUP` as stringified numeric ids and
`update.accessed_nanos`/`update.modified_nanos` onto
`FATTR4_TIME_ACCESS_SET`/`FATTR4_TIME_MODIFY_SET`, which is what those two names
claim. The timestamp name is the one **this** provider carries and the other
three storage backends deliberately do not: they refuse a timestamp update
outright, so the overlay requires the name before it issues one at all, and a
`utimensat` on a backend without it is refused to the tracee at `resolve`
instead of stopping the run from inside `prepare`. It is advertised on an *open* run only, never on the
unbound capabilities, for the same reason the limits are zero there: without a
bound transport there is nothing qualified to claim.

`OpenRunIntent::CreateNew` writes the run directory, both
contract anchors, `.provider/`, `.provider/retries/`, `.provider/epoch` and
`.provider/manifest` with the mounted adapter's names, modes and encodings. It
does not create the export or run-parent directories: those are deployment
configuration, and creating a missing one would silently relocate every run.

The `StoragePolicy` is enforced rather than recorded. `require_kernel_shadow` and
`require_strict_remote_persistence` are **still refused** before the run is
touched: strict remote persistence is a stronger promise than the barrier `flush`
certifies — it says nothing about the export's `fsync` policy or persistence after
power loss — so leaving the refusal in place preserves a guarantee rather than
weakening one. A read-only run cannot be created, because creation is itself a
mutation, and an opened read-only run refuses mutations while still serving reads.

`OpenRunIntent::OpenExisting` reads the run's own evidence before admission.
`.provider/manifest` must decode and must name the requested run, immutable base
and format version; a manifest that is absent or malformed is refused with its
bytes left on the server, never adopted and never read as a release.
`.provider/epoch` supplies an epoch floor, so a run the mounted adapter released
cleanly at epoch 7 is admitted at 8 rather than restarting the ladder at 1. An
epoch file that is not eight bytes is refused rather than treated as zero, and so
is one that is absent: every run either adapter creates writes that file, so its
absence in an existing run is missing recovery evidence rather than a run that
never had a writer.

Every request is checked against the admission it claims: a mutation naming a
different run than the open one, or presenting a writer epoch other than the
admitted one, is refused before dispatch. Neither check exists upstream —
`umbra_storage::validate_request` deliberately leaves bound-run and lease checks
to the backend and only requires that some epoch is present.

All of it — the authority latch, the run binding, the read-only policy, the
capability verdict, the input bounds and the epoch — runs as one gate *before the
journal is touched*, so a request that will be refused consumes no operation id
and leaves no record behind. `Operations::preflight` holds the rules and
`Operations::execute` re-runs them, because a direct caller reaches that seam
without a provider in front of it.

## Running a supervised command over this provider

`umbra run --registry <registry.json>` works against this backend. It reaches a
different mechanism from every other storage backend's, and the difference is
forced rather than chosen: a supervised run ordinarily services the tracee's file
operations by **rewriting** the path operand of an intercepted syscall to a
kernel-visible shadow path, and this backend has none. So the tracee's `open`,
`read`, `write` and `close` are **routed** instead — umbra services them itself,
through the overlay and this client.

### How a routed run is wired

* The storage descriptor must declare `userspace-nfsv4-v1` and
  `experimental-userspace-routing-v1`. Both are earned by `connect`'s live probe
  against the configured export; a build without `transport-raw`, or a provider
  built over the fake, advertises neither.
* The platform descriptor must declare `experimental-userspace-interpose-v1`
  alongside its usual two names. `experimental-open-rewrite-v1` is **not**
  required and must not be declared: it claims kernel-visible rewrite targets.
* `umbra run --state-dir <PATH>` names a host directory holding two things a
  routed run cannot keep in its store: umbra's own journal, and the single
  directory the enforcement profile grants the tracee. **`umbra resume` must be
  given the same value** — see *Single-host* below.
* `timeout_ms` in the registry must leave renewal headroom under the 30 s writer
  lease. The supervisor refuses anything at or above 15 s before the run starts;
  12 s is a good value for a live server.

At launch the platform loads umbra's interposer into the target image, lowers the
tracee's `RLIMIT_NOFILE` — soft *and* hard — to 4096, and breakpoints the
interposer's trap instruction. The interposer replaces libc's `open`, `read`,
`write` and `close`; umbra answers each one through the overlay's ordinary
resolve / prepare / emulate / observe / commit sequence, so a routed write is a
journaled transaction exactly like a rewritten one. Descriptors umbra issues are
at or above 4096, which the kernel's own range can no longer reach.

### Unsupported operations

Every entry here is a **refusal**, never a plausible wrong answer. Read this
before assuming an operation is covered; adding one to the interposer is not
sufficient, because umbra must be able to represent it too.

| Operation | Disposition |
| --- | --- |
| **A syscall the tracee issues from memory it wrote itself** | **Not mediated, and not mediable by this architecture.** Measured: a process can allocate memory, write `svc #0x80` into it, flip the page to `r-x` with no entitlement, and execute it. umbra breakpoints `svc` sites in the main image and in its own interposer, so an instruction at neither site is never trapped — under interposition and under syscall-breakpoint tracing alike. What fail-closes host writes is the **kernel-enforced Seatbelt profile** installed before the target's first instruction, not the interception. Closing this needs kernel-assisted whole-process syscall filtering, which is outside the accepted setup. |
| Writable shared file mappings (`mmap` `MAP_SHARED` with `PROT_WRITE`) | **Permanently unsupported by any syscall- or call-level interception.** A store to a resident page is not a call of any kind. Needs a pager. |
| `unlink` | **A routed run cannot delete a file at all against a server that does not report atomic `change_info4` for REMOVE, and it does not refuse — it stops.** Measured against the Ganesha fixture, with the descriptor open *and* after closing it: `LeaseLost during namespace.remove: authority: object identity unproven: BLOCKED_RECOVERABLE: the REMOVE of <name> succeeded, but the server did not report atomic change info`, the run left recovery-required, and the writer release withheld. This is **this provider's own authority layer**, pre-existing and unrelated to routing — the same refusal a non-routed caller of `unlink` meets. It is listed here because the slice's own docs previously said "unlink or rename *while open*" and pointed at `engine.rs::routed_binding`, which implied unlink-while-closed worked and named a mechanism that is never reached. |
| `rename`, and any other path operation, on an object **this run created or copied up** | Refused with **`ENOTSUP`** (`Errno(45)`), visibly, and the run survives. `rewrite` needs a kernel path this backend does not have, so `resolve` answers the tracee instead — see the row below. |
| A descriptor whose **name stops resolving** while it is open | `ENOENT`. A routed descriptor is bound to a logical *path* and re-resolved on every operation, so an object that loses its name has nothing left to route to; closing that needs handle-based `Storage` operations the surface does not have. Not reachable against this fixture, because `unlink` stops the run first (above). |
| **Every other intercepted path operation on an object this run created or copied up** — `fstatat`, `faccessat`, `renameat`, `linkat`, `fchownat` | Refused with **`ENOTSUP`** (`Errno(45)` — Darwin's value; routing exists on Darwin only). `rewrite` would have to name a kernel path this backend does not have, so `resolve` answers the tracee instead of stopping the run. The same operation on an object the run did *not* touch still rewrites to the read-only base's host path and works. This is a visible refusal, not a supported operation: `create a file then stat it` is what `cp`, `install` and both the Rust and Go standard libraries do, and on a routed run it fails. Routing these properly means answering `Stat`/`Access` through an ABI encoder the way **`Fstat`** already is — the engine resolves the metadata, the ABI encodes it — and that routing deliberately did **not** widen this row: an `fstat` names a descriptor, so it never reaches path resolution, and `Stat`/`Access` on a shadow object are still refused here. **This row previously offered `ReadLink` as that same precedent, and `ReadLink` is the one named here that does not work.** Measured on both decoder arms against the live fixture: a routed `readlink`(58) or `readlinkat`(473) on a symlink in the run's own shadow answers the tracee nothing at all — it **ends the run**, `UnsupportedCapability during overlay: ReadLink requires set_readlink_buffer`, with no tracee errno and no `finished:` line. `FsOp::ReadLink`'s engine arm is unconditional and resolves the link's target correctly, then requires a buffer only `Overlay::set_readlink_buffer` can bind — and no production caller on the routed path binds one: `routing_for` binds `Open`/`Read`/`Write`, the stat buffer for `Fstat` and the directory buffer for `ReadDir`, while `FsOp::ReadLink` appears nowhere in `umbra-supervisor/src/events.rs`'s `FsOp` set. So copying `ReadLink` as the template would wire `Stat`/`Access` after something that never reaches the tracee; **`Fstat` is the worked precedent to copy.** Characterized by the two `readlink` cases in `userspace_run.rs`, which assert the run-ending shape and go red when the binding lands; the binding itself is a separate slice. |
| `openat` | **Routed, via the syscall path** -- not in the `EBADF` class. `__openat` is still breakpointed and syscall 463 is still admitted, so it resolves through the same routed-`Open` arm the interposer's `open` reaches, and returns a virtual descriptor. Measured: exit 0, host destination absent. **`openat` with a *virtual* dirfd is served too**, and this row previously said the opposite: measured, `fd=4096` -> `openat` -> `fd=4097`, a second virtual descriptor. The engine has a complete `DirRef::Fd` resolver -- anchor lookup, directory check, object-identity re-check -- and `__openat`(463)/`__openat_nocancel`(464) are `TRACED_STUBS` rows, so the call never reaches the kernel and the `EBADF` mechanism the row below describes cannot apply to it. Pinned by the no-follow walk cases in `userspace_run.rs`: three of them -- `safe-path`, `symlink-in-parent` and `symlink-leaf-follow` -- compose a routed directory open with an `openat` relative to the virtual descriptor it returned, and go on to read the leaf's metadata through it. A fourth, `symlink-leaf`, is refused *at* that `openat`, but by `O_NOFOLLOW` on a symlink leaf -- not by this row's descriptor fence -- and since [#167](https://github.com/invakid404/umbra/issues/167) that refusal is **answered rather than raised**: the routed-`Open` arm returns `Ok(ResolvedAction::Deny(Errno(62)))` at `engine.rs:2575`, so the tracee gets `ELOOP`(62) and the run continues to the fixture's own stage-(b) exit. The refusal is decided at the `openat` itself, after `resolve_path_follow` and `lookup` have already returned, from the resolved final object's `stat.kind` -- not during the walk. The fifth, `flagscan`, issues no `openat` at all: it scans the directory open's admitted flag set. |
| `fstat` | **Routed, via the syscall path** -- no longer in the `EBADF` class below, and listed separately because it is the first *descriptor-relative* call to leave it. `fstat`(339)/`__fstat`(189) are breakpointed libc stubs, resolved against the descriptor's logical object and answered with an emulated `struct stat`. Every `fstat` in the process traps, including libSystem's own on kernel descriptors, so the supervisor applies the descriptor fence before resolving: below the floor the call passes through to the kernel untouched. `touch` needs this -- without it the file was created and the utility still exited 1 with `Bad file descriptor`. |
| `lseek`, `dup`, `dup2`, `fcntl`, `ftruncate`, `fsync`, `pread`, `pwrite`, `readv`, `writev` — but no longer `fchdir`, `close` or `__close_nocancel`, which this slice routed, and **no longer `openat` with a virtual dirfd**, which this table listed here and in the row above as refused and which is measured **served** (see that row) | Not interposed and not rewritable: these are **descriptor-relative**, and a routed descriptor is not a kernel object. They reach the kernel, which does not know the number, and receive `EBADF`. The `RLIMIT_NOFILE` fence is what makes that a refusal rather than an operation on someone else's descriptor. **The refusal is answered to the tracee and the run survives** -- the opposite disposition from the `O_APPEND` row below, and measured against this fixture for `lseek`, `fcntl`, `ftruncate`, `fsync`, `pread` and `pwrite`: tracee `errno` `EBADF`(9), a `finished:` line present, and `RunCompleted` recorded in the journal. A program can therefore branch on it, which is what separates this class from a propagated refusal that stops the run; the base object is left byte-identical on the host and in the shadow. **Two call-path details measured here, because the libc spelling is not the one a reader of this row would predict.** Rust's `File::try_clone` issues `fcntl` with command **67** (`F_DUPFD_CLOEXEC`), *not* the command **0** (`F_DUPFD`) the C spelling issues, and a bare `dup` is syscall **41** -- three different calls behind one row. Rust's `File::sync_all` issues `fcntl` with command **51** (`F_FULLFSYNC`) and `File::sync_data` issues the identical call, so **a Rust `File` has no `fsync`(2) call site at all** and `sync_all` never surfaces as the `fsync` named in this row. `dup2`, `readv` and `writev` are listed by mechanism only and were not exercised. Pinned in both C and Rust by the twelve descriptor cases in `userspace_run.rs` -- ten refusals, plus two **served-member controls** (C `fstat`, Rust `File::metadata`) which succeed on the very descriptor the ten are refused on, which is what makes this a statement about interposer and `TRACED_STUBS` coverage rather than about descriptors as such. |
| Directory reads (`getattrlistbulk`) | **Routed, via the syscall path** — no longer in the `EBADF` class, and the routed `open` of a directory is no longer refused. `getattrlistbulk`(461) is a breakpointed libc stub, resolved against the descriptor's logical object and answered from the overlay's shadow-merged view with encoded native records. Like `fstat`, every one of them traps — including any on a kernel descriptor — so the supervisor applies the descriptor fence before resolving. Only the one attribute request `fts` issues without metadata is served; anything wider is refused by name. |
| `open` with `O_APPEND` | Refused -- and **it does not answer the tracee, it stops the run**. Honouring it means writing at the object's end atomically with respect to other writers, and `Storage` has no append-at-end operation; stat-then-write is right for one writer and silently wrong for two. The refusal is `UnsupportedCapability`, the same error kind from the same `resolve` arm as the directory-open refusal recorded below, and it carries the same disposition: measured against this fixture, `UnsupportedCapability during overlay: routed open with O_APPEND: ... (errno: None)`, **no tracee errno**, no `finished:` line, and no `RunCompleted` in the journal. A bare append to an absent path is a different answer: it never reaches this refusal, because name resolution denies it `ENOENT` first and the run survives — `O_APPEND` reaches the refusal only when the target exists, or when `O_CREAT` carries it there. See #161, and the six `_161` cases in `userspace_run.rs` that pin it in both C and Rust. |
| `exec` of a new image | **Claimed and shipped.** The interposer follows the exec. `exec` replaces the address space, so the library arrives in the new image mapped and **inert** — dyld re-loads it, because `DYLD_INSERT_LIBRARIES` rides in `envp` and survives the exec, and re-runs its constructor, which leaves `__DATA,__umbra_arm` back at zeroes — and umbra re-arms it there exactly as it arms the launch target's. **This row previously said the opposite, and the opposite was true**: `install()` compared the running image against a target assigned once at launch and never reassigned, so only an exec of that same image re-armed. Anything else took the "this run routes nothing" arm and ran with an inert interposer, silently: no refusal, no diagnostic, no event. **The consequence is more specific than "unmediated", and the specific version is the measured one**: the tracer re-plants every `TRACED_STUBS` breakpoint after an exec, so the child's `open` **was** routed and did return a virtual descriptor; only the interposer was un-armed, and `write`/`close` reach umbra through the interposer alone. So the write went to libc with a descriptor number the kernel does not own and was answered **`EBADF`**, leaving the object **present and empty** in the export. Nothing read the host and Seatbelt refused nothing — the audit predicted otherwise and was wrong, and a review mutation settled it by reading `[]` back through the client. Every tool call a shell makes execs a different binary, so that was the common case. The image the requirement names is now per-session and follows that session's execs and its attached children. Covered by `a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image`, which asserts the exec'd image's own write **through the NFSv4 client** — an exit status cannot tell a write that reached the store from one that never left the process. **Not claimed for `posix_spawn` with non-null file actions or attributes**, which is refused before it spawns. |
| Calls made before umbra **arms** the interposer | Not routed — and the window is dyld's image loading **and every library initializer**, not just dyld's own loads. There has been no constructor since round 1: the library ships inert and umbra arms it by writing its control block *after* planting breakpoints, so everything an initializer does runs with the interposer dormant. Reads in that window reach the host; writes are refused by the Seatbelt profile, so it fails **closed** — but a program that writes from an initializer works on every other backend and fails here. The window is strictly larger than a rewrite-backed run's, which plants its breakpoints at the dyld image-notifier stop, before any initializer. Measured both ways in `umbra-platform-macos/README.md`, which also carries the arming order (`ARMING`, and `Session::arm_interposer`). |
| More than 4096 concurrent routed descriptors | **`EMFILE`**, visibly, and the run survives. umbra allocates from `[4096, 8192)` — the same size as the kernel's fenced range — so the limit the tracee is told through `getrlimit` is the limit it gets on each side, and `ProcessContext::fds` is bounded. An earlier shape scanned to `i32::MAX`, which made both halves false: measured, a tracee held 400 routed descriptors with no refusal, and the practical ceiling was the run deadline rather than the fence. |
| A **forked** child of a routed run | **Supported, and measured both ways.** The child is mediated from its first instruction: `fork` copies the armed interposer, so umbra finds its load address in the child's own image list and does not re-arm — re-running the address search resumed the child to a `main` it had already passed, leaving it unmediated until its first routed call took `SIGSYS`, which is the defect CodeRabbit found on PR #116. A routed descriptor also survives the fork *with its offset*, so a child's write continues the parent's in the store. **That offset is COPIED, not shared, and this row previously stopped at the sentence before this one** — which reads as POSIX Open File Description semantics and is not what happens. `track_process` seeds a forked child by cloning the parent's `ProcessContext` by value and `FdState::offset` is a plain `u64` with no OFD indirection anywhere in the shadow model, so the two processes leave the fork with two offsets where POSIX gives them one. Measured: the parent writes `seed`, the child writes `child` through the inherited descriptor, the parent then writes `parent` through the same descriptor, and the object holds **`seedparent`** (10 bytes) — the parent's third write lands at the offset its own FIRST write left and overwrites the child's bytes in place, where POSIX requires `seedchildparent` (15 bytes). The sentence before this one is still true as far as it goes: the child's write does continue the parent's, because the offset it inherited is a copy of the parent's current one. What does not hold is the converse, and nothing before this measured it. Pinned by `a_forked_child_s_write_and_its_parent_s_next_write_do_not_share_one_offset`, which is **GREEN at this pin** -- green is the correct disposition, because it asserts the divergent bytes and excludes the POSIX ones -- and which turns RED when shared-OFD routing lands, at which point it is the case to delete rather than to repair; its host control runs the identical binary unrouted and gets the POSIX 15 bytes, which is what makes the 10 a divergence rather than a broken fixture. **Not claimed: a shared Open File Description across `fork`.** **`exec` is the other case and it is governed by the row above, not by this one** — the two are opposites and this row used to describe the wrong one. `fork` inherits an already-armed control block and must **not** be re-armed; `exec` gets a zeroed one and must be. What keeps the two apart is the parent test in `install()`: a session with a parent finds the interposer in the image list `fork` copied and is never resumed to a `main` it has already passed, which is the `SIGSYS` defect CodeRabbit found on PR #116. A forked child that then execs falls to the fresh-image branch, because the exec cleared the image list that test reads. Covered by `a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes`, `a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store`, `a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image` and `a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it`. **Not claimed: a fork from a multithreaded tracee**, which is refused outright with `fork/deferred wait requires one thread` and stops the run. |
| **Plain `ls`, and directory reads on a virtual descriptor** | **Claimed and shipped.** `umbra run --registry <nfs-userspace> -- /bin/ls <dir>` lists the shadow's merged entries and exits 0, host untouched, measured against the live fixture. Four calls were routed together because `fts` needs all four and no fewer: `getattrlistbulk`(461), the directory read `ls` actually issues; `fchdir`(13), because once the opens are routed its operand is a *virtual* descriptor the kernel would answer `EBADF`; and `__close_nocancel`(399) with `close`(6), which release it. The routed `open` of a directory, previously refused outright, is served. **This row previously said the opposite, and described a state master could no longer reach**: it claimed `fts` falls back to `readdir` with the names printing and `ls` exiting 1 at every errno, and dying on `SIGTRAP` with the dirfd `close` refused. That was a snapshot of an older tree. Once the directory-open refusal existed the run *stopped* at the routed `open` — `UnsupportedCapability`, no tracee errno, before `getattrlistbulk` was ever reached — for **every** operand shape, including a plain file and an absent path, because `fts` opens `"."` before it looks at the operand. |
| **Metadata-requesting `ls` modes (`-l`, `-la`, `-t`, `-i`, `-p`, `-S`, `-F`, `-s`, `-n`), `ls -@`, and every other directory-read surface** | **Not claimed. Fail-closed on this registry, and out of umbra's way on the others.** The trigger is not a flag list: it is **any mode that makes `fts` ask for metadata**, which widens the `getattrlistbulk` attribute request from `common=0x8200000b file=0x00000001` to `common=0x82079e0b file=0x0000022d`. The bitmap *is* the layout of the reply, so a set umbra cannot encode is refused rather than answered with the wrong buffer — with **`ENOTSUP`, bound to the tracee**, so the run survives and the program sees an errno it can handle (measured: `ls -l` exits 1, run `finished: Some(Code(0))`). **`ENOTSUP` is umbra's own answer, not the kernel's** — measured, the kernel *serves* the wider set — which is exactly why the refusal is evaluated **after** the descriptor fence and never for a descriptor umbra does not own. `ls -l` additionally needs `listxattr`(240), in neither the interposer's four functions nor `TRACED_STUBS`. `ls -@` and ACLs need `getxattr`(234)/`acl_get_link_np`, and no ACL model is qualified. Also not claimed: symlink following inside a directory read (`DirectoryEntry` carries a name and a stat, and `FsOp::ReadDir` carries no follow semantics), extended attributes inside `getattrlistbulk`, `getdirentriesattr`(222) (permanently unsupported — a legacy bulk API whose records umbra will not fabricate), `getdirentries`(196) and `getdirentries64`(344) (reachable only through the fallback a *failed* `getattrlistbulk` triggers, which serving 461 means never entering), and `opendir`/`readdir`/`closedir`, which are that same fallback's entry points and appear nowhere on `/bin/ls`'s measured path. |
| **`ls -R`, and recursive `fts` generally** | **Not claimed, and not tested — but it is not refused either, and "fail-closed" would be false.** This row previously called the whole unclaimed set fail-closed. For the metadata modes above that is accurate; for recursion it is not, and the two were separated rather than left sharing an adjective that fits only one. Measured on this registry against host controls: `/bin/ls -R` and `/usr/bin/find` produce byte-identical output to the host on a three-level tree, and a recursive `fts` fixture yields the same entries with `errno` 0. Recursion therefore **appears to work** and is nonetheless **not claimed**: it is outside the ratified slice, no test covers it, and nothing here qualifies it. There is also nothing to refuse it *with* — when `fts` declines to descend it issues no syscall, so umbra is never asked. **A review round did once measure silent truncation here, and the cause is known: it was the descriptor-cache defect, not recursion.** A fixture that calls `fts_children` on a directory *and* lets `fts_read` descend reads that directory twice; the first enumeration ran to EOF and released its descriptor, the second got the same number back and hit a cached empty remainder, so `fts` saw an empty parent and never yielded the child. Evicting the cache on close fixed recursive traversal without anyone aiming at it — and fixtures that read each directory once, which is how this was first checked, could not have shown either the defect or the fix. Treat recursion as unqualified, not as supported. |
| **`chdir`(12)** | **Claimed and shipped**, on the tracer rather than the interposer. `chdir` is a breakpointed libc stub — measured, `_chdir` is `movz x16, #12; svc #0x80` and nothing else, exactly the shape `install()`'s verifier reads — decoded to `FsOp::Chdir`, resolved through the same path machinery every other path operand goes through, and answered by emulation. Its success moves `ProcessContext::cwd`, which is what `DirRef::Cwd` resolution anchors against, to the **absolute logical** path the namespace resolved; a relative one is refused, because `cwd` is the anchor and a relative value would anchor against itself. **This row previously said `FsOp::Chdir` was declared and inert, and that was the defect**: the call reached the kernel, moved the *host* working directory, and left umbra's logical copy at the launch directory, after which every relative path in that process resolved against the wrong anchor. Measured reachable by the two commonest real shapes there are — `git status --short` issues one `chdir` and forks nothing at all, and `bash -c 'cd ... && ...'` issues one too. Per-process by construction, because the context it moves is per-process, so a forked or exec'd child moves only its own anchor. Covered by `a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against`, which asserts on the **entry name** in both directions — the bytes are at the moved-to name, and nothing exists at the launch-directory name — because a wrong anchor does not lose a write, it puts it somewhere else. **Not claimed on the rewrite-backed registries**, where `chdir` is resumed into the kernel unchanged and `ProcessContext::cwd` still does not follow it. |
| **`getcwd`(326)** | **Not claimed, and deliberately left inert.** Nothing decodes 326, no measured caller on this path issues it, and a handler for a call that cannot arrive is prose describing a path nothing walks. A tracee that calls it receives the *kernel's* answer, which on a routed run names no directory the run's namespace knows — so `getcwd` is unserved rather than in agreement with `ProcessContext::cwd`. What `fchdir`(13) buys is stated precisely beside it because it is easy to overstate: `fts` uses the BSD save-and-restore idiom, `open(".")` then `fchdir` back to that same descriptor, so **every `fchdir` operand `ls` issues names the directory the process is already in**. It never chdirs into the directory it reads, so for `ls` the working-directory update is an identity; `fchdir` is routed for the *descriptor's* sake, not the directory's. |
| **`touch`, `cat` and `mkdir`** | **Claimed and shipped**, on the tracer rather than the interposer. `cat` needed nothing new — its `fstat` on stdout is a kernel descriptor below the fence, and it tolerates what happens on its virtual one. That is now *one answer and one refusal* rather than two refusals: its locale descriptor is virtual, the `fstat` on it is answered where it used to be refused, and `_Read_RuneMagi` then reaches an `mmap` on that descriptor which it never used to reach. Measured: the `mmap` is refused `EBADF`, `cat` exits 0 with correct bytes, and exits 1 with its own `No such file or directory` on an absent operand. `mkdir` needed bare `mkdir`(136). `touch` needed `fstat`(339) for the create path and `setattrlistat`(524) — which is how `utimensat` reaches the kernel — for the existing-file path. **`touch` on an existing file is served on this registry only**: it is the one backend that applies a timestamp, and the other three refuse it with `ENOTSUP` at `resolve` while the run survives. |

**What the shipped claim above rests on.** Routing for all three utilities is
proven **by execution against a live NFS-Ganesha fixture**, not inferred. The
end-to-end case is `standard_utilities_run_over_the_userspace_client` in
`tests/userspace_run.rs`: six cases -- `mkdir` into the shadow, `touch` on an
absent and on an existing operand, `cat` on a present and on an absent one, and
`ls` on the workspace root -- all six asserting the host workspace is untouched,
and the three that write also asserting the store holds the result *read back
through this client*. What the other three assert is their exit status: `cat` on a
present operand and `ls` exit 0 with the journal recording completion, and `cat`
on an absent one exits non-zero with its own *No such file or directory* on the
tracee's stderr; the listing entries read back through the client are proven
separately, by
`a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client`.
Seven mutation probes cover the two routed data directions, the four metadata
and directory calls this slice routed, and the residual-page eviction a closed
directory descriptor triggers, and a mismatch between the selector and the
binaries fails the case rather than passing it, so a passing probe means the
mutation was detected rather than that the case was skipped.

What that does *not* extend to: the rewrite-backed matrices in `run_fixtures.rs`
(`--local-dev` and the kernel-mount `nfs` adapter) cannot substitute for the above,
and one asymmetry shows why rather than leaving it as an assertion -- with the new
stub rows removed, `touch` on an absent operand still exits 0 on `--local-dev`,
because its descriptor there is a real kernel one, so that matrix cannot
discriminate the `fstat` case even in principle. The suite needs this crate's
`transport-raw` feature, a vendored `third_party/libnfs` at the pinned commit and a
live fixture at `UMBRA_NFS_RAW_FIXTURE`; without all three it does not run, and its
provisioning gate is a hard failure under `UMBRA_INTEGRATION_REQUIRED` rather than a
silent skip, because a skip that reports success is how these proofs sat inert once
already.

### Single-host: a routed run's journal is not in its store

`JournalControlBinding` carries a `PhysicalPath`, and this backend has none, so a
routed run's journal lives under `--state-dir` on the host that created it. Two
consequences, neither of them silent:

1. **A routed run cannot be reopened on another host.** Reopening needs the store
   *and* the log, and the log is on this host's disk. Nothing in this slice moves
   it. Fixing it means widening `JournalControlBinding` beyond a `PhysicalPath`
   plus a journal backend that writes through `Storage`.
2. **A reopen that cannot find the log refuses.** It does not report that the run
   needs no recovery. This is not politeness: a writer `journal.open` against an
   existing-but-empty directory establishes a *fresh* log, a fresh log has nothing
   pending, `Overlay::bind` therefore does not poison, and `requires_recovery()`
   would answer `false` — for a run that may have crashed mid-transaction. `run`
   mints a nonce, writes it to the store's `control/journal-id`, and writes it
   **into the log itself**, as the log's own first record; `resume` requires the
   log to open with that identity. Missing or mismatched is a structured refusal
   naming the missing evidence.

   The in-log half is what makes the check mean anything, and it is the half that
   was missing: an earlier shape compared the store's copy against a `journal-id`
   file sitting *beside* the log, and removing the log alone left that sidecar
   matching, so the reopen created a fresh log and reported "requires no
   reconciliation", exit 0.

   **Scope, precisely.** This closes the *vanished* log: one whose first record is
   not this run's identity, or which has no records at all. It does **not** close
   a log silently truncated to a shorter but still-valid prefix — catching that
   needs the run's reached sequence persisted where the log cannot forge it, which
   is durability-barrier work this slice does not do. A *torn* tail is caught
   separately, by `JournalTailRecovery`, and `resume` refuses that too.

Do not "fix" that refusal by creating the directory, seeding a nonce, relaxing the
comparison, or giving the sealing record a fresh operation id. Each one turns a
missing-evidence refusal back into a clean verdict.

### Proving it

The slate thread/fork tests also require `UMBRA_SLATE_EVIDENCE`, an absolute
writable evidence directory. C10-10 checks the live `native.rs` content SHA-256
against its characterized pin using `/usr/bin/shasum`. A mismatch fails with
`RE-CHARACTERIZE C10-10`: repeat the structural and runtime characterization
before updating the pin. No snapshot file or Git history is required:

```sh
export UMBRA_SLATE_EVIDENCE="$(mktemp -d /tmp/umbra-slate.XXXXXX)"
# With live Ganesha, UMBRA_NFS_RAW_FIXTURE and the provider binaries prepared:
"$UMBRA_SERIAL_TEST" cargo test \
    -p umbra-storage-nfs-userspace --features transport-raw \
    --test userspace_run slate_ -- --test-threads=1
```

Set `UMBRA_SERIAL_TEST` to the path of your serial wrapper; it is a local test prerequisite. C10-10 builds its guard-test
executable before entering the bounded execution window; build failure is a
prerequisite failure. The launcher uses a 60-second execution deadline and a
10-second cleanup grace (`slate_bounded`/`slate_bounded_for`). The group-scoped census permits at most three attempts for missing readings,
each capped at 400ms including a 100ms kill/reap reserve and by the phase
deadline. Completed readings return immediately; cleanup actions may produce
progress observations while grace remains. Terminal failure or exhausted
unavailable census denies qualification; case oracles are recorded before final
cleanup qualification. `ZOMBIES_ONLY` records dead table entries explicitly and
requires no live survivors, direct-child reap and pipe EOF.

Results qualify measured runs, not all schedules. C10-10 requires both the
behavioural refusal and the unchanged structural assertion; together they still
do not prove the guard fired at the guarded instruction. C10-15 observes Tokio
task-level results plus independent stored bytes and lengths, not blocking-worker
syscall errno or TID attribution. Attribution loss, wrong data, premature
completion and timeout remain falsifiers. Writer-authority reopen is separate
from journal completion. The isolated Tokio fixture builds with its own locked
Cargo package; it is not a root workspace member.

`tests/userspace_run.rs` (feature `transport-raw`, macOS arm64, gated on
`UMBRA_NFS_RAW_FIXTURE`) drives the real `umbra` binary against a live server and
checks the result three independent ways — through the raw client, through the
run's journal, and against the host. Seven **mutation probes** — cargo features
on `umbra-overlay` (`read`, `write`, `fstat`, `dircache`),
`umbra-platform-macos` (`mkdir`, `setattrlistat`) and `umbra-supervisor`
(`readdir`), so no product build contains any of them — break one routed
direction, one routed call, or one cache invariant each, and each must fail
distinguishably from the other six:

```sh
# end to end: the toy, the Rust I/O fixture and the utility matrix must pass
cargo build --workspace --bins
cargo test -p umbra-storage-nfs-userspace --features transport-raw --test userspace_run

# probe A -- read routing corrupted; the toy's compare must reject (exit 8),
# and the export must still hold the correct bytes
cargo build -p umbra-cli --features mutation-probe-read
UMBRA_MUTATION_PROBE=read cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe B -- write routing dropped but reported successful; the read-back must
# come up short (exit 7), and the export object must be absent or zero-length
cargo build -p umbra-cli --features mutation-probe-write
UMBRA_MUTATION_PROBE=write cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe C -- `fstat` on a virtual descriptor answers EBADF; `touch` on an absent
# operand must fail with its empty file still in the export
cargo build -p umbra-cli --features mutation-probe-fstat
UMBRA_MUTATION_PROBE=fstat cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe D -- the `mkdir`(136) decode arm removed; the directory must appear
# nowhere while `touch` on an absent operand still exits 0.
# NOTE the crate: this probe is on the provider executable, not on `umbra`, and
# `cargo build -p umbra-cli --features mutation-probe-mkdir` does not exist.
# Building it does not rebuild `umbra`, so clear probe C first or two probes are
# live in one run.
cargo build -p umbra-cli
cargo build -p umbra-platform-macos --features mutation-probe-mkdir
UMBRA_MUTATION_PROBE=mkdir cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe E -- `setattrlistat`(524) refused; `touch` on an existing file must fail
# while `touch` on an absent one still exits 0. Same crate as probe D, so this
# feature swap rebuilds the provider and clears the previous probe on its own.
cargo build -p umbra-platform-macos --features mutation-probe-setattrlistat
UMBRA_MUTATION_PROBE=setattrlistat cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe F -- each directory entry's name reversed; the listing must come back
# with the right number of entries, each a permutation of a real name, while the
# run still exits 0 -- no refusal of a directory read changes the exit status, so
# only the names discriminate. Probe D's note in the other direction: this probe
# is on `umbra`, so clear the provider first.
cargo build -p umbra-platform-macos
cargo build -p umbra-cli --features mutation-probe-readdir
UMBRA_MUTATION_PROBE=readdir cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run

# probe G -- a closed descriptor's residual directory page is never evicted, so
# a second enumeration of a reopened directory is served the first one's empty
# remainder. The run still exits 0 and the first pass is still correct, so only
# the second listing's names discriminate: it must come back empty where the
# unmutated run reports the whole tree. Same crate as probe F, so this feature
# swap rebuilds `umbra` and clears the previous probe on its own.
cargo build -p umbra-cli --features mutation-probe-dircache
UMBRA_MUTATION_PROBE=dircache cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw --test userspace_run
```

`cargo build -p umbra-cli --features ...` overwrites `target/<profile>/umbra` and
`cargo build -p umbra-platform-macos --features ...` overwrites the provider
executable beside it, so rebuild **both** without features before running the
baseline again.

## Golden fixtures

`tests/goldens/` pins the on-server bytes an existing run has, so the mounted and
userspace adapters cannot drift apart silently:

| Fixture | Purpose |
| --- | --- |
| `run-layout.txt` | Names, kinds and modes under a run directory |
| `manifest.json` | `.provider/manifest`: `[run_id, immutable_base, format_version]` |
| `epoch.bin` | `.provider/epoch`: a little-endian `u64`, created as zero |
| `writer-lock.bin` | `.provider/writer.lock`: the 16 raw UUID bytes of a writer token |
| `retry-intent.json` | A retry record before dispatch: `[request, null]` |
| `retry-result-ok.json` | A settled success: `[request, {"Ok": …}]` |
| `retry-result-err.json` | A settled failure, the permanent answer for its key |
| `retry-file-names.txt` | `key-<hex idempotency key>` and the `op-<uuid>` index |

`tests/goldens.rs` rebuilds each from the same `umbra-core` DTOs the mounted
adapter serialises and compares bytes, so an encoding drift fails a test instead of
surfacing on a live run. Regenerate deliberately with `UMBRA_GOLDEN_UPDATE=1` and
review the diff.

## Tests

`cargo test -p umbra-storage-nfs-userspace` runs the unit tests, the golden and
provider-template suites, `tests/fake_fault_matrix.rs`,
`tests/operations_surface.rs`, `tests/authority_recovery.rs`,
`tests/review_round_1.rs`, `tests/review_round_2.rs`,
`tests/coderabbit_round_1.rs`, and the fake half of `tests/m1_conformance.rs` and
`tests/golden_compat.rs`. Each review suite names the findings it closes in its
test names, so a regression points at the finding it reopens. There is no network, mount, service, fixture directory
or environment gate: the two harnesses that can use a server compare against the
fake when none is configured.

`tests/review_round_1.rs` covers the findings of the first independent review,
one case per finding id, each stating in its own doc comment what the pre-fix
code did and asserting the corrected behaviour: latched authority loss and
outstanding-I/O derivation, bound-run and writer-epoch validation, run-policy
refusal, persisted-manifest and epoch-floor validation, durable replay through
the retry journal, the no-replace refusal, the applied create mode, preserved
NFS statuses, and the filesystem-boundary check. Two of them install a transport
decorator rather than a fault plan, because a nested export and a changed `fsid`
are answers a server gives rather than faults it injects.

The fake transport is a shape fake: it answers the operations M1 needs and models
OPEN_CONFIRM, exclusive-create verifier reuse, short writes, write/commit
verifiers, grace and reclaim, and cookie invalidation. Everything else answers
`NFS4ERR_NOTSUPP` rather than pretending.

Its directory change attribute is faithful in both directions, which matters
because the interrupted-create recovery decides on that attribute. A create —
through `CREATE` or through a creating `OPEN` — advances the parent's
`FATTR4_CHANGE`, as [RFC 7530 §5.8.1.4][rfc-change] requires of a server whose
directory has changed, and the operation's `cinfo` reports that same transition
rather than a canned pair. An `OPEN` that creates nothing leaves it alone.
A fake that suppressed the create-through-`OPEN` bump would let a recovery accept
evidence no conforming server can produce.

[rfc-change]: https://www.rfc-editor.org/rfc/rfc7530.html#section-5.8.1.4

`tests/fake_fault_matrix.rs` drives ten state transitions against all five
`FaultPoint` values and all five `FaultAction` values, and asserts each
transition's invariant rather than one expected outcome — a fault may
legitimately produce success, a server rejection or an unknown result, and what
must hold in all three is that no state advanced beyond what the transport
proved. The fake acts on a given action only where it means something (`Fail` at
`BeforeDispatch` and `BeforeReturn`, `Substitute` at `AfterDispatch`,
`DropReply` at `OnDeadline`, `RotateVerifier` at `BeforeReturn`); `OnConnection`
is never consulted and short writes are driven by `FakeTransport::set_write_cap`.
Cells where the action is inert at that point still run and still assert the
invariant, so no cell claims coverage the fake does not provide.

`tests/operations_surface.rs` drives the operations surface over
`integration::StateSession` carrying a real client incarnation — SETCLIENTID,
SETCLIENTID_CONFIRM, open-owner minting, OPEN_CONFIRM sequencing and CLOSE all
happen as they would on the wire, with only the transport faked. It asserts
`Backend::is_live` is false, so no result there can be read as a live-server
one. It covers identity across a rename, in-place edit visibility through an
open handle, a pathname replacement leaving open handles on the original object,
retention until `CLOSE`, bounded paging with explicit cursor invalidation,
`UNSTABLE` to `COMMIT` verifier matching and its typed failure, and the refusal
of every unsupported and deferred operation.

`tests/authority_recovery.rs` is one test per crash window in
`authority::outage::CrashWindow::ALL` — the failure model's taxonomy, with its
umbra-crash row split into the before, mid and after-write windows — driven
through a `StateSession::over_fake` under fault injection. Each asserts the
state-machine transition and, wherever the window produced a failure, that the
original `NFS4ERR_*` is still readable verbatim afterwards; the tracee-crash and
in-grace-reclaim windows produce none and assert no status. Every crash-window
test then reads the durable marker back and asserts who holds it, at which epoch,
and that no timeout moved either. The competing-session test runs two genuinely
separate sessions, with separate client ids and separate open owners, over one
shared fake server: the first is admitted at epoch 1 and the second is denied,
repeatedly, naming the actual holder.

Because `FakeReplayLog` is in memory, every `RetainedError` it carries reports
`is_durable() == false`, and the suite asserts that. Consequently those tests
assert the *recovery plan* a durable journal would license rather than performing
a replay: under the fake no evidence survives a process to replay from, and a
test that pretended otherwise would be passing on volatile evidence.

The suites that use a server — `tests/raw_smoke.rs`, `tests/fault_matrix.rs`,
`tests/live_state.rs`, `tests/m1_conformance.rs` and `tests/golden_compat.rs` —
need `--features transport-raw` and use one only when
`UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names it. They speak NFSv4.0 from user
space and mount nothing.

The [`experiments/nfs-raw/`](../../experiments/nfs-raw/) NFS-Ganesha container is
the reference fixture for this suite. It exports two paths off the pseudo-root:
`/export/{probe,pagedir}` (seeded with the files the fault-matrix and live-state
cases open by name plus 120 entries for the paging case) and `/umbra/runs`
(where the conformance and golden-compat suites create run directories). CI
runs the whole raw-transport suite against it on `ubuntu-latest`, skipping
`m1_conformance::the_namespace_mutations_the_hotfix_added_run_end_to_end`
because Ganesha's VFS FSAL cannot report atomic REMOVE `change_info4`. The
same test passes under the mounted adapter, which is why this skip is a
fixture-capability gap and not code drift. Lifting it needs a Ganesha FSAL
with atomic REMOVE, and is out of scope here.

`tests/m1_conformance.rs` drives the provider through the public `Storage`
contract over both backends, asserting which one answered. It covers admission
before any binding is published, a second session denied by name, denial that
repeats because no clock is consulted, release-then-admit at the next epoch, the
whole namespace surface end to end, and that an open run answers a qualified
`flush`, keeps `Fencing::ReadOnly`, and claims no kernel shadow and no physical
path.

`tests/golden_compat.rs` is the sequential existing-run compatibility half:
it creates a run through the provider, reads the result back over NFSv4.0, and
compares the layout, `.provider/manifest` and `.provider/epoch` byte for byte
against `tests/goldens/`. The layout comparison is taken on the run as
`CreateNew` left it; a cooperative succession additionally leaves a
`writer.lock.claim.<epoch>`, which is this provider's own state rather than part
of the layout the mounted adapter writes, so it is asserted separately. It also asserts the one-way `writer.lock` limit rather
than leaving it to prose. `fault_matrix.rs` drives every `FaultPoint` against
every `FaultAction` — `assert_eq!(cells.len(), 30, "5 fault points x 6 fault
actions")` — asserting for each cell both that the transport consulted that fault
point and that the outcome matched, so an action that carries no meaning at a
point is asserted to be ignored rather than left untested. It also carries the
retirement case: a one-byte reply budget makes a real `GETATTR` overflow inside
`decode`, and with one concurrent call allowed, a registration that survived the
failure would refuse the next submission `QueueFull`. Ten consecutive over-budget
calls each reporting the decode failure, followed by ordinary calls that succeed,
is what "retirement happens on every return path" means in practice rather than
in prose.

`tests/live_state.rs` is the same idea one layer up: the protocol state machine
driven over the **live** transport. It runs the six transitions this layer can
fault meaningfully (SETCLIENTID, SETCLIENTID_CONFIRM, OPEN, OPEN_CONFIRM, CLOSE,
`OP_RENEW`) against all five fault points and five fault actions —
`assert_eq!(cells, 150, "6 transitions x 5 fault points x 5 fault actions")` —
asserting the one-directional invariant the fake matrix settled on: the client
may never claim more than the transport proved. It matters alongside the fake
matrix because the fake never consults `OnConnection` and honours each action at
a single point, while `LibnfsRawTransport` consults all five, so cells that are
inert against the fake are real here. The same file carries the acceptance
scenarios — anchored OPEN with the OPEN_CONFIRM the server actually demands,
bounded READDIR paging with cookie-verifier continuity, WRITE UNSTABLE to COMMIT
with verifier matching and a typed retained error when a restart changes the
verifier, an idle longer than the lease held open by `OP_RENEW`, and v4.0
`CLAIM_PREVIOUS` reclaim in grace with a safe surrender outside it.

Tests that restart the server additionally need
`UMBRA_NFS_FIXTURE_CONTAINER=<name>` and skip without it rather than proving
less. **Run the live suite with `-- --test-threads=1`**: several tests restart
the shared fixture. A server that has just restarted answers `NFS4ERR_GRACE` to
any open that is not a reclaim, which is correct behaviour, so the harness waits
that window out under a bounded budget instead of reading it as a failure.

## Deferred

`authority::AdmissionControl` is bound: `open_run` acquires admission and
`close_run` releases it.

Durable replay is bound too. Every supported mutation `execute` dispatches goes
through [`journal`](src/journal.rs) first: the exact request is recorded in
`.provider/retries/key-<hex>` before the operation reaches the wire, and the
exact settled result is written back afterwards, in the byte encoding the mounted
adapter uses and `tests/goldens/retry-*.json` pins. An exact-key retry is
answered from its record and never re-dispatched; an outcome whose server-side
disposition is unknown is left unsettled rather than recorded as a failure a
later retry would trust. Reads are not journalled.

An **interrupted** intent is recovered rather than stalled. Beside each record,
`.provider/retries/pre-<hex>` holds the object and parent identities the mutation
was dispatched against — the preconditions the failure model requires, kept in a
sidecar so `key-<hex>` stays byte-identical to what the mounted adapter writes.
Recovery compares that before-state against the server now: proven-not-applied
re-dispatches under the same key, proven-applied settles the record from the
observed state without repeating the effect, and evidence matching neither is a
blocked-recoverable stop with the record and the server state both retained.
A create is never settled from observation at all — the `EXCLUSIVE4` verifier is
the only thing that can say whose object is behind the name, so the decision goes
back to the server. Which is why the parent directory's change attribute does not
gate a create whose name is now present: a successful create moves that attribute
itself, so requiring it to stand still would require evidence success rules out.
Where the name is still absent, a moved parent is somebody else's work and the
run stops rather than guessing. A
record with no sidecar is a legacy one, and an ambiguous legacy intent stops for
the same reason. Nothing answers "requires reconciliation": the failure model
forbids that standing in for recovery in a window this provider supports.

**A stop is a state, not a return value.** A blocked-recoverable refusal, a record
that cannot be decoded, and any failure while resolving an interrupted intent —
including one whose own error says nothing about recovery, such as a target path
that no longer resolves — put the run into `BLOCKED_RECOVERABLE`: later mutations
are refused with the original diagnosis carried forward, the evidence is left
exactly where it is, and a cooperative release cannot succeed. It clears only by
reopening the run, which is the operator intervention the state is for.

Separately, a call whose server-side disposition could not be established stops
the run admitting *new* work. The failure model asks for both halves — "stop new
mutations and quiesce; resolve bounded outstanding operations" — and resolving an
outstanding operation means retrying its own key, so a request under an unrelated
key is refused while the outstanding key's own retry is the recovery.

**The ledger is an obligation per call, not a list of error strings.** Each entry
carries the mutation's *idempotency key* — the identity `.provider/retries` is
keyed on, and the one a caller keeps when it mints a fresh operation id for a
retry — and the dispatch phase the call was interrupted in. That is what makes
the second half reachable. A reply lost on the journal lookup or the precondition
observation is lost on a strictly read-only round trip, before any intent exists;
when that key's next attempt reads the journal cleanly and finds no record, the
absence proves the obligation discharged, because nothing that mutates was ever
submitted under it. The gate then admits the retry as its own operation.

The discharge is deliberately narrow. A reply lost on the intent write itself, or
on the mutation, leaves a create or an effect that may still land, so absence
alone proves nothing about it: only that key's own settled record discharges it —
recovery reconstructing the outcome, or a redispatch that settles. Obligations
under other keys are untouched by either, and a blocked recovery and a latched
write failure are terminal states rather than obligations, so neither is ever
discharged. A run whose obligations are all discharged mutates and releases
normally again; one still holding any of them does neither.

Records are read in bounded chunks to end of file and written in as many round
trips as the server needs, because a short `READ` or `WRITE` is a legal answer
rather than a frame boundary, and the raw transport caps a reply well below the
size a `WriteAt` record reaches.

Every small whole-file reader in the crate works the same way, through
`crud::read_whole`: `.provider/manifest` and `.provider/epoch` on the run-open
path, and the admission marker. [RFC 7530 §16.25.4](https://www.rfc-editor.org/rfc/rfc7530.html#section-16.25.4)
lets a server answer with fewer bytes than requested and leave `eof` clear, so a
reader that decoded the first reply as the whole object turned a healthy run into
a corrupt-file refusal or a healthy marker into a malformed one. The loop
advances by what arrived and asks only for the capacity that remains; a reply
with no bytes *and* no end of file is refused rather than looped on, and reaching
the bound before end of file is reported to the caller, which decides what its
own format makes of it.

How much it asks for is derived from `TransportLimits::max_read_payload`, not
from `max_reply_bytes`. A reply's budget is not all payload: `RawTransport::read`
sends the four-byte `READ_TAG`, the server echoes it, and the raw decoder charges
the echoed tag and the READ data against the same figure. Asking for the whole
budget therefore asks for a reply that cannot fit inside it — under a 128-byte
budget a full reply costs 132 and is refused as malformed, so a healthy file
failed to read. A budget with no room for the tag *and* a byte of progress is
refused before anything is dispatched, because there is no chunk size that would
work and rounding back up to one byte would reissue the over-budget request. The
retry journal's own record reader derives its chunk the same way, and the
`max_io_bytes` a binding advertises is the read payload rather than the whole
budget, so a `ReadAt` of exactly the advertised size is one the provider can
actually answer.

A write commits and compares verifiers before its open is released: an `UNSTABLE`
write is durable only once a `COMMIT` returns the verifier the `WRITE` did. A
journalled mutation that fails with an I/O status is latched — later mutations
stop with the original status carried forward, and the release that follows is
not reported clean. That covers a failed `FILE_SYNC4` journal write for a rename
or a create, not only a failed `WriteAt`.

`flush` issues a receipt over the run's already-committed writes rather than
reporting a gate; it performs no I/O and re-COMMITs nothing. Its claim is the
durability the run qualified for, and a `Remote` claim rests on the
matched-verifier `COMMIT` each `WriteAt` already performed — writing a record `FILE_SYNC4` asks the server for stability, but the
barrier's claim is not made from the journal write. `authority::MutationJournal`
remains the typed lower-layer model over the `ReplayLog` facade and is not itself
on the `Storage` path.

Held-object coherence is demonstrated against a real server.
`tests/live_state.rs`'s `r2_006_*` case runs two NFSv4 clients — separate client
ids, separate open owners — and holds client A's open while client B edits in
place, truncates, renames, replaces the name with a different object and finally
removes the object's last name. A's handle keeps its `fsid`/`fileid` throughout,
sees B's edit and truncation, still reads after the replacement and after the
unlink, while a fresh open and a `READDIR` see the replacement instead. B is an
external editor, not a second admitted Umbra session.

A second live case, `r3_005_*`, covers the branch the first one does not: B stages
a distinct object under a temporary name and atomically `RENAME`s it over a name
that is *still occupied* by the object A holds open. A's `fsid`/`fileid` and its
exact bytes are unchanged; a fresh open, stat and `READDIR` all see the
replacement; the staging name is gone; and A's open is the only remaining
reference to the now-unnamed original. Renaming the old object away first and
creating into the vacated name is name reuse, not atomic replacement, and the two
are asserted separately.

`tests/operations_surface.rs` still covers the same shape against the fake, whose
object table is permanent by construction; that case is a shape check, and the
live ones are the acceptance.

`OPEN_DOWNGRADE` remains unavailable: `transport::Nfs4Op` has no variant for it,
and the hotfix that added the four namespace mutations deliberately did not widen
further. `LOCK`/`LOCKU` are allocated and sequenced but never dispatched.
`CopyUp` needs an immutable-base materialisation seam that lives above storage.

`writer.lock` compatibility is one-way. This provider reads the mounted adapter's
16-byte legacy token, but writes the 256-byte extended record that also carries
epoch and phase, so a run this provider has admitted is not one the mounted
adapter can parse as a bare token. `tests/golden_compat.rs` pins that rather than
leaving a reader to assume byte equality.

Overlay and session recovery, fencing, and remote-storage power-loss
qualification are later milestones; split-brain resolution is explicitly deferred
to M3 fencing authority.

### Durability — what `Remote` asserts, and what it does not

The only remote durability this crate qualifies is the matched-verifier `COMMIT`
barrier `flush` certifies: **the server acknowledged, via a `COMMIT` whose
verifier matched the `WRITE`'s, that every byte in scope reached its stable
storage, and nothing in scope is outstanding.** That is exactly what a
`Durability::Remote` receipt asserts, and no more. It asserts **nothing** about
the server's hardware, the export's `fsync` policy or its media; nothing about
fencing (`Fencing::ReadOnly` is unchanged); no atomic snapshot; and no continuous
persistence. `require_strict_remote_persistence` — a stronger promise than this
barrier — is still refused. Running against a live server proves the barrier
holds; it proves nothing about persistence after power loss, and the advertised
capabilities say exactly that.

`Remote` is not a constant. It is claimed for a run only when **both** gates held
during that run's `open_run`:

1. the bound transport declared `PersistenceBoundary::RemoteServer` — a defaulted
   `RawTransport` method whose default is `PersistenceBoundary::Unqualified`, so a
   transport that declares nothing degrades rather than over-claims; and
2. a synthetic `OPEN` / `WRITE(UNSTABLE)` / `COMMIT` / verifier-compare / `CLOSE` /
   `REMOVE` probe against the run's own `.provider` succeeded, proving the
   matched-verifier `COMMIT` cycle on that mount for that invocation.

Neither gate suffices alone: an in-memory fake returns a matching verifier and so
passes any probe, while a declaration alone says nothing about a particular mount.
A read-only run cannot be probed and therefore claims `Durability::Local`. The
qualification is per `open_run` and is dropped with the run, so a reopened run is
probed again. A build with no live transport linked — the default, since the live
backend sits behind the off-by-default `transport-raw` feature — therefore ships
`Local`, never an unproven `Remote`.
