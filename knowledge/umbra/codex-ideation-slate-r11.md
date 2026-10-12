# Rank 11 — macOS spawn actions, pipes, then PTYs

Prepared 2026-10-12. Bucket C. Ideation only; no native cases executed.

## Intent and evidence boundary

Localize process admission, descriptor policy and session behavior before composing pipe launch or terminal interaction. These candidates are illustrative starting points for implementation selection, not an exhaustive checklist. The binding authoritative paragraph follows verbatim:

**11. macOS spawn actions and terminal resources.** Build a C launcher with separate modes for posix_spawn with null attributes (control), addchdir, adddup2, explicit descriptor inheritance/CLOEXEC policy, and session/signal attributes. Then reproduce E8's full pipe launch using Codex's small pty utility crate or the same API sequence, with a child that echoes through pipes and writes a unique shadow file. PTY allocation and interactive echo are a later independent mode, including E9's preserved-descriptor path. **Invariant:** each action either works exactly or is explicitly refused before the child performs filesystem effects; a successful launch preserves only intended descriptors, reaches the requested logical cwd, transports bytes/EOF, and reaps on cancellation. **Failure class:** the existing `spawn requires null attributes/file actions` guard, kernel-versus-virtual descriptor confusion, leaked pipe ends, blocked waits, process-group cleanup errors. #117's fork work alone cannot qualify this shape. Add a transient-SIGCHLD case for [#118](https://github.com/invakid404/umbra/issues/118), synchronized around a routed call rather than sleep-based luck.

The rank-10 sibling was read in full. Its case contract is extended below for parent/child ownership, executable replacement, descriptors, signal provenance and external cleanup. No compatibility result is claimed by source inspection.

## Measured source, not runtime results

Umbra HEAD was verified with git rev-parse HEAD as 73a191da57d3403a786df36bfc1f0efbf429aa28 before citation. All umbra path:line references below are at that pin. Codex was fetched into fresh /tmp/umbra-r11-ideation/codex at exactly 44fe510ce3ee61c8ef623adcbf89b901c73ddd61; Codex paths below are relative to its codex-rs directory. No current-upstream drift is used.

Admission is a kernel-stub boundary, not a check of the public C argument list. crates/umbra-platform-macos/src/abi.rs:308 maps __execve (59) and :309 maps __posix_spawn (244) to Delivery::Exec. Only number 244 reaches the null-action guard in crates/umbra-platform-macos/src/native.rs:2178. It tests register x2 for nonzero, the Darwin packed spawn descriptor; it does not separately decode attributes or file-action contents. Initialized but empty objects therefore need their own measurement. posix_spawnp has no separate table row: it reaches this boundary only insofar as libc resolves and calls the trapped spawn stub. Codex's pinned native path actually searches the child's PATH itself and calls posix_spawn (utils/pty/src/posix_child.rs:235). No vfork row exists in the table; do not claim it reaches this guard or is qualified by it. Fork has its separate Delivery::Fork arm. Execve enters Exec but bypasses the number-244 condition. No other listed trapped syscall reaches this specific refusal.

The guard is after absolute-path validation and executable cache/resigning work (native.rs:2164), but before registers are installed and the spawn syscall is resumed (:2191). Thus the hypothesis is no child creation or child filesystem effects from this refused call, not zero supervisor/cache effects. An invalid or relative executable can fail earlier. unsupported propagates a platform error and stops supervision; it is not an injected tracee ENOTSUP. Record the supervisor diagnostic and tracee termination separately; a tracee return may never occur. A public posix_spawn failure, when it returns, is an error-number return value, not necessarily errno.

For null descriptor admission, native.rs:2446 constructs a replacement packed descriptor, and :2453 builds attributes with flag 0x80 (start suspended). On successful return, :2040 reads the PID pointer and calls attach_child. At :1939 that function attaches a fresh session, retargets the interposer to the child's image, installs interception, and queues Child then ThreadStarted before normal execution. crates/umbra-supervisor/src/events.rs:219 tracks the child and parent-child relation; :1431 clones the parent's ProcessContext by value. The separate Exec event at :231 removes close-on-exec logical descriptors and clears mappings. Spawn's Child event is not itself that Exec event: correct CLOEXEC treatment in an inherited virtual descriptor table must be measured, not inferred from the Exec arm. The copied descriptor offsets also reopen rank 9's #172 question.

E8's pipe.rs:149–182 sets NewSession, Explicit descriptors, cwd, ReapOnly and piped/null stdin before command.spawn. posix_child.rs:105 reserves the reaper; child_reaper.rs:26–48 starts a shared OS thread before child creation. posix_child.rs:136 subscribes to SIGCHLD, then creates pipes and Tokio I/O wrappers; :174–240 adds cwd, dup2, explicit inheritance, SETSID, CLOEXEC_DEFAULT and SIGPIPE default restoration. pipe.rs:217 onward schedules Tokio writer/read/wait tasks after launch. This path requires a caller-supplied Tokio context; it does not itself build a multithread runtime. Even a current-thread runtime does not remove the dedicated reaper thread.

Correction to the brief: those threads do NOT imply the spawn call hits single_thread(). Fork (:2112) and Park (:2157) are gated; Exec is not. #135 is OPEN and its own correction explicitly distinguishes Node/Tokio posix_spawn from gated fork. Thread attribution, fallback fork and later parked wait can still confound a crate reproduction. Recommend the same native API sequence in single-thread C first, recording the trapped stub and packed descriptor; it qualifies that sequence only. A later pinned-crate run qualifies library runtime/reaper/fallback composition only when its actual branch is captured. Neither qualifies a whole Codex agent. Rank-10's historical statement that #135 was closed is not the current issue state.

E9 core/src/spawn.rs:70–136 rebuilds the environment, retains the macOS escalation socket through pre_exec descriptor cleanup, optionally detaches from the TTY, and uses null stdin/piped output for shell tools. This is distinct from the PTY preserved-fd path: utils/pty/src/pty.rs:147 selects it when inherited_fds is nonempty; :388–412 uses StdCommand, cloned slave stdio and pre_exec terminal setup, fd cleanup and inheritable flags. :493 calls openpty, :526 clears CLOEXEC for preserved descriptors, and :645 calls setsid and TIOCSCTTY. The portable path at :193 delegates openpty and child launch to portable-pty; this source alone does not establish its underlying Darwin syscall sequence. posix_openpt is an alternative API candidate, not a measured E9 call.

Issue evidence was read with gh issue view for #117, #118, #135, #165 and #186. #117 is CLOSED: its body describes the original multiprocess coverage gap, not a final implementation inventory. #135 identifies #129 as its closure. The pinned userspace tests include mediated grandchild coverage (crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:2460), and existing fork/exec/cwd coverage; closure does not establish spawn-action or PTY support. #118 remains OPEN for a live transient-signal return-gate test. #165 remains OPEN and reports bounded EBADF for dup/F_DUPFD and related routed descriptor operations; it explicitly excludes dup2 from measured coverage. Do not convert that into a proven adddup2 result.

The closing PR #129 was also read to resolve #117's shipped scope: it routed chdir(12), retargeted the interposer across exec and attach_child (including spawn), and adopted a new executable image only after successful exec. It shipped forkexec, chdirchild, chdirshapes, grandchild and rollbackchild cases plus the failed-exec-then-fork regression. Fork itself already worked; nonnull spawn actions, vfork and process-group waits were deferred. These historical shipped claims come from that closing PR, not from the original issue body alone.

Following the Codex callers further, utils/pty/src/child_command.rs:271–296 selects NativeChild for Explicit/NewSession, as E8 requests; compatible fallback can install pre_exec work. pty.rs:541 implements best-effort macOS descriptor cleanup with proc_pidinfo and fcntl/close, so its name is not proof that every decoy closes. configure_child_terminal at :645 also resets SIGCHLD/SIGHUP and other dispositions and clears the mask before setsid/TIOCSCTTY. process_group.rs supplies group signaling and a macOS denied-signal member fallback; group signaling alone is not ownership of descendants that create another session. Preserve these distinctions in C11-11/15/16.

#186 is OPEN although its filed body still says DRAFT ONLY. The retained diagnostic /Users/inva/.web-tty/project_knowledge/umbra/graph-audits/dg-r0za7tb3-sighup-diagnostic.md reports C10-10 supervisor SIGHUP on 2026-10-09 UTC, with no sender attribution. Eight sibling cases completed cleanly. Stable PPID/PGID do not reconstruct session-level orphan status. The workflow comment's 2026-10-10 investigation date is not a new runtime event. Its provisional issue reference is stale now that #186 exists.

## Shared expected pins and measurement contract

Every selected case runs twice: unsupervised native host control on disposable data, then enforced userspace-registry against live Ganesha. A mounted or local-rewrite pass is insufficient. Pin source, OS/architecture, libc/toolchain, debugserver, runtime configuration, exact executable/environment, routing roots, server configuration and deadlines. Native prerequisite failure invalidates the comparison.

Record supervisor and tracee status separately, and parent and every child's status separately. Capture errno immediately and also capture direct error returns from spawn/action APIs. Record phase, PID/PPID, process group, SID/session leader, controlling TTY, foreground process group, intended and leaked child descriptors, fd flags, bytes/EOF, journal disposition, writer authority and cleanup/reap. Never infer no child from absence of a tracee PID report alone: inspect external process evidence and child sentinels. Record failed observation operations explicitly.

Independently inspect exact shadow bytes through the NFS client and compare host routing-root bytes/names with the initial snapshot. Native disposable host effects are expected; routed host-root mutation is forbidden. Reporting channels must be qualified separately and cannot be the only oracle for the routed writer. Provider stdout is not presumed captured. Handle short transfers and EINTR without losing evidence.

Use qualified pipe rendezvous, or mutex/condvar in-process. No sleeps establish ordering. A readiness acknowledgment before a routed call does not prove signal delivery inside its return window. For #118 require observed window entry and an externally coordinated child release; missing window evidence is not qualification.

Use a fixed external deadline with bounded cleanup grace. Cleanup must follow descendants that change SID/PGID, not just kill the original process group; preserve PID identity to avoid recycled-PID kills. A leaked child, unreaped zombie, hung wait, unexpected SIGTRAP/SIGSYS/SIGHUP or unbounded cleanup is failure. “Any failure” is never a pass. A named known refusal requires exact stage, disposition and effect boundary, independently measured. Positive invariants are hypotheses to qualify, not shipped support.

Keep instrument-no-reading (apparatus produced nothing), progress-loop (bounded grace while making progress), and terminal failure distinct. Never retry a red into a green. Missing required prerequisites fail qualification rather than silently skip it.

Carry signal provenance in the launcher: SA_SIGINFO or an equivalently qualified observer must retain si_code, sender PID and UID where supplied, with timestamps and recipient identity. Preserve original fatal-signal semantics after recording; installing an ignore handler would change the experiment. Do not interpret unavailable or kernel-defined sender fields as an identified userspace sender. Observe supervisor/provider as well as tracee: a child-only handler cannot explain supervisor SIGHUP.

## Cases

### C11-01 — Null-attribute spawn control

**Hypothesis:** An absolute spawn with both public pointers null admits a mediated child.

**Pre-condition:** Single-thread C parent; distinct helper binary; independent child output.

**Action:** Spawn, child writes CHILD, parent waits and writes PARENT.

**Expected host and routed result:** Native produces both; routed target produces exact shadow bytes and clean reap. Source predicts admission.

**Falsifying measurement:** Missing mediation, host effects, wrong bytes or orphan; capture x2 to distinguish wrapper packing from guard behavior.

### C11-02 — Empty action and attribute objects

**Hypothesis:** Pointer presence, rather than semantic contents, selects refusal.

**Pre-condition:** C11-01; fresh runs for actions-only, attributes-only and both initialized empty.

**Action:** Initialize one object shape and spawn the same helper.

**Expected host and routed result:** Native equivalent to control; routed nonzero x2 predicts structured pre-child refusal.

**Falsifying measurement:** A different stage, child effect or unbounded cleanup; zero x2 must be reported instead of forcing the prediction.

### C11-03 — addchdir alone

**Hypothesis:** Cwd action either selects the correct logical cwd or refuses before child effects.

**Pre-condition:** Qualified control; separately seeded directory; no descriptor actions.

**Action:** Addchdir to that directory; child writes relative unique name; parent verifies its own cwd.

**Expected host and routed result:** Native writes at requested cwd; current routed hypothesis is guard refusal. Later admission must preserve parent cwd and shadow placement.

**Falsifying measurement:** Write at stale cwd, host write, success without file, or child surviving refusal.

### C11-04 — adddup2 of a kernel pipe

**Hypothesis:** A single dup action is independently localizable.

**Pre-condition:** Qualified kernel pipe rendezvous; no routed descriptor source.

**Action:** Map one pipe end to a reserved child fd; child sends tagged bytes and closes it.

**Expected host and routed result:** Native exact bytes/EOF; routed predicts guard refusal before child writes.

**Falsifying measurement:** Leaked ends, blocked EOF or wrong diagnostic; pipe transport alone never qualifies routed files.

### C11-05 — Explicit inheritance and CLOEXEC

**Hypothesis:** Only intended descriptors survive executable launch.

**Pre-condition:** Control plus fd census apparatus; kernel descriptors with distinct tags.

**Action:** Separate leaf runs: ordinary CLOEXEC with null pointers; then CLOEXEC_DEFAULT plus addinherit_np allowlist.

**Expected host and routed result:** Native census matches policy; routed null shape needs measurement, explicit actions predict guard refusal.

**Falsifying measurement:** Unexpected inherited fd, missing intended fd, or logical table retaining closed entries; do not infer spawn behavior from Exec pruning.

### C11-06 — Session attribute

**Hypothesis:** SETSID changes only the intended child session.

**Pre-condition:** Control and external SID/TTY census; no pipe actions.

**Action:** Spawn with SETSID alone, child reports PID/PGID/SID and writes sentinel.

**Expected host and routed result:** Native new session; routed predicts pre-child guard refusal.

**Falsifying measurement:** Unexplained SIGHUP, wrong session, child effect after refusal or cleanup missing a detached child.

### C11-07 — Signal attributes

**Hypothesis:** Signal dispositions and masks are preserved or changed exactly as requested.

**Pre-condition:** C11-01; record parent mask and SIGPIPE disposition.

**Action:** Separate leaves for SETSIGDEF(SIGPIPE) and SETSIGMASK; child reports state before a routed write.

**Expected host and routed result:** Native matches requested state; routed predicts guard refusal.

**Falsifying measurement:** Wrong mask/disposition, unexpected signal, or treating a fatal supervisor stop as tracee errno.

### C11-08 — adddup2 of a routed descriptor

**Hypothesis:** Virtual descriptors cannot silently become unrelated kernel descriptors.

**Pre-condition:** Kernel adddup2 localized; #165 remains a blocker; seeded routed file.

**Action:** Adddup2 routed fd to reserved child fd; attempted child write uses unique payload.

**Expected host and routed result:** Native writes through the alias; current routed guard should refuse first. Future admission must establish alias identity and shared offset.

**Falsifying measurement:** Kernel EBADF mislabelled as the measured guard, host escape, phantom success or offset mismatch; #165 does not already qualify this case.

### C11-09 — Inherited routed OFD across spawn

**Hypothesis:** Spawn inheritance asks the same shared-offset question as fork.

**Pre-condition:** Null control and inheritance census qualified; pipe handoff; one output open.

**Action:** Parent writes AA, spawned child writes BBB on inherited fd, parent writes C after acknowledgment.

**Expected host and routed result:** Native AABBBC; routed compatibility target AABBBC, with copied-context defect suspected from #172.

**Falsifying measurement:** Any divergent bytes, EBADF or stale offset; no assumed exact defective bytes without measurement.

### C11-10 — E8 full pipe launch in C

**Hypothesis:** Qualified leaves compose into cwd, descriptors, session and signal policy without leaks.

**Pre-condition:** C11-03 through 07 localized; fixed C API sequence matching pinned E8, no runtime threads.

**Action:** Subscribe SIGCHLD, create three pipes, add cwd/dup2/inheritance, SETSID/CLOEXEC_DEFAULT/SIGPIPE defaults; child echoes nonce and writes unique shadow file.

**Expected host and routed result:** Native exact streams and EOF plus file; routed currently predicts guard refusal. On future admission require every stream, cwd, census and file oracle.

**Falsifying measurement:** Echo without shadow bytes, hidden stderr, host mutation, leaked pipe or wrong refusal. A guard result qualifies refusal only, not composition support.

### C11-11 — Pipe cancellation and reap

**Hypothesis:** Cancellation retains ownership across a detached session.

**Pre-condition:** Full pipe launch positively qualified; child readiness acknowledgment.

**Action:** Cancel while child awaits input; close owned pipe ends and collect child status.

**Expected host and routed result:** Both runs terminate intended group, drain EOF and reap within grace. Current guard prevents reaching cancellation, so keep deferred until launch admission.

**Falsifying measurement:** Surviving detached child, unreaped zombie, wrong-group kill or stream held open.

### C11-12 — Transient SIGCHLD around a routed call

**Hypothesis:** The #118 return gate classifies a transient signal without losing the routed transaction.

**Pre-condition:** Single-thread parent; qualified child launch; externally observable routed return window.

**Action:** Hold child on pipe; external coordinator releases its exit only at observed routed-call window; parent handles SIGCHLD and later reaps.

**Expected host and routed result:** Native exact write/status; routed exact write, gate evidence and clean reap. This is not predicted to hit the spawn-action guard when null launch is used.

**Falsifying measurement:** Signal outside window is unqualified; wrong return, missing reply, unexpected stop or lost reap falsifies. If window hook is unavailable, defer rather than use timing luck.

### C11-13 — PTY allocation leaf

**Hypothesis:** Terminal allocation can coexist with routing without being a routed object.

**Pre-condition:** Single-thread launcher, no child; external terminal census.

**Action:** Separate runs for openpty and posix_openpt plus grant/unlock/open-slave; close all handles, then known-good routed write.

**Expected host and routed result:** Native allocates terminal resources; routed target same kernel resource behavior and shadow sentinel. No source claim of PTY virtualization.

**Falsifying measurement:** Device-path refusal, leaked handles, failed sentinel or host-root mutation; identify actual underlying calls.

### C11-14 — PTY session and interactive echo

**Hypothesis:** A deliberately created controlling terminal supports a mediated child.

**Pre-condition:** Allocation and process/session leaves qualified; pin termios; independent rendezvous.

**Action:** Child setsid then TIOCSCTTY on slave, sets foreground group, echoes tagged input and writes unique file; close/cancel/reap.

**Expected host and routed result:** Native terminal state and expected termios-transformed bytes; routed target same plus exact shadow bytes. Actual fork/exec route needs capture; generic spawn guard is not an oracle here.

**Falsifying measurement:** Line-discipline echo mistaken for child echo, wrong foreground group, SIGHUP, missed file or detached survivor.

### C11-15 — E9 preserved descriptors

**Hypothesis:** PTY and ordinary pre_exec cleanup retain only the explicit exception.

**Pre-condition:** PTY mode qualified; one tagged kernel socket/pipe exception and decoy descriptors; no virtual alias initially.

**Action:** Separate compositions: E9 core shell path retains escalation-like fd with rebuilt env; PTY preserved-fd branch configures terminal, closes others and clears intended CLOEXEC.

**Expected host and routed result:** Native intended fd works and decoys close; routed target same with mediated unique file. Record fork versus spawn and thread census.

**Falsifying measurement:** Wrong exception, fd leak, fork guard or wait guard misattributed to PTY. Defer routed-fd exception variant to C11-08/09 qualification.

### C11-16 — Pinned Codex crate composition

**Hypothesis:** The production utility retains the qualified semantics with its runtime and reaper.

**Pre-condition:** C11-10 positive; pin crate/dependencies and runtime; record actual thread count/fallback.

**Action:** Run pipe crate echo plus unique file, then separately its PTY preserved-fd path after C11-15 qualification.

**Expected host and routed result:** Native full bytes/lifecycle; routed target same, but current nonnull native spawn predicts refusal. Threaded fork fallback or Park may instead hit #135.

**Falsifying measurement:** Unattributed branch, suppressed read error, orphan or any broadened accepted failure; defer implementation until C primitives isolate failures.

## Non-goals

No production guard removal, virtual PTY implementation, descriptor/OFD redesign, runtime scheduler changes, graph authoring, native execution or NFS access in this ideation pass. Terminal bytes are kernel-device traffic; meaningful routing assertions concern mediated child file effects, lifecycle and policy. A successful terminal echo is not evidence of NFS routing, and a PTY is not a shadow file. Shell pipelines, terminal emulators, job-control stress and a whole coding agent remain outside this cut.

## Intersections and implementation selection

Rank 9 owns fork/exec/cwd lifecycle. Spawn inherits that mediation machinery and asks the same copy-versus-share OFD question tracked by #172; C11-09 extends it across a different launch boundary. Rank 10 shipped nine cases: 01–06, 10, 14 and 15. C10-07/08/09/11/12/13/16 remain deferred and are not imported here. The new cancellation case is a process-ownership test, not a substitute for those wait/thread leaves.

#117's closed multiprocess work supplies descendant coverage, not action/terminal qualification. #118 owns live transient-signal evidence; #135 remains the thread/fork/park dependency. #165 is the descriptor refusal table: kernel pipes are a deliberately separate leaf from virtual-fd aliasing, and adddup2 cannot be presumed implemented because its action was accepted by libc. #158 concerns missing rmdir/bare-unlink routing; avoid child deletion as a cleanup primitive here. External disposable-data cleanup must not turn a spawn measurement into a deletion qualification.

Recommend first cut C11-01, C11-02, C11-03, C11-04, C11-05, C11-06 and C11-07, with every multi-mode candidate split into independent invocations. Start C11-05 with ordinary kernel CLOEXEC before the explicit allowlist composition; split session from signal attributes and mask from default disposition. Each leaf introduces at most one unknown relative to the control. They can establish precise refusals today without claiming positive support. Add C11-12 only after a rendezvous can prove the return window. Add C11-08/09 as separately owned descriptor characterizations, then C11-10 once component boundaries are understood; later cancellation and PTY work require positive prerequisites. Never interpret a compound guard refusal as proof that every component works.

The CI destination is .github/workflows/ci.yml:268, native-userspace-routing, “Userspace-routed run over the live NFSv4 client,” on [self-hosted, macOS, ARM64, umbra-integration]. It provides debugger permission and Ganesha; the local fixture job and Linux transport job each lack half of this comparison. The C10-10 exclusion is at :491. No workflow is changed by this document.

An Actions process must not be assumed to have an interactive controlling TTY or to be a session leader. Those are missing guarantees, not retrospectively measured facts about the retained run. Explicit SETSID and a newly allocated PTY can be qualified in headless CI when the launcher owns their lifecycle and observes the resulting state. Inherited interactive-terminal/job-control expectations require a provisioned local terminal and are local-only unless CI deliberately provisions an equivalent environment. None of the proposed core cases requires inheriting the runner's terminal. Missing terminal permissions invalidate the corresponding required qualification, not silently qualify a skipped case.

Recommend folding #186's observation schema into the rank-11 launcher because SID, controlling TTY, foreground group and signal provenance are already necessary for these cases. This addresses its instrumentation need, not its unknown cause or issue closure; the owner decides at the design gate whether to authorize a shared diagnostic pass including C10-10. Include observation overhead/deadlines and async-signal-safe record delivery in that gate. Keep a separate #186 attribution report so a passing r11 does not pretend to resolve historical SIGHUP.

To avoid more CI --skip exclusions, qualify the external launcher and signal evidence first in the actual runner environment, select the primitive first cut for required CI, and retain terminal compositions as explicitly unqualified candidates until their prerequisites and cleanup are demonstrated. Do not land an enabled case whose signal failure is then accepted or hidden; preserve the first failing artifact and fix or defer implementation at the gate. Do not globally ignore SIGHUP, loosen guard assertions, or assume local tmux success predicts CI. Descendants that setsid need ownership beyond a single original PGID.

## Review and reporting discipline

For every selected leaf report supported-and-correct, exact known refusal, demonstrated semantic defect, or not qualified. Maintain a separate blocker inventory when a refusal characterization is green. Distinguish syscall/API returns from supervisor termination, and explicitly mark downstream phases unreachable after refusal. New signals and cleanup failures never expand an accepted outcome set automatically.

Review any admission change against the binding invariant: each action works exactly or is explicitly refused before child filesystem effects. Current source predicts the refusal side for nonnull spawn descriptors; null spawn, ordinary inherited descriptors, transient signals and PTY device paths require their own correctness measurements. Future admission must replace the refusal assertion with exact descriptors, cwd, bytes/EOF and descendant cleanup; removing a guard assertion alone is insufficient.

This document contains 16 candidate cases, with independently selectable leaves. No runtime results, source modifications, commits, issues or comments were produced by this ideation pass.
