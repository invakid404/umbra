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
still decode and an absent name grants nothing. `FinishRunRequest`,
`FinishRunReceipt` and `FailedRunRequest` describe the namespace run lifecycle,
which `capabilities::NAMESPACE_RUN_LIFECYCLE_V1` names for the handshake,
and `JournalLifecycle::RunCompleted` is the durable completion record for a fresh
command run — not a checkpoint, and no authority for takeover. `OperationId::derive`
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
crate neither promises it nor requires it, and the overlay does as much of it as
its storage surface allows
([#55](https://github.com/invakid404/umbra/issues/55)). The variant is
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

Check with `cargo check -p umbra-core`.

Derived operation IDs are reproducible for a fixed seed and `(salt, index)` pair
and distinct across pairs for that seed. They are opaque 128-bit values; consumers
must not rely on UUID version or variant bits. Transport retries reuse the
already-built request context.
