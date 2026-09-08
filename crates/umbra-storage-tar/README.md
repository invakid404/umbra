# umbra-storage-tar

A synchronous Unix storage provider requiring only an absolute tar archive filename.
No external mount, daemon, credentials, environment variables, or registry changes in
Umbra's CLI code are required. The archive's parent directory must already exist.
The provider is for API-driven storage; it supplies opaque root/control handles, not
physical paths for kernel syscall rewriting.

## Architecture and format

This implements **option 2: tar with an index**. One archive contains one run, with
entries at `<run_parent>/<run-id>/<root_anchor|control_anchor>/<logical path>` and a
`.provider/manifest.json` entry holding stable object identities, base/format/layout
identity, metadata, and mutation retry records. The default run parent is empty.
An archive filename can be chosen independently of the run ID. OpenExisting checks
the run ID, immutable base identity/fingerprint, format version, and anchor layout.
It accepts this provider's archives, not arbitrary tar imports. It does not materialize
an immutable base or perform copy-up.

An in-memory index points to byte ranges in open tar files. Reads use bounded
positional I/O. A write overlays at most one request's bytes and streams existing
content plus the patch into a new snapshot; it does not load whole file contents
into memory. Sparse gaps read as zeroes and are serialized as ordinary zero bytes.
Create, rename, unlink, directory removal, and parent creation modify a candidate
index. A successful operation publishes the complete candidate together with its
retry result before exposing it. Failed primitive results are also recorded, with
the original tree preserved.

Staging lives at `<archive_path>.provider/retries/state.tar`. Each mutation rewrites
this snapshot using a private sibling temporary file, file fsync, atomic rename,
and directory fsync. This deliberately trades speed for coherent persistent data
and retry results, including a crash between persistence and delivery of a response.
There is no extract-to-working-directory step and no mapping of logical request
paths onto host filesystem paths. Temporary snapshot files are removed on ordinary
error/drop; a process crash can leave an unused `.umbra-tar-*` temporary file.
The sidecar and staging snapshot are retained across close and drop.

This architecture was chosen to avoid platform-specific descriptor-relative syscall
bindings and archive extraction hazards while providing zero-service testing and
atomic index updates. Direct dependencies are core, storage, tar, uuid, serde_json,
and the standard library, with tempfile only for tests. `tar = "=0.4.46"` follows
the workspace's exact-pin style ([tar documentation](https://docs.rs/tar/0.4.46/tar/)).

## Shipped semantics

- Exclusive create returns AlreadyExists on a duplicate name and NotFound when a
  parent directory is missing, matching local storage's O_CREAT|O_EXCL semantics.
  CreateParents explicitly creates directories, including the final directory.
  Create and CreateParents reject mode bits outside 07777 with InvalidInput.
- ReadAt/WriteAt preserve offsets and byte names. EOF reads may be short; writes
  return the complete accepted byte count or an error, including zero-byte no-ops.
  I/O is capped at 1 MiB per request and offsets at signed 64-bit file bounds.
- Stat/Lookup expose persistent UUID object identity, logical kind/length, mode,
  logical uid/gid (initially zero), creation/write modification time, and link count
  one. No host inode metadata or rewrite target is claimed. Renames preserve IDs.
- Logical symlinks support create, ReadLink, stat, rename, and unlink. Their targets
  are opaque bytes and are never followed, even when they name an in-anchor object.
  Any symlink in a path's parent components is rejected. Unlink removes the name;
  stat does not resolve the final target. Hard links and imported tar hard-link
  entries are unsupported; `hard_links` is false.
- Rename supports Replace and NoReplace within one anchor, including directory
  subtrees. Existing nonempty directories cannot be replaced. Cross-anchor rename
  and swap fail before changing the tree. Atomic replace is advertised; atomic
  swap is false and returns UnsupportedCapability for a same-anchor request.
- List returns sorted direct byte names, excluding dot entries and separators,
  with pages of 1–4096 entries. Opaque cursors are scoped to the session, run,
  directory, and current index revision. They are single-use, invalidated on any
  successful mutation or writer acquisition, with at most 64 outstanding cursors.
- Other sessions read the snapshot they opened. Reopen or acquiring writer
  authority refreshes that snapshot; there is no live cross-session read refresh.
- CopyUp, Link, SetMetadata, Truncate, xattrs, whiteouts, and AtomicSwap are deferred
  and return UnsupportedCapability. Kernel shadow, complete emulation, remote
  durability, and kernel fencing are not advertised.

## Writer authority, retries, and durability

`<archive_path>.provider/writer.lock` is created exclusively with a random token.
The sidecar's fsynced `epoch` increases on acquisition. Leases last 60 seconds;
renewal requires the current token and a live lease. Every mutation and flush
checks run identity, current epoch, token ownership, and expiry. A stale lease
cannot renew or release. Expiry blocks writes but permits the owner to release.
It never permits another writer to take over. All takeover requests are refused:
there is no implemented independent termination verifier. The conservative
ConfirmedTermination fencing category does not assert that such a verifier exists.
Drop retains an active lock; deleting a lock based only on elapsed time is unsafe.

Retry records in the staging tar are indexed logically by operation ID and
idempotency key. Exact operation/key/payload retries return the stored success or
error. Reusing either identity with a different payload or counterpart is InvalidInput.
A newly acquired current epoch may retrieve a prior result; stale epochs are always
rejected. Data/index and retry records share one atomic snapshot publication.
The manifest, including accumulated retry payloads, is capped at **16 MiB**. Full
write payloads are retained permanently as JSON number arrays. For three-digit byte
values (100–255), a fresh archive accepts only **three 1 MiB writes**; the fourth
returns StorageUnavailable. This is approximately **3 MiB cumulative written bytes
per archive**, including overwrites, rather than a limit on live file size. The
exact capacity depends on byte values and other metadata/mutations. Flush, unlink,
and reopen do not reclaim this budget; smaller mutations may still fit afterwards.
Capacity rejection occurs before publication; reads, flush, release and close
remain available. Retry compaction and larger-scale indexing are deferred.

Flush validates writer authority and requested object IDs. Data, DataAndMetadata,
and EntireRun scopes all persist the entire snapshot (a stronger boundary than an
object subset). It streams a new sibling tar, fsyncs that file, atomically renames
it over the archive, and fsyncs the archive's parent directory. Only after those
steps does it return Durability::Local with evidence naming the actual archive path
and fsync boundary. No remote-media guarantee is made.

Release does **not** flush the canonical archive. Close rejects an active lease;
a successful release/close retains unflushed changes in the staging snapshot for
reopen. The archive alone represents the last successful flush. Copying the archive
after flush gives a self-contained run snapshot; retaining the sidecar is necessary
to retain unflushed changes and writer authority history. A pending staging/flush
I/O failure poisons the session: subsequent mutations, release, and close report
failure rather than claim clean completion. Automatic reconciliation after an
ambiguous persistence failure or abandoned writer is deferred. A poisoned session
retains writer.lock and blocks all future writer acquisition. No recovery tool is
shipped; manual recovery requires quiescing the provider and reconciling persistence
before removing the retained lock.

CreateNew publishes only a complete, valid archive, without overwriting an existing
name. Failed open never exposes a binding. The archive parent is canonicalized so
path aliases share authority; archive files with physical symlinks/hard links and
nonprivate or symlink sidecars are rejected. Archive parsing checks the manifest,
entry paths/types/sizes/metadata, duplicate/missing entries, directory ancestry,
and tar end blocks. It never extracts files. The archive parent and sidecar are
trusted provider-owned storage: hostile concurrent host edits, in-place archive
replacement by another program, and live sidecar deletion are outside the model.
Use one canonical archive location for a run, and quiesce its provider before
moving or restoring it. Archive integrity is structural, not cryptographic.

## Configuration and provider registration

`TarStorageConfig::new(archive_path)` is lazy. `connect(config)` validates the
configuration without creating a file; open_run validates storage and binds a run.
The Rust config additionally exposes `run_parent`, `root_anchor`, and
`control_anchor`. Anchors must be distinct, single, nonreserved components;
`.provider` is reserved, including within run_parent, and run_parent must use Root.
Archive paths must be absolute, NUL-free, and contain normal components.

The executable calls `serve_provider("tar", ...)`. Its only shipped option is the
JSON encoding of core's BytePath containing the absolute archive filename, itself
carried as descriptor option bytes. No additional tuning options are required.
The supplied `provider.json` is an installation template; customize the executable
and archive path as follows from the workspace root:

```python
import json
from pathlib import Path
root = Path.cwd()
descriptor = json.loads((root / "crates/umbra-storage-tar/provider.json").read_text())
descriptor["executable"] = list(bytes(root / "target/debug/umbra-storage-tar"))
descriptor["options"] = list(json.dumps(list(bytes(root / "run.tar"))).encode())
Path("tar-providers.json").write_text(json.dumps({
    "timeout_ms": 10000, "providers": {"storage": descriptor}
}))
```

```sh
cargo build -p umbra-storage-tar
cargo run -p umbra-cli -- providers --registry tar-providers.json --role storage
```

Registration probes the real provider handshake without opening a run. Unix IPC
is required; the non-Unix binary reports unsupported transport. The CLI's existing
default selection is unchanged.

## Tests and deferred work

`cargo test -p umbra-storage-tar` runs inline unit tests and `tests/tar.rs` integration
tests using fresh TempDirs. There is **no environment-variable gating**, external
mount, network service, or credential requirement. The binary handshake test uses
a local Unix socket and a real child provider process.

Tests cover config/options validation, lifecycle and failed-open rollback, writer
exclusivity across sessions and threads, deterministic expiry without sleeps,
stale tokens/epochs and abandoned locks, exclusive create, typed CRUD, maximum I/O
and overflow, sparse gaps/overwrites/EOF, pagination/cursor scope/invalidation,
NoReplace/subtree/cross-anchor rename, directory removal, logical-symlink containment,
honest hard-link/swap rejection, every flush scope and standalone archive reopen,
persistent retries/name reuse, read-only/custom-layout sessions, retained staging
without implicit flush, corrupt/truncated/escaping archives, injected publication
failures, and provider handshake plus CRUD/flush.

Deferred work includes multi-run archives, arbitrary tar import, compression/sparse
tar encoding, incremental staging and retry compaction, live reader refresh,
cryptographic integrity, administrative recovery/fencing, kernel rewrite support,
and the unsupported primitives listed above. Whole-snapshot publication and linear
retry lookup favor small local runs over large archives or high mutation throughput.
