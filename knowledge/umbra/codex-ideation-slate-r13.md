# Rank 13 — File locks and mappings

Prepared 2026-11-05. Bucket C. Ideation only; no builds or experimental runs.

## Intent and evidence boundary

Characterize locks and mappings independently under owner-scope S0–S8 and lessons L1–L53. The rank-13 brief requires Rust File::try_lock, nonblocking flock, fcntl record locks, a shared file-backed WAL-index-shaped mapping, anonymous memory and kernel-pipe controls. No success on a virtual descriptor may be fabricated; an unsupported mapping must leave the host untouched. This is primitive qualification, not SQLite qualification or a whole Codex run.

## Measured source, not runtime results

Umbra pin: c95da71f18bb89997c60a3004a326a208b1afc32, verified as workspace HEAD. Codex pin: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61, verified at /Users/inva/Coding/umbra-worktrees/codex-src-44fe510. All Umbra citations below are relative to the workspace at that pin; all Codex citations are relative to codex-rs at its pin.

Codex history takes an advisory File::try_lock before persistence (message-history/src/lib.rs:120–181); rollout exclusion also takes File::try_lock (rollout/src/writer_lock.rs:51–84). These justify independent Rust and C locking probes, not a presumed particular Rust libc lowering. Codex configures SQLite WAL and NORMAL synchronization (state/src/sqlite.rs:297–311); this motivates a WAL-index-shaped shared mapping, but proves no direct Codex mmap call.

Umbra documents virtual descriptors as non-kernel objects and predicts EBADF for unserved descriptor operations including fcntl and mmap (crates/umbra-core/src/lib.rs:1035–1040). The complete TRACED_STUBS table has no flock, fcntl or mmap entry (crates/umbra-platform-macos/src/abi.rs:303–337). These facts motivate predictions, not runtime certification. File-backed mappings and flock are explicitly gated in the provider surface (crates/umbra-storage-nfs-userspace/src/capability.rs:347–367). Provider record-lock capabilities must not be confused with the tracee reaching them: transport.rs:483–484 in that same crate discusses a backend record-lock scope.

The open issue inventory was fetched with gh issue list --state open --limit 200 --json number,title,body and retained in issues-open.json beside this document. Refs #165: its body measures duplication, positioned I/O, truncation and sync, not record locking or mapping. Its EBADF diagnosis is a useful comparison, not evidence for a new case. Refs #161: append admission is an earlier boundary, so use read/write opens without append. Refs #135: avoid adding threads or blocking waits to these leaves. Issue inventory is a dated external snapshot, not a source-pin claim.

Existing descriptor coverage at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:5848–5873 and :6104–6115 concerns F_DUPFD and Rust cloning. C13-01 through 04 extend that boundary with new operations. Existing pipe composition at userspace_run.rs:10659–10675 belongs to C11-10; C13-06 supplies a paired descriptor-isolation control inside the new lock/mapping instrument, not another standalone spawn/echo case. Fixture and userspace_run.rs searches found no existing explicit flock, F_SETLK or mmap case; references to mmap in experiments/fixtures/umbra-userspace-rustio.rs:68 concern linked symbols, not an executed mapping assertion.

## Shared expected pins and measurement contract

Every FIRST CUT leaf gets exactly one disposable native-host reading followed by one enforced userspace-registry reading against live Ganesha. UMBRA_INTEGRATION_REQUIRED is mandatory; skip or missing prerequisites are no evidence. Never use mounted NFS. Record source hashes, fixture hash, OS/architecture, SDK, Rust/compiler version, debugger, executable/environment, provider settings, routing roots and server identity. Record the Rust lock's actual operation where observable; do not assume a lowering from its API name.

Use already seeded, page-sized files opened read/write without append, truncate or exclusive creation. Independently verify valid routed open and exact seed via supported read before the new operation. The native control uses separate disposable host data. Give each leaf a 30-second external deadline and 5-second cleanup grace, including provider teardown evidence. Record return values, immediate errno/raw_os_error, phase, PID, descriptor identities, supervisor status, tracee exit, journal outcome and cleanup. Reporting uses the named report object and independent readback contract below; stdout alone is insufficient.

For predicted refusals require the exact EBADF return, a continued tracee, deliberate characterization exit 9, normal supervisor completion and RunCompleted. Unexpected success exits a distinct failure code and fails characterization; other errno, supervisor termination, timeout or signal also fail. Do not accept a set of interchangeable failures. Rust must yield an OS error with raw_os_error EBADF, not WouldBlock. These are pre-registrations, not measured findings.

Read shadow bytes independently through the NFS client and snapshot host bytes/names before and after. Refusal must preserve the host and shadow probe seeds byte-for-byte. The only permitted routed file effects are the report and sentinel objects specified below; this allowance applies to every first-cut reading, including refusals with embedded C13-06. Positive controls use the same report/sentinel contract. No host routing-root path may change. Use no sleep-based ordering. Single-process nonblocking leaves avoid child ownership ambiguity; any external watchdog kills only recorded owned identities and reaps them. Capture descriptor close and unmap results, absence of live owned processes and completion of provider cleanup. Distinguish unavailable observation from empty census. No retry-to-green; report divergent measured status honestly and file each demonstrated semantic defect separately after issue-duplicate search.


**Report transport and effect allowlist (all first-cut readings).** Use a fresh disposable routing root for each reading. Its probe is `seed.txt`; the only additional logical paths the fixture may create are `c13-report.json` and `c13-sentinel.txt`, both absent initially. These names are relative to that root, never to the provider state or evidence directory. The fixture uses unbuffered open/write/close with checked short-write handling to publish the report and sentinel through routed I/O. The native control uses the same filenames in its separate disposable host root. The harness independently reads native files via host I/O and routed objects via `read_whole_object_through_client`; it never uses fixture stdout as the file oracle. This test-only pattern follows the named report readback and separately retained supervisor/tracee evidence at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:9919–10000, with writer-authority and pristine-host checks at :10001–10043, at the Umbra pin above.

The harness supplies `case` (C13-01 through C13-05), `mode` (`native` or `routed`) and a fresh 32-character lowercase hexadecimal `nonce`, retained before launch. C13-06 is represented inside its paired case's report, not by another file. The sentinel's exact bytes are UTF-8 `C13-SENTINEL <case> <mode> <nonce>\n`, with those three values substituted and one LF, no other bytes. Write it after the primitive, pipe checks where applicable, and probe cleanup. Its presence alone never certifies success.

`c13-report.json` is exactly one UTF-8 JSON object followed by one LF. Require the following keys, reject duplicate/unknown/missing keys, invalid types, trailing data and partial reports, and compare decoded values against an independently constructed expected record. JSON key order and insignificant whitespace within the object carry no semantic evidence. Required fields: `version` (integer 1), `case`, `mode`, `nonce`, `pid` (matching the externally recorded tracee), `phase` (string `complete`), `probe_fd` (integer for file probes, null for C13-05), `seed_read_ok` (boolean true for file probes, null for C13-05), `operation` (respectively `File::try_lock`, `flock`, `F_SETLK`, `mmap_shared`, `mmap_anon`), `result` (respectively `ok`/`os_error` for Rust, `0`/`-1` for C locks, `mapped`/`MAP_FAILED` for mappings), `error` (immediate numeric errno/raw_os_error on failure, null on success), `memory_check` (true for a successful native C13-04 or either C13-05, null for lock cases and refused C13-04), `unlock` (0 after successful native lock, otherwise null), `unmap` (0 after a successful mapping, otherwise null), `probe_close` (0 for file probes, null for C13-05), `pipe`, `sentinel_written` (true), and `intended_exit` (0 for native and C13-05, 9 for routed C13-01–04). The routed refusal values are the exact pre-registered EBADF dispositions, not any nonzero error. Record the raw mapping address separately in harness diagnostics if unexpected admission occurs; a report with any unexpected result fails qualification.

For C13-01–04, `pipe` is an object with exactly `read_fd`, `write_fd` (distinct nonnegative integers, distinct from the live probe descriptor), `before`, `after`, `eof`, `writer_close`, `reader_close`. Before/after are the actual received strings, required to equal `C13-PIPE <nonce> before\n` and `C13-PIPE <nonce> after\n`; EOF is boolean true and both close values are integer 0. C13-05 has `pipe: null`. The fixture closes the probe and pipe before producing the final sentinel/report; the pipe control must succeed even when the selected primitive refuses. Report and sentinel writes and closes must all succeed for the intended exit to occur. The final report cannot attest its own close: external exit/status and journal checks are mandatory. A report failure uses a distinct failing exit, never the expected 0 or 9. Supervisor completion, actual tracee exit, RunCompleted, writer authority, owned-process census and provider cleanup are captured by the harness outside the routing root and cross-checked independently, not trusted from `intended_exit`.

Before and after the reading, independently inspect all logical names and file bytes in the disposable root and compare against the allowlist. In routed runs `seed.txt` retains exactly its initial bytes, and the shadow has only that seed plus the two specified objects; the host root retains exactly its initial names and bytes, with neither report nor sentinel appearing there. In native runs the same report/sentinel additions are allowed; C13-04 alone also permits its declared tagged-word change to the native seed, with all other seed bytes unchanged. The anonymous case preserves the seed too. Provider state, journal and harness evidence are stored outside these roots and do not relax the routing-root assertion. Missing, malformed, stale-nonce or unexpected report/sentinel contents fail qualification even if the process exits as expected. No additional report paths, probe mutations or host effects are accepted as instrumentation exceptions.

## Cases

### C13-01 — Rust File::try_lock

**Hypothesis:** A valid virtual file yields an honest bounded lock refusal.

**Pre-condition:** Seeded read/write File, no append, no clone, single thread; pin Rust toolchain.

**Action:** Call File::try_lock once; on native success unlock and close; record structured error on failure.

**Expected:** Native acquires and releases. Routed: exact known refusal, OS EBADF, tracee exit 9 and RunCompleted as specified above; unchanged host root and probe seeds, with only the shared report/sentinel shadow additions.

**Falsifying observation:** WouldBlock on an uncontended file, fabricated success, a supervisor stop, changed seed or cleanup failure. This extends descriptorstd cloning coverage, not its API operation.

### C13-02 — Nonblocking flock

**Hypothesis:** The public C lock path cannot manufacture a lock on a virtual file.

**Pre-condition:** Same seeded descriptor control as C13-01, using C.

**Action:** flock(fd, LOCK_EX | LOCK_NB); native success is followed by LOCK_UN and close.

**Expected:** Native acquisition/release; routed exact known refusal -1/EBADF, exit 9 and RunCompleted, seeds unchanged.

**Falsifying observation:** Success, EWOULDBLOCK, different disposition or changed bytes. Shares the contract with C13-01 but independently tests the public C entry point; it is not a Rust-lowering assumption.

### C13-03 — fcntl record lock

**Hypothesis:** Record-lock backend scope does not imply a usable virtual-fd API.

**Pre-condition:** Valid seeded read/write descriptor; zeroed struct flock with F_WRLCK, SEEK_SET, start 0, length 1.

**Action:** One nonblocking F_SETLK request; native success is followed by F_UNLCK and close. Do not add F_GETLK or F_SETLKW.

**Expected:** Native acquisition/release; routed exact known refusal -1/EBADF, exit 9 and RunCompleted, unchanged seeds.

**Falsifying observation:** Success without measured contention semantics, wrong errno, stop or side effect. Extends #165's F_DUPFD boundary with a distinct command.

### C13-04 — Shared file-backed WAL-index-shaped page

**Hypothesis:** Unsupported shared mapping refuses before CPU stores can escape to the host.

**Pre-condition:** Page-sized seed, offset zero, valid routed read/write descriptor; capture native page size. No truncate, SQLite, msync or second opener in this leaf.

**Action:** mmap one page PROT_READ|PROT_WRITE, MAP_SHARED. Native writes a tagged index word, checks it, unmaps and verifies bytes through an independent read. On unexpected routed success, record the address and fail without treating admission as support; bounded cleanup unmaps it.

**Expected:** Native mapping and visible changed bytes; routed exact known refusal MAP_FAILED/EBADF, exit 9 and RunCompleted, no host or probe-seed change, with only the shared report/sentinel shadow additions.

**Falsifying observation:** Any admitted mapping, another error or termination, host mutation or loss of cleanup. Later admitted mapping requires its own write/visibility experiment; refusal proves no mapped-write fidelity.

### C13-05 — Anonymous memory passthrough

**Hypothesis:** Rejecting file mappings does not blanket-reject anonymous memory.

**Pre-condition:** No file operand; MAP_ANON|MAP_PRIVATE, fd -1, offset zero, one native page.

**Action:** Map read/write memory, write/read a tagged pattern, unmap; perform the shared routed sentinel control.

**Expected:** Supported-and-correct: both runs preserve the pattern, unmap successfully and exit 0; routed RunCompleted and exact shadow sentinel with unchanged host root.

**Falsifying observation:** MAP_FAILED, wrong bytes, failed unmap, missing sentinel or abnormal lifecycle. This explicitly executes the operation only mentioned as a linked symbol by rustio.

### C13-06 — Kernel pipe isolation control

**Hypothesis:** A virtual-fd refusal leaves ordinary kernel descriptor transport intact.

**Pre-condition:** Integrate this as a separately reported control phase paired with each refusal leaf, not a duplicate standalone spawn test. Own both pipe ends; use a tiny bounded payload.

**Action:** Before and after the selected single primitive, write/read exact tagged bytes through the same kernel pipe; close writer and observe EOF, then close reader. No spawn, threads, dup or virtual alias.

**Expected:** Supported-and-correct: exact bytes and EOF in native and routed runs, clean close and normal completion. The paired refusal retains exit 9; the control's independent result is success. Require the shared report fields for this control and the exact independent routed sentinel oracle; these two allowlisted shadow objects do not permit probe-seed or host mutation.

**Falsifying observation:** EBADF on the kernel pipe, altered bytes, absent EOF, leaked descriptor or unrelated routed write failure. Extends C11-10's transport observation with descriptor isolation across a refused operation.

### C13-07 — Positive lock contention and release

**Hypothesis:** If admission becomes available, two independent openers contend and release correctly.

**Pre-condition:** Positive lock admission; separately qualified opener lifecycle and rendezvous.

**Action:** Separate leaves per lock API: opener A acquires, B nonblocking-contends, A releases, B acquires. Add an external store client only in a separately scoped cross-client leaf.

**Expected:** Supported-and-correct target: contention then acquisition after release, no host mutation. Current first-cut prediction remains refusal, so this positive target is not a present support claim.

**Falsifying observation:** Both acquire incompatible locks, release fails, or a cross-client claim without an external client.

### C13-08 — Shared mapped-write visibility and persistence

**Hypothesis:** A future admitted MAP_SHARED mapping preserves writes and visibility.

**Pre-condition:** Positive C13-04 admission, mapped-write implementation and separately qualified synchronization.

**Action:** Separate leaves for writer/reader mapping visibility, msync and reopen persistence, with explicit handoffs and external store observation.

**Expected:** Supported-and-correct target: exact tagged bytes at each declared visibility/persistence boundary, no host mutation. Not a prediction of current support.

**Falsifying observation:** Discarded stores, divergent observers, fabricated sync success or host escape.

## Non-goals

No production changes, lock service, mapping implementation, SQLite database composition, network APIs, mounted NFS, builds or test execution in this pass. No commits before ideation review acceptance.

## Intersections and implementation selection

FIRST CUT: C13-01, C13-02, C13-03, C13-04, C13-05 and C13-06. The pipe control is embedded and reported independently in each refusal reading; it adds no redundant C11 spawn case. All can measure current status with test-only changes. One changed primitive per leaf; no composed lock-plus-map case.

DEFERRED: C13-07 needs positive lock admission before it can qualify contention instead of repeating first-cut refusal. Its external-client extension additionally needs a specified client/protocol and ownership model. C13-08 needs production mapped-write support before visibility and persistence can be qualified; another MAP_FAILED reading would duplicate C13-04. Full SQLx WAL belongs to rank 14, not an additional rank-13 case. History append/trim and rollout composition are deferred because they combine earlier admission/descriptor boundaries and obscure the isolated lock result.

Implementation uses new fixtures under experiments/fixtures/umbra-slate-r13-* and a new tests/slate_r13.rs only if helpers are reusable without moving them; otherwise one contiguous slate-r13 block at the end of userspace_run.rs. CI wiring is additive, SHA-pinned where applicable, without skip or continue-on-error. Local live readings require umbra-serial-test and an idle Runner.Worker; check disk before builds and use only this workspace target. None is executed here.

## Review and reporting discipline

Eight numbered candidates: six first-cut identifiers (including the embedded pipe control) and two deferred candidates. Expected values are provisional source-based pre-registrations. Acceptance does not certify them. Report actual supported-and-correct, exact known refusal, or semantic defect with its own issue; unavailable evidence is unqualified. Never broaden the accepted result to match a run. Future positive admission must replace negative characterization with semantic assertions.

Open questions for implementation: Rust's selected lock entry point at the pinned toolchain; whether mmap yields precisely EBADF on this host; whether harness helpers can be reused without moving shared code. Resolve these through source inspection and the planned single readings, preserving any mismatch rather than silently updating Expected.

## Revision note — 2026-11-05

Applied required amendment 1 from ideation-review.md: fixed named report transport, strict required fields and independent native/NFS-client oracles; allowlisted only report/sentinel shadow additions with exact expected contents; preserved immutable routed probe seeds and all host-root bytes/names; made missing or malformed reports fail qualification. Updated C13-01, C13-04 and C13-06 wording and removed reporting transport from open questions. Case selection, predicted primitive statuses and deferrals are unchanged. No builds or experimental runs performed. The driver must archive this pre-registration revision under a versioned name per L32/L51, preserving the original archive.

STATUS: COMPLETE
