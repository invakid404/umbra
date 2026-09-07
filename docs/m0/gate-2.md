# M0 Gate 2 — descendant capture and syscall interception

**Status:** mixed. Syscall interception (libc `open`, `openat` stubs, and raw arm64 `svc`) works end-to-end with path rewrite into a shadow root. Descendant capture (fork / `posix_spawn` / exec / grandchild) is not achieved by the current disposable prototype — exactly the "principal macOS risk" flagged in handoff §6.4.

**Environment:** macOS 26.5.1 (25F80), arm64, SIP enabled, LLDB 2100.0.17.108.

## Prerequisite: enable macOS developer authorization

The tracer requires `sudo /usr/sbin/DevToolsSecurity -enable` to have been run once on the machine (adds the user to `_developer`). That was previously conflated with a separate auth-db issue — the correct picture is:

- `sudo DevToolsSecurity -enable` **is sufficient** on macOS 26.5.1. Once the user is in `_developer`, `taskgated` grants `task_for_pid` to LLDB / `debugserver` at the kernel level — no interactive prompt fires, no auth db policy change needed.
- The `security authorize -e system.privilege.taskport` CLI returns `NO (-60007)` even after DevToolsSecurity is enabled — that's a *different* code path (user-space `AuthorizationServices`) that debuggers do not go through. Do not use this CLI as a health check for the tracer.
- `debugserver`'s built-in `com.apple.private.cs.debugger` entitlement (Apple's private, not `com.apple.security.cs.debugger` from a Developer ID) is what earns it the taskgated bypass. Third-party debuggers embedding their own `debugserver`-equivalent will want the public `com.apple.security.cs.debugger` on the eventual signed umbra binary; not required for M0 verification because we invoke Apple's `debugserver`.

An earlier draft of this doc claimed DevToolsSecurity was insufficient and recommended modifying the auth db. That was wrong; the recommendation is retracted. See `docs/macos-setup.md` for the corrected setup requirements.

## Test matrix and results (2026-09-07, after DevToolsSecurity was enabled)

### Syscall interception + path rewrite: ✅

| Fixture | Case | Result |
|---|---|---|
| `demo.sh` inline hello-writer | libc `open` → `/tmp/umbra-should-not-exist` | ✅ CAPTURED; file appears at `/tmp/umbra-nfs-stub/tmp/umbra-should-not-exist`, absent from host |
| Track D `open-libc` | libc `open` (verifies path rewrite for arbitrary target) | ✅ CAPTURED |
| Track D `open-svc` | raw arm64 `svc #0x80` open in main executable | ✅ CAPTURED; main-executable svc scan hit the site, syscall dispatched by `x16=5`, path rewritten to shadow |

Sample intercept trace:

```
INTERCEPT: pid=… tid=… __open
OPEN[old]: /tmp/umbra-gate2-fork-write
OPEN[new]: /tmp/umbra-nfs-stub/tmp/umbra-gate2-fork-write
```

### Descendant capture: ❌ (with the current disposable Python prototype)

| Fixture | Result | Failure mode |
|---|---|---|
| Track D `fork-write` | ❌ | Parent captured; `FORK[entry]` fires; child gets SIGTRAP (breakpoint set on parent inherited COW into the child's address space; LLDB's `follow-fork-mode child` does not re-arm cleanly on this LLDB/debugserver combo). Child dies with `signal=5`, `errno=10 (No child processes)` in the fixture. |
| Track D `posix-spawn-write` | ❌ | `SPAWN[attach]` reports suspended child at pid X + `SPAWN[attached]`, then child hit with SIGHUP after resume. Attach transient. |
| Track D `exec-write` | ❌ | Fixture forks then execs; hits the fork race first. |
| Track D `grandchild-write` | ❌ | Fork race, deeper. |
| Track D `dup-inherit-write` | (not tested) | Not run in this pass. |

The tracer implementation is honest about this: Track A's own README states "no manual pre-mutation fork attach mechanism was established" and "follow-fork-mode child … whether this debugserver actually captures the child is unverified." That's now empirically confirmed for the current prototype.

### Correlation with M0 exit criteria (§12)

| Criterion | Status |
|---|---|
| 1. Task control of vendor binaries without unacceptable system-security configuration | ✅ (Gate 1 resign path + DevToolsSecurity -enable) |
| 2. All supported child creation paths captured before first mutation | ❌ (this gate — Python prototype does not implement race-free descendant handoff) |
| 3. Fail-closed policy grants writes only under the NFS shadow | ✅ (Gate 3) |
| 4. Direct arm64 syscall sites trapped with stable stepping | ✅ (open-svc case) |

Three of four criteria met. Criterion 2 is the substantive open question and, per handoff §12, is the go / no-go for shipping the strict native-macOS contract.

## What Track A built (all intact, all works; runtime numbers now measured above)

The tracer at `experiments/tracer/umbra_tracer.py` (20 KB) implements:

- **Hash-keyed resigned-twin cache** at `~/Library/Caches/umbra/twins/<sha256>/<basename>`; adopts the Gate-1 ent.plist + preserve-metadata flags; verifies signature after re-sign; detects and repairs corrupted cache entries; invalidates on source-hash change.
- **Direct syscall-site breakpoints** in `libsystem_kernel` at the arm64 `svc #0x80` instructions. Verified offline (`experiments/tracer/results/abi-disassembly.log`) and hit at runtime:
  - `__open` = syscall 5, path in `x0`
  - `__open_nocancel` = 398, path in `x0`
  - `__openat` = 463, path in `x1`
  - `__openat_nocancel` = 464, path in `x1`
  - `__execve` = 59, `__posix_spawn` = 244, `__fork` = 2 (with stack prologue before the `svc`)
- **Direct-syscall (raw `svc`) coverage**: aligned-instruction scan of the main Mach-O code sections for `svc #0x80`; dispatch by `x16` at each hit. Found and hit Track D's raw open site.
- **Runtime open handler:** bounded (4 KiB) path read; non-UTF-8 preservation; scratch memory allocation for the rewritten path; register update (`x0`/`x1`) to point at scratch; `OPEN[old]` / `OPEN[new]` logging. Rewrites *all* opens including reads — a real supervisor will need to distinguish read-through-to-base from copy-up-to-shadow, per handoff §4.2.
- **Exec interception:** resigns the new target and rewrites the exec path arg to the twin. Does not yet rewrite `argv[0]` back to the vendor path.
- **`posix_spawn` interception:** handles null-attribute/null-file-actions case only. Uses `POSIX_SPAWN_START_SUSPENDED` injection; pinned to macOS 26.5.1's private descriptor layout. Attach succeeds; child cleanup post-resume is where it breaks.
- **Fork/vfork:** `settings set target.process.follow-fork-mode child`. Not sufficient — see failure matrix above.

## What the Rust supervisor needs to do differently for criterion 2

Descendant capture is the primary M1 engineering problem, not a "polish the Python prototype" problem. Direction:

- **`posix_spawn` capture:** use `POSIX_SPAWN_START_SUSPENDED` (already the approach) but with a separate `debugserver` per child rather than trying to hand off LLDB's own session. Own the child's task port from `task_for_pid` at attach time; install the syscall-stub breakpoints in the child's address space before `task_resume`. Handle non-null spawn attributes and file actions (rewrite them in-place if they refer to logical paths).
- **`fork` capture:** the harder case. Options: (a) prohibit `fork` in supervised code (few modern agents actually fork without immediately exec'ing — enforce fork-then-exec-only invariant, then handle via the spawn/exec path), (b) implement a proper `PT_ATTACHEXC` on the child from a helper thread that races the child's user-code entry, (c) inject a stub via `mprotect` + code patching at `_dyld_start` in the child (heavy). Option (a) is my recommendation: pragmatically eliminate the race by policy.
- **Simultaneous parent + child supervision:** each child gets its own `debugserver` instance rather than trying to multiplex LLDB sessions. The Rust supervisor is the multi-session orchestrator on top.

## Reproducer

- Tracer: `experiments/tracer/umbra_tracer.py`
- Single-case invocation: `/usr/bin/python3 experiments/tracer/umbra_tracer.py [--redirect-root ROOT] [--timeout SECONDS] <target> [args...]`
- Full six-case matrix: `experiments/tracer/run_gate2.py --timeout 30`
- End-to-end demo: `experiments/tracer/demo.sh`
- Offline verification: `experiments/tracer/verify_static.py`
- Per-case logs from the earlier BLOCKED run: `experiments/tracer/results/`
