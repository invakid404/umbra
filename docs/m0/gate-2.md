# M0 Gate 2 — descendant capture and syscall interception

**Status:** ✅ PASS on all six fixture cases in the disposable Python prototype. Syscall interception (libc + raw arm64 svc) and descendant capture (fork / posix_spawn / exec / grandchild) both proven end-to-end. This closes M0 exit criterion 2, which was the last outstanding item — all four §12 criteria are now met.

**Environment:** macOS 26.5.1 (25F80), arm64, SIP enabled, LLDB 2100.0.17.108.

## Descendant capture result — v2 tracer (2026-09-07)

| Fixture | Result | How |
|---|---|---|
| `open-libc` | ✅ CAPTURED | libc `open` intercepted at `__open` svc stub, path rewritten to shadow root |
| `open-svc` | ✅ CAPTURED | raw arm64 `svc #0x80` open in main executable; dispatched by `x16=5`; path rewritten |
| `fork-write` | ✅ CAPTURED | see "race elimination" below; child attached before its first open |
| `posix-spawn-write` | ✅ CAPTURED | suspended child (`POSIX_SPAWN_START_SUSPENDED`) attached, SIGHUP suppressed, resumed under supervision |
| `exec-write` | ✅ CAPTURED | fork race handled first; exec then re-installs breakpoints in the new image |
| `grandchild-write` | ✅ CAPTURED | recursive application of the fork attach path |

All six cases verified independently against a fresh set of paths: host output ABSENT, shadow output PRESENT, content matches the expected fixture bytes (`libc`, `fork`, `grandchild`), tracer and every supervised process exited 0.

Structured evidence at `experiments/tracer/results/v2/matrix.json`; per-case logs in the same directory.

### How the fork race was eliminated

The original v1 attempt failed at fork because the parent's software breakpoints (svc trap replacement bytes) are COW-inherited into the child's address space. The child hits an inherited breakpoint before any debugger is attached to it; with no handler, it dies via SIGTRAP.

Codex's v2 fix: **patch `svc+4` in the parent (the fork return site) to `b .` — an infinite branch-to-self — before executing the fork syscall.** The child inherits that spin, cannot reach any inherited syscall trap, and holds its own PC at the `b .` until the tracer's attach completes. After attach, the tracer:

1. Restores the original instruction at `svc+4` in both parent and child.
2. Restores the original bytes at all other inherited software breakpoint sites in the child (LLDB then reinstalls the child's own breakpoints in its own address space, tracked separately).
3. Continues both processes.

Hardware breakpoints alone (my original proposal) were not sufficient — codex tested and reported this. The `b .` gate is the trick.

### Related fixes codex delivered along the way

- **SIGHUP suppression** on newly attached children via `SBUnixSignals` (fixed posix_spawn).
- **`__wait4` / `__wait4_nocancel` breakpoints** that defer the parent's blocking wait until the supervised child actually exits. Debugger attachment temporarily reparents the child, which without this fix produces ECHILD in the parent's `waitpid()`. Return breakpoints track reaped children.
- **Exec-time breakpoint preservation**: at the early dyld stop after exec, libsystem_kernel is not yet loaded, but module-relative breakpoints are retained and re-resolved as dyld maps the image. Raw syscall sites are re-scanned in the new main executable.
- **Embedded watchdog**: the external `bounded_run.py` was folded into the tracer, so `umbra_tracer.py` is now self-contained (33 KB, 666 lines).

## M0 exit criteria (§12) — status

| Criterion | Status |
|---|---|
| 1. Task control of vendor binaries without unacceptable system-security configuration | ✅ (Gate 1 resign path + `sudo DevToolsSecurity -enable`) |
| 2. All supported child creation paths captured before first mutation | ✅ (this gate — v2 tracer, all six fixtures) |
| 3. Fail-closed policy grants writes only under the NFS shadow | ✅ (Gate 3) |
| 4. Direct arm64 syscall sites trapped with stable stepping | ✅ (open-svc case) |

Per handoff §12: "Go only if all of these hold." **All four criteria hold.** The plan graduates from disposable Python prototype to the Rust supervisor (M1).

## What still needs qualification (the honest list)

The prototype proves the mechanisms are achievable; several things remain for M1+ engineering:

- **Multithreaded fork.** The v2 tracer's fork gating rejects multithreaded processes. Real agents may spawn worker threads before forking; the `b .` gate needs to interact correctly with sibling threads.
- **Raw `vfork`.** Rejected in v2. Modern code prefers `posix_spawn`, but coverage should be principled, not accidental.
- **`posix_spawn` with non-null attributes / file actions.** v2 handles the null case only; the pinned macOS 26.5.1 private descriptor layout must be generalised.
- **`WNOHANG` / stopped/continued wait options / process-group wait selection.** Code paths exist for some but aren't fixture-exercised.
- **Exec `argv[0]` rewrite to preserve vendor path** for `getprogname()`.
- **Copy-up semantics** (§4.2): distinguish read-through-to-base from write-triggering-materialisation. The current tracer rewrites *all* opens including reads, which will break arbitrary applications that read from paths not populated in the shadow.
- **Rosetta / newly executable mappings / JIT code** (§6.6).
- **Relative paths, dirfds, symlink containment, hard links** (§4.2-4.4).
- **Real NFS durability** (§9) — the fixture protocol uses `/tmp/umbra-nfs-stub/`, not the actual mount.
- **Fail-closed enforcement** (§3, §6.5): the tracer is not a sandbox. Track C's `umbra.sb` profile is separate; the two must be composed for the strict contract.
- **`dup-inherit-write`** was not tested in the v2 six-case protocol (though implemented in the fixtures).

These are M1+ work items, not M0 gates.

## Setup prerequisite (documented for reproducers)

The tracer requires `sudo /usr/sbin/DevToolsSecurity -enable` to have been run once (adds the user to `_developer`). That is sufficient on macOS 26.5.1 — `taskgated` then grants `task_for_pid` to LLDB / `debugserver` (which carries Apple's private `com.apple.private.cs.debugger` entitlement) at the kernel level, non-interactively, for callers in `_developer`. No auth db policy modification, no signed umbra binary required.

Diagnostic note: `security authorize -e system.privilege.taskport` returns `NO (-60007)` even when DevToolsSecurity is enabled — that's a separate code path (`AuthorizationServices`) that debuggers do not use. Do not rely on it as a health check.

## Reproducer

- Tracer (v2, self-contained): `experiments/tracer/umbra_tracer.py`
- Codex's own analysis of the v2 fix: `experiments/tracer/REPORT-v2.md`
- Structured verification: `experiments/tracer/results/v2/matrix.json` and per-case logs
- Verifier: `/usr/bin/python3 experiments/tracer/verify_v2.py` (rebuilds the matrix)
- Single-case invocation: `/usr/bin/python3 experiments/tracer/umbra_tracer.py [--redirect-root ROOT] [--timeout SECONDS] <target> [args...]`
- End-to-end demo (libc-open case only): `experiments/tracer/demo.sh`
- v1 (blocked) evidence, kept for history: `experiments/tracer/results/` (top level)
