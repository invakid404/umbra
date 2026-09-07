# umbra-platform-linux

Empty precedent for a future Linux platform backend (handoff §7).

`LinuxTraceBackend` implements all seven `TraceBackend` methods, and
`LinuxSyscallAbi` implements all three `SyscallAbi` methods. `TraceControl` advertises no capabilities or architectures, and its lifecycle
methods return explicit stub errors. Every fallible method returns
`Err(UmbraError::not_implemented("linux: <method>"))` without side effects. The core
constructor categorizes these errors as `NotImplemented`.

The crate depends only on `umbra-core` and `umbra-platform`, inherits workspace
metadata, and uses no native bindings. `cargo check -p umbra-platform-linux` works
on macOS. Compilation does not establish Linux support.

Future work includes ptrace transport, aarch64/x86-64 ABI decoding, enforcement,
and platform qualification. The package supplies a provider executable using the
shared platform harness, plus an installation descriptor template. Follow the
[platform backend walkthrough](../umbra-platform/README.md#adding-a-new-platform-backend)
for implementation, runtime registration, and qualification requirements.
