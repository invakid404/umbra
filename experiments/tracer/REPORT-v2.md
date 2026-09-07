# Track A v2 results

All six required fixtures passed the final 25-second, fixed-path protocol on
2026-09-07. Every host output was absent, every shadow contained the expected
bytes, and the tracer and all supervised processes exited 0. Full logs and the
tested tracer SHA-256 are in `results/v2/`; rerun with
`/usr/bin/python3 verify_v2.py`.

Python 3.9 syntax checks passed. A copy containing only `umbra_tracer.py`, launched
with isolated Python from `/tmp`, also passed `grandchild-write`; see
`results/v2/standalone.log`. All 14 tracee PIDs from final verification and that
standalone check were confirmed absent afterward. The delivered tracer's hash
matches the recorded matrix, and the canonical tracer matches its initial bytes.

| Fixture | Result | Reason |
|---|---|---|
| open-libc | CAPTURED | libc open redirected; expected bytes, no host output, exit 0. |
| open-svc | CAPTURED | Raw arm64 svc redirected; expected bytes, no host output, exit 0. |
| fork-write | CAPTURED | Child attached before its open; expected bytes, no host output, both exits 0. |
| posix-spawn-write | CAPTURED | Suspended child attached and resumed; expected bytes, no host output, both exits 0. |
| exec-write | CAPTURED | Fork child remains intercepted after exec; expected bytes, no host output, both exits 0. |
| grandchild-write | CAPTURED | Two recursive fork attachments; expected bytes, no host output, all three exits 0. |

## Implementation

`umbra_tracer.py` is self-contained: the existing external watchdog was embedded,
so copying this one file suffices on the specified LLDB/Python installation.
The umbra repository was read only; all source changes are in this scratch directory.

- Child sessions suppress SIGHUP, SIGTRAP and SIGSTOP through `SBUnixSignals`.
  The event loop also suppresses recognized nonfatal signal stops before
  continuing, while retaining errors for fatal faults and Mach exceptions.
- Fork uses parent follow mode, a hardware breakpoint at svc+4, and disables
  the entry software breakpoint before the syscall. **The proposed hardware
  breakpoint alone was insufficient:** the child attach timed out. The final
  implementation temporarily patches svc+4 to `b .` before fork. The child
  inherits that loop and cannot reach the inherited syscall traps before attach.
  Once attached, the tracer restores the original instructions at the gate and
  inherited public software breakpoint sites, then installs the child's own
  breakpoints. Parent instructions are restored while it remains stopped.
- The same attachment path runs recursively for children and grandchildren.
- Debugger attachment temporarily reparents children, which initially caused
  otherwise successful fixtures to fail with ECHILD. Breakpoints on `__wait4`
  and `__wait4_nocancel` defer a blocking wait for supervised children until a
  child exits. The original syscall then obtains the actual kernel wait status.
  Return breakpoints track reaped children. WNOHANG returns zero while matching
  supervised children remain alive.
- At exec's early dyld stop, libsystem is not yet loaded. Its existing
  module-relative breakpoints are retained for LLDB to resolve; raw syscall
  sites are rescanned in the new main executable, and scratch state is cleared.

API references: [LLDB signal dispositions](https://lldb.llvm.org/python_api/lldb.SBUnixSignals.html),
[hardware breakpoint API](https://lldb.llvm.org/python_api/lldb.SBBreakpoint.html).
The wait deferral is based on observed ECHILD failures and XNU's
[`reap_child_locked` reparenting behavior](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exit.c).

## Remaining limits

This is a pinned macOS arm64 feasibility result, not proof of the strict
process-tree filesystem contract. Fork gating and deferred blocking waits reject
multithreaded processes; raw vfork is rejected. The single-thread check does not
establish support for arbitrary concurrent process creation. Process-group wait
selection and stopped/continued-child wait options are unsupported. WNOHANG and
repeated sibling waits have code paths but are not exercised by these six fixtures.

Spawn still requires null attributes/file actions and the existing macOS 26.5.1
private descriptor layout. General signal-handler semantics, background process
lifecycle, hostile code, inherited internal debugger breakpoints, JIT/newly loaded
raw syscall sites, and other architectures need qualification. The temporary
fork loop consumes CPU while attachment completes and depends on debugger text
patching. Fatal errors/timeouts terminate the experiment's observed process tree.

Existing filesystem limitations remain: all opens are rewritten, relative/dirfd
paths are unsupported, and this tracer is not a fail-closed filesystem sandbox.
The six-case protocol does not test `dup-inherit-write` or real NFS durability.
