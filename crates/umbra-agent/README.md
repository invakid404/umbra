# umbra-agent

The object-safe `Agent: Send` contract describes agent launch, resume, session
discovery, and orderly stop. This crate depends only on `umbra-core`; it contains no
vendor CLI implementation or process launcher. Rust edition 2021 and the workspace's
pinned Rust 1.98.1 toolchain apply.

```rust,ignore
pub trait Agent: Send {
    fn capabilities(&self) -> AgentCapabilities;
    fn launch_plan(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan>;
    fn resume_plan(
        &self,
        request: &AgentResumeRequest,
        session: &AgentSession,
    ) -> Result<AgentLaunchPlan>;
    fn observe(&mut self, event: &AgentEvent) -> Result<Option<AgentSessionUpdate>>;
    fn stop_plan(&self, session: &AgentSession) -> Result<AgentStopPlan>;
}
```

The DTOs are owned by `umbra-core` and re-exported here. `Result` is
`umbra_core::Result`. Consumers can inject `Box<dyn Agent>` without importing an
implementation crate. Errors retain a machine-readable category and context.

Configuration paths are logical byte paths, preserving non-UTF-8 names. A launch
plan supplies argv, environment, cwd, and state-directory requirements. The
supervisor validates the plan, prepares the namespace, installs enforcement, and
launches through the platform contract. An adapter does not spawn a child, signal
a process, create state directories, or mutate supervisor state.

Persist an `AgentSession` with an open provider ID, exact agent version, stable
session ID, logical state paths, and versioned adapter metadata. Serialize the
shared core record using its supported format; do not serialize a Rust trait object
or use Debug output as a persistent format. The supervisor owns durable publication
in the run's control state. Records exclude physical mount roots, process handles,
and external credentials. Resume validates identity, versions, and metadata before
starting a new process; an incompatible or missing session is an error. This is
session resumption, not live process migration.

## Adding a new agent backend

1. Create `crates/umbra-agent-example` and add it to the root workspace members.
   Inherit workspace metadata and use these direct Umbra dependencies:

   ```toml
   [package]
   name = "umbra-agent-example"
   version.workspace = true
   authors.workspace = true
   license.workspace = true
   edition.workspace = true

   [dependencies]
   umbra-agent = { path = "../umbra-agent" }
   umbra-core = { path = "../umbra-core" }
   ```

2. Define your concrete adapter and implement all five methods above. Keep its
   constructor outside the trait. Pin and qualify an exact vendor CLI version;
   document supported session/metadata formats and advertise only verified
   capabilities. Check actual flags and environment behavior against that version.
   Return explicit unsupported or compatibility errors for unqualified behavior.

3. Map configuration and state locations into the plan's logical namespace.
   Preserve common HOME, TMPDIR, XDG_CONFIG_HOME, XDG_CACHE_HOME, XDG_STATE_HOME,
   and the logical workspace cwd. Model additional roots as adapter data rather
   than introducing an `AgentKind` enum or vendor methods on this trait.

   | Adapter example | Required logical state and policy |
   | --- | --- |
   | Codex | CODEX_HOME, a separately persisted CODEX_SQLITE_HOME, exact CLI version, stable session ID, and history needed for transcript resume |
   | Claude Code | CLAUDE_CONFIG_DIR, stable CLAUDE_CODE_PROJECT_DIR_NAME, CLAUDE_CODE_TMPDIR, exact CLI version, stable session ID, and history retention |

   Disable automatic updates for the resumable run. Cooperative process wrapping
   must re-enter supervision; it does not establish descendant capture. Both
   vendor settings and any inner sandbox supplement outer enforcement. Keep
   Keychain and other credentials external; the destination authenticates
   independently or through an approved secret provider.

4. Implement `observe` over bounded supervised evidence and lifecycle events. Emit
   an update only when evidence establishes the stable session identity; return
   `None` for irrelevant events and an error for invalid discovery evidence.
   Persist all metadata needed to resume, including any stable project-history
   identity. In `resume_plan`, validate provider/version/format compatibility and
   reuse the recorded ID and logical paths. Never fall back to a new or latest
   session. In `stop_plan`, describe the orderly shutdown action for the supervisor.
   Only the supervisor reconciles transactions, flushes state, handles descendants,
   and publishes a clean checkpoint.

5. For in-process library use, construct the adapter and inject it as
   `Box<dyn umbra_agent::Agent>`. For the specified CLI architecture, supply a
   provider binary and install an explicit runtime descriptor with an open ID,
   agent role, protocol version, executable path, and capabilities. Select it by
   runtime configuration. The provider constructs its own adapter; the generic
   agent transport must own bounded, framed, versioned DTO messages, request IDs,
   explicit errors, a pre-launch handshake, timeouts, and disconnect handling.
   Keep its private IPC resources out of tracees. Use `provider::serve_provider` and `provider::Proxy`; both are implemented
   in this crate. Do not add backend imports, factory match
   arms, or features to the supervisor or CLI to bypass that boundary.

6. Test with injected adapters: launch plans, required roots, update/retention
   policy, session discovery, and serialization round trips retaining session IDs,
   metadata, and non-UTF-8 paths. Reject incompatible versions, missing external
   credentials, and discovery failures without changing persisted data. Run a
   supervised launch, orderly checkpoint, and resume at a different physical mount
   point and verify the same logical workspace, history, project identity, and
   session ID. Exercise provider disconnects when the transport is integrated.
   Build the contract with `cargo check -p umbra-agent` and the adapter with its
   own package check. A successful build does not qualify a vendor CLI or platform.
