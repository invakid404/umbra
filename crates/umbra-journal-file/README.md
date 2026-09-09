# umbra-journal-file

File-backed implementation of the Journal contract: a bounded, CRC32-checksummed
append-only framed log plus immutable checkpoint publication, beneath the runtime
control directory supplied through `Journal::open`. This crate owns `src/lib.rs`,
`src/main.rs`, its manifest and `provider.json`. It accepts no provider options
and performs no lease acquisition; writer authority is injected.

Layout under `<control>/journal/`: `log` holds a format header followed by frames,
`checkpoints/<uuid>.json` holds snapshot contents, and `checkpoint` names the
current snapshot. Each frame is `length: u32 BE | crc32: u32 BE | payload`, where
the payload is the encoded record carrying the sequence the log actually assigned.
Length is validated before any allocation. CRC-32 is computed in-crate rather than
pulled in as a dependency.

Boundaries the implementation keeps:

- `open` validates format version, run identity, access intent and injected writer
  authority before opening anything for mutation, then replays to establish the
  last valid sequence and the prepared-but-uncommitted operations for the
  namespace owner to reconcile. Recovery is clean only with no pending operations
  and an intact tail. Readable frames are never treated as evidence of a
  previous flush receipt. A zero-length log left by interrupted creation is
  treated as absent; writer open reinstalls its header. A nonempty invalid header
  remains corruption.
- A demonstrably incomplete final frame is the only recoverable log damage, reported as
  `IncompleteFinalFrame`; truncating it requires writer authority, so a read-only
  open reports it without repairing. A complete frame with a bad checksum, or any
  interior corruption or sequence gap, is `CorruptJournal`.
- `append` assigns and persists the sequence itself; a caller's sequence is not
  authority to place a frame. A record whose epoch does not match the session's
  writer authority is `LeaseLost`. Appends refuse to exceed the 512 MiB replay
  limit; an oversized existing log is `UnsupportedCapability`, not corruption.
  Acceptance is not durability. The session retains its opened log handle for
  append, flush, replay and close, so replacing the pathname does not redirect
  a live session's writes.
- `flush` fsyncs and returns a receipt; a sequence beyond the log is rejected.
- `write_checkpoint` requires the checkpoint's last committed sequence to have been
  flushed, and writes snapshot contents durably before the reference that names
  them. The reference is written to `checkpoint.tmp`, fsynced, renamed over
  `checkpoint`, then the journal directory is fsynced. An empty reference left
  by an older writer falls back to full log replay without selecting a snapshot.
- `close` surfaces the error from its final flush rather than reporting a clean
  shutdown over a failed write.

Resumable checkpoint recovery is out of scope here: this backend publishes and
reads checkpoints, but reconciling a nonempty recovery into a running namespace is
not implemented in the overlay.

The package supplies its own provider binary and `provider.json` installation
template. See [provider setup](../../docs/providers.md).

```sh
cargo check -p umbra-journal-file
cargo test -p umbra-journal-file
```
