# Rank 12 — Temporary-file publication and deletion

Prepared 2026-11-05 for dg-q5d2cqrb. Ideation only; no builds or runtime measurements.

## Intent and evidence boundary

Characterize family 12: closed-file renameat replacement and unlinkat first, open-handle leaves separately, then E10 temporary publication and E2 destination-write/source-delete. Ordinary pre-effect refusal must preserve names and bytes; success must publish the entire replacement; uncertainty after an effect must prohibit clean completion. This is characterization, not recovery implementation.

## Measured source, not runtime results

Umbra pin: c95da71f18bb89997c60a3004a326a208b1afc32 (workspace HEAD verified). Codex pin: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61 (HEAD verified at /Users/inva/Coding/umbra-worktrees/codex-src-44fe510). Umbra citations are relative to this workspace; Codex citations are relative to codex-rs in that read-only checkout. All citations below use those pins.

Umbra routes renameat and unlinkat through named stubs; the complete table has no bare rename or unlink entry (crates/umbra-platform-macos/src/abi.rs:303-337). Do not infer that Rust filesystem APIs hit the at-family. The storage replacing rename pins source identity and verifies the moved object (crates/umbra-storage-nfs-userspace/src/ops.rs:1073-1132); NoReplace is explicitly refused before dispatch (:418-438). These are storage facts, not evidence that the end-to-end path succeeds.

REMOVE can already have succeeded when its proof fails. The dispatcher accepts only atomic change evidence matching the captured directory change; otherwise it emits BLOCKED_RECOVERABLE and explicitly says the REMOVE succeeded and must not be retried (crates/umbra-storage-nfs-userspace/src/namespace/dispatch.rs:381-434). Its documented disposition retains evidence, latches refusal of further mutations, and prevents clean release (:396-400). Thus the Ganesha stop is a post-effect uncertainty, not ENOTSUP and not rollback. Whether this server supplies the required evidence is a runtime observation still owed.

Codex E10 writes a temporary snapshot, validates it, then renames it; validation and rename failure have cleanup paths (core/src/shell_snapshot.rs:284-325). E2 writes destination bytes before removing the original and records a destination change before removal (apply-patch/src/lib.rs:620-665). Neither source establishes the Darwin syscall chosen by Rust or the filesystem executor.

Open issues were retrieved with gh issue list --state open --limit 200 --json number,title,body. Refs #57 (real overlay/supervisor abort contract), #78 (commit before resume), and #158 (bare unlink/rmdir interception gap). Their bodies are historical claims, not replacement source pins. #158 explicitly labels its end-to-end result unmeasured. #57's old unlink destruction description must not be read as current behavior: the preparation arm at crates/umbra-overlay/src/engine.rs:3771-3778 queues pending.destroy and pending.whiteouts; the destructive commit branch at :4151 removes the symlink index and then the shadow object. Preparation records deferred work; commit performs destruction. Preserve cross-component evidence rather than extrapolating old issue prose.

## Shared expected pins and measurement contract

Each selected leaf gets one native host control on fresh disposable data, followed by one enforced userspace-registry run against live Ganesha with UMBRA_INTEGRATION_REQUIRED. Missing prerequisites invalidate qualification; a skip is no evidence. No mounted NFS run. Pin executable hash, toolchain/libc/OS, debugger, provider configuration, server image/configuration, routing roots, source hashes, and exact commands in the reading. No build or execution is authorized by this document's preparation.

Use a single-thread C fixture with independently initialized SRC and DST payloads of different lengths. Seed shadow objects through qualified routed writes and close them before the focal syscall; record the seed phase separately. Host control gets its own identical disposable files. Snapshot host roots before routing; inspect them afterwards for escape. Inspect shadow names, bytes, and identities through an independent NFS client, not tracee stdout. No absence conclusion from a failed lookup or unavailable observer. Use explicit phase records and capture errno immediately if the syscall returns; distinguish supervisor termination from a tracee error return.

Use a 30-second run deadline and 10-second bounded cleanup grace per leaf, recorded before execution. Capture supervisor/provider/tracee exits, owned process identities and reap evidence; cleanup only recorded owned processes. Retain storage journal, overlay journal, transaction identity, writer epoch, release disposition and RunCompleted presence/absence. On uncertainty, assert no clean completion and retain the partial server state. C12-02 and C12-04 require the same-backend authority oracle below, without a conditional exemption. A persisted recovery marker alone is not behavioral evidence. Never open a new run and call its fresh authority proof about the old one. Missing oracle records or unavailable independent server evidence fail qualification.


### Test-only same-authority oracle for C12-02 and C12-04

Use a forwarding Storage implementation defined under crates/umbra-storage-nfs-userspace/tests/, served by a dedicated entry test in the integration-test executable (selected explicitly by the registry command arguments, with a private provider-role environment flag). Its provider entry calls the public umbra_storage::provider::serve_provider with the normal provider ID. The factory constructs the real NfsUserspaceStorage using NfsUserspaceConfig::from_options and NfsUserspaceStorage::connect, exactly as the production entry does (crates/umbra-storage-nfs-userspace/src/main.rs:5-9). The generic factory and backend dispatch are public at crates/umbra-storage/src/provider.rs:224-261. The adapter delegates every Storage method unchanged until the single designated unlink failure; it adds no production instrumentation, fake transport, new admission or replacement backend. Its entry must keep stdout free of test-harness chatter that could contaminate provider operation; use the test executable's explicit entry selection and a sidecar file for records. Validate the entry/handshake during later implementation before taking a reading.

Retain the actual OpenRun identity, acquired WriterLease, incoming unlink RequestContext (including writer epoch), and the original error. The wrapper intercepts the real backend execute return while that exact backend and session are still alive, before returning the error to the supervisor. Match only the designated SRC Unlink and the preregistered post-effect BLOCKED_RECOVERABLE diagnostic; other errors are recorded and returned without treating them as the expected trigger. Public Storage methods provide execute, flush, release_writer and close_run (crates/umbra-storage/src/lib.rs:32-92); the same backend's admission is inspectable through admission() (crates/umbra-storage-nfs-userspace/src/storage.rs:720). No reopen or reacquire is permitted.

After the matched failure, perform exactly one WriteAt probe to the already seeded, otherwise untouched DST, offset zero, with a distinct probe payload and fresh idempotency key but the SAME run and writer epoch. Clone the original context and change only the key and an unexpired bounded deadline. This is an authority observation after the focal mutation, not a retry of REMOVE. Assert the post-failure WriteAt execute returns Err with ErrorKind::InvalidState and a context embedding the original BLOCKED_RECOVERABLE diagnosis. Record the original REMOVE error kind separately: the predicted uncertainty is ErrorKind::LeaseLost, constructed through unproven/AuthorityError::IdentityUnproven (crates/umbra-storage-nfs-userspace/src/namespace/dispatch.rs:428-434, :498-500) and mapped by crates/umbra-storage-nfs-userspace/src/error.rs:372-382. Do not require equality between the REMOVE and WriteAt error kinds. Retain operation labels separately rather than demanding an identical operation field, and return the original REMOVE error unchanged to supervision. The source basis for the later WriteAt refusal is execute's preflight at crates/umbra-storage-nfs-userspace/src/storage.rs:1794 and the blocked mutation gate at :1050-1072, which explicitly constructs InvalidState while embedding the retained diagnosis; the failure latch is at :1241-1252. Independently read DST through the NFS client and require the exact pre-probe bytes and length; assert SRC remains absent. A successful probe or any changed DST byte is a semantic failure, even if a later call refuses.

On that same backend, make one EntireRun flush with the retained run/epoch and require Err with the retained blocked diagnosis, never a durability receipt (storage.rs:2082-2088). Attempt release_writer once with the exact saved lease and require Err, recording its full kind/context and admission identity before and after; the held-admission release path is storage.rs:837 onward. Record the normal subsequent close_run result as well, and require that no clean release/close was reported. Retain raw admission-marker bytes before/after for the same run and epoch and assert no transition to Released; this is persisted-state corroboration, not a substitute for the rejected WriteAt. The close path releases admission before dropping the run and retains it on an unclean release (storage.rs:2165-2200).

Write these records synchronously to a unique harness-owned sidecar before returning the ORIGINAL unlink error unchanged to the supervisor. Record provider PID, run ID, writer epoch, request keys, ordered stages and every API result, and require the sidecar's terminal observation record. Do not fabricate a successful probe result from a missing record. The outer harness then checks the original supervisor failure, absent RunCompleted, independent server contents and cleanup using the existing run metadata/journal access (userspace_run.rs:870-877, :1337). Reopening as used by the successful-run check at :1545-1565 does not qualify this failed authority. These observations are part of each case's single routed reading and share its deadline/cleanup budget. If the adapter cannot deliver these records within that budget, the case fails qualification; no journal-only fallback and no production edit is authorized.

One reading per planned run; no retry-to-green. Instrument-no-reading, bounded cleanup progress after an intervening action, and terminal failure remain distinct. A status differing from Expected is recorded honestly and triggers review. A new demonstrated production defect gets a separate crate-prefixed issue after gh issue list --search duplicate checking, with its case ID and semantic assertion. No production fix, weakened assertion, skip or continue-on-error belongs here. Preserve evidence before disposable cleanup; verify cleanup and leave recovery evidence intact until archived by the driver.

## Cases

### C12-01 — Closed-file replacing renameat

**Hypothesis:** Replacing a closed destination publishes the entire source object.

**Pre-condition:** Independently seeded shadow SRC and DST; both closed; same routing anchor; native comparison prepared identically.

**Action:** One renameat(AT_FDCWD, src, AT_FDCWD, dst), flags absent. Inspect both names independently after exit.

**Expected:** supported-and-correct: native and routed success, source absent, destination exactly SRC, no DST suffix, host roots unchanged in routed run, clean journal completion and writer release. This is a prediction, not measured support.

**Falsifying observation:** Kernel host mutation, wrong bytes or identity, surviving source, any refusal, or clean completion inconsistent with server/journal state.

### C12-02 — Closed-file unlinkat with post-effect evidence

**Hypothesis:** The current Ganesha path stops on unprovable REMOVE evidence after deleting the name.

**Pre-condition:** Separate closed SRC and untouched DST objects; native control deletes SRC successfully.

**Action:** One unlinkat(AT_FDCWD, src, 0). Independently inspect SRC, DST, journals and writer authority even if the supervisor fails.

**Expected:** exact known refusal: BLOCKED_RECOVERABLE with the diagnostic that REMOVE succeeded but atomic change evidence cannot prove the pinned object, SRC absent on server, DST intact, run failed/recovery-required, no RunCompleted, retained evidence and no clean release or usable old writer authority. This is explicitly post-effect, not ordinary pre-effect refusal (dispatch.rs:396-434 above).

**Falsifying observation:** Different diagnostic/stage, source still present, DST corruption, clean completion/release, continuing old writer authority, or unavailable server evidence. Atomic matching proof and successful deletion would falsify the predicted refusal and must be reported as supported-and-correct, never forced into this expected category.

### C12-03 — Replacement while destination descriptor remains open

**Hypothesis:** An old destination descriptor remains bound to DST after publication of SRC.

**Pre-condition:** C12-01 setup with exactly one additional read-only open of DST; native control confirms identity behavior.

**Action:** Same renameat; if admitted, read old fd and reopen destination separately.

**Expected:** supported-and-correct: old fd yields DST, new name yields SRC, source absent, normal close/release. No pathname rebinding. This deliberately tests the end-to-end invariant beyond storage's identity proof (namespace/mod.rs:23-28).

**Falsifying observation:** Old fd reads SRC, wrong error, lost handle, unexpected refusal, or any shared-contract failure. A precise refusal discovered here is a new measured status requiring a revised implementation registration, not a permissive alternative assertion.

### C12-04 — Unlink while target descriptor remains open

**Hypothesis:** Holding a read descriptor does not turn uncertain removal into clean success.

**Pre-condition:** C12-02 setup with exactly one additional read-only open of SRC. Native control proves SRC remains readable by fd after name removal.

**Action:** Same unlinkat. Attempt old-fd read only if tracee resumes; otherwise mark that phase unreachable and independently inspect server and authority.

**Expected:** exact known refusal, same post-effect BLOCKED_RECOVERABLE disposition as C12-02. No claim that open-handle semantics were qualified when supervision stops. Native old fd still reads SRC.

**Falsifying observation:** Clean completion after uncertainty, changed unrelated bytes, any different refusal boundary, or old-fd rebinding if the operation succeeds. A successful routed result must additionally preserve the original object through the fd.

### C12-05 — E10 Rust temporary-write/rename composition

**Hypothesis:** A temporary write validated before publication exposes complete bytes or a precisely located failure.

**Pre-condition:** C12-01 positive; pin the actual Rust/library syscall path, and qualify write/close first.

**Action:** Write TEMP, close and validate it, then Rust filesystem rename over DST; exercise cleanup separately from the publication leaf.

**Expected:** supported-and-correct target: complete TEMP at destination, temporary name absent, clean release. This is a deferred target, not predicted current support; bare rename routing has not been established (abi.rs:303-337).

**Falsifying observation:** Any ordinary rename refusal, temporary-name survival after reported success, destination bytes or length differing from TEMP, missing clean release, host-path escape, partial publication, cleanup erasing failure evidence, or an unqualified syscall route. Each contradicts the supported-and-correct target; none is an accepted alternate result.

### C12-06 — E2 destination-write/source-delete composition

**Hypothesis:** Destination publication survives a subsequent deletion failure and must not be represented as a rolled-back move.

**Pre-condition:** C12-02 disposition characterized; the actual E2 executor deletion route must be positively established as mediated unlinkat before this registration is runnable; separate SRC and DST with qualified destination writer. If the actual executor uses a non-mediated route, this branch stays unqualified/DEFERRED and requires a separate later registration. Do not substitute unlinkat for the executor API.

**Action:** Write replacement DST, close it, then remove SRC using the pinned E2 operation shape.

**Expected:** exact known refusal target for the selected mediated-unlinkat branch: post-effect BLOCKED_RECOVERABLE, destination contains the new bytes, source absent, no clean completion or writer release. The non-mediated bare-unlink branch is unqualified/DEFERRED, with no runtime status claimed here. Its proposed defect characterization needs a separate registration and measured semantic assertion before an issue disposition (Refs #158); it cannot count as this branch passing.

**Falsifying observation:** Claimed rollback, destination bytes or length differing from the completed write, source surviving the predicted post-effect removal, a different refusal stage/diagnosis, clean completion or release after uncertain deletion, continued old writer authority, unavailable evidence, or an unobserved/non-mediated syscall path. The last route invalidates this branch qualification and leaves the composition deferred.

## Non-goals

No recovery implementation, crash injection, concurrent namespace adversary, cross-anchor move, directory deletion, no-replace/exchange flags, whole Codex agent, or production changes. No commits until ideation_review ACCEPT; the accepted document must become the byte-identical first documentation commit per S4.

## Intersections and implementation selection

FIRST CUT: C12-01, C12-02, C12-03, C12-04, each independently invocable and with its own control. Closed leaves execute first. Open leaves add one descriptor lifetime variable; their statuses are measurable with test-only additions even if they discover a defect or refusal. Do not postpone a measurable defect merely because positive behavior needs a production fix.

DEFERRED: C12-05 because Rust bare-rename routing and the validation/cleanup composition have not been qualified; reproducing a generic write or rerunning closed renameat would duplicate component coverage without qualifying E10. C12-06 because the E2 executor's actual delete path is not established, and substituting unlinkat silently would cease to qualify that composition. Both remain explicit candidates for a later registration after primitive readings; no claim that their production prerequisite has shipped.

No duplicate is proposed: experiments/fixtures/umbra-test-child.c:302-348 already exercises dirfd moves to new names, while C12-01 adds independently seeded replacement and cross-journal evidence. C12-03 extends that with retained destination identity. The rank-4 nonempty directory refusal at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:2925-2964 and experiments/fixtures/umbra-userspace-edges.c:1217-1258 is a different pre-effect directory error; C12-02 adds regular-file post-effect uncertainty, and C12-04 adds a live descriptor. The Rust buffered-write case at userspace_run.rs:5017 is a component prerequisite, not temporary publication.

Prefer tests/slate_r12.rs only if helpers can be reused without moving them; otherwise one contiguous slate-r12 block at the end of userspace_run.rs. Fixtures use umbra-slate-r12-* names. Later live runs require the serial wrapper and idle runner; builds require the scope's disk check and this checkout's target only.

## Review and reporting discipline

Six candidates: four FIRST CUT, two DEFERRED. Expected is the pre-registration; no measured status is claimed here. Open questions: actual Ganesha change-info disposition; overlay replacement behavior with an open destination; exact E10/E2 Darwin syscall routes; whether helpers can be reused from a separate integration test. Resolve instrumentation prerequisites before taking readings, and report unavailable evidence honestly. Driver owns durable mirroring under L51.

## Revision note — 2026-11-05

Applied the three required amendments from ideation-review.md: specified the test-only same-backend authority oracle and exact behavioral/persisted-state assertions for C12-02/04; corrected unlink preparation versus destructive commit citations; completed C12-05 falsification and selected C12-06’s mediated branch while explicitly deferring the non-mediated branch. Six cases and the four/two selection remain unchanged. No builds, tests or live readings performed. This revision awaits review; the driver must archive it under a new revision name and preserve the original pre-registration.

## Revision note — 2026-11-05 (second amendment)

Applied the sole required amendment from the independent re-review in ideation-review.md: the same-authority WriteAt probe now requires ErrorKind::InvalidState with the embedded original BLOCKED_RECOVERABLE diagnosis. The original REMOVE kind is recorded separately (predicted LeaseLost), and the original error is returned unchanged to supervision. Verified both error paths at the pinned umbra source; no production change, build, test or live reading performed. Case count and selection remain six candidates, four FIRST CUT and two DEFERRED. Driver must archive this revision under a new revision name, preserving earlier pre-registrations.

STATUS: COMPLETE
