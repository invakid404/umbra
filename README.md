# Umbra

Umbra is a Rust prototype for supervising coding-agent process trees with a
per-run filesystem shadow. The intended guarantee is that each regular-filesystem
mutation reaches the selected shadow or is denied by an independent enforcement
boundary. Strict remote persistence requires qualified remote storage; local
storage is an explicit development or local-persistence mode.

Start with [ARCHITECTURE.md](ARCHITECTURE.md) for the 15-crate workspace,
per-trait extension points, runtime provider design, dependency rules, and links
to crate documentation. The [engineering handoff](docs/handoff.md) explains the
product contract, namespace semantics, feasibility plan, and qualification work.
Its illustrative `fsvirt-*` crate names are superseded by the `umbra-*` topology
in the architecture document.

M1 and M1.5 have delivered the 15-crate pluggable workspace, a working
[Darwin arm64 Rust tracer](crates/umbra-platform-macos/README.md), real local and
NFS storage backends, and an overlay with copy-up, read-through, whiteouts and
logical symlinks. All seven Rust fixture cases are CAPTURED on the
qualified host and the M2 post-exec breakpoint gap is closed; on newer
macOS releases `posix-spawn-write` refuses by design against its pinned
descriptor layout.
[M0 Gate 2](docs/m0/gate-2.md) preserves the separate Python prototype evidence.

The end-to-end CLI is still not wired. Operational supervisor/CLI methods, the
file journal and vendor agent adapters remain stubs; production enforcement,
recovery and remote durability are not qualified. Resume means restarting an
agent from persisted filesystem and session state, not migrating a live process.

The workspace uses Rust edition 2021 and the toolchain pinned in
[rust-toolchain.toml](rust-toolchain.toml).
[CI](.github/workflows/ci.yml) runs formatting, Clippy with warnings denied,
workspace checks, tests, and doctests on macOS and Linux for pushes to `master`
and pull requests.
Provider executables can be checked through the [runtime registry](docs/providers.md).
