# umbra-platform-macos

Darwin arm64 tracing backend. Direct Umbra dependencies are `umbra-core` and
`umbra-platform`; `mach2` and `libc` are gated to `cfg(target_os = "macos")`.
Non-arm64-macOS builds still compile (through `src/unsupported.rs`) — this
covers non-macOS targets and macOS on x86_64 — but every runtime call
structurally reports `UnsupportedCapability`.

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

**Provider IPC.** `TraceBackend::launch` and the compatibility method
`launch_experimental` share `native.rs::MacosTraceBackend::launch_traced`:
both launch suspended, sanitize inherited descriptors, and install the same
syscall interception loop before returning. Launch requires explicit
`LocalDevelopment`, stdio descriptors only, absolute executable/cwd, and
non-empty argv. A backend accepts one run, including after termination.
The caller must rewrite write-intent opens before resuming; this remains the
M1 feasibility tracer, without independent production sandbox qualification.

Invoke the shipped binary through a platform `ProviderDescriptor` and
`umbra_core::provider::Client::connect`, then send `Request::Capabilities`,
`Request::Launch(spec)`, and the `NextEvent` / memory / register / `Resume`
requests. The binary's `serve_provider("macos", …)` handles that handshake
and dispatch. For an in-process socketpair, use `serve_provider_on(connection,
"macos", …)`, which shares the same handshake and dispatcher.
`Request::Quiesce(process)` interrupts live sessions and waits for stopped
acknowledgements, returning the root and live task IDs. Stop replies remain
queued for `NextEvent`; consume them before resuming that thread. If a syscall
return is in flight, quiesce leaves the tree stopped and reports an error:
drain events to a transaction boundary before retrying. Interrupted transport
failures terminate held tasks. Finish with `Request::Terminate` using
`TerminationPolicy::Immediate`.

`tests/provider_ipc.rs` exercises the socketpair handshake and real `open-libc`
capture through these requests, including stopped and running quiescence.
It uses the fixture env vars and prints `SKIP` if either is missing:

```
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos --test provider_ipc -- --nocapture
```

## Test coverage

`tests/fixtures.rs` drives Track D's C fixtures through the Rust tracer.
It reads `UMBRA_TEST_FIXTURE_PATH` (path to `umbra-test-child`) and
`UMBRA_TEST_REDIRECT_ROOT` (shadow root); when either env var is unset,
each test body skips via `eprintln` — the binary still reports the cases as
passed, so qualification requires the `CAPTURED <case>` stderr verdict.
All seven cases are enabled. Fixture cases:

| Case | State |
|---|---|
| `open-libc`         | **CAPTURED** — the M1 minimum acceptance bar |
| `open-svc`          | **CAPTURED** |
| `fork-write`        | **CAPTURED** |
| `posix-spawn-write` | **CAPTURED** |
| `exec-write`        | Enabled; fails inside a post-exec debugger interaction loop (see M2 gap below) |
| `grandchild-write`  | **CAPTURED** |
| `dup-inherit-write` | **CAPTURED** |

Software breakpoint ownership is per debugserver connection: successful
`Z0` installs populate that session's registry, and successful `z0`
removals remove entries. Fork children receive only inherited instruction
repairs before installing their own breakpoints. Temporary return and dyld
breakpoints use the same registry; the parent's fork gate uses `Z1`/`z1`.

The `exec-write` case is enabled without `#[ignore]` so its running repro
travels with the tree. Its assertions remain intact.

## M2 gap: `exec-write` post-exec breakpoint loop

M1.5 closed the fork-gate teardown E08 for the other five multi-process
fixtures and reacquired the child's Mach task port on `reason:exec`
(fixing the pre-M1.5 `mach_vm_read` status `268435459` /
`MACH_SEND_INVALID_DEST` failure). `exec-write` now advances past the
`reason:exec` boundary, emits an `Exec` event, and reaches its first
post-exec `SyscallEntry` at PC `0x18c9d2690` — an `__openat`-family stub
in the shared cache. From there it enters a bounded live-lock:

- The tracer sends `z0,18c9d2690,4` (`OK`) + `Z0,18c9d2694,4` (`OK`) +
  `c` — remove the entry BP, install the return-gate BP at `pc+4`,
  continue.
- The very next stop reports PC = `0x18c9d2690` again — the ENTRY
  address, NOT the gate `0x18c9d2694`.
- This repeats ~3024 times with successful register reads between
  continuations until the 25 s session watchdog fires with `session
  timed out`.

Both `z0` and `Z0` are ACK'd; the tracee behaves as if `z0` did not
clear the trap. Two live hypotheses, neither yet distinguished:

- **(H-A) Debugserver Z0 shadow-state desync with the post-exec address
  space.** Debugserver tracks per-address original instruction bytes to
  restore on `z0`. If the post-exec shared-cache mapping presents a
  different physical page, its recorded shadow may no longer match the
  live memory and its `z0` restore may write bytes that don't clear the
  BRK trap.
- **(H-B) Post-exec BRK-exception PC-advance / i-cache coherency
  issue.** If the CPU re-executes the previous instruction bytes at PC
  on re-continuation (e.g. because the `M` write we made to clear the
  BRK on this cycle is not coherent with the instruction fetch
  pipeline), the CPU loops on the same BRK site.

Reproducing the forensic signature: run
`cargo test -p umbra-platform-macos --test fixtures exec_write --
--nocapture` with `UMBRA_RSP_LOG=1`; the log will contain 3024 identical
`T…thread:<id>;` stops at PC `0x18c9d2690`, each followed by successful
register reads and a `c` continuation. Distinguishing the hypotheses
requires interactive debug: `qMemoryRegionInfo` around the entry PC,
`M` reads of the four bytes at the entry PC immediately after the `z0`
ACK, and single-stepping (`vCont;s`) the first post-exec continuation
instead of `c`.

Run with verdicts visible:

```
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos --test fixtures -- --nocapture
```

`src/rsp.rs::tests::debugserver_reverse_connect_no_ack_handshake` spawns a
real debugserver and asserts the negotiated `qHostInfo.ostype` is
`macosx`, guarding the reverse-connect nonblocking-inheritance fix from
regressing. It requires `xcode-select -p` to resolve; adding a
skip-on-missing-tools guard is on the M1.5 polish list.

## Verification

```
cargo check -p umbra-platform-macos
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos
```

Successful compilation does not qualify tracing on any target — the M1
minimum bar is what `open-libc` CAPTURED demonstrates.
