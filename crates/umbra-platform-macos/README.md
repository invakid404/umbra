# umbra-platform-macos

Darwin arm64 tracing backend. Direct Umbra dependencies are `umbra-core` and
`umbra-platform`; `mach2` and `libc` are gated to `cfg(target_os = "macos")`.
Non-arm64-macOS builds still compile (through `src/unsupported.rs`) — this
covers non-macOS targets and macOS on x86_64. Fallible tracing/control calls
report `UnsupportedCapability`, and capabilities are empty. The pure arm64 ABI
module remains available on those hosts.

The crate implements the v2 experimental tracer described in
`experiments/tracer/umbra_tracer.py` and `experiments/tracer/REPORT-v2.md`
using a hybrid debugger interface: **Apple's `debugserver` over GDB Remote
Serial Protocol** for stop-and-control, and **`mach2` (`task_for_pid` +
`mach_vm_read_overwrite`) for victim-side memory reads**. Rationale: the
signed Apple debugserver provides the attach model used by Python v2;
runtime behavior still requires qualification on each supported OS release.
Direct Mach for memory keeps the decode callback out of the RSP round-trip.
The RSP client (`src/rsp.rs`) is a
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
  paths are baked into the code. When `debugserver` is unset, the
  `UMBRA_DEBUGSERVER` environment variable overrides discovery before
  `xcode-select -p` is consulted; it exists so the test suites can run on
  hosts whose selected Xcode keeps debugserver under `SharedFrameworks`
  rather than the `Library/PrivateFrameworks` path probed below.

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

**Launch and enforcement.** `TraceBackend::launch` and `launch_experimental`
share `native.rs::MacosTraceBackend::launch_traced`: both launch suspended,
sanitize inherited descriptors, and install the same syscall interception loop
before returning. Launch requires explicit `LocalDevelopment` or
`NfsClientFsync` persistence, stdio descriptors only, absolute executable/cwd,
and non-empty argv. A backend accepts one run, including after termination.
The caller must rewrite write-intent opens before resuming.

The `SandboxRequirement` on the spec decides enforcement, and there is no
default. `Required(profile)` launches an explicitly addressed
`/usr/bin/sandbox-exec` — resigned as a twin, never reached through a shell —
with the rendered profile as bounded argv and the target's own twin as the
command. The backend then drives that trusted bootstrap internally, consuming
its startup events rather than exposing them as workspace syscalls, and returns
only at the target's exec stop, verified with `proc_pidpath` to be the intended
image. That exec stop is the handoff boundary: the policy is in force and the
target has not run an instruction. An installer that exits, forks, execs
something else, or does not reach the exec within its event budget fails the
launch, and the tree is killed rather than returned without a handle.
`UnsandboxedExperiment` is the only way to run unenforced, it is a named
selection rather than an omission, and `launch_experimental` accepts nothing
else.

The backend advertises `sandboxed-stopped-launch-v1` and
`experimental-syscall-rewrite-v1`. `tests/sandbox_launch.rs` qualifies the first
in both directions on this host: with interception rewriting the open, the
payload lands in the shadow and the host destination stays absent; with the
identical fixture and destination but no rewrite, the open fails `EPERM` and
neither file appears, which is the only way to show the policy is real rather
than that a launch merely failed.

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
These are direct tracer tests: they exercise interception without enforcement,
and are lower-level than the integrated `umbra run` matrix, which drives the
same seven cases through storage, journal, namespace and an installed sandbox.
All seven cases are enabled. Fixture cases:

| Case | State |
|---|---|
| `open-libc`         | **CAPTURED** — the M1 minimum acceptance bar |
| `open-svc`          | **CAPTURED** |
| `fork-write`        | **CAPTURED** |
| `posix-spawn-write` | **CAPTURED** |
| `exec-write`        | **CAPTURED** — see the closed M2 gap below |
| `grandchild-write`  | **CAPTURED** |
| `dup-inherit-write` | **CAPTURED** |

Software breakpoint ownership is per debugserver connection: successful
`Z0` installs populate that session's registry, and successful `z0`
removals remove entries. Fork children receive only inherited instruction
repairs before installing their own breakpoints. Temporary return and dyld
breakpoints use the same registry; the parent's fork gate uses `Z1`/`z1`.

Debugserver's own registrations are reference counted and **survive
`execve` on that connection**, so this crate's registry must be released
rather than discarded across an exec — see the closed M2 gap below. A
duplicate `Z0` at one address makes the matching `z0` decrement without
restoring the instruction, and debugserver answers `OK` either way.

`posix_spawn` needs no per-release layout knowledge. `struct
_posix_spawnattr` is private and moves between releases — `sizeof` 248 with
`psa_ports` at 192 on macOS 26, 256 and 200 on 27.0 — so this crate never
reconstructs it. `spawn_attributes` has the host's own libc build the block
(`posix_spawnattr_init` plus `posix_spawnattr_setflags`), takes its length
from `malloc_size` rather than a constant, and copies the bytes without
interpreting them.

The kernel copies `offsetof(_posix_spawnattr, psa_ports)` from the attribute
block and `sizeof(struct _posix_spawn_args_desc)` from the descriptor, both
its own constants, and tests `attr_size` only against zero
(`bsd/kern/kern_exec.c`). Neither length is reported to userspace, so both
buffers are padded to `SPAWN_BUFFER_BYTES` and zero-filled beyond the bytes
written: every trailing field is a pointer or a size/pointer pair, so
anything the kernel reads past the snapshot reads as absent rather than as
neighbouring scratch. The one retained assumption is `psa_flags` leading the
struct, checked before use so a future move refuses instead of misbehaving.

`native.rs::tests::live_kernel_accepts_the_spawn_buffers` is the drift
detector for that arrangement. The attribute side is sound by construction —
libc ships with the kernel, so a `malloc_size` snapshot always covers the
prefix the kernel reads — but the descriptor's length is not measurable from
userspace and is only assumed to fit `SPAWN_BUFFER_BYTES`. Rather than assert
remembered offsets, the test builds both buffers through the helpers the
tracer uses and invokes `posix_spawn` directly, so the live kernel judges
them. It needs no debugger, fixture or environment variable and runs anywhere
the crate compiles. Mutation-checked in both directions: shrinking the
descriptor below the kernel's struct fails with `EINVAL`, and clearing the
suspend flag fails on the marker the child then leaves behind. The marker
command is exec'd directly rather than through a shell, so a temporary
directory containing whitespace cannot split the path and turn a regression
into a silent pass; both mutations were re-confirmed under such a `TMPDIR`.


## Closed M2 gap: `exec-write` post-exec breakpoint loop

`exec-write` used to live-lock after `execve`: the first post-exec
`SyscallEntry` landed on an `__openat`-family stub, the tracer sent
`z0,<entry>,4` (`OK`) + `Z0,<entry+4>,4` (`OK`) + `c`, and the very next
stop reported the ENTRY address again rather than the gate. It repeated
thousands of times until the 25 s session watchdog fired. Both packets
were ACK'd, so the tracee behaved as if `z0` had not cleared the trap.

**Cause.** Debugserver's breakpoint registrations survive `execve` on the
same connection, and its `Z0` handler is reference counted. The exec reset
dropped this crate's own `breaks` registry without releasing those
registrations, then called `install()`, which re-registered the same
shared-cache addresses. Debugserver's count for each reached two, so the
single `z0` that retires an entry site decremented to one and left the
`BRK` in place — while still answering `OK`, because the request itself
succeeded.

The mismatch was invisible to the obvious probe: debugserver masks its own
breakpoints in `m` reads, so `m<entry>,4` returned the original
`svc #0x80` bytes both before and after the `z0`. A `mach_vm_read` of the
same address returned `BRK` in both cases, and `vCont;s` did not advance
the PC. That pairing is what identified the leak.

**Fix.** The `reason:exec` handler now sends `z0` for every address the
session still owns before clearing `breaks` and re-resolving the new
image. A refused release is not fatal — the new image need not map an
address the old one did, and a refusal means the registration is already
gone. See the `reason:exec` branch in `src/native.rs`.

`src/rsp.rs::tests::debugserver_reverse_connect_no_ack_handshake` spawns a
real debugserver and asserts the negotiated `qHostInfo.ostype` is
`macosx`, guarding the reverse-connect nonblocking-inheritance fix from
regressing. It resolves the binary through the same `discover_debugserver`
helper `connect` uses — `UMBRA_DEBUGSERVER` first, then the path below
`xcode-select -p` — and checks that the resolved path is a file before
connecting; missing tools print `SKIP` and return. Sharing that helper is
what stops the test from skipping on a host where only the environment
override names a usable binary.
That guard is implemented. A present debugger still requires socket and
debugging permissions.

## Verification

```
cargo check -p umbra-platform-macos
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos
```

Successful compilation does not qualify tracing on any target — the M1
minimum bar is what `open-libc` CAPTURED demonstrates.
