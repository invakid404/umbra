# umbra-storage

Synchronous, backend-independent storage contracts. The only dependency is
`umbra-core`; this crate performs no filesystem I/O and selects no implementation.
Shared request/result, identity, error, capability, and byte-path types belong to
core and are re-exported here. There are no implementation-specific associated
types: every backend can be injected as `Box<dyn Storage>` or `&mut dyn Storage`.

`StorageCapabilities::features` is an open set of narrowly named qualified
behaviors, alongside the existing booleans. It is the extension point for behavior
too specific to deserve a boolean: a backend advertises only what it has actually
qualified, and a consumer requires names rather than recognizing backend
identities. `provider::serve_provider` maps that set into the handshake, so a
descriptor requiring a name cannot connect to a backend that does not offer it.
The field defaults to empty, keeping older encodings decodable; an empty set
grants nothing.

`Storage` owns one active run. Its required methods are `capabilities`, `open_run`,
`acquire_writer`, `renew_writer`, `release_writer`, `execute`, `flush`, and
`close_run`. Constructors and backend connection options live outside the trait.
The convenience methods `read_at`, `write_at`, `create`, `unlink`, `remove_directory`,
`list`, `stat`, and `atomic_swap` dispatch typed requests through `execute`;
implementations retain one place to enforce lifecycle, containment, idempotency, and
writer authority. They are trait defaults, so a backend that answers the corresponding
operation in `execute` acquires them without declaring anything.
All fallible methods use `umbra_core::Result`.

Paths preserve arbitrary non-NUL bytes, including non-UTF-8 names. Storage paths
are relative to an explicit root/control anchor; absolute paths and `..` are
rejected. Physical mount paths belong only to runtime bindings, never persistent
identity. Resolve each component using anchored operations and contain symlinks;
`canonicalize()` followed by path concatenation is not sufficient. The overlay
owns namespace policy, directory merging, and transaction sequencing.

Reads/writes and directory pages are bounded. Short reads and writes are allowed;
a zero-byte read on a nonempty buffer denotes EOF. File offsets are explicit and
must not modify a shared seek position. Create is exclusive; unlink removes a
name and preserves other hard links. Remove-directory removes an empty directory
name; a non-empty directory is the backend's error and never a recursive removal,
and the refusal's error kind is the backend's own rather than a shared one. Stat
does not follow the final symlink.
Atomic swap exchanges two existing names in one atomic operation, including when
they denote different object types if the backend advertises that support; it
must never be emulated as a sequence of visible renames. Atomic replacement is
available separately through the rename operation.

Every mutation carries a run ID, operation ID, idempotency key, and fencing epoch.
Repeated requests must not apply a mutation twice; reusing a key with a different
payload is an error. Reject the wrong run, stale epochs, stale handles, invalid
paths, unsupported capabilities, and mismatched response kinds with structured
errors. Copy-up accepts an approved base-object identity/handle, never an arbitrary
host path. Logical symlink targets are bytes and must not expose host symlinks to
the tracee. Metadata, xattrs, hard links, and whiteouts are typed primitives.

Acquiring, renewing, and releasing writer authority is a backend responsibility.
Lease expiry alone cannot justify takeover. Fencing must cover existing writable
kernel descriptors and mappings as well as later API calls; otherwise require
confirmed termination/quiescence of the former writer or refuse takeover.
Acquisition is atomic and competing writers cannot both succeed. Lease loss stops
mutations. Close surfaces persistence failures and must not silently discard an
active writer lease.

`flush` returns a durability receipt only after the requested data/metadata scope
has reached the advertised persistence boundary. An operation completing or an
atomic rename becoming visible does not imply durability. Local persistence is
never reported as qualified remote persistence. Kernel rewrite targets require
an adequate shadow filesystem; a blob-only service needs complete emulation or
must reject that capability.

## Adding a new storage backend

1. Create `crates/umbra-storage-<x>` with `src/lib.rs`, `src/main.rs`, and a README.
   Add `"crates/umbra-storage-<x>"` to the root workspace's `members`. Use this
   manifest, substituting the new package name:

   ```toml
   [package]
   name = "umbra-storage-example"
   version.workspace = true
   authors.workspace = true
   license.workspace = true
   edition.workspace = true
   publish = false

   [dependencies]
   umbra-storage = { path = "../umbra-storage" }
   umbra-core = { path = "../umbra-core" }
   ```

   These are its only direct Umbra dependencies. Put native bindings, service
   clients, and backend-specific configuration in the new package.

2. Define a backend struct and constructor in that package, then implement
   `umbra_storage::Storage`. Implement all eight required methods and dispatch
   every supported `StorageOperation` in `execute` to its corresponding response.
   Unsupported operations must fail before mutation. Return runtime shadow/control
   bindings, stable object identities, bounded pages, exact byte paths, structured
   lifecycle errors, and truthful capabilities. Use the default blob helpers, or
   override them only while preserving the same request and response semantics.
   A consumer accepts the implementation without knowing its concrete type:

   ```rust,ignore
   let backend = ExampleStorage::connect(options)?;
   let storage: Box<dyn umbra_storage::Storage> = Box::new(backend);
   ```

3. Document and implement atomic writer acquisition, renewal, release, and actual
   fencing. Explain what happens to the old writer's open FDs and mappings, how
   duplicate operation IDs are recovered, and when takeover must be refused.
   Describe exactly what a successful flush guarantees for data and metadata.

4. For runtime deployment, supply a provider binary that constructs this backend
   and exposes it through the storage provider server harness. Install a descriptor
   with an open provider ID such as `example.storage`, role `storage`, protocol
   version, explicit executable path, and capabilities; select that descriptor in
   the runtime role registry with opaque backend options. Do not add a factory
   match arm, CLI feature, assembly dependency, or peer backend import. Reuse the
   generic storage IPC proxy and overlay engine.

   `provider::serve_provider` owns server dispatch; `provider::Proxy` implements
   `Storage`. The shared transport supplies private, bounded, versioned frames,
   handshake validation, request IDs, errors, timeouts and backpressure.
   Provider loss blocks mutations; it must never trigger host-write fallback.

5. Run `cargo check -p umbra-storage-example` and the backend's conformance tests.
   Exercise exclusive creation, short offset I/O, copy-up, whiteouts, non-UTF-8
   paths, symlink containment, bounded directory iteration, hard-link identity,
   rename and swap crash recovery, duplicate requests, stale handles, flush
   failures, and competing writers including old writable FDs/mappings. Shared
   conformance infrastructure is not supplied by this trait-only scaffold; reuse
   it when available and keep backend qualification tests in the new package.
   Test real service durability, disconnects, and restart recovery. State whether
   the backend qualifies for strict remote persistence and record the tested
   service/deployment assumptions; local tests cannot establish NFS durability.

## Who the writer is

`RunBinding::admitted_writer` reports the writer identity a backend has
**already** admitted, when it admits at `open_run` rather than at
`acquire_writer`. Backends differ here for a real reason.

`local`, `nfs` and `tar` take writer authority in `acquire_writer` and record
whatever identity the caller names, so they answer `None`. `nfs-userspace`
admits during `open_run`, before it publishes any binding at all, because
one-session-one-Umbra is what makes a binding safe to hand out -- and admission
writes the writer identity into a durable marker on the server. By the time a
caller could name a writer, the marker already names one, and asking that
backend to acquire under a different name is asking it to report an admission
that writer never obtained.

So a caller receiving `Some` must acquire under exactly that identity. The field
is `#[serde(default)]`, so a provider that does not send it decodes as `None` --
which is also the right answer for every backend that admits late.

## Proxy capabilities move when a run opens

**A proxy advertises the backend's answer for whatever the backend currently has
open**: the connect-time answer while no run is open, and the run binding's own
capabilities while one is. `RunBinding::capabilities` is defined as the backend's
answer for that run, so adopting it is the same value the backend would give if
asked again, taken from a reply the caller already has.

It has to move. The local bounds checks in `read_at`/`write_at` read
`capabilities().max_io_bytes`, and a backend whose finite I/O limits exist only
for an open run -- `nfs-userspace` advertises `max_io_bytes: 0` before one,
deliberately, because without a bound transport there is no bound it could
honour -- would otherwise have every byte of I/O refused with a bounds error
naming a limit the run did not have.

**The `features` set moves with it, and that is a behaviour change on two shared
backends, stated here rather than left to be found.** `umbra-storage-local` and
`umbra-storage-nfs` set their ownership and parent-identity flags immediately
before building the binding, precisely so `capabilities()` reports them -- so an
open run over either now advertises names the connect-time answer did not, and
`umbra-overlay` reads `capabilities()` live for its ownership-carry decision.
That is the intended reading of those names: each one's doc requires
qualification "against a live store, never from configuration alone", and a run
is what supplies the live store. The stale connect-time cache was the defect.

A backend must therefore make its binding's capabilities *be* its own answer for
that run. `nfs-userspace` reads them back through the trait after installing the
run's surface, for exactly this reason: its binding used to come from a narrower
expression that did not include its probe-gated names, so a proxy fronting it
advertised strictly less than it did for the life of the run.
`Advertised::adopt`/`restore` and their unit tests hold the rule in one place.
