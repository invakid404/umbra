# umbra-storage-nfs

Live NFSv4 storage backend for Umbra. This README covers this crate's mounted
provider, binary, and tests. Direct Umbra dependencies are
`umbra-core` and `umbra-storage`; native access via `libc`, IDs via
`uuid`, provider options via `serde_json`.

`NfsStorage` implements run lifecycle, bounded file I/O, directory paging
and supported mutations through `umbra_storage::Storage` against an
externally managed NFSv4 mount. Hard links, logical symlinks, xattrs,
kernel-shadow qualification and strict remote persistence are unsupported.

`connect` validates an existing exact NFSv4 mount before any run I/O. A backend
built with `new` advertises `mounted-nfsv4-v1` only after `open_run` performs that
validation; configuration alone never earns the capability. `experimental-open-rewrite-v1` is
advertised alongside it, since runs supply real physical paths for kernel syscall
rewriting and the sandbox write root. Nothing here advertises remote durability:
`strict_remote_persistence` stays false and `durability` stays `Local`, because
the boundary reached is the client fsync.
Construction does not mount anything. `NfsStorage::new` retains the config and
defers validation to `open_run`; `NfsStorage::connect` validates eagerly.

## Public API

- `NfsStorage::new(NfsStorageConfig)` — lazy, retains config; validation
  runs on `open_run`.
- `NfsStorage::connect(NfsStorageConfig)` — eager, validates config +
  negotiated NFSv4 before returning (used by the provider `main.rs`).
- `NfsStorage::mount_root()` — current runtime mount point.
- `NfsStorageConfig { mount_root, run_parent, root_anchor, control_anchor }`
  with `NfsStorageConfig::new(mount)` and
  `NfsStorageConfig::from_options(json_bytes)` for the provider handshake.

## Shipped behavior

- **Mount validation** (`src/mount.rs`): parses `/sbin/mount` output with
  byte-preserving octal-escape unescaping, and on macOS reads
  `/usr/bin/nfsstat -m` for the *Current mount parameters* scoped to the
  exact mount path. Requires negotiated NFSv4 (`vers=4`, `nfsvers=4`, or
  `4.X`); rejects wrong filesystem, a different mount path, and downgraded
  `vers=3` even when v4 was originally requested. Two unit tests
  (`darwin_current_options_are_scoped_to_mount`,
  `exact_mount_and_protocol`) guard the parser.
- **Path resolution** (`src/native.rs::walk`, `parent`): byte-preserving,
  descriptor-relative, `O_NOFOLLOW` on every component. Absolute paths and
  `..` escape from the logical anchor are rejected by `StoragePath`'s
  constructor before they reach this crate.
- **Atomic operations** (`src/operations.rs`, `src/native.rs`):
  - `create` — `mkdir -p` parents through `walk(create=true)`, then
    `openat` with `O_CREAT|O_EXCL|O_NOFOLLOW`.
  - `rename` — `renameat` with anchor descriptors; `RenameMode::NoReplace`
    routed to the underlying flag.
  - `unlink` / `RemoveDirectory` — `unlinkat` with `AT_REMOVEDIR` selection.
  - `stat` — `fstatat(AT_SYMLINK_NOFOLLOW)`; does not follow the final
    symlink.
  - `list` — bounded pages through opaque cursor tokens keyed by run,
    invalidated on `(ino, mtime+nsec, ctime+nsec)` directory-stamp change.
  - `atomic_swap` — `renameatx_np(RENAME_SWAP)` is implemented, but
    `capabilities().atomic_swap` is currently `false` and
    `umbra_storage::validate_request` rejects `StorageOperation::AtomicSwap`
    before dispatch. Rationale in `capabilities`: *NFSv4 RENAME has no
    atomic exchange; native exchange is implemented but cannot be advertised
    for an unqualified export.* Native code is retained for future
    qualification.
- **Writer authority**: `acquire_writer` / `renew_writer` / `release_writer`
  use a 60-second lease with epoch fencing via `.provider/writer.lock`
  and `.provider/epoch`. Takeover is refused; epoch checks do not fence
  existing kernel descriptors or mappings. Exact retries with the same
  idempotency key recover recorded results from `.provider/retries`;
  reusing an operation ID with a different key is rejected, and incomplete
  intents require reconciliation.
- **Persistence barrier** (`src/native.rs::flush`): blocking `flush()` supports
  only `FlushScope::EntireRun`; object scopes return `UnsupportedCapability`
  before I/O. It synchronizes regular files, directories after their children
  (including the root, control, and provider-private trees), and the pinned run
  parent last, using `File::sync_all`.
  It does not follow symlinks. Device boundaries, changed entries and directory
  stamps, excessive depth, mount replacement, and lost writer authority fail
  the barrier. These checks detect some races; the caller must keep writers
  and writable mappings quiescent for a coherent checkpoint.
  An observed concurrent external write deliberately returns `InvalidState`
  ("persistence outcome unknown; entry changed during barrier") and retains
  the failure. This is the intended contract, covered by
  `native::tests::concurrent_external_write_during_barrier_is_rejected_and_retained`.
  Only a completed OS sync barrier with all final checks passing returns
  `Durability::Local`. Its evidence states that NFS WRITE/COMMIT and verifier
  recovery belong to the kernel and cannot be independently checked through
  `File`, and remote stable storage is unqualified. Local does
  not promise a durable client replica. There is no separate COMMIT RPC,
  `fdatasync` follow-up, or qualified Remote mode.
  The existing stronger `Durability::Remote` would require a qualified client,
  server/export/storage deployment and complete write-error coverage; no such
  qualification or Remote-returning path ships here. Success acknowledges only
  the requested run, writer epoch, and scope at the barrier, not later writes,
  an atomic checkpoint, or safe takeover. Reusing a flush idempotency key runs
  the barrier again and cannot bypass retained errors.
  Exposed transport errors retain their errno and use `StorageUnavailable`;
  stale filehandles use `StaleHandle`. Failed barriers and known mutation I/O
  failures are retained for the provider instance, blocking further mutations,
  lease release, and close/reopen. A failed OS sync returns an error, not a
  Local receipt. Hard-mounted NFS can block in the kernel; an IPC timeout does
  not cancel writeback or authorize takeover. External tracee error collection
  and recovery across process restart still require supervisor integration;
  a fresh tree walk cannot recover already-lost writes. Same-device bind mounts
  and server failover/storage configuration are not independently qualified.
- **Runtime config**: mount root, `run_parent`, `root_anchor`,
  `control_anchor` all flow through `NfsStorageConfig`. The mount root is
  supplied at runtime; `root`/`control` are configurable library defaults,
  and `.provider` is reserved internal state. Shipped provider options carry
  only the mount root as a JSON-encoded `BytePath`; custom layout fields
  are available through the library config.

## Provider binary

`src/main.rs` calls `umbra_storage::provider::serve_provider("nfs",
|options| NfsStorage::connect(NfsStorageConfig::from_options(options)?))`.
The eager `connect` runs the mount validation during the handshake so a
misconfigured export fails fast. Register it via `umbra providers
--registry <path> --role storage`. See `provider.json` template and
`docs/providers.md`.

## Test coverage

Unit tests inject sync failures and authority loss, check child-before-parent
ordering and symlink containment, and reject namespace changes, stale identities,
depth overflow, concurrent external writes, and retries after a persistence
failure. Exposed transport and writeback errors are injected to verify error
classification and retention. These tests do not qualify server storage or
exercise NFS wire recovery.

`tests/mounted.rs` contains eight ordinary tests and two automated fault tests
that are ignored by default. Seven ordinary tests use `UMBRA_TEST_NFS_MOUNT` and
skip only when it is unset; an explicitly supplied mount that is not writable
fails the fixture. `absent_mount_rejected_without_creating_it` always runs.
Ordinary cases:

- `flush_external_writes_reports_kernel_boundary_and_rechecks_repeated_key`
- `open_run_qualifies_a_new_backend_after_mount_validation`
- `absent_mount_rejected_without_creating_it`
- `provider_protocol_handshake_and_crud`
- `crud_bytes_pagination_and_durability`
- `containment_nofollow_and_request_bounds`
- `retries_conflicts_and_cursor_invalidation`
- `competing_writers_persistent_epochs_and_no_takeover`

The external-write test synchronizes the original writable descriptor and
quiesces the writer before flushing. It checks Local evidence, receipt identity,
and fresh traversal on a repeated key. A green run with the mount unset is not
evidence of NFS coverage; inspect `--nocapture` output for skips.

Verified on the scratch mount on 2026-09-09: 13 library tests and seven ordinary
mounted tests passed; both opt-in fault tests also ran and passed individually
(22 passing tests across those runs, zero failures).

## Verification

```sh
cargo check -p umbra-storage-nfs
UMBRA_TEST_NFS_MOUNT=/absolute/path/to/mount cargo test -p umbra-storage-nfs -- --nocapture
```

### Automated scratch fault tests

Both tests are automated and have passed. Run them individually with explicit
opt-in, only on the idle scratch export. They validate the scratch mount,
Compose identity, and export volume before disrupting `umbra-nfs-nfs-1`.

1. Quiesce all external writers and avoid concurrent mounted test runs. Use only
   `~/umbra-scratch/nfs/mnt/umbra-nfs` and container `umbra-nfs-nfs-1`.
2. Run the transport test alone:

   ```sh
   UMBRA_TEST_NFS_FAULTS=1 UMBRA_TEST_NFS_MOUNT=~/umbra-scratch/nfs/mnt/umbra-nfs cargo test -p umbra-storage-nfs -- --ignored --test-threads=1 scratch_transport_outage_recovers_to_local_without_remote_claim
   ```

3. Immediately inspect the server, even if the test fails or is interrupted:

   ```sh
   docker inspect --format '{{.State.Status}} {{.State.Paused}} {{.State.Health.Status}}' umbra-nfs-nfs-1
   ```

   If paused, run `docker unpause umbra-nfs-nfs-1`. If stopped, run
   `docker start umbra-nfs-nfs-1`. Repeat inspection until
   `running false healthy` before continuing.
4. Only after that healthy check, run the crash test alone:

   ```sh
   UMBRA_TEST_NFS_FAULTS=1 UMBRA_TEST_NFS_MOUNT=~/umbra-scratch/nfs/mnt/umbra-nfs cargo test -p umbra-storage-nfs -- --ignored --test-threads=1 scratch_server_killed_after_flush_preserves_export_bytes
   ```

5. Immediately repeat step 3 and leave the server `running false healthy`.

The transport test synchronizes and closes the external writer, pauses the
server, and checks that the barrier does not return during a two-second outage.
After unpause it checks Local evidence and matching payload bytes. This exercises
transparent kernel transport recovery, not an exposed transport errno or a
controllable userspace gap between fsync and COMMIT.

The crash test SIGKILLs and restarts the NFS-Ganesha process after a successful
Local flush, before any subsequent payload read or provider sync. Once healthy,
it compares both a direct server export read (bypassing the original client's
NFS data cache) and a mounted read with the payload. The OrbStack VM and backing
volume survive. This does not prove power-loss durability, verifier-change replay,
or Remote qualification, and does not authorize abandoned-run takeover.

### Deferred stronger qualification

Stronger qualification remains deferred. On a dedicated disposable deployment,
quiesce writers, write a known payload, retain its expected digest outside the
tested storage, collect the original writer's sync errors, and flush. Interrupt
the storage VM/power or exercise controlled failover before any read; restore it
and verify bytes and namespace directly on the server or through a fresh client.
Capture NFS WRITE/COMMIT/verifier changes and replay, and exercise lost replies
and writeback errors separately. A server-process SIGKILL with the backing
volume still powered cannot substitute for these checks or qualify Remote.

`new` advertises no mounted capability. Successful mount validation in either
`connect` or `open_run` enables `mounted-nfsv4-v1`; a returned run binding therefore
reflects validation performed by `open_run` too.
