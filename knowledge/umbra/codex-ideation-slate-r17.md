# Rank 17 — Native watcher failure and degraded behavior

Prepared 2026-11-06. Bucket C. Ideation only; no build or runtime reading.

## Intent and evidence boundary

Characterize family 17 in slate.md: separate notify construction from registration, mutate only the shadow through a supervised writer, isolate EVFILT_VNODE, and retain independent read/list usability. Registration alone must never certify remote notification support. No native remote notification implementation is proposed.

## Measured source, not runtime results

Umbra pin: `9a4d29bbdbe41940586d2570e9788651fffece78`, workspace `/Users/inva/Coding/umbra-worktrees/slate-r17`. Codex pin: `44fe510ce3ee61c8ef623adcbf89b901c73ddd61`, read-only checkout `/Users/inva/Coding/umbra-worktrees/codex-src-44fe510`. Both verified with git rev-parse. All Umbra references below are relative to the former; all Codex references are relative to the latter's codex-rs directory.

Umbra docs/design/syscall-matrix.md:81-85 declares notifications outside scope, says virtual EVFILT_VNODE registration is rejected, and requires explicit degradation. The capability entry at crates/umbra-storage-nfs-userspace/src/capability.rs:337-345 records a policy, not executable registration handling. The existing test at crates/umbra-storage-nfs-userspace/tests/operations_surface.rs:985-1006 checks that declaration. This does not establish the actual syscall error or supervisor disposition. We must measure them.

Codex Cargo.toml:410 and Cargo.lock:10442-10457 pin notify 8.2.0 (lock checksum 4d3d07927151ff8575b7087f245456e549fea62edf0ec4e565a5ee50c8402bc3). file-watcher/src/lib.rs:379-395 constructs recommended_watcher and starts an event loop. Its noop at :398-405 is inert, with no inner watcher. Registration at :599-607 checks path existence and warns on watch error. app-server/src/skills_watcher.rs:43-47 selects noop after constructor failure. Therefore constructor fallback does not prove registration failure is propagated to subscribers, and noop is not polling. Use the pinned notify API in a tiny fixture, not a whole agent; record the selected backend instead of inferring it from the dependency list.

Open issues were read using `gh issue list --state open --limit 200 --json number,title,body` on invakid404/umbra; retained in open-issues.json beside this document. No watcher/kevent/FSEvents match was found in that snapshot. Refs #165 for virtual descriptor refusals, #135 for thread boundaries, #152 for post-write metadata limitations, #217 for helper consolidation. These are adjacent risks, not evidence of a watcher result. Recheck issue dedup before filing any measured defect.

## Shared expected pins and measurement contract

Every FIRST CUT leaf gets one native host control on fresh disposable data, then one enforced userspace-registry run against live Ganesha, with UMBRA_INTEGRATION_REQUIRED enabled. No mounted NFS. Record executable and fixture hashes, OS/architecture, compiler/dependency lock, debugger, backend, routing roots, provider configuration, commands and timestamps. A skipped prerequisite is no evidence. Never rebuild or rerun to turn a measured failure green.

Use separate processes/runs for constructor-only and registration leaves. Record API phase, exact direct return, errno immediately, callback error versus event, supervisor termination and tracee status separately. Capture readiness before mutation; use explicit rendezvous, not a sleep for ordering. A callback's existence is insufficient: match nonce/path and mutation phase. No-event observations mean only no event within the registered bound, never proof of impossibility.

Pre-register 60 seconds external whole-run deadline, a 5-second event observation window after writer acknowledgment, and 10 seconds cleanup grace. For C17-02 and the admitted native C17-03 control only, native event control must receive the tagged change within that window. C17-01 has no event window or workload mutation: it requires construction/teardown evidence only. The C17-03 routed refusal arm requires unchanged workload effects and successful post-refusal read/list evidence; neither a shadow mutation nor a routed callback is required. Record monotonic start/end and callbacks, including unrelated events. Close watcher/queue and all descriptors, join threads, reap owned children, and retain before/after PID identity census and cleanup status. Kill only recorded owned processes. Incomplete observation is unmeasurable, not empty. Terminal failures are not retried.

Only C17-02 and the admitted branch of C17-03 write a nonce-bearing workload shadow sentinel through ordinary routed I/O. Evidence-file writes described below are fixed instrumentation in every leaf, separate from workload mutation and outside the watched subtree; C17-01 does not register or mutate a watched/workload path. Independently verify exact shadow bytes with the NFS client and compare the host routing-root bytes/names to its initial snapshot. The corresponding native mutation controls intentionally mutate their own disposable roots; C17-01 and the routed C17-03 refusal arm leave workload bytes/names unchanged. Do not use stat-on-shadow as the sole write oracle. Preserve stdout/stderr and provider evidence under evidence/. A refusal before mutation makes later phases unreachable; never claim those phases passed.

### Test-only evidence channel and rendezvous

Provider tracee stdout is unavailable in the pinned harness (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:3026-3027,3199-3202,3512-3516); slate_launch captures its launched process streams at :8355-8381, which does not prove callback capture. All three fixtures therefore use an explicit result-file protocol, not stdout as an oracle. The existing raw open/write loop at experiments/fixtures/umbra-userspace-threads.c:201-209 and routed listing-file oracle at userspace_run.rs:3512-3516 are the source precedent, not qualification of the new protocol.

Give each invocation a fresh nonce and a dedicated evidence directory, a sibling of the workload directory under the routed root, never inside the watched subtree. Native controls use the analogous disposable host directory. Main opens a fresh result file with O_CREAT|O_EXCL|O_WRONLY and emits versioned, length-delimited records with monotonically increasing sequence, nonce, PID, phase and monotonic timestamp using raw write loops handling EINTR and short writes. No buffered stdio, append flag, rename or fsync is required. The harness reads native evidence directly and routed evidence through the independent NFS client after termination; it does not need to poll tracee stdout or a live report to release the fixture. Include BEGIN, channel self-check, constructor result, registration result/receipt where applicable, READY, mutation completion where applicable, observation start/end, each callback/error, read/list results, teardown result, and terminal COMPLETE with final record count. Nonapplicable phases are marked NOT_APPLICABLE. The self-check and terminal framing must round-trip exactly before any semantic status is accepted. A missing COMPLETE, partial frame, sequence gap, lost callback/overflow or failed result-file close is incomplete evidence, never zero events. If supervision stops early, retain the partial records and supervisor diagnostic as a failure reading; do not synthesize terminal success.

For C17-01 only constructor/callback-error and teardown records apply; READY/mutation/observation phases are NOT_APPLICABLE. For C17-02 the callback records into a bounded mutex-protected in-memory queue with timestamps and sequence, without routed I/O from the callback thread. Main records registration success and READY, then performs the supervised workload write itself; its successful write/close and monotonic timestamp are the writer acknowledgment that starts the five-second window. Main drains records, records the closed observation interval, drops the watcher, and serializes the final evidence on the same raw-write channel. Callback errors, overflow and teardown failures invalidate the predicted no-event tuple. Preserve late callbacks separately from callbacks within the interval; zero matching events requires a complete interval and intact recorder. This same sequence in the native control establishes tagged event delivery. No external sleep establishes readiness.

For C17-03 main records the receipt before doing anything else. On refusal it records unchanged workload expectations, explicit read/list results and teardown, with mutation/observation NOT_APPLICABLE. On native admission the same single thread records READY, writes the tagged bytes through a separately opened ordinary writer descriptor for that same file, closes it, records writer acknowledgment, then collects the queued NOTE_WRITE event with a bounded kevent wait. Registration completion precedes mutation by program order; no child, fork, spawn or helper thread is introduced to supply the mutation. If routed admission unexpectedly occurs, use the same in-tracee writer sequence, preserving it as a falsifying reading. The receipt and event are distinct records. Workload snapshots exclude only the named instrumentation directory, whose files are separately enumerated and verified; evidence writes never satisfy the workload sentinel oracle. External cleanup checks all owned processes and retains the result artifacts even when terminal completion is absent.

Expected below is pre-registration, not a measured claim. Report a different actual status verbatim. A semantic defect requires a discriminating assertion, its own issue draft in defects/, and ISSUE-TBD(slug) until the driver files it. Unexpected errors cannot be folded into an accepted refusal set. Exact refusal predictions must be checked against native success and independent effects.

## Cases

### C17-01 — notify construction without a path (FIRST CUT)

**Hypothesis:** Constructing the recommended watcher is separable from registering remote paths.

**Pre-condition:** Tiny Rust fixture with notify exactly 8.2.0 and a retained lock; no watched path. Same runtime configuration for both legs.

**Action:** Construct with a callback that records errors, emit constructor outcome, drop and join/exit. No registration or workload file mutation in this leaf; only the shared fixed evidence-file instrumentation writes records.

**Expected:** supported-and-correct for construction only: native and routed constructor return Ok and clean teardown. This certifies no remote notification capability.

**Falsifying observation:** Constructor Err, supervisor termination, unexpected callback error, thread leak or deadline. Preserve the exact failure; it is not a registration refusal.

### C17-02 — notify registration and shadow mutation (FIRST CUT)

**Hypothesis:** A watcher may accept the unchanged host directory while missing a routed mutation; successful registration must not certify remote events.

**Pre-condition:** C17-01 constructor success, fresh existing directory, no mounts, callback recorder. Native control establishes tagged event delivery.

**Action:** Add one NonRecursive watch, record its direct result, then write a new nonce file through the supervised writer and observe for five seconds. Read/list the new file explicitly in the same run and verify it independently. These are fixed correctness oracles, not separate varied primitives.

**Expected:** known defect hypothesis: watch returns Ok but no matching callback within the bound despite verified shadow mutation, with host root unchanged. Disposition: record notifications as unavailable, draft a separate issue for silent registration success, and assert the exact semantic tuple (Ok, committed shadow bytes, unchanged host, no matching event), not merely exit success. Native must deliver its tagged event. The bounded result does not prove permanent event impossibility.

**Falsifying observation:** Explicit registration refusal, constructor failure, a matching remote event, or failure of read/list/write. Record these as different measurements; do not accept them as the predicted defect. If construction prevents reaching registration, report this leaf unqualified rather than a duplicate constructor finding.

### C17-03 — EVFILT_VNODE on the routed file descriptor (FIRST CUT)

**Hypothesis:** Native vnode registration exposes a precise refusal on a virtual remote descriptor.

**Pre-condition:** Single-thread C fixture, existing file, successful ordinary routed open/read, newly created kernel kqueue. Native control uses the same API sequence and observes NOTE_WRITE after a synchronized mutation.

**Action:** Submit one EV_ADD|EV_CLEAR EVFILT_VNODE NOTE_WRITE change using EV_RECEIPT, with room for its receipt. Record syscall return and receipt flags/data separately. On refusal, continue ordinary read/list and close; on admission, mutate and collect evidence within the event window.

**Expected:** exact known refusal hypothesis: kevent returns one EV_ERROR receipt with data EBADF for the virtual descriptor; tracee continues, ordinary reads/listing remain usable, supervisor exits normally and no host-root effects occur. Disposition: unsupported remote vnode notifications. This is a prediction to test, not a claim that the policy table implements it.

**Falsifying observation:** syscall-level -1 instead of receipt, another error, supervisor fatal refusal, registration success, host effects or broken ordinary operations. A different refusal needs its own exact measured disposition; do not broaden the assertion.

### C17-04 — Pinned Codex noop degradation (DEFERRED)

**Hypothesis:** An inert watcher permits explicit rescans but produces no automatic invalidation.

**Pre-condition:** Isolated notify leaves localized, pinned Codex library/runtime usable, observable subscriber registration and explicit rescan.

**Action:** Force constructor failure through test-only injection, exercise the actual caller's noop branch, mutate shadow, then explicitly rescan. Keep injection distinct from real backend failure.

**Expected:** supported-and-correct degraded contract: no automatic notification; explicit read/list discovers exact bytes and names; unavailable state is visible to the adapter. No polling claim.

**Falsifying observation:** Claimed polling, automatic freshness inferred from noop, missing explicit rescan result, or hidden unavailability.

### C17-05 — Required-watcher adapter preflight (DEFERRED)

**Hypothesis:** A required remote watcher without fallback is rejected before workload effects.

**Pre-condition:** A concrete adapter preflight implementation and user-visible required/optional policy.

**Action:** Request required remote notifications, retaining effect sentinels and exact diagnostic.

**Expected:** exact known refusal target: preflight reports remote notifications unavailable and launches no workload. Diagnostic spelling must be pinned when implementation exists; this is not an executable acceptance oracle today.

**Falsifying observation:** Launch, silent noop substitution, or host-path watcher advertised as remote coverage.

## Non-goals

No production edits, native notification implementation, polling implementation, mount, full Codex run, builds or measurements in this ideation node. No claim of remote watcher support from queue creation, registration success, or timeout alone.

## Intersections and implementation selection

FIRST CUT: C17-01, C17-02, C17-03. They need only fixtures and tests to obtain measurements; C17-02 remains explicitly prerequisite-dependent. Five candidates total, three selected and two deferred.

C17-03 extends the routed-descriptor read-versus-refusal contrast at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:5982-5996 with vnode registration. C17-01/02 extend the thread/lifecycle harness used by prior slate cases with a new notify backend boundary; reuse slate_launch at :8331 and, when needed, c13_launch at :11341. Neither repeats raw descriptor refusal, publication, locking, mapping or SQL cases. Searches of userspace_run.rs and experiments/fixtures found no existing native notification fixture. The capability declaration test at operations_surface.rs:985 is not a runtime duplicate. Reuse the harness in one contiguous slate-r17 block at the end of userspace_run.rs; its private helpers make a new standalone test file inappropriate without moving/copying helpers.

DEFERRED C17-04: full pinned caller composition introduces runtime, subscriber and rescan boundaries before the native API failures are isolated; forcing noop alone would only repeat its explicit inert constructor. DEFERRED C17-05: docs/design/syscall-matrix.md:84 is a requirement, not evidence of a concrete required-watcher preflight path; implementing policy first exceeds tests-only scope. Recursive watching, rename/delete storms and external-client event streams are deferred extensions, because the basic registration boundary is not yet qualified. Native service access through mounted NFS is excluded, not a later test here.

## Review and reporting discipline

Keep supported construction distinct from unsupported remote events. Native prerequisite failure invalidates comparison. A first-cut runtime blocker remains a recorded blocker, never a green skip. Review exact API receipts, callback attribution, effect evidence and teardown independently. Add no skip or continue-on-error entries. Only after ideation review accepts may these bytes become the first ideation commit; this node creates no commit.

Open questions: What exact outcome does notify construction have under supervision? Does registration bind the unchanged host path, fail in notify, or terminate supervision? Does vnode failure arrive as the predicted receipt or another boundary? Can explicit same-run read/list remain usable after each failure? Does the actual adapter expose unavailability to users beyond its warning? These are measurement questions, not reasons to presume support.

## Revision note — 2026-11-06

Applied exactly ideation-review.md required amendments 1-3: scoped event/mutation obligations to the applicable branches, specified the test-only raw-write evidence protocol and same-thread rendezvous (including terminal completeness and missing-record handling), and corrected the C17-05 preflight citation to syscall-matrix.md:84. Case selection and Expected predictions are unchanged. No builds or runtime readings were taken. The pre-amendment bytes are retained in ideation.pre-amend-2026-11-06.md; the driver must archive this revision separately under L32/L51.

STATUS: COMPLETE
