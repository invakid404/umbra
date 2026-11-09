# Rank 11 deferred — macOS spawn actions, pipes, then PTYs

Prepared 2026-11-09 for dg-4hbqk2s8. Ideation only; no build or runtime reading.

## Intent and evidence boundary

Re-check all eleven deferred candidates under S0–S8 and L72. FIRST CUT means a new measurable boundary, including a precise refusal; it does not mean positive support. Original definitions and deferrals are in knowledge/umbra/codex-ideation-slate-r11.md, Cases C11-03 through C11-16 (original definitions at :79, :91, :115, :127, :139, :175, :187, :199, :211, :223 and :235 respectively). Preserve their ids and scope.

## Measured source, not runtime results

Umbra pin: 9aa330df803159ef35c3eae21ec9c0b07abeb5bb, verified in the current workspace. Codex pin: 44fe510ce3ee61c8ef623adcbf89b901c73ddd61, verified at /Users/inva/Coding/umbra-worktrees/codex-src-44fe510. Codex citations below are relative to codex-rs; umbra citations are relative to the workspace. All citations refer to these pins.

The spawn guard remains at crates/umbra-platform-macos/src/native.rs:2211–2214: syscall 244 with nonzero packed descriptor x2 ends supervision with `spawn requires null attributes/file actions`. It follows executable validation/resigning (:2197–2210), precedes syscall continuation (:2224–2225), and is not a tracee ENOTSUP reply. The mapping remains abi.rs:308–310 in that crate. C11-02 already pins it at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:10267 and :10273; C11-10 pins the full sequence at :10664. Existing fixture modes are experiments/fixtures/umbra-userspace-spawn.c:105, :219 and :282. No new action-only repeat is warranted.

The original inherited-descriptor defects are no longer blockers: crates/umbra-supervisor/src/events.rs:1550–1564 sweeps spawn CLOEXEC and shares OFDs. C11-05 now rejects the old victim corruption (userspace_run.rs:10421–10425); C11-09 requires AABBBC (:10505), not historical AACBB. This does not implement action decoding. Required CI's Proof 1 selects all userspace_run tests with no C11 exclusions (.github/workflows/ci.yml:554–559). Historical runner SIGHUP is not an accepted result. Run isolation has a new-session spawn mechanism (crates/umbra-isolation/src/spawn.rs:18–20, :37–40; relay.rs:258 passes true and guardian.rs:78 passes false to inherit that session); neither it nor descriptor sharing removes the guard.

Open issues were read through `gh issue list --repo invakid404/umbra --state open --limit 200 --json number,title,body`; snapshot: open-issues.json beside this document. #118 covers the live transient-signal gap; #165 covers missing routed descriptor operations, not measured adddup2 support; #135 covers threaded fork/park. No open issue in that inventory specifically tracks packed spawn-action admission or PTY device acquisition. Drafts below request driver deduplication before filing.

## Shared expected pins and measurement contract

Inherit rank 11's entire shared expected pins and measurement contract (knowledge/umbra/codex-ideation-slate-r11.md:37–53). Each selected leaf gets a disposable native host control followed by an enforced userspace-registry live-Ganesha run (`UMBRA_INTEGRATION_REQUIRED`), never mounted NFS. Record exact executable/environment, OS, libc/toolchain, debugserver, roots, server and source pins. Native prerequisite failure means NOT QUALIFIED.

One reading per planned invocation, no retry-to-green. Use the existing slate_launch and rank-11 observer/ledger helpers, without copying their implementations. Record supervisor and tracee disposition separately, direct API return and errno, phase and owned descriptors, terminal state, PID/start identity and signal provenance. Verify shadow bytes independently through NFS and compare host root to its initial snapshot. A refusal is an exact stage and effect boundary, not any failure. Preserve first failures and file unexpected semantic defects separately.

No first-cut run is selected after the L72 novelty review. The earlier C11-13 deadline, cleanup and sentinel design is retained only in the pre-amendment copy; it is not an instruction to execute a duplicate refusal. Any revived allocation reading must first establish a distinct underlying branch or outcome and receive an amended preregistration, preserving the inherited native-first, deadline and cleanup contract.

## Installed allocation route — 2026-11-09

Read-only inspection on macOS 26.5.1 build 25F80, arm64e. No allocation API was called, no target was launched or attached, and no fixture/build/test ran. LLDB disassembled bytes read from its own installed shared-cache images through ctypes. The reproduction script is https://github.com/invakid404/umbra/issues/244#issuecomment-6742290066; raw bytes, SHA-256 hashes, symbol addresses, image-relative offsets and disassembly are https://github.com/invakid404/umbra/issues/244#issuecomment-6742290066. Image UUIDs: libsystem_c.dylib 694b7881-1bf3-3d0f-8f19-b50ae4e8ef8a; libsystem_kernel.dylib cc1cf985-bc65-3725-809f-4c1e36b8f4ba. These pin the installed binary evidence independently of the downloaded Apple source.

The installed openpty call at 0x183522f88 targets posix_openpt at 0x18352307c (log :22, :84–95). posix_openpt loads the /dev/ptmx operand and calls import stub 0x183579a0c (:90–91, :174–180, :233). The slot resolves to open$NOCANCEL, not public open (:180); that wrapper calls __open_nocancel at 0x18362e940 (:197, :232). The latter has mov x16,#398 followed by svc #0x80 (:163–164). /dev/ptmx is a character device on this host (https://github.com/invakid404/umbra/issues/244#issuecomment-6742290066). Both proposed APIs therefore reach the same first device-open primitive before grant/unlock/slave setup. The initial inspection's public-open equality hypothesis failed; its raw output is retained as installed-pty-disassembly-initial.txt. The completed inspection follows the actual non-cancel import and does not execute or retry a PTY reading.

At umbra 9aa330df803159ef35c3eae21ec9c0b07abeb5bb, crates/umbra-platform-macos/src/native.rs:842–885 resolves every traced stub locally, validates its syscall instruction and installs interception; shared-cache addresses are system-wide and remote instructions are verified. crates/umbra-platform-macos/src/abi.rs:305 includes __open_nocancel/398 as Namespace; :364 identifies its path operand; :821 decodes it with the ordinary open family. native.rs:2134–2141 emits the namespace syscall event. Thus internal libc calls need not be caught by public open interposition: this installed path reaches the traced stub directly. Overlay resolve performs path resolution and lookup (crates/umbra-overlay/src/engine.rs:2591–2596); lookup delegates to base.stat (:862–875), whose metadata conversion rejects character devices (crates/umbra-supervisor/src/base.rs:264–278, :312–316). The file is rejected before kernel acquisition; later allocation operations are unreachable in this route.

This establishes B3 by installed instruction inspection plus pinned production source, not by a new runtime refusal. The result is scoped to these installed images; a different host image needs route revalidation. Both allocation APIs repeat the same device-entry guard already recorded for #193 B-T1 in #201. The cancellable/non-cancellable open entry difference converges on the same decoded operation and base.stat guard; L72 explicitly excludes a case that would only reach an already pinned guard.

## Cases

### C11-03 — addchdir alone

Re-check: cwd action; originally predicted the spawn guard. Codex utils/pty/src/posix_child.rs:176–178 adds cwd. At this base native.rs:2211–2214 in crates/umbra-platform-macos/src still refuses the non-null packed descriptor before decoding its contents. Descriptor sharing at crates/umbra-supervisor/src/events.rs:1550 does not change that boundary.

**Decision: FIX-PHASE ITEM.** Blocker B1, #243. Would repeat C11-02 (userspace_run.rs:10267) and C11-10 (:10664), not extend their reading. After admission, restore the original primitive-specific invariant. C11-08 additionally needs virtual alias semantics; #165 remains a dependency, not evidence that adddup2 itself was measured.

### C11-04 — adddup2 of a kernel pipe

Re-check: one kernel descriptor action; originally predicted the spawn guard. Codex utils/pty/src/posix_child.rs:180–186 maps child stdio. At this base native.rs:2211–2214 in crates/umbra-platform-macos/src still refuses the non-null packed descriptor before decoding its contents. Descriptor sharing at crates/umbra-supervisor/src/events.rs:1550 does not change that boundary.

**Decision: FIX-PHASE ITEM.** Blocker B1, #243. Would repeat C11-02 (userspace_run.rs:10267) and C11-10 (:10664), not extend their reading. After admission, restore the original primitive-specific invariant. C11-08 additionally needs virtual alias semantics; #165 remains a dependency, not evidence that adddup2 itself was measured.

### C11-06 — Session attribute

Re-check: SETSID alone; originally predicted the spawn guard. Codex utils/pty/src/posix_child.rs:209 selects NewSession. At this base native.rs:2211–2214 in crates/umbra-platform-macos/src still refuses the non-null packed descriptor before decoding its contents. Descriptor sharing at crates/umbra-supervisor/src/events.rs:1550 does not change that boundary.

**Decision: FIX-PHASE ITEM.** Blocker B1, #243. Would repeat C11-02 (userspace_run.rs:10267) and C11-10 (:10664), not extend their reading. After admission, restore the original primitive-specific invariant. C11-08 additionally needs virtual alias semantics; #165 remains a dependency, not evidence that adddup2 itself was measured.

### C11-07 — Signal attributes

Re-check: separate default-disposition and mask leaves; originally predicted the spawn guard. Codex utils/pty/src/posix_child.rs:214–217 restores SIGPIPE. At this base native.rs:2211–2214 in crates/umbra-platform-macos/src still refuses the non-null packed descriptor before decoding its contents. Descriptor sharing at crates/umbra-supervisor/src/events.rs:1550 does not change that boundary.

**Decision: FIX-PHASE ITEM.** Blocker B1, #243. Would repeat C11-02 (userspace_run.rs:10267) and C11-10 (:10664), not extend their reading. After admission, restore the original primitive-specific invariant. C11-08 additionally needs virtual alias semantics; #165 remains a dependency, not evidence that adddup2 itself was measured.

### C11-08 — adddup2 of a routed descriptor

Re-check: virtual aliasing behind action admission; originally blocked by the guard and #165. Codex utils/pty/src/posix_child.rs:180–186 supplies the action. At this base native.rs:2211–2214 in crates/umbra-platform-macos/src still refuses the non-null packed descriptor before decoding its contents. Descriptor sharing at crates/umbra-supervisor/src/events.rs:1550 does not change that boundary.

**Decision: FIX-PHASE ITEM.** Blocker B1, #243. Would repeat C11-02 (userspace_run.rs:10267) and C11-10 (:10664), not extend their reading. After admission, restore the original primitive-specific invariant. C11-08 additionally needs virtual alias semantics; #165 remains a dependency, not evidence that adddup2 itself was measured.

### C11-11 — Pipe cancellation and reap

Re-check: original prerequisite was positive full pipe launch. Codex utils/pty/src/posix_child.rs:174–232 still supplies actions/attributes; native.rs:2213 still stops it before child creation. Existing C11-10 at userspace_run.rs:10664 already characterizes that refusal. The new run session and OFD sharing do not make this launch reachable.

**Decision: FIX-PHASE ITEM.** B1, #243. After admission, cancel only after child-ready acknowledgment, close ends, drain EOF and reap within grace; no substitute null-spawn cancellation case under this id.

### C11-12 — Transient SIGCHLD around a routed call

Re-check: original deferral was absence of an exported return-window hook. Window state remains private to Session (crates/umbra-platform-macos/src/native.rs:263); return_stop (:684) and stop's pending-gate classification (:2365–2401) have no external rendezvous notification. The transient branch still calls continue_absorbed (:2389). A pre-call acknowledgment does not locate this window.

**Decision: FIX-PHASE ITEM.** B2, Refs #118. Production observability is required to release the exiting child only after actual window entry. Null spawn avoids B1 but does not supply that observability. No sleep-based or probabilistic replacement.

### C11-13 — PTY allocation leaf

Re-check: the original later allocation leaf (knowledge/umbra/codex-ideation-slate-r11.md:199–209) needs no child, but its novelty must include earlier terminal readings. Codex utils/pty/src/pty.rs:493 calls libc openpty; this alone does not establish the installed Darwin syscall route.

Additional pinned source: apple-oss-distributions/Libc commit `71bbe350ab79eef58113991d817ccc6165061a64`, util/pty.c:72–82 calls posix_openpt(O_RDWR|O_NOCTTY), then grantpt/unlockpt/ptsname and open-slave. At the same pin stdlib/grantpt.c:42–46 implements posix_openpt as open("/dev/ptmx", flags); :57–59 and :141–143 supply the later ioctls. Exact source copies are evidence/apple-pty.c and evidence/apple-grantpt.c; evidence/libc-tree.json records the repository commit. The installed binary route is independently established above; matching the downloaded source to a release is unnecessary to that proof. The Apple source is corroborating evidence only.

Umbra active public opens route (crates/umbra-platform-macos/interpose/umbra_interpose.c:426–441); the specific traced open stubs are in crates/umbra-platform-macos/src/abi.rs:304–307. Both installed allocation paths reach this mediation and base lookup, as the instruction chain above establishes; crates/umbra-supervisor/src/base.rs:264–278, :312–316 rejects the device; crates/umbra-overlay/src/engine.rs:862–875 propagates the lookup. B3 is established for these installed allocation paths and their dependent compositions.

**Decision: FIX-PHASE ITEM.** Both proposed leaves repeat a pinned guard under L72, by the installed route proof above. Earlier #193 B-T1, reported in open #201, already measured `open("/dev/tty")` reaching `UnsupportedCapability during base.stat: special host entries (devices, sockets, FIFOs) are unsupported`; the current fixture primitive is experiments/fixtures/umbra-isolation-fixture.c:201–204. Against B-T1, the proposed /dev/ptmx lookup changes the device pathname but predicts the same open primitive, guard, branch and outcome. Against each other, openpty and posix_openpt begin with the same primitive in the pinned Apple source. Neither proposed leaf reaches a distinct operation before that guard; later grant/unlock/slave operations would be unreachable under the prediction. API spelling and the unexecuted downstream calls cannot establish novelty.

The source-established disposition is UnsupportedCapability/base.stat with `special host entries (devices, sockets, FIFOs) are unsupported`, ending supervision before allocation; this is not a newly measured reading. No duplicate runtime run is selected. Refs #244, now a production-blocker draft. #201 supplies the existing refusal evidence but tracks inherited-fd-2 reading rather than allocation support.

### C11-14 — PTY session and interactive echo

Re-check: originally required positive allocation and process/session leaves. Codex terminal setup is setsid/TIOCSCTTY (utils/pty/src/pty.rs:662–670). The upstream boundary is base.rs:264–278, reached by the installed allocation route established in C11-13. A fork-based C launcher is not automatically blocked by the spawn guard (abi.rs:310; native.rs:2145); claiming that guard as its result would be wrong.

**Decision: FIX-PHASE ITEM.** B3, #244: allocation is production-blocked before session or echo operations. Repeating the device refusal would not measure session, foreground-group or echo behavior. Once acquisition is admitted, use owned terminal, fixed termios, single-thread fork and independent child file oracle. Do not preallocate outside supervision to bypass the original prerequisite.

### C11-15 — E9 preserved descriptors

Re-check both original branches. Core shell pre_exec descriptor cleanup is Codex core/src/spawn.rs:70–136. PTY preserved-fd selection and setup are utils/pty/src/pty.rs:176–177, :398–412; it allocates via :493 before terminal setup. Ordinary OFD inheritance is improved (events.rs:1550–1564), but does not establish explicit cleanup policy or device acquisition.

**Decision: FIX-PHASE ITEM.** B3 blocks the installed allocation route and thus the original positive-PTY prerequisite; #244. Keep the ordinary core-shell leaf with this case for the subsequent fix-phase sequence, not as a claim that its pre_exec is itself a non-null spawn guard. #135 remains a conditional dependency if the actual fallback forks with threads; native.rs:644 and :2145 enforce that guard. Do not substitute an unrelated null-spawn inheritance repeat of C11-05/09.

### C11-16 — Pinned Codex crate composition

Re-check: original positive C11-10 prerequisite still fails admission. Codex utils/pty/src/child_command.rs:271–296 chooses the native backend; posix_child.rs:174–232 builds non-null actions/attributes, hitting native.rs:2213. Its PTY preserved-fd branch additionally depends on positive C11-15 (pty.rs:398–412, :493).

**Decision: FIX-PHASE ITEM.** B1 for pipe composition and B3 for PTY composition; #243, #244. A library-level run repeating C11-10 is excluded by L72. No crate fixture or dependency changes now. On revival pin each external version exactly to this Codex Cargo.lock and capture the actual backend; #135 is conditional for threaded fork/park, not a spawn gate.

## Non-goals

No production edits, builds, native executions, Ganesha access, mounts, commits, issue filing or workflow modifications in this node. No terminal emulators, whole-agent claim or inherited runner TTY assumption.

## Intersections and implementation selection

The first cut is empty. All eleven ids are FIX-PHASE ITEM. B1 and B2 are source-substantiated; B3 combines installed binary inspection with pinned production source and the existing B-T1 refusal. C11-13's two APIs reach that same guard; C11-14 and the PTY branches of C11-15/16 cannot reach their later operations. C11-15's ordinary-shell branch retains the original positive-PTY prerequisite, rather than being asserted to contain a direct device blocker. Under S1 the later PR carries only the accepted ideation including its fix-phase summary. No fixture, harness or CI change is selected.

## Fix-phase summary

| Blocker | Production site at umbra pin | Cases | Issue | Fix sketch |
|---|---|---|---|---|
| B1 packed spawn descriptor admission | crates/umbra-platform-macos/src/native.rs:2213 | C11-03/04/06/07/08/11/16 pipe | #243 | Decode and enforce cwd, actions and attributes before child effects; then qualify cancellation/composition. |
| B2 observable return-window rendezvous | crates/umbra-platform-macos/src/native.rs:684, :2365 | C11-12 | #118 | Expose precise coordinated window entry for a live signal fixture. |
| B3 terminal-device acquisition | crates/umbra-supervisor/src/base.rs:264–278 | C11-13 both proposed leaves; C11-14/15/16 PTY | #244 | Define a safe kernel terminal-resource route and ownership before positive allocation, session/echo and descriptor-preservation qualification. |

#165 is C11-08's downstream aliasing dependency; #135 applies only if future composition takes threaded fork/park. Neither is a substitute issue for B1 or B3.

## Review and reporting discipline

Predictions above are preregistration only. Preserve actual measured statuses, exact refusal disposition and evidence of cleanup; missing observation is NOT QUALIFIED. Unexpected semantic defects need their own issue and semantic assertion. If C11-13 succeeds, revisit downstream selection at review with that evidence rather than silently changing this document after acceptance. The installed route is now established by instruction inspection; no new runtime result is claimed. Revalidate the route if the installed image UUIDs change.

Filed: #243 (defects/spawn-actions.md)
Filed: #244 (defects/pty-device-acquisition.md)
## Revision — 2026-11-09 (ideation-review AMEND)

Applied required amendments 1–3. B3 and its issue draft are conditional on installed acquisition reachability. Added pinned Apple source and the earlier B-T1/#201 reading to the novelty comparison; both proposed C11-13 leaves lack a distinct predicted reading under L72, so the first cut is empty. PTY-dependent decisions preserve original sequencing and must be reassessed if a distinct C11-13 reading becomes available. Corrected NewSession to posix_child.rs:209 and the victim assertion to userspace_run.rs:10421–10425. No build or runtime reading was taken. Driver must archive this revision separately from the original preregistration.

## Revision — 2026-11-09 (review revision 2, amendment 1)

Supersedes the preceding revision's conditional B3 conclusions. Inspected the installed allocation call chain without invoking it; resolved open$NOCANCEL through __open_nocancel/398 and the existing Namespace/base.stat route. B3 is now a source-and-binary-established blocker, both allocation leaves repeat B-T1's guard, and the empty first cut and dependent decisions are final for the recorded pins. Updated both summaries and the issue draft. No build, allocation execution, tracee launch, Ganesha run or test was performed. Preserve earlier revisions and the initial failed public-open import assumption as evidence.

STATUS: COMPLETE
