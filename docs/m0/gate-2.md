# M0 Gate 2 — descendant capture and syscall interception

**Status:** blocked by a one-command per-user macOS setup (`sudo DevToolsSecurity -enable`). Tracer code is written and offline-verified; runtime verification requires the developer-mode authorization that is off by default on stock macOS user accounts.

**Environment:** macOS 26.5.1 (25F80), arm64, SIP enabled, LLDB 2100.0.17.108.

## Root cause of the harness stall

Both the Gate-1 side-quest, Gate 3's LLDB Python composition test, and Track A's Gate 2 tracer all hit the same failure mode: LLDB (or `SBTarget.Launch()`, or `debugserver` invoked directly) reaches the point of calling `task_for_pid` on a resigned twin, and then hangs indefinitely. No error is printed. No breakpoint fires.

Track A traced this to macOS's per-user Developer Tools authorization:

```
$ /usr/sbin/DevToolsSecurity -status
Developer mode is currently disabled.

$ security authorizationdb read system.privilege.taskport
# ...
# authenticate-user: true
# class:            user
# group:            _developer
# comment: "Used by task_for_pid(...). ... only if the requesting and target
#           programs are run by the same user; it will never authorize access
#           to the program of another user."
```

When developer mode is disabled and the calling user is not in `_developer`, `task_for_pid` triggers an interactive authorization prompt from Security Server. In a graphical session that prompt shows up as a dialog. In a headless subshell (Claude Code, codex-in-tmux, non-tty bash `--batch` invocations) the prompt has no display target and the call blocks indefinitely waiting for input that will never arrive.

The one-time fix is `sudo /usr/sbin/DevToolsSecurity -enable`, which adds the user to `_developer`. From then on `task_for_pid` succeeds without the prompt.

## Impact on the umbra product contract

- **User setup requirement:** the umbra installer on macOS must document (or run, with the user's permission) `sudo DevToolsSecurity -enable` as a first-launch step. Every macOS Xcode/CLT user hits this same requirement; it is not new territory.
- **§14 additions:** a new row — "macOS developer authorization not enabled" — with mitigation "run once at install time; document as a supported prerequisite."

## What Track A built (all offline-verified, no runtime verification yet)

The tracer at `experiments/tracer/umbra_tracer.py` (20 KB) implements:

- **Hash-keyed resigned-twin cache** at `~/Library/Caches/umbra/twins/<sha256>/<basename>`; adopts the Gate-1 ent.plist + preserve-metadata flags; verifies signature after re-sign; detects and repairs corrupted cache entries; invalidates on source-hash change.
- **Direct syscall-site breakpoints** in `libsystem_kernel`. Confirmed offline (`experiments/tracer/results/abi-disassembly.log`):
  - `__open` = syscall 5, path in `x0`
  - `__open_nocancel` = 398, path in `x0`
  - `__openat` = 463, path in `x1`
  - `__openat_nocancel` = 464, path in `x1`
  - `__execve` = 59, `__posix_spawn` = 244, `__fork` = 2 (with stack prologue before the `svc`)
  - All four open stubs are `mov x16, #N; svc #0x80` before their error prologues.
- **Direct-syscall (raw `svc`) coverage**: aligned-instruction scan of the main Mach-O code sections for `svc #0x80`; dispatch by `x16` at each hit. Found the fixture's raw open site in Track D's binary. Does not (yet) scan every library, dyld's private stubs, newly executable mappings, JIT code, or Rosetta transitions.
- **Runtime open handler:** bounded (4 KiB) path read; non-UTF-8 preservation; scratch memory allocation for the rewritten path; register update (`x0`/`x1`) to point at the scratch; `OPEN[old]`/`OPEN[new]` logging. Rewrites *all* opens including reads — a real supervisor will need to distinguish read-through-to-base from copy-up-to-shadow, per handoff §4.2.
- **Exec interception:** resigns the new target and rewrites the exec path arg to the twin. Does not yet rewrite `argv[0]` back to the vendor path (self-discovery via `_NSGetExecutablePath` will still see the twin path).
- **posix_spawn interception:** handles null-attribute/null-file-actions case only. Uses `POSIX_SPAWN_START_SUSPENDED` injection; pinned to macOS 26.5.1's private descriptor layout (144-byte descriptor, 248-byte attribute object per local disassembly). Rejects non-null spawns rather than silently miscovering them.
- **Fork/vfork:** `settings set target.process.follow-fork-mode child` in LLDB; not yet capable of simultaneously supervising both parent and child from one LLDB session.

Also written:
- `demo.sh` — end-to-end demo (compiles inline C hello-writer, expects redirect to `/tmp/umbra-nfs-stub/tmp/umbra-should-not-exist`). Times out on `SBTarget.Launch()` under the current harness.
- `run_gate2.py` — full Gate 2 matrix runner against Track D fixtures with per-case logs.
- `bounded_run.py` — external Python watchdog that survives LLDB blocking on task_for_pid; owns and cleans up its descendant tree.
- `verify_static.py` — offline checks for signing/cache behaviour and breakpoint resolution.

## Verified in this run

| Static check | Result |
|---|---|
| Signing cache reuse without mtime change | PASS |
| Corrupted-cache repair | PASS |
| Source-hash change invalidation | PASS |
| Placement of 7 libsystem syscall breakpoints (`open`/`open_nocancel`/`openat`/`openat_nocancel`/`execve`/`posix_spawn`/`fork`) | PASS |
| Raw `svc #0x80` site scan on Track D fixture | PASS |
| ABI: path-arg register per stub via disassembly | PASS |

| Runtime check | Result |
|---|---|
| Any of the six traced fixture runs completing | **BLOCKED** — `task_for_pid` stall (root cause above) |

## What remains after enabling developer mode

Run these against Track D's fixtures on a working harness:

1. `demo.sh` — proves libc `open` interception + path rewrite end-to-end.
2. `run_gate2.py --timeout 30` — six-case matrix: libc open, raw svc open, fork+write, posix_spawn+write, exec+write, grandchild+write.

Independently outstanding regardless of harness:
- `posix_spawn` with non-null attributes / file actions.
- `fork` capture with parent AND child both under supervision (LLDB's fork-follow picks one).
- Exec `argv[0]` rewrite to preserve vendor path in `getprogname()`.
- Copy-up semantics: distinguish read-through-to-base from write-triggering-materialisation, per §4.2.
- Rosetta / newly executable mappings / JIT code — §6.6 later stages.

## Reproducer

- Tracer: `experiments/tracer/umbra_tracer.py`
- Six-case matrix: `experiments/tracer/run_gate2.py`
- Offline verification: `experiments/tracer/verify_static.py`
- Full report of this run's blocked state: `experiments/tracer/README.md`, `experiments/tracer/gate-2-report.md`
- Debugserver stall log ending at `about to task_for_pid(...)`: `experiments/tracer/results/debugserver-launch.log`
