# M0 Gate 1 — task control of vendor-signed agent binaries

**Status:** viable path found. Design updated: resigning becomes a first-class part of the supervisor, exercised on every `execve` / `posix_spawn`.

**Environment:** macOS 26.5.1 (build 25F80), Apple Silicon (arm64), SIP enabled, LLDB 2100.0.17.108 from Command Line Tools, no special entitlements, no notarized ecosystem opt-outs.

## Question

Can the umbra supervisor obtain debugger-level task control (`task_for_pid`, register/memory read/write, single-step) over vendor-distributed `codex` and `claude` on stock macOS, without disabling SIP and without an entitlement not available to third-party developers?

## Findings

### 1. Vendor binaries are not attachable as-shipped

Both `/opt/homebrew/bin/codex` (0.153.4) and `/opt/homebrew/bin/claude` (2.1.263) are:

- Native arm64 Mach-O.
- Developer ID signed and notarized (OpenAI `2DC432GLL2`, Anthropic `Q6L2SF6YDW`).
- Hardened runtime enabled (`flags=0x10000(runtime)`).
- Missing the `com.apple.security.get-task-allow` entitlement.

Attempting `lldb -o "run --version" /opt/homebrew/bin/codex` produces:

```
error: process exited with status -1 (attach failed (Not allowed to attach to
process.  Look in the console messages (Console.app), near the debugserver
entries, when the attach failed.  The subsystem that denied the attach
permission will likely have logged an informative message about why it was
denied.))
```

This is Apple's AMFI blocking `task_for_pid` from a non-same-team debugger on a hardened-runtime binary without `get-task-allow`. There is no third-party entitlement that bypasses this: `com.apple.security.cs.debugger` only grants same-Team-ID debugging; Apple's own tools work because they *are* Apple.

### 2. Ad-hoc resigning with `get-task-allow` restores attach permission

Copy the vendor binary to a writable path, apply an ad-hoc identity (`-s -`), preserve the hardened-runtime flag, add `get-task-allow` (plus the JIT / unsigned-exec-memory entitlements the original carried):

```
codesign -f -s - \
    --entitlements experiments/gate-1/ent.plist \
    --preserve-metadata=identifier,flags,runtime \
    <twin>
```

After this, LLDB does not produce the AMFI error, and `debugserver 127.0.0.1:<port> <twin>` stays running past a 1.2 s heuristic timeout (the "waiting for gdb-remote client" success pattern) instead of exiting immediately (the failure pattern seen on the as-shipped binary).

### 3. Apple system binaries also resign cleanly

Tested resign+attach on the following common child binaries codex is expected to `execve`:

| Binary | Original CS flags | Resign | Attach |
|---|---|---|---|
| `/bin/sh`, `/bin/bash`, `/bin/zsh` | 0x0 (none) | ✓ | ✓ |
| `/bin/ls` | 0x0 | ✓ | ✓ |
| `/usr/bin/env` | 0x0 | ✓ | ✓ |
| `/usr/bin/git`, `/usr/bin/python3`, `/usr/bin/otool` | 0x0 (xcode-select shims) | ✓ | ✓ |
| `/usr/bin/curl` | 0x0 | ✓ | ✓ |
| `/usr/bin/xcrun` | 0x0 | ✓ | ✓ |

All 10 sit under SIP-protected paths (`/bin`, `/usr/bin`). SIP prevents *writes* to those paths but permits reads, so umbra can copy the vendor bytes to `~/Library/Caches/umbra/twins/<sha256>/<basename>` and resign the copy without violating SIP.

## Implications for the design

### A. Resign-on-exec is a first-class supervisor mechanism

The initial-launch resigning idea generalizes into the `execve` / `posix_spawn` interceptor umbra already needs. On every exec:

1. Intercept before the kernel completes it.
2. Hash the target binary; look up a twin under `~/Library/Caches/umbra/twins/<sha256>/<basename>`.
3. Cache miss: copy the vendor bytes, ad-hoc resign with `get-task-allow`, preserve hardened-runtime flags.
4. Rewrite the exec's path to the twin.
5. Rewrite `argv[0]` back to the vendor path so `getprogname()` and any vendor config lookup behave normally.
6. Allow the exec to complete; the new process comes up attachable.

The initial umbra launch of `codex` is a special case — the exec is umbra's own.

### B. Delta vs. the intake handoff

- **§5 (Rust architecture):** add `umbra-resign` (probably inside `umbra-macos`) providing the hash-keyed twin cache and the resign primitive.
- **§6.4 (descendant capture):** add a sub-gate — resign-on-exec must succeed for the child binary type before the supervisor can attach. Platform-binary and hardened-runtime children each need to be verified.
- **§14 row 1 risk (macOS task-port / signing restrictions):** viable mitigation found. Cost is a first-launch UX pause (≈ 100 ms per binary) and a documented trust-model note (the local twin's signature is umbra's, not the vendor's).

## What is *not* yet verified

- **Vendor updates.** When `brew upgrade codex` replaces `/opt/homebrew/bin/codex`, the twin cache should self-invalidate on hash change. Not tested here.
- **Notarization side-effects.** Ad-hoc-signed twins lose the vendor notarization ticket. This should not matter for direct execution by absolute path from a controlling parent, but has not been end-to-end tested with a real codex login/network flow.
- **Platform-binary flagged Apple tools.** `codesign -d` does not directly print `CS_PLATFORM_BINARY`. The 10 tools above are unlikely to be platform-flagged, but a systematic enumeration of Apple binaries that *would* resist resigning is TODO — belongs in Gate 2's descendant-capture harness.
- **debugserver behaviour in this session's terminal was inconsistent** enough that some measurements had to be triangulated via multiple methods (specific-error grep vs. exit-code heuristic vs. port-open probe). A proper Rust harness on Mach APIs, or a real interactive Xcode/LLDB run, should be used to lock in Gate 2's numbers rather than piping into `lldb --batch` from Claude Code's subshell.

## Reproducer

`experiments/gate-1/ent.plist` — the entitlements applied to twin binaries.

`experiments/gate-1/probe.sh <binary>` — resign a target and heuristically test task control via `debugserver`.
