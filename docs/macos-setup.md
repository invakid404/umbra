# umbra: macOS setup requirements

Running list of setup steps, snags, and per-user configuration umbra needs on macOS Apple Silicon. Populated during M0 feasibility work; treat as the installer's checklist.

**Tested on:** macOS 26.5.1 (build 25F80), Apple Silicon (arm64), SIP enabled.

## Required per-user setup

### 1. Enable macOS Developer Tools authorization

```
sudo /usr/sbin/DevToolsSecurity -enable
```

**Why:** adds the current user to `_developer`. On macOS 26.5.1 this is **sufficient** — kernel `taskgated` then grants `task_for_pid` to LLDB / `debugserver` (which carries Apple's `com.apple.private.cs.debugger` entitlement) non-interactively for callers in `_developer`. No prompt, no auth db modification, no signed umbra binary needed for the tracer to attach.

**Common misdiagnosis:** the user-space `security authorize -e system.privilege.taskport` CLI still returns `NO (-60007)` after `DevToolsSecurity -enable`. That's a *separate* code path (`AuthorizationServices`) that debuggers do not use. Do not use this CLI as a health check for umbra's tracer — it will always fail and it doesn't matter.

**Discovered during:** M0 Gate 2 verification. Full analysis in [`docs/m0/gate-2.md`](m0/gate-2.md).

### 2. Command Line Tools

```
xcode-select --install   # if not already installed
```

**Why:** umbra depends on `codesign`, `codesign --entitlements`, `debugserver` (from `/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Resources/debugserver`), and LLDB's Python bindings — all shipped with CLT.

## Behaviour umbra encapsulates for you (not manual steps, documented for transparency)

### Ad-hoc re-signing of traced binaries

Vendor-shipped `codex` and `claude` binaries have hardened runtime enabled and lack the `com.apple.security.get-task-allow` entitlement, so Apple's AMFI denies debugger attach as-shipped. Umbra re-signs each binary it traces with an ad-hoc identity + `get-task-allow`, preserving the hardened-runtime flag. The signed twin is cached under:

```
~/Library/Caches/umbra/twins/<sha256>/<basename>
```

The vendor binary at `/opt/homebrew/bin/codex` (etc.) is not modified. Vendor updates (`brew upgrade`, in-app updates) continue to work against the vendor binary; umbra observes the new binary's SHA-256 and rebuilds the twin on next launch.

**Trust-model implication:** the local twin is adhoc-signed by umbra, not by the vendor. The vendor's Developer ID signature and notarization ticket apply to the vendor binary. Umbra never modifies vendor bytes on disk.

**Discovered during:** M0 Gate 1. Full analysis in `docs/m0/gate-1.md`.

### Per-run NFS mount path (not `/mnt`)

macOS's sealed system root prevents `/mnt` creation without a system config change + reboot. Umbra places its per-run NFS mount under the user's data directory instead:

```
~/.local/state/umbra/runs/<run-id>/mount/
```

The mount root is a per-run runtime configuration, not a compile-time constant. Persistent run metadata records **logical** paths only, never physical mount points, so a run can be resumed on another machine with the mount at a different physical location.

**Discovered during:** M0 Track B (NFS infra). Full context in `experiments/nfs/README.md`.

### Sandbox profile — parameterised mount path

The Seatbelt profile umbra installs (`experiments/seatbelt/umbra.sb` for reference) requires two carve-outs beyond the obvious deny-default + read + write-under-mount rules:

- `(allow mach-priv-task-port (target same-sandbox))` — LLDB launches `debugserver` and the tracee as siblings, so a children-only rule is insufficient.
- `(allow file-write-data (literal "/dev/null"))` — required by LLDB's `target.disable-stdio` launch path.

**Discovered during:** M0 Gate 3 / Track C. Full context in `docs/m0/gate-3.md`.

## Optional (M3)

### Endpoint Security (audit only)

Umbra's fail-closed enforcement is `sandbox-exec`-based. Endpoint Security can provide an additional audit signal but requires a restricted entitlement from Apple (`com.apple.developer.endpoint-security.client`). Not needed for M0; plan the entitlement request during M3 if the shipping story requires it.

### Rosetta / x86-64 process trees

Not supported in M0. See handoff §6.6 for the intended architecture-support ordering.

## Known-good state (verified 2026-09-07)

- SIP: **enabled** (do not disable — umbra is designed to work with SIP on)
- LLDB: 2100.0.17.108 (CLT)
- OrbStack: 1.13+ (for the local NFS test infrastructure; not required at runtime by umbra itself)
- Docker CLI: 29.4.0 (via OrbStack; only needed for NFS test infra bring-up)
