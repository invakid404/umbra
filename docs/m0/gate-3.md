# M0 Gate 3 — fail-closed sandbox composable with debugger control

**Status:** preliminary PASS on the composition question; full breakpoint verification remains pending a better harness.

**Environment:** macOS 26.5.1 (25F80), arm64, SIP enabled, LLDB 2100.0.17.108, `sandbox-exec` from `/usr/bin`.

## Question

Can a fail-closed macOS sandbox profile (Seatbelt / SBPL) that (a) allows reads broadly, (b) permits writes only under the umbra NFS mount, and (c) denies everything else — be composed with LLDB task-port control over a resigned twin? Or does the sandbox block task-port acquisition, forcing us to choose between fail-closed enforcement and syscall interception?

## Findings

### 1. Local-write denial works

Track C's `verify-deny.sh` inside a shell running under `sandbox-exec -f umbra.sb sh`:

| Check | Result |
|---|---|
| Sandbox starts and reads `/bin/ls` | PASS |
| `touch /tmp/umbra-should-be-denied` | PASS — `Operation not permitted` |
| `touch /mnt/umbra-nfs/umbra-should-be-allowed` | SKIP — mount path absent on macOS |

The `/tmp` denial and `/bin/ls` read confirm the profile's `(deny default) + (allow file-read*) + (allow file-write* (subpath "/mnt/umbra-nfs"))` triple is enforced. The NFS write is skipped rather than fake-passed because Track B mounted at `~/umbra-scratch/nfs/mnt/umbra-nfs` (see [gate-1 sealed-root note](gate-1.md) implication and [track-b readme](../../experiments/nfs/README.md)) and the profile still references the notional `/mnt/umbra-nfs`.

### 2. Composition-with-LLDB: harness limitation, then a clean re-test

Track C's `verify-lldb-composition.sh` reported FAIL on all five nesting-order cases, **including the unsandboxed control**. Reading the logs (`experiments/seatbelt/results/debugserver-baseline.log`, `gdb-packets.log`), the failure mode is a stall at debugserver's `task_for_pid` — same subshell/tty artifact I hit while verifying Gate 1 via LLDB's Python `SBTarget.Launch()`. Even the unsandboxed baseline stalled, so those results cannot answer the composition question — a harness that can't complete `task_for_pid` on a resigned twin can't measure whether the sandbox does or does not block it.

I re-ran the same question with the debugserver exit-code heuristic that worked reliably for Gate 1:

| Case | debugserver behaviour past 1.5 s | Interpretation |
|---|---|---|
| Unsandboxed `debugserver … <resigned /bin/ls twin>` | still running, listening | task_for_pid completed, waiting for gdb-remote client |
| **`sandbox-exec -f umbra.sb debugserver … <twin>`** | **still running, listening** | **task_for_pid completed under the sandbox** |
| Unsandboxed `debugserver … <resigned codex>` (Gate 1 control) | still running, listening | matches the Gate 1 finding |

The sandbox does not block task-port acquisition. This is the load-bearing composition question. On this signal alone, Gate 3 looks viable.

### 3. What is *not* verified

- **Full breakpoint hit under sandbox.** I did not connect an LLDB client to the sandboxed debugserver and confirm that `b main` fires and the process resumes cleanly. Track C attempted the LLDB-scripted equivalent and hit the harness stall; a proper interactive terminal or Rust `mach2`-native harness will settle this.
- **The `/mnt/umbra-nfs`-write allow rule** was never exercised end-to-end because the mount lives under the user's home dir on macOS. The profile itself must eventually take the mount path as a variable, not a literal — see the [track-b findings](../../experiments/nfs/README.md).
- **Full agent workload composition** (codex spawning git, sh, python, curl inside the sandbox) is out of scope for Gate 3 and belongs in Gate 2 / M3.

## Design deltas

- **§6.5 (macOS fail-closed enforcement):** confirmed viable in principle with `sandbox-exec`. Deprecated interface risk noted; M3 must define a supported deployment story (§14 row 3).
- **Profile parameterisation:** the mount-root path must be a template variable at profile-load time, not a literal, because on macOS the physical mount lives under `~` (Track B). Umbra's supervisor will render the profile per run.
- **`mach-priv-task-port (target same-sandbox)` allow rule** is required for debugger composition — LLDB spawns debugserver and the twin as siblings, not parent/child. Documented in `experiments/seatbelt/umbra.sb`.
- **`file-write-data (literal "/dev/null")` allow rule** is required for LLDB's `target.disable-stdio` launch path. Documented and scoped to that single device sink.

## What to do next

1. **A real breakpoint hit under sandbox.** Run `lldb -o "b main" -o "run" -- sandbox-exec -f umbra.sb <resigned-ls-twin>` from an interactive Terminal.app on this Mac (i.e., not via bash-piped `--batch` in a headless subshell) and confirm the stop line prints. This is a five-minute test in the right terminal.
2. **A Rust `mach2`-native harness** long-term for repeatable Gate 3 tests that don't depend on `debugserver`'s subshell quirks.
3. **Parameterise the mount path** in `umbra.sb` when the supervisor's profile-render code lands.

## Reproducer

- Profile: `experiments/seatbelt/umbra.sb`
- Deny test: `experiments/seatbelt/verify-deny.sh`
- LLDB-Python composition (has known harness stall on this OS/session): `experiments/seatbelt/verify-lldb-composition.sh`
- The debugserver exit-code heuristic re-test is inline in the `docs/m0/gate-3.md` finding above; not scripted separately.
