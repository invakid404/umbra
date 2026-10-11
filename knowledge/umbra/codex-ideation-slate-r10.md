# Rank 10 — Threads without children, then fork with another thread alive

Prepared 2026-10-07. Bucket C. Ideation only; no native cases executed.

## Intent and evidence boundary

Localize thread attribution, return-window serialization, and descriptor sharing before composing runtime file I/O or process creation. These are illustrative starting points for implementation selection, not an exhaustive checklist or a requirement to implement every candidate.

Source pin: 213137db. The working checkout is detached at 03bbf65; source was read with git show, without switching it. The requested overlay/native.rs and overlay/events.rs paths do not exist at the pin. Correct references below are relative to that pinned tree.

The authoritative original slate was found at /Users/inva/.web-tty/project_knowledge/umbra/codex-ideation-slate.md, not in this checkout. Its 145 lines contain the original numbered families 1–17, including prior families 1–9 and the binding rank-10 paragraph; they do not contain the described 300–800-line shipped-rank expansions. Those expansions were not available at that location. This sibling follows its bucket/intent/invariant/failure/intersection conventions, adding explicit case contracts. Prior shipped outcomes must remain separately attributable; this document does not claim to have read unavailable expansions.

## Measured source, not runtime results

- crates/umbra-platform-macos/src/native.rs:2099 admits Namespace entries by inserting the trapping thread into the entry map. The inspected Fork arm at :2112 calls single_thread(); the Park arm at :2157 does likewise. Neither establishes universal startup refusal.
- native.rs:563–633 documents and implements continue_thread using vCont;c:<tid>. The return-window design holds siblings while the shared entry breakpoint is disarmed. continue_absorbed resumes the pending owner when a window exists, otherwise the whole process. Per-thread slots therefore do not imply simultaneous open return windows.
- native.rs:633 begins single_thread: it requests qfThreadInfo and refuses a non-m reply or a reply containing a comma with “fork/deferred wait requires one thread”. It does not iterate subsequent thread batches. The two-thread fixture must record evidence that both threads were alive at the guarded call; a census loophole is a defect, not support.
- native.rs:582–587 explicitly records the sibling-dependent pipe-read stall and watchdog tradeoff. This motivates a separate bounded-degradation case; it does not excuse timeouts in independent regular-file cases.
- crates/umbra-supervisor/src/events.rs:1414 track_process clones the parent's ProcessContext by value. This is process creation, not evidence that ordinary pthread creation clones descriptor offsets. Rank 9's shadow-OFD-model concern must not be imported as an expected pthread offset defect.
- Closed issue #135 was read through gh issue view, including its body and empty comments list. Its historical slices distinguish per-thread slots, sibling holding, guard removal, and fixtures. The current source retains guard removal as deferred. Issue closure is not proof that every proposed slice shipped. Its old single-slot and bare-c findings describe the earlier snapshot, not the current implementation.

## Shared expected pins and measurement contract

Every candidate below has two independent runs: an unsupervised native host control on disposable data, then an enforced userspace-registry run with live Ganesha. A mounted or local-rewrite pass is insufficient. Pin revision, OS/architecture, debugserver/toolchain, toy arguments, runtime versions, routing roots, server configuration and deadline.

Use unique thread-tagged payloads and paths. Seed base inputs externally. Inspect exact shadow bytes independently through the NFS client after completion or refusal; compare the actual host routing root against its initial bytes and names. The native control is allowed to mutate its own disposable root. The routed run must not mutate the host routing root. Do not confuse these two meanings of “host”.

Record supervisor and tracee statuses separately, call return/errno immediately on the originating thread, phase reached, task/TID attribution where observable, bytes/lengths, journal disposition, writer authority, and cleanup. Provider stdout is not assumed captured. Use independently qualified records or phase exit codes; missing records after a fatal stop are not evidence that no effect occurred. A record writer must not be the sole oracle for the routed writer it is checking.

Use pthread mutex/condition-variable handshakes or another qualified in-memory rendezvous; do not assume pthread_barrier_t is available on the target. No sleeps establish ordering. A release barrier establishes eligibility to race, not simultaneous execution inside the tracer. Record partial syscall results and complete them correctly; do not mistake legal short transfers for lost data.

Assign a fixed external deadline with cleanup grace before running. Unexpected SIGTRAP/SIGSYS, panic, lost reply, cross-thread bytes or unbounded cleanup are failures. An ordinary thread case may become a named known-refusal characterization only after the exact stage, disposition and effect boundary are measured. “Any failure” is never a pass. Keep intended compatibility assertions separate from current defects and exact bounded refusals.

All thread-positive invariants below are hypotheses to qualify, not claims of shipped support. Native prerequisite failures invalidate the corresponding comparison. Missing permissions, features or Ganesha fail the required qualification job rather than silently qualifying a skipped case.

## Cases

### C10-01 — Single-thread routed control

**Hypothesis:** The basic primitive sequence works without threading.

**Pre-condition:** Seed input ABCDEF; no child or helper.

**Action:** Open/read/close input; create output and write MAIN; close and independently inspect.

**Expected host and routed result:** Host control reads ABCDEF and stores MAIN. Routed run does likewise in shadow, preserving host inputs and output absence.

**Falsifying measurement:** Wrong bytes, refusal, or timeout invalidates the primitive foundation; do not attribute later failures to threads.

### C10-02 — Main I/O while helper stays alive

**Hypothesis:** Merely having a second live thread does not invoke the fork/wait guard.

**Pre-condition:** Helper acknowledges readiness and waits on an in-memory condition until main finishes.

**Action:** Main performs the C10-01 sequence while helper remains alive, then releases and joins it.

**Expected host and routed result:** Native succeeds. Routed target is exact MAIN and normal completion; any refusal needs its actual operation and diagnostic, not a presumed startup gate.

**Falsifying measurement:** A fork/deferred-wait diagnostic without a fork or parked wait, missing bytes, or a timeout falsifies the expected thread-only boundary.

### C10-03 — Helper owns all routed I/O

**Hypothesis:** Return values and routing follow the trapping helper.

**Pre-condition:** Main stays alive waiting for completion; helper has its own buffers and result fields.

**Action:** Helper opens, reads, creates, writes HELPER and closes. Main joins and validates its result fields.

**Expected host and routed result:** Host control and supported routed run produce HELPER with exact input readback.

**Falsifying measurement:** Main receiving the helper reply, stale return/errno, absent output, or unexplained stall falsifies attribution.

### C10-04 — Alternating independent files

**Hypothesis:** Changing the active caller does not reuse the previous thread transaction.

**Pre-condition:** Two live threads; distinct paths and buffers; bounded deterministic rounds.

**Action:** Main writes M00, releases helper to write H00; repeat with uniquely numbered records in independently opened files.

**Expected host and routed result:** Both runs produce every record in its own file; routed host root is unchanged.

**Falsifying measurement:** Missing, duplicated or swapped records, wrong fd results or transaction-state errors falsify the hypothesis.

### C10-05 — Barrier-released independent opens and writes

**Hypothesis:** Competing callers remain correctly attributed across shared breakpoint windows.

**Pre-condition:** Each thread owns a different file and payload; no shared fd or cross-thread I/O dependency.

**Action:** Release both at a rendezvous before open and before write; repeat a modest fixed number of rounds with fresh paths.

**Expected host and routed result:** Native and supported routed run produce both complete payload sets; ordering between threads is unconstrained.

**Falsifying measurement:** Any crossed bytes/replies, base mutation, phantom success or timeout falsifies correctness. A passing schedule is bounded evidence, not proof of all interleavings.

### C10-06 — Same descriptor, ordered handoff

**Hypothesis:** Threads of one process share one logical descriptor offset.

**Pre-condition:** Main opens a fresh output once; helper receives that fd through a synchronized handoff.

**Action:** Main writes AA, helper writes BBB, main writes C, then close after both complete.

**Expected host and routed result:** Host is AABBBC. Routed compatibility target is AABBBC; copied process offsets from rank 9 are not an acceptable pthread oracle.

**Falsifying measurement:** Overwrite, isolated per-thread offsets, EBADF in helper or length other than six falsifies the target.

### C10-07 — Independent opens of the same object

**Hypothesis:** Per-open offsets remain independent while file bytes are shared.

**Pre-condition:** Seed six dots; each thread independently opens the same file without truncation; no dup or seek.

**Action:** Main writes AA at its initial offset; helper then writes B at its independent initial offset; close both.

**Expected host and routed result:** Both controls should end BA....; routed result must be independently read back.

**Falsifying measurement:** AAB..., an offset carried from the other open, or lost update falsifies independence. This is distinct from C10-06.

### C10-08 — Cross-thread close and descriptor reuse

**Hypothesis:** Descriptor lifetime is process-wide and stale thread state does not survive reuse.

**Pre-condition:** Open fd in main and hand it to helper; all operations strictly ordered.

**Action:** Helper closes it; main attempts read on the now-closed fd before opening a fresh file. Then perform a known-good fresh write.

**Expected host and routed result:** Native closed-fd read returns EBADF. Routed target matches and fresh write succeeds; capture actual fd numbers rather than requiring reuse.

**Falsifying measurement:** Old contents returned, success on the closed fd, or corruption of the newly opened object falsifies the target. If reuse does not occur, report that branch unexercised.

### C10-09 — Sibling-dependent pipe read

**Hypothesis:** The documented held-sibling limitation has a bounded, diagnosed outcome.

**Pre-condition:** A kernel pipe is created before the measured phase. Reader announces entry phase; writer is released to write only after that announcement. No child.

**Action:** One thread issues blocking read while the sibling must supply its byte. Distinguish schedules where the byte arrives before the read blocks; retain only a demonstrated dependency as limitation evidence.

**Expected host and routed result:** Native completes when writer runs. Routed source predicts possible watchdog timeout if the return window freezes the necessary sibling; exact classification requires observing that window.

**Falsifying measurement:** Silent success with missing data, indefinite hang or surviving tracee falsifies containment. An ordinary successful schedule does not refute the documented reachable stall.

### C10-10 — Fork while helper is alive

**Hypothesis:** The explicit guard rejects multithreaded fork before child effects.

**Pre-condition:** Helper acknowledges readiness and remains alive; parent writes a completed pre-fork marker first.

**Action:** Main calls fork. Child branch, if unexpectedly reached, only uses prearranged async-signal-safe operations to write a unique sentinel and _exit.

**Expected host and routed result:** Host control forks, child writes sentinel and parent reaps. Routed source expectation is a structured unsupported stop naming fork/deferred wait requires one thread; pre-marker persists, child sentinel is absent, cleanup completes.

**Falsifying measurement:** Child marker, successful unmediated child, wrong diagnostic, leaked process or hang falsifies the guard expectation. Do not demand a tracee errno when the supervisor stops the run.

### C10-11 — Fork after helper joins

**Hypothesis:** The guard concerns current thread census rather than historical thread creation.

**Pre-condition:** Run the same helper lifecycle, but join it before fork.

**Action:** Fork a minimal child writing its own distinct file, then reap; use separate descriptors to avoid rank-9 inherited-offset composition.

**Expected host and routed result:** Native succeeds. Routed target follows qualified single-thread fork behavior with child mediation and exact bytes.

**Falsifying measurement:** Refusal based solely on past threading, missed child effects or cleanup failure falsifies the lifecycle hypothesis. Record actual census if the debugger still reports an exiting thread.

### C10-12 — Deferred wait with helper alive

**Hypothesis:** Park refusal is independently reachable without attempting multithreaded fork.

**Pre-condition:** Fork while parent is single-threaded. Child remains alive using a prearranged non-file synchronization protocol; only then create parent helper.

**Action:** Parent calls blocking waitpid for the unreaped child while helper remains alive. Arrange child release without depending on a frozen parent sibling; externally bound every process.

**Expected host and routed result:** Native eventually reaps. Routed source predicts the same one-thread refusal if WaitPlan::Park is actually selected.

**Falsifying measurement:** Claiming a Park test when child was already done, a leaked child, or an unexplained wait hang falsifies the characterization. This setup needs proof of live-child state at selection.

### C10-13 — Polling wait with helper alive

**Hypothesis:** A nonblocking wait does not inherit Park admission rules.

**Pre-condition:** Use C10-12 setup with a demonstrably live child and preserved status sentinel.

**Action:** Call waitpid with WNOHANG, then terminate/reap through a qualified cleanup route.

**Expected host and routed result:** Native returns zero without changing status for a running child. Routed Poll arm is ungated and should preserve that behavior.

**Falsifying measurement:** One-thread refusal on verified Poll, changed status or accidental reap falsifies branch separation. Cleanup must not silently turn into a second unqualified blocking-wait test.

### C10-14 — Tokio runtime with synchronous file control

**Hypothesis:** Runtime worker presence alone does not change the qualified synchronous file result.

**Pre-condition:** Pin Tokio version/features; build a two-worker runtime on a spawned thread as in E7; keep original thread alive. No child launch.

**Action:** Perform the qualified synchronous small-file sequence on the runtime-owning thread while workers remain alive.

**Expected host and routed result:** Native produces RUNTIME; routed target matches C10-02.

**Falsifying measurement:** Failure before file phase identifies startup dependency; failure in the qualified primitive identifies runtime/thread interaction. Do not call either async-file support.

### C10-15 — Tokio async file round trip

**Hypothesis:** Async file dispatch preserves exact bytes across its actual worker transitions.

**Pre-condition:** C10-14 and pthread attribution cases localized; pin actual Tokio dependency and instrument phase/task versus OS-thread identity where feasible.

**Action:** Await tokio::fs::write of a unique file, then await read and compare; independent external readback verifies completion.

**Expected host and routed result:** Native and supported routed run return exact ASYNC bytes. Read-only and write-only modes may isolate the first failure with external seeding.

**Falsifying measurement:** A task completing before correct stored bytes, blocking-pool attribution loss or timeout falsifies the target. Do not assume async tasks equal OS threads or that runtime worker count bounds blocking threads.

### C10-16 — Concurrent Tokio files

**Hypothesis:** The async composition preserves independent file operations under concurrent scheduling.

**Pre-condition:** C10-15 localized; two unique paths and payloads, bounded task count.

**Action:** Release two tasks together, each await write/read/compare on its own file; await both results explicitly.

**Expected host and routed result:** Native and supported routed run produce exact TASK-A and TASK-B.

**Falsifying measurement:** Ignored task error, swapped files, missing bytes or incomplete join falsifies the target; link the first failing primitive back to pthread leaves before expanding runtime scope.

## Non-goals

No production guard removal, thread scheduler redesign, shared-OFD implementation, graph authoring or test implementation. No universal multithread startup rejection. No real coding-agent smoke, throughput benchmark, arbitrary stress duration, signal storm, debugger pagination repair or proof of every possible schedule.

Do not add spawn actions, PTYs or pipelines here: rank 11 owns that composition. Do not turn the thread slate into a parallel recursive-search qualification; rank 8's discovery and metadata prerequisites remain separate. Tokio filesystem results do not qualify process spawning, locks, SQLite, watchers or durability.

## Intersections and implementation selection

Rank 1 supplies exact small-file bytes and wrapper baselines. Rank 3 requires honest buffered completion; raw pthread calls keep buffering out of initial attribution. Rank 4 contributes immediate errno capture and independent liveness after recoverable errors. Ranks 5–7 own append and descriptor/no-follow admission; keep those primitives out of thread localization. Rank 8's parallel walk is a later consumer of established thread support, not an initial leaf.

Rank 9 establishes process-context cloning and its shadow-OFD limitation. C10-06 deliberately asks a different question: sharing within one process. C10-10 keeps inherited descriptors out so a refusal is not conflated with copied offsets. C10-11 reconnects to the already qualified single-thread process boundary.

Issue #135 slices 1–2 motivate C10-03 through C10-05; the current code already has those mechanisms. Slices 3–4 motivate the fork/park boundary and future admission tests, but the two guard sites remain. Its historical mt-spawn orphan and writer-lease failures motivate cleanup evidence, not a new spawn case here.

E7 motivates runtime creation on a spawned thread and the distinction between worker and blocking pools. E1 motivates async write/read composition. Those dependency statements are inherited from the authoritative slate's pinned Codex source measurements, not a fresh claim about current upstream Codex or Tokio internals.

Suggested first implementation cut: C10-01 through C10-06, C10-10, then C10-14/C10-15. Add descriptor lifetime and wait-branch leaves when their setup can expose the intended boundary without introducing a second unknown. C10-09 merits an explicit limitation pin only with adequate evidence that a return-window dependency was reached; timing luck cannot qualify it.

## Review and reporting discipline

For each selected leaf report supported-and-correct, exact known refusal, demonstrated semantic defect, or not qualified. Keep a separate blocker inventory even when refusal characterization is green. Do not bless a new failure by broadening an accepted-outcome set automatically.

A future guard-removal implementation changes C10-10/C10-12 admission expectations only after explicit review. Positive descendant mediation then needs independent assertions; simply deleting a refusal assertion is insufficient. Exact host preservation, bytes, lifecycle and cleanup remain required.

This document contains 16 candidate cases. The source surprises are the corrected module paths, already shipped per-thread slots and sibling holding, the intentionally bounded pipe dependency stall, and retained first-batch thread census. No execution measurements or compatibility qualification are claimed here.
