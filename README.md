# Umbra

Umbra is a Rust prototype for supervising coding-agent process trees with a
per-run filesystem shadow. The intended guarantee is that each regular-filesystem
mutation reaches the selected shadow or is denied by an independent enforcement
boundary. Strict remote persistence requires qualified remote storage; local
storage is an explicit development or local-persistence mode.

Start with [ARCHITECTURE.md](ARCHITECTURE.md) for the 16-crate workspace,
per-trait extension points, runtime provider design, dependency rules, and links
to crate documentation. The [engineering handoff](docs/handoff.md) explains the
product contract, namespace semantics, feasibility plan, and qualification work.
Its illustrative `fsvirt-*` crate names are superseded by the `umbra-*` topology
in the architecture document.

M1 and M1.5 have delivered the 16-crate pluggable workspace, a working
[Darwin arm64 Rust tracer](crates/umbra-platform-macos/README.md), real local,
NFS and tar storage backends, and an overlay with copy-up, read-through,
whiteouts and logical symlinks. All eleven Rust fixture cases are CAPTURED and
the M2 post-exec breakpoint gap is closed.
[M0 Gate 2](docs/m0/gate-2.md) preserves the separate Python prototype evidence.

`umbra run` is now wired end to end: the [CLI](crates/umbra-cli/README.md) turns
an explicit provider registry into a run request, and the
[supervisor](crates/umbra-supervisor/README.md) opens storage, takes the single
writer lease, opens the [file journal](crates/umbra-journal-file/README.md),
binds the namespace, renders a per-run Seatbelt profile, launches the target
stopped behind an installed sandbox, drives the syscall loop, and tears the run
down in order. Nothing in that path prompts: a missing capability, provider,
mount or permission is an actionable error and a nonzero exit.

```sh
umbra run --registry providers.json --workspace . --experimental   -- /absolute/path/to/program args...
```

Measured on macOS 26.5.1 / Apple Silicon: all seven fixture subcommands captured
into the run's shadow with exact expected bytes, no host writes, the writer lease
released and a durable run-completion record, in both the local-development and
mounted-NFSv4 storage modes.

Vendor agent adapters remain stubs, so `--agent` reports `NotImplemented`.
Checkpoint, resume, recovery from a nonempty journal, and strict remote
durability are not implemented or not qualified; a run's base layer detects
workspace change rather than snapshotting content. Resume means restarting an
agent from persisted filesystem and session state, not migrating a live process.

The workspace uses Rust edition 2021 and the toolchain pinned in
[rust-toolchain.toml](rust-toolchain.toml).
[CI](.github/workflows/ci.yml) runs formatting, Clippy with warnings denied,
workspace checks, tests, and doctests on macOS and Linux for pushes to `master`
and pull requests. The additional native qualification job runs the sandbox,
IPC, tracer fixture, and local-storage CLI matrix suites on an
`umbra-integration` macOS ARM64 runner. It requires debugger permission. The
NFS-backed CLI matrix (`nfs_fixture_matrix`) is opted out of CI via
`UMBRA_TEST_SKIP_NFS_MATRIX` because the current storage-nfs adapter needs a
real NFSv4 kernel mount and macOS Sequoia/Tahoe blocks that path from a
launchd context without user-approved MDM; that test still runs in local dev
when `UMBRA_TEST_NFS_ROOT` is set. A userspace NFSv4 backend that removes the
local-mount requirement is planned; when it lands the opt-out is retired.
Fork pull requests cannot run the qualification job, and its checkout does
not persist job credentials. A labeled runner must be provisioned before this
check can supply a CI signal; a queued qualification job is not enforcement
evidence.

If you're contributing, [the Memoria guide](docs/memoria.md) explains how we keep
READMEs connected to the code they describe. It covers the review flow and the
documentation check that runs in CI.

Provider executables can be checked through the [runtime registry](docs/providers.md).
