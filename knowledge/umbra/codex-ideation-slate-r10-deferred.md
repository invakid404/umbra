# Rank 10 deferred — Threads, descriptor lifetime and waits

Prepared 2026-11-09 for dg-e4ny7jr0. Ideation only; no builds or execution measurements.

## Intent and evidence boundary

Finish the deferred rank-10 leaves under S0-S1 and L72. FIRST CUT means a proposed discriminating measurement, not certified compatibility. FIX-PHASE ITEM means production work or missing production observability prevents a qualified new reading.

Pins: umbra `9aa330df803159ef35c3eae21ec9c0b07abeb5bb`, workspace `/Users/inva/Coding/umbra-worktrees/slate-r10-deferred`; Codex `44fe510ce3ee61c8ef623adcbf89b901c73ddd61`, read-only `/Users/inva/Coding/umbra-worktrees/codex-src-44fe510`. All relative source citations below refer to the umbra pin unless prefixed Codex. Both HEAD hashes were checked. Original deferrals are selection deferrals, not seven demonstrated production failures: `knowledge/umbra/codex-ideation-slate-r10.md:246` selects the initial cut and conditions later leaves on isolating their boundary.

## Measured source, not runtime results

The current namespace entry keeps the trapping thread (`crates/umbra-platform-macos/src/native.rs:2135`); Fork and Park call the same `single_thread` function (:2145, :2190), which requests one thread batch and emits “fork/deferred wait requires one thread” (:644). Poll instead writes a zero result, clears carry and resumes (:2181). Holding siblings during a return window is deliberate (:583, :609), including the documented pipe dependency stall (:600).

Descriptor opens create independent OFDs even for the same object (`crates/umbra-supervisor/src/events.rs:1289`); successful transfers advance OFD views (:1303); close removes the process binding and releases the OFD (:1336). Missing routed bindings answer EBADF as a denial, not a supervisor stop (`crates/umbra-overlay/src/engine.rs:1833`). This is current evidence rather than carrying forward the historical fork-offset limitation.

Open issues were read with `gh issue list --repo invakid404/umbra --state open --limit 200 --json number,title,body`; snapshot: `evidence/open-issues.json`. #135 explicitly tracks remaining Fork/Park guard removal and thread support. #216 concerns the short-lived PID oracle; #217 tracks launcher consolidation. #227 is a measured Rust File teardown failure in a different fixture, not evidence that rank-10 Tokio fails. Do not accept that failure as success here.

## Shared expected pins and measurement contract

Inherit the original rank-10 shared contract (`knowledge/umbra/codex-ideation-slate-r10.md:28`): disposable native control first, then enforced userspace registry with live Ganesha and `UMBRA_INTEGRATION_REQUIRED=1`; no mounted NFS. Record exact source/provider/fixture hashes, OS, toolchain, debugserver, arguments, routing roots and server config. Each leaf gets one reading per planned run, 60-second execution budget and 10-second cleanup grace. Use existing `slate_launch` (`crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:8331`), not a copied launcher. Reuse the existing evidence and cleanup conventions (:8079, :8273). Census failure is UNMEASURABLE, never an empty census. Prove ownership before killing; record direct-child reap, pipe EOF and absence of owned live members. No retries of terminal failures.

Every positive Expected below means **supported-and-correct**: native and routed tracee/supervisor normal completion, exact semantic result, unchanged routed host root, independent NFS byte/length verification, journal completion/replay, writer-authority reacquisition with Refuse takeover, and qualified cleanup. Missing evidence or skipped inputs cannot qualify. Record call results and errno immediately on the issuing pthread. Capture partial transfers and finish them correctly. Mutex/condition handshakes establish order; sleeps do not. A record file is never the sole storage oracle. Keep setup, target primitive and cleanup separately attributable.

Codex relevance: Codex `codex-rs/core/src/installation_id.rs:19` dispatches file work through `spawn_blocking` (:22); this motivates worker/file composition but does not make these leaves a qualification of its locks, metadata or sync operations (:32, :36, :59). Codex `codex-rs/Cargo.lock:15567` locks Tokio **1.52.3**. The existing rank-10 fixture explicitly pins **=1.48.0** (`experiments/fixtures/slate-r10-tokio/Cargo.toml:14`). S2/L69 require keeping that pin. C10-16 therefore qualifies the historical rank-10 dependency, not version parity with this Codex pin. No new dependency is required. This discrepancy is disclosed rather than silently changing either pin.

## Cases

### C10-07 — Independent opens of the same object

**Re-check:** Originally held after C10-06 pending isolated descriptor setup (original ideation :110, :246). Current `events.rs:1289` (full path above) explicitly allocates a new OFD per open. No production blocker identified. Existing C10-06 is a single-fd handoff (`userspace_run.rs:9059`); the C fixture selects that mode at `experiments/fixtures/umbra-userspace-threads.c:290`. This leaf changes only open-description identity.

**Decision: FIRST CUT.**

**Hypothesis:** Independently opened descriptors keep independent offsets while sharing file bytes.

**Pre-condition:** Externally seed six dots; main and helper each open the same regular file without truncation, dup or seek. Acknowledge both opens before writes.

**Action:** Main writes AA; hand off; helper writes B; join and close both. Record fd/TID and transfer counts.

**Expected:** supported-and-correct; exact `BA....`, length six, with the shared completion disposition above.

**Falsifying observation:** `AAB...`, lost bytes, shared position, unexpected refusal, wrong reply or timeout.

### C10-08 — Cross-thread close and descriptor reuse

**Re-check:** Originally held for isolated lifetime setup (original ideation :122, :246). Current close removes the process binding (`crates/umbra-supervisor/src/events.rs:1336`); the stale-fd path returns EBADF (`crates/umbra-overlay/src/engine.rs:1838`). This extends C10-06's live-fd handoff (`userspace_run.rs:9059`) with lifetime termination, not another offset test.

**Decision: FIRST CUT.**

**Hypothesis:** Helper close invalidates main's fd without retaining stale thread state.

**Pre-condition:** Main opens a readable seed and hands the fd to helper. Strict in-memory handshakes; no I/O race.

**Action:** Helper closes and acknowledges; main reads the closed fd before any new open, records -1/EBADF, then opens another path and writes FRESH. Record both fd numbers; report reuse only if equal. Close/join.

**Expected:** supported-and-correct; closed read returns -1/errno 9, fresh output is `FRESH`, length five, seed unchanged; normal completed disposition despite recoverable EBADF. Unequal fd numbers leave reuse unexercised, explicitly reported.

**Falsifying observation:** Old bytes returned, success on closed fd, fresh-object corruption, supervisor abort on expected EBADF or cleanup failure.

### C10-09 — Sibling-dependent pipe read

**Re-check:** Original deferral specifically required evidence of the return-window dependency (original ideation :134, :246). The current implementation still freezes siblings (`crates/umbra-platform-macos/src/native.rs:609`) and documents this pipe stall (:600). Announcing read intent does not prove the read entered that window before the writer ran. Existing C10-05's competition (`userspace_run.rs:9055`) supplies no such proof; a timeout cannot distinguish this mechanism from unrelated stalls.

**Decision: FIX-PHASE ITEM.** Production blocker: held-sibling return window at `crates/umbra-platform-macos/src/native.rs:600` and :609; blocks C10-09. Source evidence only. #135 covers sibling holding broadly, but its remaining slices do not specify a dependency-window witness or resolution of this stall. Track the focused gap as `ISSUE-TBD(pipe-return-window-witness)`. A future leaf needs an externally observable owner/window witness and bounded cleanup before any exact timeout preregistration. Do not mint a timing-luck case.

### C10-11 — Fork after helper joins

**Re-check:** Original selection postponed lifecycle composition (original ideation :158, :246). Current guard reads the current thread list, with no historical-thread bit (`crates/umbra-platform-macos/src/native.rs:644`), before fork (:2145). C10-10 already pins rejection with a live helper (`userspace_run.rs:9488`); this leaf changes only joining before fork. Separate child output avoids inherited OFD composition.

**Decision: FIRST CUT.**

**Hypothesis:** A completed helper lifecycle permits the single-thread fork path.

**Pre-condition:** Helper acknowledges readiness, exits, and is joined before fork. Record joined phase; no other helper remains.

**Action:** Fork a minimal child using prearranged async-signal-safe calls to create/write CHILD to its distinct path and _exit(0); parent reaps and records PID/status.

**Expected:** supported-and-correct; mediated child output `CHILD`, length five, parent wait returns that child with exit zero, completed disposition.

**Falsifying observation:** One-thread refusal after join, missing child mediation or bytes, incorrect wait status, timeout or survivors. If debugserver retains an exiting thread, retain the refusal and census evidence as a new finding; never retry it into success.

### C10-12 — Deferred wait with helper alive

**Re-check:** Original proposed Park reachability needed live-child proof (original ideation :170). At current base Park still calls `single_thread` (`crates/umbra-platform-macos/src/native.rs:2189`), exactly the function Fork calls (:2145), yielding the same diagnostic (:647). C10-10 pins this refusal (`userspace_run.rs:9488`) and the structural test pins both callers (`native.rs:3556`). A second call site does not make the shared rejecting guard a new outcome under L72.

**Decision: FIX-PHASE ITEM.** Blocker: `native.rs:2190` → :644. Blocks C10-12 (and C10-10 positive admission). Evidence: source plus existing refusal test contract, not a new run. Refs #135, remaining slices 3–4. Fix sketch: implement safe multithread fork/park admission and thread accounting before replacing refusal with positive child/wait semantics.

### C10-13 — Polling wait with helper alive

**Re-check:** Original held for the live-child setup shared with C10-12 (original ideation :182, :246). Current `wait_plan` selects Poll for managed non-ready children with WNOHANG (`crates/umbra-platform-macos/src/native.rs:211–214`, specifically :213); Poll never calls the guard and supplies the zero/carry-cleared reply (:2181–2187). Thus C10-10's refusal does not block this leaf. This extends its child-lifecycle setup with polling rather than blocked admission. The existing single-thread WNOHANG fixture (`experiments/fixtures/umbra-test-child.c:487`) already uses a pipe-held child and status sentinels (:508). C10-13 changes the live-parent-helper precondition, not the polling ABI or expected zero reply; it specifically probes thread admission to Poll. Keep one public waitpid primitive rather than repeating that fixture’s entry-point matrix.

**Decision: FIRST CUT.**

**Hypothesis:** WNOHANG returns zero and preserves status with a live managed child and live parent helper.

**Pre-condition:** Fork while single-threaded. Child announces readiness and waits for an external harness release; use a prearranged kernel IPC gate, never a routed regular-file polling loop. Only after child readiness create the parent helper and acknowledge its liveness. The harness must keep release withheld until the parent reports the poll result, so child exit cannot race selection. Record the owned child PID; census is supplementary, not the sole live-child oracle.

**Action:** Parent calls waitpid(child, &sentinel, WNOHANG) once. Record result and unchanged status; stop/join helper, then ask harness to release child and reap while single-threaded. Separate cleanup wait evidence from the focal poll.

**Concrete release protocol (required amendment 1):** Use the existing launcher ledger, with `SlateWatch` configured `ledger=true` for both native and routed launches (`crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:8619–8630`). Call `slate_launch(command, evidence, Duration::from_secs(60), Some(&watch))` directly from the new contiguous test block. It supplies piped stdin in ledger mode (:8372–8384), drains stdout/stderr (:8387–8404), and processes complete `ledger pid=<decimal>\n` lines by recording birth identity and writing one `G` to stdin (:8683–8711). This is the existing fixture handshake (`experiments/fixtures/umbra-userspace-spawn.c:92–102`), with its announcement deliberately postponed. No new controller thread, launcher copy, stdin override or change to the ledger parser is needed.

| Endpoint | Owner and purpose |
|---|---|
| `ready[0]`, `ready[1]` | Kernel pipe created before fork: parent retains read end; child retains write end to send one `R`. Each closes its opposite end immediately after fork. |
| `go[0]`, `go[1]` | Second kernel pipe created before fork: child retains read end; parent main retains the sole write end. Child closes its write end; parent closes its read end. Only parent main may forward the external `G`. |
| Fixture fd 1 → launcher stdout reader | Parent alone writes newline-terminated readiness, poll result, helper-joined and ledger records using checked raw writes. Child and helper never announce a ledger entry. |
| Launcher `ChildStdin` → fixture fd 0 | The launcher owns the external acknowledgment writer. Parent main alone reads its one `G` after joining the helper. The child closes its inherited fd 0; the helper never reads it. |

The proposed fixture sequence is fixed:

1. Fork single-threaded with both pipes already created. Child sends `R`, closes `ready[1]`, then reads exactly one byte from `go[0]`. It may exit successfully only after receiving `G`; EOF, wrong token or read error is an explicit failed child status. Parent keeps `go[1]` open until release. Child stays in the run's process group/session; it never execs or detaches.
2. Parent reads `R`, closes `ready[0]`, and emits `c10-13 ready child=<pid>` to fd 1. It creates the helper and waits for its in-memory ready acknowledgment. Helper remains alive on a condition variable until the main thread releases it after the focal poll. No pipe read by a parent thread occurs while that helper is alive.
3. Parent initializes the status sentinel, performs exactly one focal `waitpid(child, &status, WNOHANG)`, immediately captures return/errno/status, and writes `c10-13 poll child=<pid> result=<r> errno=<e> status=<s>`. It then releases and joins the helper and emits `c10-13 helper-joined`. Only after these records does it write the sole parser-recognized `ledger pid=<child>\n` line. Retain even unexpected poll results before following the bounded cleanup path; they still fail the semantic assertion.
4. The launcher receives readiness and poll evidence through its collected stdout stream. Its parser does not interpret those semantic records or independently gate on their values: delaying the only ledger line establishes their ordering before its immediate acknowledgment. It identifies the still-unreaped child's PID/start time and sends `G`. Parent reads and records this acknowledgment, writes `G` to `go[1]`, closes that endpoint, and reaps the child while single-threaded. Child records consumption via its exact successful exit status. The harness validates the complete stdout order, child PID consistency, zero poll and unchanged sentinel, helper join before ledger, acknowledgment before forwarding, and later exact wait status. It must reject missing/reordered records and unrelated timeout evidence.

The child cannot normally exit before the poll: its only successful exit follows `go` consumption, whose sole writer is downstream of the external acknowledgment, whose sole trigger is downstream of the poll and helper join. An unexpected signal, EOF or child error invalidates that proof and fails qualification; readiness alone is not claimed to prove liveness. The managed-child selection is corroborated by the fixture's fork PID, closed-gate protocol and subsequent exact reap; the launcher birth record adds ownership evidence, not a substitute for ordering.

The external releaser is the existing launcher loop, outside the supervised tracee. After the poll there is no live parent helper when main reads the acknowledgment, forwards the token or blocks in reap. Thus release depends on the parent caller and external launcher, never a held parent sibling. A wrong blocking Poll cannot trigger a ledger acknowledgment and reaches the existing deadline instead of being released into a false pass. Before ledger announcement, both fixture processes remain in the run-owned group/session for existing cleanup; afterwards the birth ledger supplements that ownership. On malformed input, failed acknowledgment, premature child exit or deadline, retain the protocol prefix, let the reused launcher enforce its 60-second execution/10-second cleanup budgets, close remaining pipe endpoints as processes terminate, and require its direct-child reap, pipe EOF and owned-member cleanup evidence. There is no separate releaser process or thread to leave running. Native control must first demonstrate this complete protocol; routed setup or protocol failure is unqualified evidence, never permission to relax the assertions.

**Expected:** supported-and-correct; poll zero, unchanged sentinel, later exact child exit status and completed disposition. No output file is required; independently verify host and NFS roots remain unchanged.

**Falsifying observation:** Guard refusal, modified sentinel, nonzero poll while gate remains closed, accidental reap, missing readiness/release proof or timeout. Gate apparatus must qualify natively before routed measurement; inability to prove its ordering leaves the case unqualified.

### C10-16 — Concurrent Tokio files

**Re-check:** Original deferred until C10-15 was localized (original ideation :218, :246). Current fixture performs one sequential async roundtrip (`experiments/fixtures/slate-r10-tokio/src/main.rs:88`); C10-15's test selects it (`userspace_run.rs:9074`). Worker callbacks and readiness are already present (fixture :78). This extends C10-15 by concurrent independent tasks, distinct from the pthread callers in C10-05 (:9055).

**Decision: FIRST CUT.**

**Hypothesis:** Concurrent task dispatch preserves each independent write/read result.

**Pre-condition:** Preserve the fixture's exact pin and features. Use its acknowledged runtime worker lifecycle. Two fresh paths and payloads, bounded to two tasks; an in-memory start gate releases both. No runtime or worker-count inference about blocking-pool TIDs.

**Action:** Each task awaits write then read/compare for its own TASK-A or TASK-B; await and validate both JoinHandle and operation Results. Keep records attributable by task and operation, including observable poll TIDs; preserve all existing fixture modes.

**Expected:** supported-and-correct; TASK-A and TASK-B on their respective paths, each length six, both joins successful and completed disposition. Inherit C10-15's task-level attribution limit: no claimed per-syscall blocking-worker errno/TID.

**Falsifying observation:** Ignored task error, crossed payloads, missing join, premature completion, File teardown abort (including #227-shaped failure), lost attribution or timeout. No generic failure is accepted as a known defect without semantic evidence and a separately filed finding.

## Non-goals

No production edits, guard removal, scheduler redesign, mounted-NFS runs, dependency bumps, new skips or benchmarks. No universal schedule coverage or qualification of Codex process spawning, locks, watchers, SQLite or durability. No build or run in this ideation node.

## Intersections and implementation selection

FIRST CUT: C10-07, C10-08, C10-11, C10-13, C10-16. FIX-PHASE ITEM: C10-09 and C10-12. Use additive fixture modes and one contiguous test block at the end of `userspace_run.rs` to reuse its private helpers; no helper copy or refactor. Existing mode selection is explicit in `experiments/fixtures/umbra-userspace-threads.c:287` and Tokio fixture :71. Existing slate entry points are inventoried in `userspace_run.rs:9034`, :9488, :10255, :11294, :11924, :12819, :13373, :13548, :13764; none replaces the specific contracts above. Descendant-offset tests (`userspace_run.rs:10472`) remain separate from independent opens within one process.

| Blocker | Production site | Cases | Issue | Fix sketch |
|---|---|---|---|---|
| Held sibling pipe dependency lacks a qualifying window witness | crates/umbra-platform-macos/src/native.rs:600, :609 | C10-09 | ISSUE-TBD(pipe-return-window-witness) | Expose owner/window evidence and design safe dependent blocking-I/O progress without reopening the mediation gap. |
| Shared single-thread admission guard | crates/umbra-platform-macos/src/native.rs:644, :2190 | C10-12; positive C10-10 | #135 | Complete safe multithread admission and test child mediation/park semantics before removing guard. |

## Review and reporting discipline

These Expected entries are preregistrations, not readings. Preserve unexpected outcomes and all partial evidence; never widen accepted failures or retry terminal readings. Review must confirm the C10-13 gate and cleanup are independent of frozen parent siblings. Open questions: whether post-join debugger census immediately reflects exit; whether fd reuse is observed; and which actual task schedules occur. These are measurement questions, not assumed passing facts. The Tokio pin discrepancy is explicit and resolved for scope by retaining S2's historical pin; matching current Codex Tokio requires a separately authorized remeasurement.

Driver: archive these process documents and mirror evidence under S8/L51. The accepted ideation is committed only after ideation_review ACCEPT, byte-identically under S4. No commit is requested at this node.

ISSUE NEEDED: defects/pipe-return-window-witness.md

## Revision note — 2026-11-09

Applied exactly the two required amendments from `ideation-review.md` (AMEND): specified C10-13's pipe endpoints, ownership, readiness/poll evidence, delayed existing-ledger acknowledgment, release ordering and bounded cleanup; corrected its Poll selection citation to `crates/umbra-platform-macos/src/native.rs:211–214` (:213), retaining the ungated reply at :2181–2187. First-cut/fix-phase decisions and Expected statuses are unchanged. No build, test or production edit was performed. Prior proposal bytes are retained in `evidence/ideation-before-amend-2026-11-09.md`; driver archives this revision separately under L32/L51. Independent re-review is pending.

STATUS: COMPLETE
