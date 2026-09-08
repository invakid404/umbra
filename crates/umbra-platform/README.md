# umbra-platform

Pure synchronous tracing contracts, depending only on `umbra-core`. This crate has
no native bindings, backend selection, implementation dependencies, or mandatory
`Send` bound. Shared DTOs are defined in core and re-exported here. The workspace
uses Rust 1.98.1 and edition 2021.

The public contracts preserve the spec's signatures:

```rust,ignore
pub trait TraceBackend {
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle>;
    fn next_event(&mut self) -> Result<TraceEvent>;
    fn read_memory(&mut self, task: TaskId, address: u64, out: &mut [u8]) -> Result<()>;
    fn write_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()>;
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet>;
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()>;
    fn resume(&mut self, command: ResumeCommand) -> Result<()>;
}

pub trait TraceMemory {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()>;
}

pub trait SyscallAbi {
    fn decode_entry(
        &self,
        regs: &RegisterSet,
        memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>>;
    fn apply_rewrite(&self, regs: &mut RegisterSet, rewrite: &PreparedRewrite) -> Result<()>;
    fn emulate_result(&self, regs: &mut RegisterSet, result: &EmulatedResult) -> Result<()>;
}

pub trait TraceControl: TraceBackend {
    fn capabilities(&self) -> PlatformCapabilities;
    fn quiesce(&mut self, process: ProcessHandle) -> Result<QuiescedTree>;
    fn terminate(&mut self, process: ProcessHandle, policy: TerminationPolicy) -> Result<()>;
    fn prepare_rewrite(
        &mut self,
        thread: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite>;
}
```

`prepare_rewrite` plans a path redirection for a stopped thread's current syscall:
the backend allocates bounded scratch memory in the tracee, validates the operand
slots for its ABI, and returns the argument and memory writes that would perform
the rewrite. It writes nothing and resumes nothing. `operation` is the *prepared*
operation, whose flags may differ from the ones the tracee issued, because the
namespace may already have created the target. It is the one method with a default
implementation, and that default refuses, so a backend without a qualified scratch
mechanism cannot be mistaken for one that has it.

Every `LaunchSpec` carries a `SandboxRequirement`. A backend that accepts
`Required(profile)` must apply that profile and return only once the target is
stopped before its first instruction; if it cannot prove that boundary it must
fail the launch, and it must not advertise `sandboxed-stopped-launch-v1`.

Every `Result` is `umbra_core::Result`; errors retain category, operation/context,
and optional platform errno. `decode_entry` returns `None` only for a positively
classified non-filesystem syscall. Unknown potential mutations and unsupported
architectures return structured unsupported/denied errors. Memory adapters bind
reads to the stopped task, check pointer/length bounds, and preserve path bytes.
Normalized syscall exits include outcomes; events also carry child/fork/exec/exit
notifications. Backends own capture, instruction repair, sandboxing, and signals.

`PlatformSession` pairs `Box<dyn TraceControl>` with `Box<dyn SyscallAbi>`; its
factory must negotiate the same session and architecture for both. Constructors
stay outside the traits. Use `T: TraceControl + ?Sized` for consumers that need
both tracing and lifecycle methods, avoiding trait-object upcasting. Construct
thread-affine objects on the dedicated debug-control thread. Add `Send` only at
injection points that cross worker boundaries.

## Adding a new platform backend

1. Create `crates/umbra-platform-<x>` (for example,
   `crates/umbra-platform-freebsd`) with `src/lib.rs`, a README, and this manifest:

   ```toml
   [package]
   name = "umbra-platform-freebsd"
   version.workspace = true
   authors.workspace = true
   license.workspace = true
   edition.workspace = true

   [dependencies]
   umbra-platform = { path = "../umbra-platform" }
   umbra-core = { path = "../umbra-core" }
   ```

   Add `"crates/umbra-platform-freebsd"` to the root `[workspace].members`.
   These are its only direct Umbra dependencies. Native bindings and
   target-specific build settings belong in the new backend crate.

2. Implement all methods of `TraceBackend` and `TraceControl` for the tracing
   transport, and `SyscallAbi` for each supported ABI. Implement `TraceMemory`
   on an adapter holding the stopped task and access to its transport. For
   example, its `read` delegates to `backend.read_memory(task, address, out)`;
   do not resume the task while decoding. Return a `PlatformSession` from a
   constructor outside the traits, with both objects bound to the same session.
   Negotiate architecture/ABI pairs inside the provider. Preserve normalized
   entry/exit, structured errors, and byte-preserving memory semantics above.

3. Implement stopped launch, descendant capture before the first mutation,
   exec re-arming, memory/register access, signal/wait behavior, inherited-fd
   sanitation, and independent sandbox installation before unrestricted
   execution. Quiescence rejects new children/mutations and stops the whole
   tree at known transaction boundaries. Controller loss keeps tracees stopped
   or terminates them safely. Bound event polling so lease renewal remains
   timely. Advertise only capabilities qualified on the actual target.

4. Supply a provider binary in the same package, plus a versioned descriptor
   with an open provider ID (such as `freebsd`), platform role, executable path,
   protocol version, and capabilities. Install it explicitly and select its
   executable and opaque options in runtime role configuration. The specified
   integration uses a shared platform server harness and paired control/ABI
   proxy: use `provider::serve_provider` and `provider::connect`. Both are
   implemented, including nested read-only control calls during ABI memory callbacks.
   `provider::serve_provider_on` runs the same handshake and dispatcher on an
   injected private connection, allowing in-process socketpair integration tests.
   Adding a backend requires no factory match arm, supervisor dependency, CLI
   feature, or changes to existing backend crates.

   The shared protocol must use private control resources never inherited by
   tracees, framed/versioned/bounded messages, request IDs, explicit errors,
   owned bytes, and opaque resource tokens. Validate role/version/capabilities
   before launch; enforce timeouts, backpressure, and fail-closed disconnects.
   During `decode_entry`, a separate or multiplexed callback lane must let the
   server's `TraceMemory` request reads serviced by the client. Keep transport
   available and never hold a connection lock across a callback. Define this
   once in `umbra-platform`, not separately in each backend.

5. Run `cargo check -p umbra-platform-freebsd` and platform conformance cases
   covering raw/libc syscalls, immediate child and grandchild writes, exec,
   denied unsupported operations, and provider disconnects. A shared runtime
   conformance harness is not included in this trait-only crate; it remains
   required for backend qualification. Run an independent leakage oracle and
   record OS, architecture, signing, and enforcement prerequisites. Extend the
   qualification matrix for newly supported cases. Fixtures do not prove
   untested multithreaded, JIT, or vfork behavior. Gate 2's macOS six-case evidence
   is not production completeness or proof of real NFS durability.

Check this contract crate with `cargo check -p umbra-platform`, and compile its
trait-object smoke example with `cargo test -p umbra-platform --doc`.

The provider dispatcher rejects `UnsandboxedExperiment` before calling a backend:
IPC launch is exclusively the supervised contract path. ABI capability identities
use `<platform>-<arch>-abi-v<decimal version>`; run composition requires exactly one.
