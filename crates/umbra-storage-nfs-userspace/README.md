# umbra-storage-nfs-userspace

**Current state: frozen facade interfaces, the Umbra-owned NFSv4.0 protocol state
machine, and a provider scaffold. No I/O.** This crate holds the private facade
contracts for Umbra's userspace NFSv4.0 backend, the client state machine in
`src/state/` that drives them, and a `Storage` implementation whose methods
return `NotImplemented` naming the node that will wire them, or
`UnsupportedCapability` where the semantics will not be offered. No live NFS
transport is bound, no capability is advertised, and no test here contacts a
server: the state machine runs against the in-memory fake in `src/fake.rs`.

**Ownership boundary.** This README owns `crates/umbra-storage-nfs-userspace/`:
`src/`, `tests/`, `Cargo.toml` and `provider.json`. The mounted adapter is a
separate boundary at [umbra-storage-nfs](../umbra-storage-nfs/README.md); the
contract this crate implements is [umbra-storage](../umbra-storage/README.md).

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
  buffers and verifier accounting. A transport implementation is expected to
  supply raw RPC/XDR and task primitives only.
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

`src/storage.rs` holds the provider configuration and the `Storage` scaffold;
`src/lib.rs` exports the provider id and the run-layout constants. `src/state/`
consumes the facades rather than adding to them.

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

The crate denies `unsafe_code`. A future libnfs binding is the only module that
may opt back in, and only with a documented safety contract, so the FFI stays
visible in review.

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

## Configuration and registration

`NfsUserspaceConfig` carries the server host and port, a server-relative `export`
and `run_parent`, the two anchor components, and a per-COMPOUND deadline. Export
and run parent are validated with the storage contract's own path rule, so neither
can be absolute or contain `.` or `..`. There is no mount root and no host path.

Options are the JSON encoding of that struct, carried as descriptor option bytes.
`provider.json` is an installation template; `tests/provider_template.rs` checks it
decodes as a storage descriptor for id `nfs-userspace` and that its options
validate. A userspace client has no kernel-visible run root, so this provider will
supply opaque handles and no `physical_path`, and `umbra run` will not be able to
select it. It binds no run today: `open_run` reports the gate.

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

`cargo test -p umbra-storage-nfs-userspace` runs 80 tests across six suites: unit
tests, the golden and provider-template suites, and `tests/fake_fault_matrix.rs`.
There is no network, mount, service, fixture directory or environment gate.

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

## Deferred

Transport implementation, operations, admission and recovery wiring, and every
acceptance criterion in the M1 plan that needs a live server. Within protocol
state, the `OPEN_DOWNGRADE` wire operation is unavailable and `LOCK`/`LOCKU` are
allocated and sequenced but never dispatched. Overlay and session recovery,
fencing, and remote-storage power-loss qualification are later milestones.
Nothing in this crate qualifies remote durability.
