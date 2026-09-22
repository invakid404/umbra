# umbra-storage-local

Working Unix development storage for a private, trusted directory. Runs use
`<root>/<run-id>/{root,control}`. Supports bounded positioned file I/O, metadata,
anchored paths, directory paging, explicit local flushes, exclusive writers and
session-local retry deduplication. It rejects symlink components and non-UTF-8 paths
retain their bytes. Path-based checks are not a race-proof sandbox: callers must keep
the directory private and prevent concurrent external changes.

It advertises `local-development-v1`, `experimental-open-rewrite-v1` and
`ownership-fidelity-v1`, so a run must select local development explicitly; it
advertises nothing about remote durability. It advertises a fourth name,
`storage-parent-identity-v1`, conditionally: only for a non-read-only run whose
backing filesystem a live per-run probe has measured to hand a new object its
parent directory's gid. There is no `cfg(target_os)` behind this — the property
is measured, not configured — so a local-disk BSD/macOS run advertises it, while
Linux (whose child takes the process filesystem gid absent a setgid parent) and
any mount whose server assigns child identity stay silent. So does a single-group
host, where the probe finds no differing group to test with, and one where the gid
a new object would receive anyway already equals the group the probe tests with,
since a match would then be coincidence rather than inheritance. Any error during
the probe leaves it silent too, including a failure to remove the scratch
directory it created: the probe never fails the run over a capability nothing has
asked for, and never reports a measurement while its own leftovers survive.
Its scratch directory lives at `<root>/.umbra-probes/<uuid>`, a reserved sibling of
the run directories rather than inside one, so a flush (which walks only the run
directory) never meets it and a late probe cannot disturb the run it was opened for;
the probe answers silently if that directory is not on the run directory's
filesystem. The probe runs on its own thread and `open_run` waits at most 1.5 seconds for
it: a probe that has not answered by then (an unresponsive network mount, say)
also leaves the run silent and logs a `tracing` warning, and the stalled probe
thread is left to finish or clean up on its own. At most sixteen probe threads run
per backend at once; once that many are outstanding a further open answers
silently without starting another, so a wedged mount strands at most sixteen. Only
the probe is bounded by that wait. `open_run`'s own layout I/O on the storage root (creating
and validating `<root>/<run-id>/` and reading its manifest) runs on a separate thread
too, and `open_run` waits at most 3 seconds for it: past that, `open_run` fails
with `StorageUnavailable` and a `tracing` warning, publishes no binding, and leaves
the stalled thread to finish on its own. Nothing is rolled back. Late layout work
stays inside `<root>/<run-id>/`, and a run whose layout never completed is refused on
reopen, like one interrupted by a crash, because the manifest is written last. At
most four stalled layout threads are allowed per storage instance; past that,
`open_run` fails at once without starting another.
`ownership-fidelity-v1` means the uid/gid a `SetMetadata` names are applied
through `lchown` and read back by the
next `Stat`, not that the kernel permits every chown: an unprivileged cross-uid
chown arrives as `Denied` and the caller is told so. `SetMetadata` applies mode,
uid and gid; an update naming nothing or a mode outside 07777 is `InvalidInput`,
and one naming a timestamp is `UnsupportedCapability` and applies none of the
update. Runs supply real physical paths, so the supervisor can bind kernel
syscall rewrites and the sandbox write root to `<root>/<run-id>/root`.

Kernel shadow qualification, remote persistence, writer takeover, arbitrary xattrs,
logical symlinks and crash-persistent idempotency are unsupported. Abandoned writer
markers require recovery outside this implementation; expiry never proves termination.
Provider options are JSON encoding of a BytePath containing the physical storage root.
The library is Unix-only; the executable reports unsupported transport on other hosts.

The package supplies its own provider binary and `provider.json` installation template.
See [provider setup](../../docs/providers.md) and the corresponding trait walkthrough.

Check with `cargo check -p umbra-storage-local`.
