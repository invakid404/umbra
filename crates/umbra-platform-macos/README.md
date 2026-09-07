# umbra-platform-macos

macOS tracing implementation boundary, depending only on `umbra-core` and
`umbra-platform` among Umbra crates.

`MacosTraceBackend` implements `TraceBackend` and `TraceControl`; `DarwinArm64Abi`
implements `SyscallAbi`. Both are resource-free stubs available on every build target.
Capability reporting returns no supported architectures or capabilities. All runtime
operations return `UmbraError::not_implemented("macos: <method>")` without
changing buffers, registers, or process state.

`mach2` and the `MachPort` alias are gated to macOS. No Mach APIs are called.
The Python v2 tracer in `experiments/tracer/umbra_tracer.py` is behavioral reference
for a future port; none of its behavior is implemented here. Lifecycle control,
provider IPC, tracing, enforcement, and ABI support remain unimplemented.

Check with `cargo check -p umbra-platform-macos`; a non-macOS target can be checked
with `cargo check -p umbra-platform-macos --target x86_64-unknown-linux-gnu` once
that Rust target is installed. Successful compilation does not qualify tracing.
