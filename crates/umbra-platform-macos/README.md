# umbra-platform-macos

Darwin arm64 tracing backend. Direct Umbra dependencies are `umbra-core` and
`umbra-platform`; `mach2` and `libc` are gated to `cfg(target_os = "macos")`.
Non-macOS builds still compile (through `src/unsupported.rs`), but every
runtime call structurally reports `UnsupportedCapability`.

The crate implements the v2 experimental tracer described in
`experiments/tracer/umbra_tracer.py` and `experiments/tracer/REPORT-v2.md`
using a hybrid debugger interface: **Apple's `debugserver` over GDB Remote
Serial Protocol** for stop-and-control, and **`mach2` (`task_for_pid` +
`mach_vm_read_overwrite`) for victim-side memory reads**. Rationale: the
signed Apple debugserver keeps working across OS releases and maps cleanly
onto the Python v2's attach model; direct Mach for memory keeps the decode
callback out of the RSP round-trip. The RSP client (`src/rsp.rs`) is a
small, bounded reverse-connect implementation with framed `$…#CC` packets,
RLE decode, and a per-request deadline.

## Public API

- `MacosTraceBackend` implements `umbra_platform::TraceBackend` and
  `umbra_platform::TraceControl`.
- `DarwinArm64Abi` implements `umbra_platform::SyscallAbi`; ABI decode and
  path rewrite live in `src/abi.rs` as a pure module (no debugger I/O),
  matching the spec's testable-core requirement.
- `Options { debugserver, twin_cache, timeout_ms }` — runtime configuration.
  `debugserver` overrides the `xcode-select -p` discovery; `twin_cache`
  overrides the `$HOME/Library/Caches/umbra/twins` default. No user-specific
  paths are baked into the code.

## Shipped mechanisms

The M1 mechanisms named in the tracker spec are all present:

1. **Resign twin cache** at `$HOME/Library/Caches/umbra/twins/<sha256>/<basename>`
   via `codesign -f -s - --entitlements … --preserve-metadata=identifier,flags,runtime`.
   Entitlements match `experiments/gate-1/ent.plist`; a copy is shipped as
   `ent.plist` alongside `src/`.
2. **Arm64 `svc #0x80` breakpoints** verified byte-for-byte in
   `libsystem_kernel` for `__open`, `__open_nocancel`, `__openat`,
   `__openat_nocancel`, `__execve`, `__posix_spawn`, `__fork`.
3. **Bounded 4 KiB path decode** from `x0`/`x1` with page-boundary-aware
   chunked reads (`abi::read_path`).
4. **Rewrite** via scratch allocation on the tracee task plus register
   update (`Session::allocate` + `abi::prepare_path`).
5. **`svc+4 → b .` fork gate**: the parent's return site is patched to an
   infinite branch-to-self before executing fork; the child's COW-inherited
   copy spins there until we `task_for_pid` + attach a fresh debug session,
   restore bytes at the gate and at each inherited breakpoint site in the
   child, then reinstall the child's own breakpoints. See
   `native.rs::ReturnKind::Fork`.
6. **`__wait4` / `__wait4_nocancel` deferred wait**: debugger attach
   transiently reparents children, so parents get `ECHILD`; the tracer
   intercepts and defers, and return breakpoints track reaped children.
   See `native.rs::ReturnKind::Wait`.
7. **SIGHUP suppression** on newly attached children.
8. **Embedded watchdog**: an `mpsc`-cancellable worker thread terminates
   all held Mach tasks on deadline (`native.rs::Watchdog`).

## Provider binary

`src/main.rs` is the shipped provider binary. It calls
`umbra_platform::provider::serve_provider("macos", …)` returning a
`PlatformSession { control: MacosTraceBackend, abi: DarwinArm64Abi }`, and
decodes JSON `Options` from opaque provider options (empty falls back to
`Options::default()`). Register it via `umbra providers --registry <path>
--role platform`.

## Test coverage

`tests/fixtures.rs` drives Track D's C fixtures through the Rust tracer.
It reads `UMBRA_TEST_FIXTURE_PATH` (path to `umbra-test-child`) and
`UMBRA_TEST_REDIRECT_ROOT` (shadow root); the whole test binary is skipped
when either env var is unset. Fixture cases:

| Case | State |
|---|---|
| `open-libc`         | **CAPTURED** — the M1 minimum acceptance bar |
| `open-svc`          | **CAPTURED** |
| `fork-write`, `posix-spawn-write`, `exec-write`, `grandchild-write`, `dup-inherit-write` | `#[ignore]` — deferred to M1.5 |

The multi-process cases are gated behind `#[ignore]`. The post-fork
teardown path (`native.rs::ReturnKind::Fork { restore }`) currently emits
`z0,ADDR,4` to the child's freshly attached debugserver session for
addresses that session never installed, so debugserver returns `E08`. The
fix is deferred to M1.5. Opt-in run:

```
cargo test -p umbra-platform-macos --test fixtures -- --ignored
```

`src/rsp.rs::tests::debugserver_reverse_connect_no_ack_handshake` spawns a
real debugserver and asserts the negotiated `qHostInfo.ostype` is
`macosx`, guarding the reverse-connect nonblocking-inheritance fix from
regressing. It requires `xcode-select -p` to resolve; adding a
skip-on-missing-tools guard is on the M1.5 polish list.

## M1.5 roadmap gaps

- **`TraceBackend::launch` and `TraceControl::quiesce` return
  `UnsupportedCapability`.** The trait doc requires that failure never
  permit an uncontrolled host mutation, so the honest refusal is
  appropriate for M1. The consequence is that the provider IPC path is not
  yet reachable end-to-end — the fixture test drives the inherent
  `MacosTraceBackend::launch_experimental` (which does the fork-gate
  attach) instead. Wiring the provider `Request::Launch` through the same
  path is on the M1.5 roadmap.
- Multi-process fork-gate breakpoint teardown (`E08` above).

## Verification

```
cargo check -p umbra-platform-macos
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos
```

Successful compilation does not qualify tracing on any target — the M1
minimum bar is what `open-libc` CAPTURED demonstrates.
