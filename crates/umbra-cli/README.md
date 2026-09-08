# umbra-cli

Clap command interface for the `umbra` binary. Every valid run/stop/checkpoint/resume/inspect command prints
its parsed arguments to stdout, reports `not implemented` on stderr, and exits 1.
Help/version exit 0; invalid arguments exit 2. No run, checkpoint, or agent process is
created. Paths and trailing agent arguments preserve native OS bytes.

```text
umbra [--storage-root PATH] run [--agent ID] [--workspace PATH] [-- AGENT_ARGS...]
umbra [--storage-root PATH] stop RUN_ID
umbra [--storage-root PATH] checkpoint RUN_ID
umbra [--storage-root PATH] resume RUN_ID [--workspace PATH] [-- AGENT_ARGS...]
umbra [--storage-root PATH] inspect RUN_ID
```

`RUN_ID` is a UUID. Defaults are `.umbra` for local development storage, `codex` for
the run agent ID, and `.` for the logical workspace. `--storage-root` also works
after a subcommand. Agent IDs are open strings; operational commands do not yet
select providers. Runtime selection works through the `providers` command below.
Resume will eventually use the recorded agent identity. These flags are an initial
parsing interface; handoff §5 specifies the command names but no exact flag syntax.

`composition::build_supervisor(run_id, registry)` connects platform, agent, storage
and journal providers from an explicit `ProviderRegistry`, then delegates standard
namespace construction to `umbra-overlay`. A configured `namespace` provider replaces
the standard engine and owns its storage/journal connections through opaque options.
The CLI has no implementation-crate dependencies, including in tests.

`umbra providers --registry PATH [--role ROLE]` validates installed provider connections.
Without `--role`, it uses the actual supervisor assembly path; with a role, it checks
that contract's generic proxy. This command starts trusted provider processes but no
tracees. Local storage provider construction creates its configured directory.
Operational run/stop/checkpoint/resume/inspect handlers remain explicit stubs.
See [registry configuration](../../docs/providers.md) for format and installation.

```sh
cargo check -p umbra-cli
cargo build -p umbra-cli
```
