# umbra-cli

Clap command interface for the `umbra` binary, plus the in-process composition that
turns an explicit provider registry into a run request. This crate owns
`src/lib.rs`, `src/main.rs`, `src/composition.rs`, the `src/commands/` handlers and
the CLI tests; it has no implementation-crate dependencies, including in tests.

`run` is operational. `stop`, `checkpoint`, `resume` and `inspect` are stubs that
report `not implemented` on stderr and exit 1; they no longer echo their parsed
arguments, because argv and environment carry paths and secrets.

```text
umbra run --registry PATH [--workspace PATH] --experimental
      [--local-dev | --strict-remote] [--agent ID]
      [--env NAME=VALUE]... [--inherit-env NAME]... -- COMMAND [ARGS...]
umbra [--storage-root PATH] stop|checkpoint|inspect RUN_ID
umbra [--storage-root PATH] resume RUN_ID [--workspace PATH] [-- AGENT_ARGS...]
umbra providers --registry PATH [--role ROLE]
```

## No prompts

Missing configuration, capabilities, permissions, tools or mount prerequisites
produce an actionable error and a nonzero exit. The CLI never asks for
confirmation, reads stdin for setup, invokes `sudo`, runs an installer, mounts
anything, or falls back from the requested storage to something weaker. stdin
belongs to the supervised program; the CLI's own tests run with it closed.

## Selecting a run

`--registry` is required and is the single source of storage configuration.
`--experimental` is required: it acknowledges a bounded experimental tracing mode
as an argument, and disables nothing.

Storage mode is chosen by flag and enforced by negotiated capability, not by
provider identity. The default is a validated NFSv4 mount with client-fsync
durability, requiring the storage descriptor to declare `mounted-nfsv4-v1`;
`--local-dev` requires `local-development-v1` instead. Both additionally require
`experimental-open-rewrite-v1`, and the platform descriptor must declare
`sandboxed-stopped-launch-v1` and `experimental-syscall-rewrite-v1`. Descriptors
are checked before any provider is started, and the provider handshake then
rejects a connection whose backend does not actually advertise the name.
`--strict-remote` is a deterministic `UnsupportedCapability` error: no storage
provider qualifies strict remote durability.

Omitting `--agent` runs the trailing `--` arguments as a command; the executable
argument must be absolute, because PATH is never searched. Under the required
sandbox profile, the target observes `argv[0]` as its resigned twin cache path:
`sandbox-exec` provides no way to preserve the requested `argv[0]`. `argv[1..]`
and `_NSGetExecutablePath` are unaffected by enforcement (the latter already
returns the twin path on both launch paths). Supplying
`--agent ID` requires the registry's agent provider to have that exact ID and then
reports `NotImplemented`, since adapters are not implemented.

The supervised environment is built, not inherited: `--env NAME=VALUE` sets a
variable and `--inherit-env NAME` forwards one from the caller, failing if it is
unset rather than passing an empty value. Repeated names across either flag are
rejected, so the child never receives duplicate environment entries. The supervisor
adds `TMPDIR`.

## Exit codes and streams

Clap usage diagnostics exit 2. Every structured failure exits 1, including a
supervised program that exited nonzero, which surfaces as `ProcessFailed` after a
clean teardown. Exit 0 means the root process succeeded **and** the run's data,
journal and writer lease were closed cleanly. The child's own exit code is not
propagated: a 0 would describe the child while saying nothing about persistence.
Run status goes to stderr; stdout stays the supervised program's.

## Compatibility changes

`--agent` no longer defaults to `codex`; the default prevented a raw command
route. `--storage-root` no longer defaults to `.umbra` and is rejected outright by
`run` rather than silently ignored, since storage lives in the registry.

## Composition

`composition::load_registry(path)` performs a bounded read, decode and validate.
`composition::run_spec(args, registry, observer)` builds a `RunSpec` without
provider I/O: it canonicalizes the workspace, preserves argument and path bytes,
rejects NUL, and constructs the explicit environment.
`composition::build_supervisor(run_id, registry)` remains the `providers`
diagnostic path, connecting platform, storage and journal, optionally connecting
an agent when the registry declares one, and delegating
standard namespace construction to `umbra-overlay`. A configured `namespace`
provider replaces the standard engine there; `run` refuses one, because the
namespace protocol has no run lifecycle.

`umbra providers --registry PATH [--role ROLE]` validates installed provider
connections and starts trusted provider processes but no tracees. See
[registry configuration](../../docs/providers.md).

```sh
cargo check -p umbra-cli
cargo test -p umbra-cli
```

The committed `tests/run_fixtures.rs` runs seven cases each against local storage
and an existing NFSv4 mount. Build providers with `cargo build --workspace --bins`
and compile `experiments/fixtures/umbra-test-child.c` first. Set
`UMBRA_TEST_FIXTURE_PATH` and `UMBRA_TEST_NFS_ROOT`, then run
`UMBRA_INTEGRATION_REQUIRED=1 cargo test -p umbra-cli --test run_fixtures -- --nocapture`.
Required mode fails on missing inputs; ordinary developer tests may skip. Failed
commands report their captured stderr before any required run-ID parse. Cleanup
uses an emitted prepared ID even when that status assertion fails; without one,
it touches no directory in the shared NFS mount.

Set `UMBRA_TEST_SKIP_NFS_MATRIX=1` to skip the `nfs_fixture_matrix` case even
under `UMBRA_INTEGRATION_REQUIRED=1`. CI uses this because the current
storage-nfs adapter needs a real NFSv4 kernel mount and macOS Sequoia/Tahoe
gates that path from a launchd context without user-approved MDM. Local dev
runs the case normally; the escape hatch retires once a userspace NFSv4
backend lands.
