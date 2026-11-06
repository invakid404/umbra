# Rank 15 — A helper executable reached through a shadow-only alias

Prepared 2026-11-06. Bucket D. Ideation only; no builds or runtime readings.

## Intent and evidence boundary

Characterize family 15: a harmless already-built host helper reached through a logical symlink in a shadow-only directory. The helper must remain mediated or be explicitly refused; neither a host alias nor execution of a different host object is acceptable. Locking, readlink, and execution are independent leaves. This is not executable materialization or a whole Codex startup qualification.

## Measured source, not runtime results

Umbra pin: 9a4d29bbdbe41940586d2570e9788651fffece78, verified in /Users/inva/Coding/umbra-worktrees/slate-r15. Codex pin: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61, verified in /Users/inva/Coding/umbra-worktrees/codex-src-44fe510 (read-only). All following repository file:line references are at these respective pins; Codex paths include codex-rs.

Codex creates a per-session directory, opens its lock file and calls try_lock before constructing Unix symlinks to current_exe (codex-rs/arg0/src/lib.rs:378, :383, :390, :400, :405). PATH prepending is the stated API purpose (:336), and environment installation is at :175. Reading the alias is an experimental diagnostic, not a claimed Codex call at this site.

Umbra handles exec by reading its path, rejecting relative spelling, and calling cache::resign on that path (crates/umbra-platform-macos/src/native.rs:2196, :2199, :2202). The cache immediately calls host fs::canonicalize and maps failure to “twin source” (crates/umbra-platform-macos/src/cache.rs:88–89). The source therefore predicts an explicit supervisor error for an absolute alias absent from the host, before executable replacement; it does not predict an ENOENT return delivered to the tracee. This is a prediction, not a measured refusal.

Existing tests already isolate routed readlink's buffer-binding refusal and its exact “ReadLink requires set_readlink_buffer” diagnostic (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:7272, :7346); Refs #170, open in the captured issue list. Symlink-follow and no-follow cases exist at :6693, :6773, :6815. Their trees are created in-run because inventory rejects host-seeded symlinks (:7063). Rust locking and flock already have independent cases C13-01 and C13-02 (:11924, :11929), backed by experiments/fixtures/umbra-slate-r13-primitives.c:69, :79. Existing forkexec uses a direct helper executable and verifies its writes (experiments/fixtures/umbra-userspace-edges.c:884–911). C15-01 extends that executable boundary by changing only its pathname to a shadow alias, rather than adding another direct-exec qualification.

Open issues were read with gh issue list --state open --limit 200 --json number,title,body; the complete response is open-issues.json beside this document. Refs #217 concerns helper consolidation; reuse the launch harness rather than copying it. Refs #139 is historical exec-image coverage context, not evidence that aliases work. No dedicated shadow-alias execution issue was found in this inventory; any new measured semantic defect requires driver-side dedup before filing.

## Shared expected pins and measurement contract

For each implemented leaf, first run a native control on disposable host data, then an enforced userspace-registry run against live Ganesha with UMBRA_INTEGRATION_REQUIRED. Missing prerequisites or a skip provide no reading. No mounted NFS. Before implementation readings, record source hashes, fixture digest, macOS/architecture, toolchain, debugger, exact argv/environment, routing roots and Ganesha configuration. Commit fixtures/tests before readings; use the serial-test wrapper only with the runner idle and check disk space before each build. No reading is authorized during this ideation.

Use one single-thread launcher and a distinct harmless helper. The helper writes an exact nonce through an unbuffered routed write, then exits. Run its absolute host path as the paired positive control before testing the alias. Native setup creates the directory and symlink in disposable data. Routed setup creates them in-run; assert their exact shadow identities and link target through the independent NFS client. Keep readlink and locks out of the exec path. Record setup and pre-exec markers through a qualified observation channel; marker absence alone proves no effect only when independently corroborated.

Use a fixed 60-second external deadline per launch and a 10-second bounded cleanup grace. Reuse slate_launch (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:8331) and its evidence channels (:8342, :8360); verify its cleanup obligations meet these budgets when implementing. Capture supervisor exit/diagnostic separately from tracee status; an unavailable tracee return is not errno. Retain stdout/stderr, journal disposition, shadow link target and exact bytes/names, host before/after snapshot, owned PID identity and cleanup/reap evidence. A supervisor refusal must have no helper sentinel, no host mutation, no live owned descendants and bounded termination. Cache effects outside the routing root are not forbidden by this hypothesis.

One reading per planned run. A terminal failure is never retried to green. Instrument-no-reading, bounded cleanup progress with intervening actions, and terminal failure remain distinct. Kill only provably owned processes. Positive exec requires exact helper identity, exact shadow nonce, unchanged host routing-root bytes and names, and complete reap. Unexpected success must be evaluated against these semantics, never accepted by broadening the refusal assertion.

## Cases

### C15-01 — Absolute shadow alias exec (FIRST CUT)

**Hypothesis:** Logical alias creation does not make the alias available to the host executable cache; exec refuses explicitly before helper effects.

**Pre-condition:** Direct absolute host-helper control succeeds natively and routed with the exact mediated nonce. Fresh directory and alias exist only in shadow for the routed run; independently verify target equals the host helper path. Do not call tracee readlink, stat or locking before exec.

**Action:** Execute the same helper with the same arguments via the absolute logical alias, changing only the executable pathname. No fork, spawn attributes or PATH search is added.

**Expected:** Exact known refusal (pre-registration): native alias executes and produces the nonce; routed supervisor terminates at cache source canonicalization with “twin source” and underlying ENOENT, no successful exec/tracee completion, no helper nonce, no host alias or mutation. Record actual supervisor exit value at the first reading; no arbitrary nonzero exit qualifies. This is an explicit unsupported boundary, not supported alias execution and not yet a demonstrated semantic defect.

**Falsifying observation:** Different error/stage, tracee ENOENT instead of supervisor refusal, silent successful completion, wrong executable, escaped write, missing setup evidence, timeout or survivor. Correct mediated execution falsifies the refusal prediction and must be reported supported-and-correct with its full oracle.

### C15-02 — Independent lock acquisition (DEFERRED: duplicate)

**Hypothesis:** The startup lock reaches the already characterized routed lock boundary.

**Pre-condition:** Disposable lock file, no alias or exec.

**Action:** Acquire/release a nonblocking lock, independently of helper launch.

**Expected:** Exact known refusal characterized by C13-01/C13-02; reuse their existing assertions, not a new C15 test or an inferred Codex-wide result.

**Falsifying observation:** A differing measured status or effects in those existing cases. Defer this duplicate rather than allowing it to gate C15-01.

### C15-03 — Alias creation and readlink (DEFERRED: duplicate)

**Hypothesis:** Alias existence and readlink service are distinct from executable lookup.

**Pre-condition:** Fresh shadow directory; no lock or exec.

**Action:** Create alias and read its target in an independent run.

**Expected:** Exact known refusal: existing readlink case stops with “ReadLink requires set_readlink_buffer”; creation is checked independently through NFS. Reuse that case, including its disposition assertion.

**Falsifying observation:** Wrong target, host alias, successful readlink or another diagnostic. Existing coverage at userspace_run.rs:7346 owns this measurement; C15-01 uses creation only as a verified prerequisite.

### C15-04 — PATH lookup through the helper directory (DEFERRED: later composition)

**Hypothesis:** Search selection cannot substitute a same-named helper from a later PATH entry.

**Pre-condition:** Absolute alias exec positively qualified; explicitly pinned PATH and search API.

**Action:** Prepend only the helper directory and execute the helper basename.

**Expected:** Supported-and-correct target: chosen alias helper emits its nonce under mediation. Current absolute alias refusal makes this composition premature; no exact libc search disposition is claimed from the inspected sources.

**Falsifying observation:** Fallback helper executes, wrong nonce, host mutation or an unbounded search. Defer until C15-01 admission is available, normally requiring a production change outside this arc.

### C15-05 — Host spelling collision (DEFERRED: independent setup needed)

**Hypothesis:** A logical alias must never execute a different host executable at the same spelling.

**Pre-condition:** Independently qualified test-only construction of a shadow alias overriding a host regular executable; two harmless helpers with distinct sentinels.

**Action:** Exec the logical alias while the host spelling resolves to the decoy.

**Expected:** Supported-and-correct target: only the intended helper executes, emits its exact nonce under mediation, exits successfully within the deadline, and is completely reaped within the cleanup grace; the routed host snapshot remains unchanged. Native execution produces the intended nonce on disposable data. No refusal is an alternative passing outcome. Host canonicalization makes wrong-helper execution a defect hypothesis, not a measured finding (native.rs:2202; cache.rs:89).

**Falsifying observation:** Any refusal or unrelated error, absent helper or missing intended nonce, wrong executable identity or decoy sentinel, routed host effects, timeout, survivor or incomplete cleanup falsifies the supported-and-correct target. Defer because replacement/whiteout setup adds an unqualified primitive; do not hide that primitive inside C15-01 or declare a defect without executing its semantic assertion.

## Non-goals

No production code changes, executable materialization, arbitrary compilation, full Codex run, lock implementation, PATH fallback redesign, mounted tests, builds or runtime readings in this node.

## Intersections and implementation selection

Five candidates; FIRST CUT is C15-01 only, including its paired direct-helper controls. C15-02 and C15-03 are deferred duplicates retained explicitly to keep lock and readlink independent. C15-04 is later composition after admission. C15-05 needs isolated replacement setup. Direct-helper controls are prerequisites within C15-01, not new slate cases. New fixture names use experiments/fixtures/umbra-slate-r15-*.

The existing launch helper is private in userspace_run.rs:8331. Prefer one contiguous slate-r15 block at its end so it can be reused without moving helpers or copying bodies. Add only necessary CI lines, no skips. After ideation review acceptance, commit this document byte-identically at knowledge/umbra/codex-ideation-slate-r15.md as the first commit, per S4. Do not commit during ideation.

## Review and reporting discipline

Expected is pre-registration; no runtime status is certified here. Report the actual first reading even if it contradicts this prediction. A demonstrated wrong-helper or host-effect defect needs a semantic assertion and a separate driver-filed issue after dedup; use ISSUE-TBD placeholders and an issue draft per S2. Explicit refusal remains a qualification blocker, even when its characterization test passes.

Open questions for implementation: exact rendered supervisor status at the canonicalization refusal; availability of an NFS link-target inspection helper without new harness duplication; whether existing cleanup observation catches a short-lived exec tracee (Refs #216). None requires a production edit to attempt the first-cut measurement. If the instrument cannot establish these observations, report unqualified rather than infer success.

## Revision note — 2026-11-06

Applied required amendment 1 from ideation-review.md: C15-05 now selects only the supported-and-correct target, with exact intended-helper identity/nonce, successful bounded completion and complete cleanup. Refusal, unrelated error, missing helper/nonce, wrong identity, host effects, timeout and incomplete cleanup explicitly falsify it. The justified deferral, first-cut selection and case count are unchanged. No builds or runtime readings were made.

STATUS: COMPLETE
