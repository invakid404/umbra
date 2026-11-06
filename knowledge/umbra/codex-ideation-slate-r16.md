# Rank 16 — Hard-link/no-clobber publication fallback

Prepared 2026-11-06 for dg-15kb9az2. Ideation only; no builds or runtime readings.

## Intent and evidence boundary

Reproduce family 16's E13 publication decision with preseeded temporary bytes: hard_link, then the pinned tempfile persist_noclobber fallback for errors other than AlreadyExists. An existing destination must never be overwritten. A fallback is usable only if it publishes exact bytes while preserving a competing destination. Sync, timestamps, decompression and cleanup are independent prerequisites, not evidence of publication.

## Measured source, not runtime results

Umbra pin: 9a4d29bbdbe41940586d2570e9788651fffece78, verified HEAD in /Users/inva/Coding/umbra-worktrees/slate-r16. All crates/ references below use this pin. Codex pin: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61, verified HEAD in /Users/inva/Coding/umbra-worktrees/codex-src-44fe510. Codex references are relative to codex-rs at that pin.

Codex rollout/src/compression.rs:153 catches hard_link success and AlreadyExists, invoking persist_temp_file_noclobber for other errors. The helper at :183 wraps TempPath::try_from_path and treats persist_noclobber AlreadyExists as success. Flush and sync precede publication (:149-151); timestamps and further sync follow it (:159). This cut deliberately removes those confounders. network-proxy/src/certs.rs:826 has a DIFFERENT fallback (existence check then rename); it is outside E13 and this cut.

Codex Cargo.lock:15148 pins tempfile 3.27.0 with checksum 32497e9a4c7b38532efcdebeef879707aa9f794296a4f0244f6f69e9bc8574bd. Supplementary local dependency source: /Users/inva/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tempfile-3.27.0/src/file/imp/unix.rs:94-134. It attempts renameat_with(NOREPLACE) on macOS; only NOSYS or INVAL reaches its hard-link fallback, while other errors return immediately (:117-129). This is dependency source inspection, not a syscall trace or checksum verification of the cached tree. Implementation must verify the dependency archive against the lock checksum and preserve its resolved dependency lock before readings.

Umbra traps linkat and __renameatx_np (crates/umbra-platform-macos/src/abi.rs:316-319). Nonzero renameatx_np flags yield UnsupportedCapability with diagnostic `renameatx_np flags {:#x}` (:840-845). Link has no admitted match in overlay's path resolver, which returns UnsupportedCapability with `operation requires descriptor or metadata support beyond MVP` (crates/umbra-overlay/src/engine.rs:2543-2564). This is a resolver error, not Deny(ENOTSUP). The later generic routed Deny(Errno(45)) at :3046-3049 cannot be used as the Link oracle. Userspace capability explicitly refuses Link (crates/umbra-storage-nfs-userspace/src/capability.rs:256-261).

Open issues were read with gh issue list --state open --limit 300 --json number,title,body; raw snapshot: issues.json beside this file. Refs #79 owns missing Link wiring and rollback; Refs #165 owns descriptor sync refusals; Refs #217 owns launch-helper consolidation. These issue descriptions are historical evidence, not proof of current runtime behavior. No new defect is claimed from inspection.

## Shared expected pins and measurement contract

Every FIRST CUT case has one fresh native host control on disposable data, followed by one enforced userspace-registry run against live Ganesha with UMBRA_INTEGRATION_REQUIRED. No mounted NFS. Record exact fixture/dependency hashes, compiler, OS/architecture, provider/export configuration, routing roots, argv/environment and journal. Missing prerequisites or skip means unqualified, never pass. Freeze and commit the instrument before readings. Check disk and runner-idle rules, use the serial wrapper and workspace target as S3 requires.

Use a 30-second external deadline per invocation and a separate 10-second cleanup grace. Reuse slate_launch (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:8331); its private signature favors one contiguous slate-r16 block at the end of that file, without moving/copying helpers. Capture supervisor and tracee statuses separately, phase records before each operation, immediate errno/IoError and exact diagnostics, completion versus abort journal, owned PID census and reap. A fatal supervisor refusal may prevent any tracee return. Assert source/destination bytes and namespace independently through the NFS client; snapshot the host routing root before/after. Never use routed stat as the sole oracle. Preseed via the qualified fixture writer, then close before the focal primitive; retain readiness proof and independently verify seed bytes. Unexpected signal, timeout, host mutation or cleanup failure fails the reading.

Native controls require successful publication for absent destinations and unchanged incumbent bytes for existing destinations. Preserve artifacts before external disposable-tree cleanup; record cleanup success and zero owned survivors. The expected refusal boundary preserves the preseeded source and incumbent (or absent destination); downstream cleanup in the Codex function is unreachable. No retries to green. Report actual disposition if it differs from this pre-registration.

## Cases

### C16-01 — E13 hard-link entry, absent destination — FIRST CUT

**Hypothesis:** E13 stops at its first Link resolver error before executing its fallback.

**Pre-condition:** Preseed complete TEMP bytes in the shadow, destination absent; use the pinned E13 decision in a small synchronous Rust fixture and record entry/return phases.

**Action:** std::fs::hard_link followed by E13's real error dispatch; no injected error.

**Expected:** Exact known refusal: UnsupportedCapability `operation requires descriptor or metadata support beyond MVP`, supervisor abort, no successful tracee Link return, TEMP unchanged and destination absent. Native publishes exact TEMP bytes. Refs #79; fallback unreachable in routed run.

**Falsifying observation:** Returned errno instead of abort, fallback phase reached, destination creation, source change, host effect, or different diagnostic. Capture actual Rust syscall path; source prediction does not substitute for that evidence.

### C16-02 — E13 hard-link entry, incumbent destination — FIRST CUT

**Hypothesis:** The same refusal precedes E13's AlreadyExists handling and preserves an incumbent.

**Pre-condition:** C16-01 setup with distinct incumbent bytes at destination; only destination occupancy changes.

**Action:** Same E13 decision.

**Expected:** Exact known refusal with C16-01 diagnostic and abort disposition; both objects unchanged. Native AlreadyExists is accepted by E13 and preserves incumbent, with TEMP retained for independent inspection.

**Falsifying observation:** Clobbered incumbent, successful publication claim without expected bytes, or any disposition other than the registered refusal.

### C16-03 — Direct pinned no-clobber helper, absent destination — FIRST CUT

**Hypothesis:** Invoking the fallback independently exposes a separate no-replace ABI refusal.

**Pre-condition:** C16-01 seed; exact tempfile 3.27.0 and locked dependencies. Direct helper entry is explicitly an isolation probe, not evidence E13 reaches it.

**Action:** TempPath::try_from_path then persist_noclobber, preserving E13's error mapping.

**Expected:** Exact known refusal: supervisor UnsupportedCapability `renameatx_np flags 0x4` at the ABI boundary, abort without tracee return; TEMP remains and destination absent. Native publishes exact bytes. The 0x4 Darwin NOREPLACE expectation must be checked against the resolved rustix dependency before instrument freeze; disagreement requires a documented pre-reading correction, not an expanded accepted set.

**Falsifying observation:** A different syscall/flag, returned error, successful fallback, source removal, destination creation or host mutation in routed execution.

### C16-04 — Direct pinned no-clobber helper, incumbent destination — FIRST CUT

**Hypothesis:** The ABI refusal occurs before collision handling, preserving the incumbent.

**Pre-condition:** C16-03 with distinct incumbent bytes; only occupancy changes.

**Action:** Same helper; record underlying AlreadyExists separately from helper-level success in native control. Account separately for TempPath's error-path destructor cleanup.

**Expected:** Exact known refusal with C16-03 diagnostic and supervisor abort; routed TEMP and incumbent unchanged. Native incumbent unchanged and E13 helper returns success on AlreadyExists; record whether TEMP cleanup occurred, without interpreting cleanup as publication.

**Falsifying observation:** Incumbent replacement, routed helper success, different refusal, or downstream cleanup after the registered abort.

### C16-05 — Forced secondary link fallback — DEFERRED

**Hypothesis:** tempfile's NOSYS/INVAL branch cannot provide usable publication through unsupported Link.

**Pre-condition:** A qualified way to select that branch without changing production behavior.

**Action:** Exercise secondary hard_link after a real no-replace unavailability result.

**Expected:** Exact known refusal at Link as in C16-01 if reached.

**Falsifying observation:** Supported exact publication or a different disposition.

Deferred because current ABI error prevents the returned NOSYS/INVAL prerequisite; an injected result only tests the fixture and repeats C16-01's Link boundary.

### C16-06 — Competing publisher after rendezvous — DEFERRED

**Hypothesis:** An admitted fallback preserves a destination published after TEMP preparation.

**Pre-condition:** Positive publication admission, deterministic rendezvous and separately qualified writer.

**Action:** Release publication only after an independent writer acknowledges incumbent creation.

**Expected:** Supported-and-correct: exact winner retained and no partial publication. At the underlying operation layer, the losing hard_link or persist_noclobber returns AlreadyExists. At the E13 decision/helper layer, hard_link AlreadyExists is accepted without error (Codex rollout/src/compression.rs:155), and persist_noclobber AlreadyExists becomes helper Ok(()) (:185-187). Record both layers separately; neither accepted collision claims that the loser published its TEMP bytes.

**Falsifying observation:** Clobber, mixed bytes, cleanup loss, or a false publication claim inconsistent with the retained winner. E13 accepting AlreadyExists or the helper returning Ok(()) for that collision is expected behavior, not false success.

Deferred because current refusals prevent the positive path; today it would repeat C16-02/04. No timing sleeps or stress retries substitute for a rendezvous.

## Non-goals

No production changes, mounted tests, decompression, metadata/sync repair, whole Codex execution, certificate publication or hard-link identity/rollback implementation. No new CI skips or softened assertions. Full restoration remains deferred until publication and its independent sync/timestamp/cleanup prerequisites qualify.

## Intersections and implementation selection

FIRST CUT: C16-01 through C16-04. DEFERRED: C16-05 and C16-06 for the reasons above. Six candidates, four selected independent leaves.

All four extend rank-12 publication cases rather than repeating them: experiments/fixtures/umbra-slate-r12-publication.c:51-53 uses ordinary renameat or unlinkat; userspace_run.rs:11184 records C12-01/03's routed rewrite refusal. C16-01/02 add Link and E13 error dispatch; C16-03/04 add tempfile's nonzero no-replace flags and helper error mapping. Existing userspace_run.rs and all experiments/fixtures were searched for hard_link, linkat and noclobber: existing symlink/readlink and descriptor cases are not hard-link publication. Sync coverage associated with #165 is reused as a prerequisite limitation, not proposed again. No existing slate case is duplicated.

## Review and reporting discipline

These are predictions, not measured compatibility. Each implemented leaf must pin supported-and-correct, exact known refusal with disposition, or a demonstrated semantic defect. Never treat any failure as the expected refusal. A new semantic defect gets its own drafted issue with case ID and measurement after dedup; use ISSUE-TBD placeholders and driver filing per S2. Leave issues open, using Refs wording.

Open questions before instrument freeze: verify dependency checksum and Darwin NOREPLACE value; capture Rust's actual hard_link stub; confirm supervisor journal termination details without inferring tracee errno; determine TempPath cleanup on the native AlreadyExists branch. These do not require production changes or prevent test-only characterization. No runtime statuses have been certified.

## Revision note — 2026-11-06

Applied exactly the required amendments in ideation-review.md: corrected the flush/sync citation to Codex rollout/src/compression.rs:149-151 and the routed Deny citation to crates/umbra-overlay/src/engine.rs:3046-3049; clarified C16-06’s underlying AlreadyExists versus E13 acceptance/helper Ok(()) observations and limited false success to a publication claim inconsistent with the retained winner. C16-06 remains DEFERRED. Case selection, prerequisites and all other predictions are unchanged. Source locations were verified at the declared pins; no builds or runtime readings were taken.

STATUS: COMPLETE
