# umbra-storage-local

Working Unix development storage for a private, trusted directory. Runs use
`<root>/<run-id>/{root,control}`. Supports bounded positioned file I/O, metadata,
anchored paths, directory paging, explicit local flushes, exclusive writers and
session-local retry deduplication. It rejects symlink components and non-UTF-8 paths
retain their bytes. Path-based checks are not a race-proof sandbox: callers must keep
the directory private and prevent concurrent external changes.

Kernel shadow qualification, remote persistence, writer takeover, arbitrary xattrs,
logical symlinks and crash-persistent idempotency are unsupported. Abandoned writer
markers require recovery outside this implementation; expiry never proves termination.
Provider options are JSON encoding of a BytePath containing the physical storage root.
The library is Unix-only; the executable reports unsupported transport on other hosts.

The package supplies its own provider binary and `provider.json` installation template.
See [provider setup](../../docs/providers.md) and the corresponding trait walkthrough.

Check with `cargo check -p umbra-storage-local`.
