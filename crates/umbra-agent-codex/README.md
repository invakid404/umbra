# umbra-agent-codex

Stub implementation of `umbra_agent::Agent`, depending only on `umbra-agent` and
`umbra-core`. `CodexAgent` holds `CodexConfig`: the logical executable path, exact
CLI version, logical `CODEX_HOME` and `CODEX_SQLITE_HOME`, command prefixes, history
retention and disabled-update requirements. Common HOME/TMPDIR/XDG roots and the
logical workspace cwd are supplied by `AgentLaunchRequest`.

The default interactive command shapes are `codex [arguments] [prompt]` and
`codex resume [arguments] <recorded session ID> [prompt]`. The session ID comes from
`AgentSession`; resume must preserve its exact version, logical roots, transcripts,
SQLite state and versioned metadata. Vendor settings and sandbox composition still
need qualification against the pinned CLI version.

All four fallible Agent methods return `UmbraError::not_implemented`; capabilities
are empty. Construction only stores configuration. No plans, process launches,
filesystem changes, session discovery, credentials handling or provider IPC are
implemented. Process control and enforcement belong to the supervisor.

Validate with `cargo check -p umbra-agent-codex`.
