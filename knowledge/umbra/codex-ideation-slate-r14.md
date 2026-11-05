# Rank 14 — Codex-configured SQLite WAL

Prepared 2026-11-05 for dg-t123jqvb. Ideation only; no builds or runtime readings.

## Intent and evidence boundary

Characterize family 14's SQLx/bundled-SQLite workload: open, read back WAL configuration, create a table, commit a sentinel, acquire a second connection, read, checkpoint, close and reopen within one supervised run. State lives in a disposable CODEX_HOME-shaped routing root outside the project subtree. A successful workspace write alone cannot qualify this workload. No agent or network API is involved.

## Measured source, not runtime results

Verified umbra base: c95da71f18bb89997c60a3004a326a208b1afc32. Verified read-only Codex checkout /Users/inva/Coding/umbra-worktrees/codex-src-44fe510: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61. All umbra citations below refer to the former; all Codex citations are relative to codex-rs at the latter.

Codex state/src/sqlite.rs:299-310 selects filename, create_if_missing(true), WAL, NORMAL synchronous, INCREMENTAL auto-vacuum, five-second busy timeout, disabled statement logging and a maximum of five connections. Its location is supplied through sqlite_home (state/src/sqlite.rs:124-141); the toy's CODEX_HOME-shaped location is a deliberate fixture choice, not a claim that every deployment uses the same directory. Cargo.toml:479-492 pins SQLx 0.9.0 with sqlite-bundled and runtime-tokio; Cargo.lock:9868-9875 pins libsqlite3-sys 0.37.0 and its checksum. Preserve the resolved dependency versions/checksums in the fixture lockfile, not merely the manifest constraints. Record sqlite_version(), compile_options and the selected VFS at measurement time; the SQLite engine version is not inferred from the Rust crate version.

Existing umbra descriptor cases explicitly characterize fsync, pread and pwrite (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:5927-6050). Their shared oracle requires tracee EBADF, nonzero supervisor outcome, RunCompleted, pre/post routing sentinels and pristine host bytes (:5671-5734). This establishes a diagnostic foundation, not SQLx's error translation or selected Darwin VFS calls. Rust sync_all uses a distinct fcntl path (:6177-6190). No SQLite/SQLx fixture or test was found by case-insensitive enumeration of experiments/fixtures and crates/umbra-storage-nfs-userspace/tests at the pin. Existing fixtures umbra-userspace-descriptor.c and umbra-userspace-descriptorstd.rs and the rank-10 Tokio fixture are dependencies to extend, not new rank-14 cases.

Current open issues were fetched with gh issue list --state open --limit 200 --json number,title,body on 2026-11-05; retained in open-issues.json beside this document. Refs #165 for descriptor refusal, #152 for shadow pathname stat, #158 for bare unlink, #135 for thread composition, #196 for replay exhaustion, and #108 for remaining durability boundaries. These issue reports do not determine the first failure of this unmeasured library composition. No dedicated SQLite issue was identified in that inventory. A newly demonstrated defect requires its own crate-prefixed issue after gh issue list --search, naming the case and semantic assertion; do not file a prediction as a defect.

## Shared expected pins and measurement contract

Every selected case gets one native host reading on fresh disposable data, followed by one enforced userspace-registry reading against live Ganesha. Require UMBRA_INTEGRATION_REQUIRED (existing enforcement: userspace_run.rs:173-178); a skipped or failed prerequisite gives no qualification. Never run mounted NFS. Future local measurements use the serial-test wrapper only while Runner.Worker is absent; check disk before any build and stop below 5 GiB. This ideation performs neither.

Pin OS/architecture, compiler, debugserver, source and fixture hashes, dependency lockfile, executable, SQLx/runtime settings, SQLite version/VFS/compile options, server/export configuration, routing roots and seeds. Start with max_connections(1), explicitly deviating only from Codex's pool limit. A current-thread Tokio executor is an experimental choice, not proof of a single-threaded SQLx process; record actual threads and attribute any thread refusal separately.

Use phase records before and after each awaited operation: connect; configuration readback; schema; begin; insert; commit; second acquisition; read; checkpoint; close; reopen; integrity. Record SQLx error variant, SQLite primary/extended code and message, and failed syscall/errno when observable. An unobserved syscall remains unknown. Do not fabricate IOERR_READ or a lock refusal from generic SQLx text. Report supervisor and tracee outcomes separately, journal terminal record, unresolved prepares, writer-lock disposition and cleanup. Provider stdout is not the sole oracle: use a qualified external record channel, phase-specific exits and independent NFS readback.

The pre-registration below deliberately predicts supported-and-correct as the semantic target. Confidence in reaching it on the current pin is low because of the cited primitive gaps. There is insufficient evidence to pre-register an exact SQLx refusal stage/code honestly. A differing first reading must be retained as measured, with downstream phases marked unreachable. It is not an accepted alternative to the target and must not make a positive test green. Implementation must preserve that first reading and document an exact measured refusal or file a semantic defect before landing characterization assertions. No broad failure set, retry-to-green, skip or production repair is authorized.

For each invocation impose a 60-second external deadline, a 45-second provider deadline, and 10-second cleanup grace. Capture all owned process identities; terminate only recorded owned descendants, reap children, close pool/observer handles and record survivors including zombies. Timeout, crash, missing census or incomplete reap is failure, never a named SQLite refusal. L26 instrument-no-reading, intervening-action progress and terminal failure remain distinct.

Snapshot host project and state roots before/after. Independently enumerate and read shadow database, -wal and -shm at phase barriers using the NFS client; inspect sizes and hashes and compare host snapshots. Capture sidecars while connections are held, as well as after close; their final absence is not proof they were never created. After successful close, export a coherent closed artifact set to disposable host storage for independent read-only row/integrity verification. Do not open a live copied database as if an arbitrary file copy were a coherent snapshot. SQL-level integrity must also run through the supervised connection. Readback is not crash durability or cross-machine WAL qualification.

The first-cut observation apparatus is a test-only extension, implemented in experiments/fixtures/umbra-slate-r14-sqlite/src/main.rs (with its own Cargo.toml/Cargo.lock) and one contiguous slate-r14 block in crates/umbra-storage-nfs-userspace/tests/userspace_run.rs. Choose that block so private harness helpers remain in place. The existing bounded launcher and owned-process census are reusable foundations (userspace_run.rs:8304-8448, :8273); they do not supply this new protocol. Reuse slate_watched with ledger-enabled ownership observation and retain its stdout/stderr/census/cleanup artifacts. The new test coordinator runs outside supervision on a joined host thread while that launcher watches the run. No production file or production manifest changes are needed.

Transport: a host TcpListener bound to 127.0.0.1 on an ephemeral port before launch; pass its address and a per-run nonce as fixture arguments. The fixture connects with a blocking TcpStream, read/write deadlines, and no extra observer thread inside the tracee. This deliberately avoids routed files, provider stdout and inherited-descriptor assumptions. Before SQLite initialization, the fixture sends HELLO with nonce, protocol version and PID, followed by a tagged PING; the host persists both and returns matching acknowledgments. Test-only qualification modes exercise normal handshake, a reported synthetic SQL-error record and abrupt disconnect after a persisted BEFORE record, in native and routed environments before any database reading. They prove transport/loss handling only, never SQLite support. A failure of this prerequisite invalidates the apparatus and cannot count as a database refusal.

Frames are length-prefixed UTF-8 JSON, capped at 64 KiB, carrying nonce, monotonically increasing sequence, phase and kind. Every BEFORE and AFTER record requires ACK of the identical nonce/sequence before advancing; only one frame is outstanding. Bound initial readiness to 5 seconds and each frame/ACK round trip to 3 seconds, always capped by the remaining 45-second run budget; these bounds do not extend the 60-second external deadline. The host appends and flushes the complete record to evidence before acknowledging. BEFORE precedes each awaited operation; AFTER contains either its result or the SQLx error variant/message and available SQLite codes. On SQLx failure the fixture sends ERROR, waits for its ACK, then attempts bounded pool close and sends terminal status. The host therefore retains the error even if subsequent cleanup stops. On supervisor stop or tracee death the host retains all previously received records and marks the outstanding operation interrupted; it does not invent a SQLx error that never arrived. EOF or timeout without a complete terminal exchange is not a clean library refusal.

At post-connect, post-commit and post-close AFTER barriers (or the ERROR barrier), the fixture issues no new database work until ACK. The host records database/-wal/-shm names, types, sizes and complete byte hashes through a fresh NFS client while the connection remains held where applicable. This is an observation barrier, not a claim to freeze internal library activity or make a live backup. Implement an r14-specific bounded reader/enumerator using the transport operations illustrated by userspace_run.rs:1186-1220; do not reuse that helper's Option result as an absence oracle, because :1198-1200 collapses lookup errors. Distinguish explicit NOENT from transport/permission errors, read all chunks, record pre/post attributes and mark an unstable live read as such. Cap the entire barrier inspection at 2 seconds (within the 3-second ACK window), passing the remaining budget to each NFS call. In native control, inspect the disposable host state directory at the same barriers. If an operation fails before a barrier, retain the last phase and perform separately labeled post-stop inspection after confirmed cleanup. A sidecar never observed is not asserted never to have existed.

The coordinator records wrong nonce/sequence, malformed/oversize/truncated frame, missing readiness/ACK, NFS observation failure and deadline exhaustion as APPARATUS_FAILURE with the last valid record. It closes the socket, stops issuing ACKs and lets the owned-process launcher enforce termination and the 10-second cleanup grace; the host observer thread is joined within that grace. An unexpected supervisor stop is retained separately as a run failure, with any incomplete phase evidence explicitly marked; neither category may pass a SQLite refusal assertion. Cleanup evidence includes recorded process identities, termination/reap outcomes, zombies/survivors, closed observer sockets, and final journal/writer disposition. All raw artifacts go under /tmp/graph-dg-t123jqvb/evidence/ for driver mirroring. Instrument qualification remains a required future check, but the transport, barriers and failure behavior are specified here rather than left as a design question.

The two first-cut cases use one common SQL sequence and schema: `CREATE TABLE IF NOT EXISTS slate_rows (value TEXT PRIMARY KEY NOT NULL)`. Both run that statement after option readback, then `SELECT value FROM slate_rows ORDER BY value COLLATE BINARY`, `BEGIN`, the same parameterized `INSERT INTO slate_rows(value) VALUES (?)` with value `r14-new`, `COMMIT`, the same ordered SELECT, supervised `PRAGMA integrity_check`, and close. Before insertion, C14-01 expects `[]` and C14-02 expects `["r14-seed"]`; after commit they expect exactly `["r14-new"]` and `["r14-new", "r14-seed"]`, respectively, with integrity_check returning exactly one `ok` row. Compare complete ordered row vectors, not counts or membership alone. The idempotent schema statement is identical in both cases; no SQL branch repairs or replaces existing data.

Prepare C14-02's seed natively using the same pinned executable/dependencies and database options, that exact schema, and a single committed `r14-seed` row. Await clean pool close, independently verify the seed's schema/ordered row/integrity, close the verifier, then retain the complete closed artifact set and hashes. Copy this identical set into separate fresh disposable native-control and routed-base directories. Do not copy an open database or drop a remaining sidecar; record configuration and all copied names. Seed generation/verification failure invalidates the prerequisite. C14-01 begins with an empty directory. Initial database state and its corresponding expected row vector are the only case differences; both are library compositions, not duplicate raw-syscall probes.

Each leaf changes one variable. The SQLx sequence is explicitly a composition, with its first failed phase linked to ranks 6/13; it does not independently qualify positioned I/O, locks, mappings, sync or deletion.

## Cases

### C14-01 — Fresh state database, one connection

**Hypothesis:** Codex's writable options produce a genuine WAL database with exact committed contents in the shadow.

**Pre-condition:** Empty disposable state directory outside project root; exact pinned dependencies/options except max_connections(1); phase and cleanup observers qualified.

**Action:** Connect; read journal_mode, synchronous, auto_vacuum and busy_timeout; execute the common idempotent schema/ordered-read/transaction sequence above; verify the exact row vector and integrity; close. Observe sidecars at barriers. No second connection, explicit checkpoint or reopen in this leaf.

**Expected:** supported-and-correct: native and routed configuration is wal/1/2/5000, exact ordered result `["r14-new"]` and integrity_check=ok, successful close, no host mutation in routed run, clean RunCompleted and released writer authority. These numeric values are fixture expectations to verify through native readback, not measured claims. Confidence is low; report an earlier precise failure without inventing its disposition.

**Falsifying observation:** Non-WAL mode, wrong options/rows, any failed phase, host effect, phantom success, unresolved journal state or cleanup failure. SQL success without independently observable database effects is insufficient.

### C14-02 — Existing state database, one connection

**Hypothesis:** Opening an existing database preserves committed content and adds exactly one committed row.

**Pre-condition:** Same apparatus as C14-01; native-seeded, cleanly closed WAL database prepared by the common seed protocol above, with exactly `r14-seed` in `slate_rows` and no outstanding transaction. Copy the complete verified closed artifact set into fresh native-control and base state roots; record seed hashes and configuration.

**Action:** Execute exactly C14-01's common SQL sequence against that seed: the idempotent schema statement, initial ordered result `["r14-seed"]`, insertion of `r14-new`, final ordered result `["r14-new", "r14-seed"]`, integrity check and close. Existing versus absent database is the sole changed input.

**Expected:** supported-and-correct: native and routed return exactly `["r14-new", "r14-seed"]` in that order and integrity_check=ok with WAL selected, original base bytes unchanged under routing, clean completion and cleanup. This is a target, not a prediction of a specific SQLite error.

**Falsifying observation:** Loss/duplication of seed row, false empty database, different failure disposition hidden by a generic error check, host mutation or incomplete cleanup.

### C14-03 — Second simultaneous pooled connection

**Hypothesis:** A second independently held connection observes the committed sentinel.

**Pre-condition:** C14-01 positively qualified; fresh run; same options except max_connections(2). Hold connection A so acquisition B cannot reuse it.

**Action:** Commit on A; acquire B and select sentinel while A stays held; close both.

**Expected:** supported-and-correct: exact row visibility and configuration on B, bounded acquisition, clean close/completion and pristine host roots.

**Falsifying observation:** Pool reuse presented as independent visibility, missing row, lock error, hang or sidecar escape.

### C14-04 — Explicit checkpoint

**Hypothesis:** A completed checkpoint preserves committed rows and reports its actual outcome honestly.

**Pre-condition:** C14-01 positively qualified; one connection, no competing reader, committed sentinel; capture WAL state before checkpoint.

**Action:** Issue PRAGMA wal_checkpoint(TRUNCATE), record all returned fields and sidecar transitions, then reread the row and integrity_check.

**Expected:** supported-and-correct: native and routed nonbusy completed checkpoint, exact row and integrity_check=ok; clean close/completion and unchanged host roots. Do not require a nonzero frame count when automatic checkpointing has already handled frames.

**Falsifying observation:** Busy/error claimed as completion, row loss, failed integrity, unaccounted sidecar effects or cleanup failure.

### C14-05 — Close and reopen within one run

**Hypothesis:** Committed state survives complete pool closure and recreation in the same routed session.

**Pre-condition:** C14-01 positively qualified; fresh one-connection run; no explicit checkpoint.

**Action:** Await pool.close(), create a fresh pool with the same options/path, read exact sentinel and integrity_check, close again.

**Expected:** supported-and-correct: wal mode, exact row, integrity_check=ok after reopen; clean completion and released authority, no host mutation.

**Falsifying observation:** In-memory pool reuse, missing row, inaccessible database, false clean completion or sidecar leakage.

### C14-06 — Codex five-connection limit

**Hypothesis:** The source pool limit preserves already qualified visibility semantics across distinct connections.

**Pre-condition:** C14-03 positively qualified; same disposable workload, max_connections(5) matching state/src/sqlite.rs:308.

**Action:** Hold five distinct acquisitions concurrently, perform ordered sentinel reads after one committed insertion, then release and close. No concurrent writer stress.

**Expected:** supported-and-correct: all five see the exact sentinel with bounded acquisition and clean lifecycle; host roots unchanged.

**Falsifying observation:** Reused acquisition masquerading as five connections, stale rows, thread/lock refusal, replay exhaustion or incomplete cleanup.

## Non-goals

No production changes, primitive reimplementations, mounted NFS, whole-agent smoke, crash/recovery campaign, cross-host contention, throughput claim or remote-durability certification. Native WAL success does not establish remote filesystem suitability. No files under crates/*/src are edited; no commits are made at this stage.

## Intersections and implementation selection

FIRST CUT: C14-01 and C14-02. Both can measure exact library-level outcomes today using test-only changes, even if their semantic target fails. They extend the descriptor refusal cases at userspace_run.rs:5982-6050 by adding actual SQLx translation, home-shaped routing and sidecar effects. They do not repeat raw pread/pwrite tests. The two seed conditions can expose distinct first failures without bypassing one through a production patch.

DEFERRED: C14-03 needs successful opening/commit before visibility is reachable; C14-04 needs a committed WAL before checkpoint semantics are measurable; C14-05 needs successful first close before reopen can distinguish persistence; C14-06 needs the two-connection boundary localized before changing to five. Until then these would merely repeat the first refusal. No production patch is part of this arc. Rank 13 owns isolated lock/mapping diagnostics; rank 12 owns isolated publication/deletion. Raw fsync/positioned-I/O repetitions are excluded because existing descriptor cases already cover them. Crash durability and cross-client WAL are deferred beyond this family cut because they require separate ownership/recovery and durability protocols.

Place fixture sources under experiments/fixtures/umbra-slate-r14-*. Prefer a new slate_r14.rs only if existing harness helpers can be reused without moving them; otherwise add one contiguous slate-r14 block at the end of userspace_run.rs. CI additions must enable both fixture build and required live userspace runs, with no skips or continue-on-error. The fixture/harness build-path gap is tracked by open issue #155.

## Review and reporting discipline

Six candidates: two FIRST CUT, four DEFERRED. Open questions: Which VFS syscall fails first for absent versus existing state? Does SQLx expose its primary and extended error without thread or metadata failure masking it? Does the observed failure complete the journal and release writer authority, or demand recovery? These require the first readings, not speculation.

The implementation report must distinguish observed status from the targets above and retain the first evidence. A known refusal gets an exact phase/code/disposition assertion; a demonstrated semantic defect gets its own issue and semantic evidence. Neither counts as compatibility. After review acceptance, this document must be committed byte-identical at knowledge/umbra/codex-ideation-slate-r14.md as the first commit, with the prescribed subject/trailer. Driver archives process documents and mirrors evidence per S8/L51.

## Revision note — 2026-11-05

Applied the two required amendments from ideation-review.md (AMEND). Amendment 1 specifies the loopback TCP transport, fixture/test file ownership, bounded readiness/ACK protocol, persisted error/interruption evidence, live sidecar observation and apparatus/cleanup failure oracles, with pinned reusable harness citations. Amendment 2 specifies the common idempotent schema and SQL sequence, seed construction and exact ordered before/after row vectors. Updated the corresponding first-cut actions/expectations and removed the unresolved apparatus-design question. Case selection, six-case count, deferrals and status targets are unchanged. No builds or runtime measurements were performed. Driver must archive this revision under a fresh versioned name per L32/L51, preserving the earlier pre-registration bytes.

## Revision note — 2026-11-05 (citation amendment)

Applied the sole required amendment from the revised ideation-review.md: corrected the lookup-error citation to userspace_run.rs:1198-1200. Verified directly with git show at umbra c95da71f18bb89997c60a3004a326a208b1afc32 that lookup is on line 1199 and `.ok()?` on line 1200. The apparatus design, schema/seed protocol, cases, expectations and selection are unchanged. No builds or runtime measurements were performed. Driver must preserve prior archived bytes and archive this revision under a fresh versioned name per L32/L51.

STATUS: COMPLETE
