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
`UnsupportedCapability`; `flush` answers `NotImplemented`, because a durability
receipt would assert a persistence boundary nothing here has qualified.

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

Three rules shape it.

1. **Illegal transitions do not compile.** A `ConfirmedClient` is reachable only
   through a `SETCLIENTID_CONFIRM` the server accepted; an `OwnerLease` is consumed
   by its OPEN attempt; `close` consumes the `OpenFile`; an unconfirmed open's
   `stateid()` keeps refusing. None of these is a runtime guard.
2. **Nothing advances further than the server proved.** A server answer resolves
   the seqid by the RFC 7530 section 9.1.7 rule and leaves the owner reusable. An
   answer that never arrived poisons the owner instead, because the seqid the
   server observed cannot be inferred from silence. The cost is one retired owner
   name per lost reply.
3. **Renewal is not authority.** `LeaseClock::takeover_by_timeout` always returns
   `AuthorityError::TakeoverRefused`. An expired lease means this client's own
   state may be gone; it is never evidence that another session terminated.

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
4. **A page is bounded at every level.** One `List` issues at most
   `pages::MAX_SERVER_PAGES` `READDIR` operations, each capped by `max_count`,
   and returns at most the requested limit; nothing accumulates a whole
   directory. A changed `cookieverf` invalidates outstanding cursors explicitly
   rather than restarting silently or answering from a cached snapshot.

A `WriteAt` records a durable intent through the replay facade before dispatch,
reports the count and stability the server actually reached rather than the ones
requested, and writes `UNSTABLE`. An unstable write is durable only once a
`COMMIT` returns the verifier the `WRITE` did; a changed verifier is
`ReplayError::VerifierChanged`, meaning the bytes must be rewritten from the
retained payload. No receipt is issued here, so nothing claims persistence.

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
  whiteouts; `AtomicSwap`. `OUT_OF_SURFACE` adds file-backed `mmap`, ACLs,
  `flock`, and — out of scope by the syscall matrix's notifications decision —
  `kqueue`/`kevent` with `EVFILT_VNODE` and FSEvents. Nothing in this crate
  registers, delivers or emulates a file-change notification.
- **Deferred**, answered with `ErrorKind::NotImplemented` naming the owner:
  `CopyUp`, which needs a base-materialisation seam that lives above storage, and
  `flush`, whose receipt would assert a persistence boundary nothing here has
  qualified.

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

**No live transport is constructed for `Storage` here.** `NfsUserspaceStorage`
runs its operations over whatever `with_facades` was given, which is the fake in
every test in this crate. Constructing a `LibnfsRawTransport` for the provider,
and the acceptance that goes with it, is deferred.

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
   stand behind.
3. **Nothing dispatches before its intent is durable.** `DispatchTicket` has no
   public constructor; the only way to get one is `MutationJournal::begin`
   returning `Acknowledged::Dispatch`, which happens after the intent, the
   payload and the authorising epoch have reached the log. Backpressure is
   applied before that admit, so an exhausted buffer refuses the mutation instead
   of letting it reach the wire with nowhere to record its outcome.
4. **The first error is latched.** `OutageMachine` keeps the failure that opened
   a window in a frozen `RetainedError` and counts later attempts without
   replacing it, so an `NFS4ERR_NOSPC` is still 28 after three reconnects. A
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
is three calls on one name and nothing else — no path resolution, no anchoring,
no capability. Binding it into `Storage` is deferred.

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
supplies opaque session-bound handles and no `physical_path`, and `umbra run`
cannot select it.

`open_run` establishes a client incarnation, resolves or creates the run's
anchors over the bound transport, acquires product admission, and only then
publishes a binding whose advertised `max_io_bytes` and `max_directory_entries`
come from that transport's own limits. `Durability::None` and `Fencing::ReadOnly`
stay: no persistence boundary is qualified and no independent termination
verifier exists. `OpenRunIntent::CreateNew` writes the run directory, both
contract anchors, `.provider/`, `.provider/retries/`, `.provider/epoch` and
`.provider/manifest` with the mounted adapter's names, modes and encodings. It
does not create the export or run-parent directories: those are deployment
configuration, and creating a missing one would silently relocate every run.

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
`tests/operations_surface.rs`, `tests/authority_recovery.rs`, and the fake half of
`tests/m1_conformance.rs` and `tests/golden_compat.rs`. There is no network,
mount, service, fixture directory or environment gate: the two harnesses that can
use a server compare against the fake when none is configured.

The fake transport is a shape fake: it answers the operations M1 needs and models
OPEN_CONFIRM, exclusive-create verifier reuse, short writes, write/commit
verifiers, grace and reclaim, and cookie invalidation. Everything else answers
`NFS4ERR_NOTSUPP` rather than pretending.

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

`tests/m1_conformance.rs` drives the provider through the public `Storage`
contract over both backends, asserting which one answered. It covers admission
before any binding is published, a second session denied by name, denial that
repeats because no clock is consulted, release-then-admit at the next epoch, the
whole namespace surface end to end, and that an open run claims no durability, no
fencing, no kernel shadow and no physical path.

`tests/golden_compat.rs` is the sequential existing-run compatibility half:
it creates a run through the provider, reads the result back over NFSv4.0, and
compares the layout, `.provider/manifest` and `.provider/epoch` byte for byte
against `tests/goldens/`. It also asserts the one-way `writer.lock` limit rather
than leaving it to prose. `fault_matrix.rs` drives every `FaultPoint` against
every `FaultAction` — `assert_eq!(cells.len(), 30, "5 fault points x 6 fault
actions")` — asserting for each cell both that the transport consulted that fault
point and that the outcome matched, so an action that carries no meaning at a
point is asserted to be ignored rather than left untested.

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
`close_run` releases it. `authority::MutationJournal` is **not** yet on the
`Storage` path — durable intents exist and are tested, but no contract method
routes through them, so `flush` still reports its gate rather than issuing a
receipt.

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
to M3 fencing authority. **Nothing in this crate qualifies remote durability.**
Running against a live server proves the protocol works; it proves nothing about
persistence after power loss, and the advertised capabilities say so.
