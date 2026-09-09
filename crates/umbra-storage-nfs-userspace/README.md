# umbra-storage-nfs-userspace

**Current state: frozen facades, a provider scaffold, and a raw-RPC transport
behind an off-by-default feature.** This crate holds the private facade contracts
for Umbra's userspace NFSv4.0 backend and a `Storage` implementation whose methods
return `NotImplemented` naming the node that will wire them, or
`UnsupportedCapability` where the semantics will not be offered.

A default build performs no I/O: it binds no transport, advertises no capability,
needs no native toolchain, and no test contacts a server. Building with
`--features transport-raw` adds `transport::raw::LibnfsRawTransport`, which does
speak NFSv4.0 to a server. `Storage` is unaffected either way — no `Storage`
method reaches the transport yet, so the provider still opens no run.

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

`src/storage.rs` holds the provider configuration and the `Storage` scaffold;
`src/lib.rs` exports the provider id and the run-layout constants.

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
rather than silently desynchronising.

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

`cargo test -p umbra-storage-nfs-userspace` runs unit tests plus the golden and
provider-template suites. There is no network, mount, service, fixture directory
or environment gate.

The two suites that do use a server, `tests/raw_smoke.rs` and
`tests/fault_matrix.rs`, need `--features transport-raw` and skip unless
`UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names one. They speak NFSv4.0 from user
space and mount nothing. `fault_matrix.rs` drives every `FaultPoint` against
every `FaultAction` — `assert_eq!(cells.len(), 30, "5 fault points x 6 fault
actions")` — asserting for each cell both that the transport consulted that fault
point and that the outcome matched, so an action that carries no meaning at a
point is asserted to be ignored rather than left untested.

The fake transport is a shape fake: it answers the operations
M1 needs and models OPEN_CONFIRM, exclusive-create verifier reuse, short writes,
write/commit verifiers, grace and reclaim, and cookie invalidation. Everything else
answers `NFS4ERR_NOTSUPP` rather than pretending.

## Deferred

Protocol state, operations, admission and recovery wiring, and every acceptance
criterion in the M1 plan. No `Storage` method reaches the raw transport yet. Overlay and session
recovery, fencing, and remote-storage power-loss qualification are later
milestones. Nothing in this crate qualifies remote durability.
