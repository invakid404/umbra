# umbra-storage

Synchronous, backend-independent storage contracts. The only dependency is
`umbra-core`; this crate performs no filesystem I/O and selects no implementation.
Shared request/result, identity, error, capability, and byte-path types belong to
core and are re-exported here. There are no implementation-specific associated
types: every backend can be injected as `Box<dyn Storage>` or `&mut dyn Storage`.

`Storage` owns one active run. Its required methods are `capabilities`, `open_run`,
`acquire_writer`, `renew_writer`, `release_writer`, `execute`, `flush`, and
`close_run`. Constructors and backend connection options live outside the trait.
The convenience methods `read_at`, `write_at`, `create`, `unlink`, `list`, `stat`,
and `atomic_swap` dispatch typed requests through `execute`; implementations retain
one place to enforce lifecycle, containment, idempotency, and writer authority.
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
name and preserves other hard links. Stat does not follow the final symlink.
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
