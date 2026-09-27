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
    fn io_buffer(&self, regs: &RegisterSet) -> Result<Option<IoBuffer>>;
    fn encode_stat(&self, stat: &BlobStat) -> Result<Vec<u8>>;
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
the rewrite. The client proxy requires the returned operation and single path
operand to match the request before exposing the plan. It writes nothing and resumes nothing. `operation` is the *prepared*
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
use `<platform>-<arch>-abi-v<decimal version>`; run composition requires exactly
one for its negotiated architecture. Protocol version 2 rejects stale providers
before decoding the required sandbox policy and rewrite messages.

## Data transfers and the `IoBuffer` seam

`FsOp` is ABI-independent: `FsOp::Read` and `FsOp::Write` say how *many* bytes
move and never *where*. Only the ABI can read an address out of a register set,
so `SyscallAbi::io_buffer` reports it beside the decoded operation rather than
inside it, and the two must agree about the byte count -- a disagreement would
write past the binding. It takes no `TraceMemory`, because the address is in the
registers the caller already holds.

Its default answers `None`, which is correct for every path operation and for a
backend that services no data transfer. A consumer needing a buffer and getting
`None` fails with its own diagnosis rather than reading address zero.

`fstat` is the one call whose buffer is an *output* rather than a transfer, and
it is reported through the same seam: its length is not a caller-supplied count
but the fixed width of the layout the kernel would have written, so a partial
write would leave the tail of the tracee's `struct stat` stale. A null or
overflowing address is refused there with `EFAULT` carried **inside** the error,
for the caller to bind as a tracee-visible refusal rather than raise --
`fstat(fd, NULL)` is an ordinary program bug and must not stop a run.

`SyscallAbi::encode_stat` is the other half of answering it. The namespace
resolves *what* the metadata is and only the ABI knows what it looks like in
memory, so the two meet at the caller: a pure function of the metadata, no
registers and no stopped task, which is what lets the caller hold it until after
resolution. It must describe the **logical** object -- a logical symlink is a
symlink with its target's length, never the placeholder a shadow stores for it
-- and must fail rather than approximate a kind or mode its layout cannot
represent. Its default refuses, so a backend with no native stat layout cannot
be mistaken for one that has it.

`LaunchPolicy` carries two fields a backend must honour together or refuse:
`interpose`, which loads umbra's userspace-routing library into the target image
before its first instruction, and `descriptor_limit`, which fences
`RLIMIT_NOFILE` -- soft *and* hard -- before the final exec so the kernel's
descriptor range and the interposer's virtual one cannot overlap for the
lifetime of the tracee or any descendant. A backend that cannot do both must
refuse the launch: an interposed tracee whose descriptors are not fenced can be
handed a kernel descriptor number that already names something else, which is a
wrong-object read rather than a refusal.
