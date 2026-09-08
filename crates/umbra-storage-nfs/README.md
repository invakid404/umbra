# umbra-storage-nfs

Live NFSv4 storage backend for Umbra. Direct Umbra dependencies are
`umbra-core` and `umbra-storage`; native access via `libc`, IDs via
`uuid`, provider options via `serde_json`.

`NfsStorage` implements run lifecycle, bounded file I/O, directory paging
and supported mutations through `umbra_storage::Storage` against an
externally managed NFSv4 mount. Hard links, logical symlinks, xattrs,
kernel-shadow qualification and strict remote persistence are unsupported.
Construction does not mount anything; it validates that the configured mount
root is an existing NFSv4 export before the first run I/O.

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
  intents require reconciliation. `flush` returns
  `Durability::Local`; the evidence string names the persistence boundary
  and explicitly declines to claim qualified remote NFS durability.
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

`tests/mounted.rs` (396 lines) contains six cases. Five exercise supported
operations against a live mount when `UMBRA_TEST_NFS_MOUNT` is set and
skip when unset; `absent_mount_rejected_without_creating_it` always runs.
Cases:

- `absent_mount_rejected_without_creating_it`
- `provider_protocol_handshake_and_crud`
- `crud_bytes_pagination_and_durability`
- `containment_nofollow_and_request_bounds`
- `retries_conflicts_and_cursor_invalidation`
- `competing_writers_persistent_epochs_and_no_takeover`

When creating the run tempdir under the mount hits
`ErrorKind::PermissionDenied`, the fixture returns `None` with an `eprintln`,
and the five live tests skip. The error alone does not identify whether
permissions, TCC or an enclosing sandbox caused the denial.
`cargo test -p umbra-storage-nfs` still exits 0 even in that unprivileged
case. Note: a green run under skip is not
evidence of NFS coverage. An explicit executed-coverage assertion remains
unimplemented; inspect `--nocapture` output for skips when qualifying a mount.

## Verification

```
cargo check -p umbra-storage-nfs
UMBRA_TEST_NFS_MOUNT=<abs> cargo test -p umbra-storage-nfs
```
