# umbra-cli

Clap command interface for the `umbra` binary, plus the in-process composition that
turns an explicit provider registry into a run request. This crate owns
`src/lib.rs`, `src/main.rs`, `src/composition.rs`, the `src/commands/` handlers and
the CLI tests; `[dependencies]` names no implementation crate, so nothing links a
backend into the binary. Exactly two reach the tests — `umbra-journal-file` and
`umbra-storage-local`, for the reason recorded at `Cargo.toml:27-29`.

`run` and `resume` are operational. `stop`, `checkpoint` and `inspect` are stubs
that report `not implemented` on stderr and exit 1; they no longer echo their
parsed arguments, because argv and environment carry paths and secrets.

`resume` reopens an existing run and reports whether what the last session left
can be reconciled. It does not relaunch anything — that would need checkpoint-based
recovery, which is unimplemented. A run that requires recovery prints a status line
on stderr and exits nonzero, so a script cannot read success from a run it cannot
use; a healthy run exits 0. Run enumeration is out of scope: the run ID is an
argument.

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
`--local-dev` requires `local-development-v1` instead. Descriptors are checked
before any provider is started, and the provider handshake then rejects a
connection whose backend does not actually advertise the name.

`run` additionally requires `experimental-open-rewrite-v1` from the storage
descriptor, and `sandboxed-stopped-launch-v1` plus
`experimental-syscall-rewrite-v1` from the platform descriptor. `resume` requires
neither set: it resolves nothing, rewrites nothing and launches nothing, so
demanding capabilities it never exercises would turn an unqualified claim into a
passing check. It connects storage and journal only, and enforces the persistence
mode exactly as `run` does.
`--strict-remote` is a deterministic `UnsupportedCapability` error: no storage
provider qualifies strict remote durability. A registry that configures a
`namespace` role is refused the same way, whatever that descriptor declares: the
supervisor does not yet route a run to a configured namespace provider, so
neither `umbra run` nor `umbra resume` can use one — both go through the same
admission, so a registry one refuses the other cannot accept. `namespace-run-lifecycle-v1` names the capability such
a provider will have to advertise; adding it to a descriptor changes the error
message, not the outcome. Capabilities in a registry are operator-written claims,
qualified by the provider handshake only for roles the run actually connects.

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

Run status goes to stderr, so `umbra`'s own diagnostics never reach stdout. The
supervised program's streams are not the caller's, though: it inherits fds 0-2
from the platform provider process, which the provider transport starts with
stdin and stdout on `/dev/null` and stderr inherited
(`umbra-core/src/provider/transport.rs:321-323`). A supervised program's stderr
therefore reaches the caller and its stdout is discarded — `/bin/cat` under
`umbra run` exits 0 and prints nothing. Plumbing the tracee's stdout through to
the caller is unimplemented.

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
supervisor does not yet route a run to a configured namespace provider — see
[Selecting a run](#selecting-a-run).

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

The same file's `local_utility_matrix` and `nfs_utility_matrix` run standard
utilities behind the same gating — `/usr/bin/touch`, `/bin/mkdir`, `/bin/cat` and
`/bin/ls` — one `umbra run` per invocation, each against its own freshly seeded
workspace. `/bin/mkdir` is expected to fail there: bare `mkdir(2)` is outside the
tracer's rewritten syscall set, so its operand reaches the kernel unrewritten and
the unconditional sandbox refuses it, and nothing is then created on the host or
in the shadow.

The two read utilities each run twice, on an operand that exists and on one that
does not. The failure leg pins that the program ran at all, by requiring its own
`No such file or directory` line on stderr: a tracee's stderr reaches the caller
even though its stdout does not, so that line is evidence nothing a no-op could
produce. Neither leg pins *which* resolver answered — each operand is present or
absent on the host exactly as it is in the run — so redirection into the shadow
is pinned by the `/usr/bin/touch` case's exact-bytes assertion instead.

Three further cases re-execute the test binary itself as the tracee to pin the
exit surface: a child that exits 41, one that raises `SIGKILL` and one that
raises `SIGSEGV`. The exit-41 case leaves `umbra` at exit 1 because a child's
status is never propagated. The two signalled cases never reach that rule: the
platform backend refuses a stop it cannot resume from, so the run fails with the
platform's error, records no completion, and still exits 1.

Set `UMBRA_TEST_SKIP_NFS_MATRIX=1` to skip the `nfs_fixture_matrix` and
`nfs_utility_matrix` cases even under `UMBRA_INTEGRATION_REQUIRED=1`. CI uses
this because the current storage-nfs adapter needs a real NFSv4 kernel mount
and macOS Sequoia/Tahoe gates that path from a launchd context without
user-approved MDM. Local dev runs the cases normally; the escape hatch retires
once a userspace NFSv4 backend lands.
