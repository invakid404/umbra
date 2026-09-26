# umbra-core

Shared byte paths, stable IDs, filesystem IR, storage/journal/namespace/agent DTOs,
and structured errors. All DTO families support serde; BytePath, RegisterSet and
StoragePath validate during deserialization, and write payloads have an explicit bound.
Storage types are available both at the root and through `storage` for compatibility.
Core has no Umbra dependencies and forbids unsafe code.

`capabilities` holds the open capability names crates negotiate over the provider
handshake, so every consumer spells the same requirement the same way. They are
configuration strings, not a backend enum: a name maps to no implementation, and
advertising one is a claim to be qualified by measurement.

`sandbox` carries `SandboxProfile` and `SandboxRequirement`, the rendered
enforcement policy attached to every `LaunchSpec`. The requirement has no default
and no `Option`, so a launch cannot become unsandboxed by omitting a field;
running without a policy is the explicitly named `UnsandboxedExperiment`.

`UmbraError::launch_tree_terminated` carries explicit backend evidence that a
failed launch's tracees were reaped; it defaults to false, including when absent
on the wire. Error categories alone provide no termination evidence.

`ErrorKind::ProcessFailed` reports a supervised child that exited nonzero or was
signalled, which is a run result rather than an Umbra malfunction.
`PersistencePolicy::NfsClientFsync` distinguishes a mounted run whose durability
boundary is the client fsync from local development and from unqualified strict
remote persistence. `StorageCapabilities::features` is the open extension point
for narrowly qualified storage behavior, defaulting to empty so older encodings
still decode and an absent name grants nothing.
`capabilities::STORAGE_OWNERSHIP_FIDELITY_V1` is another name that uses it: a
backend advertising it applies the `uid`/`gid` a `MetadataUpdate` names and
reflects the result in the next `BlobStat`. It claims what the kernel permits is
applied and a refusal reported, never that every chown succeeds, so a consumer
requires the name to decide whether to attempt an ownership carry and still
handles `ErrorKind::Denied` on the attempt. Adding it changed no struct and no
`PROTOCOL_VERSION`, which is the property the `features` set exists to give.
`FinishRunRequest`,
`FinishRunReceipt` and `FailedRunRequest` describe the namespace run lifecycle,
which `capabilities::NAMESPACE_RUN_LIFECYCLE_V1` names for the handshake,
and `JournalLifecycle::RunCompleted` is the durable completion record for a fresh
command run — not a checkpoint, and no authority for takeover.
`JournalLifecycle::RecoveryRequired` is the session's own verdict that its effects
could not be accounted for; since
[#65](https://github.com/invakid404/umbra/issues/65) it has a writer, and
`RecoveryState::recovery_required` is how a backend reports that verdict back to
the namespace owner. It carries no `#[serde(default)]`, so a payload from a peer
that predates the field fails to decode rather than silently reading as "no
verdict"; adding it changed no on-disk format and no `PROTOCOL_VERSION`. `OperationId::derive`
mints distinct storage operation identities from one logical transaction, because
a backend may bind an operation ID to the single idempotency key it was first
used with.

`AbortReason::KernelRefused(Errno)` is the one abort reason that reports an
observed syscall outcome rather than a failed interception: the rewritten call
reached the kernel and the kernel refused it, and the tracee is owed the errno in
its own return register. Every other variant still means the interception broke
down. It is a licence for the namespace to reconcile instead of poisoning, not an
instruction to — a namespace that cannot account for the aborted transaction's
effects, or cannot take them back, must still refuse, and may discharge that
obligation with a conservative approximation that refuses more often than
strictly needed. Rolling anything back is the namespace's own business: this
crate neither promises it nor requires it. The overlay now records an undo for
every prepare-time creation it makes — file (`unlink`), directory materialised
over nothing (`remove_directory`), and logical symlink (three ordered `unlink`s
of its backing index, target blob, and placeholder) — and on a corroborated
`KernelRefused`, `abort` unwinds the recorded list in reverse insertion order;
the three `unlink`s inside a symlink entry run in field order (backing index
first, so no later removal outlives the identity-dependent name that resolves
the index — see [#69](https://github.com/invakid404/umbra/issues/69)). An undo
that
itself fails still poisons; the mirror direction (prepare-time destruction with
no recorded inverse) also still poisons, tracked in
[#66](https://github.com/invakid404/umbra/issues/66); and a `KernelRefused`
whose errno the session did not observe never reaches the reconcile path at
all — no recorded undo runs, and the abort poisons unconditionally. See
[#53](https://github.com/invakid404/umbra/issues/53) →
[#55](https://github.com/invakid404/umbra/issues/55) →
[#64](https://github.com/invakid404/umbra/issues/64) →
[#69](https://github.com/invakid404/umbra/issues/69) for the arc. The variant is
appended, so already-encoded values still decode; a peer built without it cannot
decode the new name, and `provider::PROTOCOL_VERSION` is unchanged because both
ends ship together in-tree.

`provider` contains runtime registry descriptors, JSON encoding and bounded private
Unix transport primitives. Common protocol version 2 requires current installation
descriptors and rejects older peers before sandbox/rewrite request decoding.
Role-specific schemas, proxies and dispatch belong in the
five trait crates. Runtime executable paths/options never belong in checkpoint identity.
`provider::accept_connection` exposes the same identity/role/version/capability
handshake as `accept` for injected private connections such as test socketpairs.

A registry's `timeout_ms` (default `provider::DEFAULT_TIMEOUT_MS`, 5000, bounded to
1–60000) is the authoritative deadline for a whole request, callbacks included: it is
the bound a backend's own in-process timeouts are sized to fit inside, so the backend's
outcome surfaces instead of a deadline miss. A missed deadline invalidates the
connection, and dropping the client kills the provider and reaps it with a bounded
wait — a child still stuck in the kernel is handed to a detached reaper thread rather
than blocking the caller, so teardown proceeds and at worst a zombie outlives the drop.

Check with `cargo check -p umbra-core`.

Derived operation IDs are reproducible for a fixed seed and `(salt, index)` pair
and distinct across pairs for that seed. They are opaque 128-bit values; consumers
must not rely on UUID version or variant bits. Transport retries reuse the
already-built request context.
