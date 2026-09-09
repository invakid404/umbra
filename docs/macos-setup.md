# umbra: macOS setup requirements

Running list of setup steps, snags, and per-user configuration umbra needs on macOS Apple Silicon. Populated during M0 feasibility work; treat as the installer's checklist.

**Tested on:** macOS 26.5.1 (build 25F80), Apple Silicon (arm64), SIP enabled.

## Required per-user setup

### 1. Enable macOS Developer Tools authorization

```
sudo /usr/sbin/DevToolsSecurity -enable
```

**Why:** changes developer-tool authorization policy so users already in `admin` or `_developer` can use Apple's signed debugger tools without an additional password prompt; it does not add group membership (see `man DevToolsSecurity`). This enabled LLDB / `debugserver` attachment in the recorded macOS 26.5.1 M0 run. The Rust tracer also calls `task_for_pid` directly for memory reads, so verify its fixture from the intended execution environment; the M0 result does not establish permissions inside another sandbox.

**Common misdiagnosis:** in the recorded M0 run, `security authorize -e system.privilege.taskport` returned `NO (-60007)` even after debugger attachment worked. That authorization probe is not a tracer health check; use the Rust fixture's `CAPTURED` verdict instead.

**Discovered during:** M0 Gate 2 verification. Full analysis in [`docs/m0/gate-2.md`](m0/gate-2.md).

### 2. Command Line Tools

```
xcode-select --install   # if not already installed
```

**Why:** the Rust tracer uses `codesign` and Apple's `debugserver`, discovered below `xcode-select -p` at `Library/PrivateFrameworks/LLDB.framework/Resources/debugserver` (or overridden by `Options::debugserver`). Check that the binary exists; some Xcode layouts lack that path. LLDB's Python bindings are needed only for the historical Python experiment.

## Implemented behaviour and planned runtime integration

### Ad-hoc re-signing of traced binaries

The vendor `codex` 0.153.4 and `claude` 2.1.263 binaries tested in M0 had hardened runtime enabled and lacked the `com.apple.security.get-task-allow` entitlement, so Apple's AMFI denies debugger attach as-shipped. Umbra re-signs each binary it traces with an ad-hoc identity + `get-task-allow`, preserving the hardened-runtime flag. The signed twin is cached under:

```
~/Library/Caches/umbra/twins/<sha256>/<basename>
```

The vendor binary at `/opt/homebrew/bin/codex` (etc.) is not modified. Vendor updates (`brew upgrade`, in-app updates) continue to work against the vendor binary; umbra observes the new binary's SHA-256 and rebuilds the twin on next launch.

**Trust-model implication:** the local twin is adhoc-signed by umbra, not by the vendor. The vendor's Developer ID signature and notarization ticket apply to the vendor binary. Umbra never modifies vendor bytes on disk.

**Discovered during:** M0 Gate 1. Full analysis in `docs/m0/gate-1.md`.

### Per-run NFS mount path (not `/mnt`)

macOS's sealed system root prevented `/mnt` creation in M0. The NFS backend consumes an externally managed mount; Umbra does not create or mount it. The proposed per-run location is:

```
~/.local/state/umbra/runs/<run-id>/mount/
```

The tested mount is `~/umbra-scratch/nfs/mnt/umbra-nfs`. Configure its absolute path in the NFS provider options; the mount root is runtime configuration, not a compile-time constant. Persistent run metadata records **logical** paths only, never physical mount points, so a run can be resumed on another machine with the mount at a different physical location.

**Discovered during:** M0 Track B (NFS infra). Full context in `experiments/nfs/README.md`.

### Sandbox profile — parameterised mount path

`experiments/seatbelt/umbra.sb` is now a template with a single `{{UMBRA_RUN_ROOT}}` token; the pinned `/mnt/umbra-nfs` literal is gone. The supervisor embeds that template and renders it per run against the opened run's own root (`umbra_supervisor::sandbox`), resolving the root first because Seatbelt matches resolved paths. The macOS backend launches a traced `sandbox-exec` bootstrap, installs the required
profile, and returns only at the verified target exec stop. It advertises
`sandboxed-stopped-launch-v1`; inability to prove enforcement fails the launch.

The former LLDB-specific `/dev/null` write and `mach-priv-task-port` same-sandbox
grants were removed after re-measurement with `sandbox-exec` and debugserver on
2026-09-09. The shipped profile and required native qualification use no such grants.

**Discovered during:** M0 Gate 3 / Track C. Full context in `docs/m0/gate-3.md`.

## Optional (M3)

### Endpoint Security (audit only)

The Rust tracer integrates fail-closed enforcement through a traced `sandbox-exec` bootstrap. Endpoint Security can provide an additional audit signal but requires a restricted entitlement from Apple (`com.apple.developer.endpoint-security.client`). Not needed for M0; plan the entitlement request during M3 if the shipping story requires it.

### Rosetta / x86-64 process trees

Not supported in M0. See handoff §6.6 for the intended architecture-support ordering.

## Known-good state (verified 2026-09-07)

- SIP: **enabled** (do not disable — umbra is designed to work with SIP on)
- LLDB: 2100.0.17.108 (CLT)
- OrbStack: 1.13+ (for the local NFS test infrastructure; not required at runtime by umbra itself)
- Docker CLI: 29.4.0 (via OrbStack; only needed for NFS test infra bring-up)
