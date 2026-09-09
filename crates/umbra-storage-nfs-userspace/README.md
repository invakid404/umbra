# umbra-storage-nfs-userspace

**Current state: frozen facades, the Umbra-owned NFSv4.0 protocol state machine,
a raw-RPC transport behind an off-by-default feature, and a storage operations
surface running over whichever transport is injected.** This crate holds the
private facade contracts for Umbra's userspace NFSv4.0 backend, the client state
machine in `src/state/` that drives them, the operations surface in `src/ops.rs`
and the modules beside it, and a `Storage` implementation that opens an existing
run, resolves paths, stats, enumerates, reads, creates files and writes. Writer
authority, admission and durability receipts still answer `NotImplemented`
naming `authority_recovery`; semantics this provider will not offer answer
`UnsupportedCapability`.

A default build needs no native toolchain and no test contacts a server: every
suite in this crate runs against the in-memory fake in `src/fake.rs`. Building
with `--features transport-raw` adds `transport::raw::LibnfsRawTransport`, which
speaks NFSv4.0 to a server, and lets the same code drive a real one. A transport
is injected through `NfsUserspaceStorage::with_facades`; nothing here constructs
a live one for the provider, so `Storage` performs I/O only against what the
caller supplied.

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
  `Stat`, `List`, `ReadAt`, `WriteAt`, and `Create` of a file.
- **Unsupported**, answered with `ErrorKind::UnsupportedCapability`: hard links;
  logical symlinks and `ReadLink`, which are overlay-owned; extended attributes;
  whiteouts; `AtomicSwap`. `OUT_OF_SURFACE` adds file-backed `mmap`, ACLs,
  `flock`, and — out of scope by the syscall matrix's notifications decision —
  `kqueue`/`kevent` with `EVFILT_VNODE` and FSEvents. Nothing in this crate
  registers, delivers or emulates a file-change notification.
- **Deferred**, answered with `ErrorKind::NotImplemented` naming the owner:
  `Unlink`, `RemoveDirectory`, `Rename`, `Create` of a directory,
  `CreateParents`, `SetMetadata`, `Truncate`, and `OpenRunIntent::CreateNew`.

The deferred set is a contracts gap, not a capability decision. The frozen
`transport::OpCode` enumerates `Remove`, `Rename`, `Create` and `SetAttr`, but
the frozen `transport::Nfs4Op` — the union that carries arguments into a
COMPOUND — has no variant for any of them, so no COMPOUND this crate can build
encodes one. `namespace::NamespaceDispatcher` is the typed seam, injected and
unbound, exactly as `state::open_owner::downgrade_with` already handles the same
gap for `OPEN_DOWNGRADE`. Closing it is a contracts revision, not work a
consumer may do by widening the facade.

`namespace::apply` enforces one rule whoever dispatches: a rename moves a name,
not an object, so an outcome reporting an identity other than the one the caller
pinned is refused rather than believed.

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

`open_run` resolves an existing run's anchors over the bound transport and
publishes a binding whose advertised `max_io_bytes` and `max_directory_entries`
come from that transport's own limits. `Durability::None` and `Fencing::ReadOnly`
stay: no persistence boundary is qualified and no independent termination
verifier exists. `OpenRunIntent::CreateNew` is deferred, because creating the run
directories needs the `CREATE` operation the frozen `Nfs4Op` cannot encode.

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
provider-template suites, `tests/fake_fault_matrix.rs`, and
`tests/operations_surface.rs`. There is no network, mount, service, fixture
directory or environment gate.

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

The suites that use a server — `tests/raw_smoke.rs`, `tests/fault_matrix.rs` and
`tests/live_state.rs` — need `--features transport-raw` and skip unless
`UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names one. They speak NFSv4.0 from user
space and mount nothing. `fault_matrix.rs` drives every `FaultPoint` against
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

Writer authority, admission, epochs, retry policy and durability receipts:
`acquire_writer`, `renew_writer`, `release_writer` and `flush` report the gate
naming `authority_recovery`. A mutation without a writer epoch, or without open
owners from a confirmed incarnation, is refused rather than performed.

Namespace mutation — `REMOVE`, `RENAME`, `CREATE` of a directory, `SETATTR` —
is typed and seamed but has no wire encoding in the frozen `Nfs4Op`; closing
that is a contracts revision. `OPEN_DOWNGRADE` is unavailable for the same
reason, and `LOCK`/`LOCKU` are allocated and sequenced but never dispatched.
`CopyUp` needs an immutable-base materialisation seam this provider does not
have.

No live transport is constructed for the provider here. Overlay and session
recovery, fencing, and remote-storage power-loss qualification are later
milestones. Nothing in this crate qualifies remote durability, and nothing in it
has been run against a live server.
