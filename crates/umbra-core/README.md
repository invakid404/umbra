# umbra-core

Shared byte paths, stable IDs, filesystem IR, storage/journal/namespace/agent DTOs,
and structured errors. All DTO families support serde; BytePath, RegisterSet and
StoragePath validate during deserialization, and write payloads have an explicit bound.
Storage types are available both at the root and through `storage` for compatibility.
Core has no Umbra dependencies and forbids unsafe code.

`provider` contains runtime registry descriptors, JSON encoding and bounded private
Unix transport primitives. Role-specific schemas, proxies and dispatch belong in the
five trait crates. Runtime executable paths/options never belong in checkpoint identity.
`provider::accept_connection` exposes the same identity/role/version/capability
handshake as `accept` for injected private connections such as test socketpairs.

Check with `cargo check -p umbra-core`.
