# Implementation — Multithreaded closure, slice 1 (graph `dg-0ved1w0e`)

Node `implement`, visit 1; **updated in round 1 after review**, which is recorded
inline rather than as an appendix so no paragraph states a superseded fact. Round
1's own account of what changed and why is `fix-r1.md`. Date 2026-09-29.
Baseline: `master` **`d8a42def`** ("test(tracer): measure the ungated
multithreaded paths (slice 0)", the #134 merge).
Change: **`qqxtnynk`**, bookmark `feat/mt-closure`.
Contract: the `RATIFICATION` section of `/tmp/graph-dg-0ved1w0e/design-gate.md`
(human, sig-001, revision 9), with `/tmp/graph-dg-0ved1w0e/audit.md` as its
evidence. Both were read end to end before any code was written.

> **Pinned by change id, not by commit id.** A jj working-copy commit id
> re-timestamps on every snapshot and every `jj describe`; the change id does
> not. Every figure below was measured against `qqxtnynk`, whose commit id moved
> on every edit of this arc. Resolve it with `jj log -r qqxtnynk`.

> **The files in the diff are what `jj diff -r qqxtnynk --name-only` returns.**
> That command is the authority and this document does not restate it as a
> count — lesson 28's remedy 3, applied to the one value this document would
> invalidate by stating it, since this file is itself in the diff.

---

## 0. Headline

**Both of #134's `#[ignore]`d fixtures now pass, and the mechanism that closes
them is the one the human ratified: per-thread resume.** The `z0`/`Z0` sequence —
the safety basis of the ruling — is **byte-identical to master**. Not one added or
removed line in the diff touches the release, the gate plant, the hardware gate,
the re-arm, or either registry primitive. Only *which threads run* changed.

```
mt_write   master d8a42def: MISSED, 3/3, host file holding Some("two\n")
           qqxtnynk:        CAPTURED, 3/3
mt_spawn   master d8a42def: panic at native.rs:571, 3/3
           qqxtnynk:        CAPTURED, 3/3
```

**One ratified premise did not survive measurement, and it is recorded here
rather than quietly carried.** The audit's mutation probe **M2** predicted that
reverting `pending` to a single slot, with the per-thread resume kept, would fail
`mt_spawn` — "the slots are still load-bearing". It does not. Both fixtures pass,
3 runs of 3. §4.

---

## 1. What was ratified, and what was built

| # | Ruling | Built |
|---|---|---|
| **D1** | Shape 3 only — `vCont;c:<tid>`; `z0`/`Z0` untouched; Mach sibling-hold shelved | `Session::continue_thread`, called from `return_stop` in place of the bare `c`. No `task_threads`, no `thread_suspend`, no new `unsafe`, no new dependency. §2.1 |
| **D2** | The trapping TID is carried on `Pending` and read from there, **never** from `s.thread`; M3 ships | `Pending::thread`, read by `finish_return`. M3 ships as a source pin, and §4 states exactly why it could not ship as a live test. §2.3 |
| **D3** | IN: per-thread slots, window fix, both fixtures. DEFERRED: `single_thread()` removal, 2-thread-parent-fork, orphan leak, writer lease | `pending`/`entry` keyed by `ThreadId`; both fixtures un-`#[ignore]`d and passing; `single_thread()` and both call sites byte-identical, pinned by a new test. Nothing from items 3/4b/5/6 was touched. §5 |
| **D4** | Three waivers, and only those | (a) Session shape consumed exactly as enumerated; (b) `vCont` is the only new RSP surface; (c) the sibling freeze is documented in the code as watchdog-kill degradation. §2.5 |
| **D5** | `single_thread()` at `:498`; shapes in #134's `impl.md` §3; lesson 28 fired 4× | Accepted as stated; no re-derivation. |

**Success criterion, as ratified:** both fixtures assert on entry names and bytes,
never on exit code. `mt_fixture` already did — host file absent *and* shadow
content byte-equal — so the test change is the removal of the two `#[ignore]`
attributes and nothing else about what is asserted. The `debug_assert!` the
ruling cites at `native.rs:571` — that is its line at `d8a42def`; in this tree it
is the one in `return_stop`, and citing a line here would be a number this change
invalidates — is **converted to a per-thread invariant, not deleted**.

---

## 2. The change

### 2.1 The closure: `return_stop` resumes the trapping thread alone

```
return_stop:   z0 entry  →  plant gate  →  record Pending incl. TID
               →  vCont;c:<tid>        ← the only line that changed
finish_return: retire gate  →  re-arm entry  →  report the exit
               →  the process resumes on the next unqualified `c`
```

The window between the `z0` and the re-arm is not narrowed; it is emptied of
anyone who could walk into it. A debugserver `Z0` is per-process, so the release
un-arms the stub for every thread — but with every sibling stopped, there is no
other thread running to reach it.

`continue_run` is unchanged and still sends the bare `c`. Two new siblings sit
beside it:

- **`continue_thread(thread)`** — `vCont;c:<tid>`. One packet. Per-thread
  addressing is not new to this backend: `g`, `p` and `P` already carry
  `;thread:<tid>;` under `QThreadSuffixSupported`.
- **`continue_absorbed()`** — the bare `c` when no window is open, and the
  window owner alone when one is. This is what the two paths in `stop()` that
  absorb a signal without opening a transaction now use. A bare `c` from either
  of them would have released the siblings into the open window — the `mt-write`
  escape with a signal in front of it.

### 2.2 Per-thread slots

`pending: BTreeMap<ThreadId, Pending>` and `entry: BTreeMap<ThreadId, u64>`.
Every touch point the audit enumerated was converted; the complete set is in the
diff. These are worth naming because they are decisions rather than mechanical
rekeying:

1. **The map is for what it makes expressible, not for what it answers.** A single
   slot answers "which thread owns this window" through `Pending::thread` — M2
   demonstrated it, with both fixtures green. Round 0's field doc justified the map
   by that answerability, which overstates it; the corrected doc says what is
   actually true: the map is what lets the per-thread invariant be *stated*, and it
   becomes load-bearing at the deferred `single_thread()` removal, when a second
   concurrent window becomes reachable.
2. **`thread_index`** resolved a session by `s.thread == thread` — a slot every
   stop overwrites, so with a live multithreaded tracee it matched on *the last
   thread that stopped*. `registers`, `set_registers` and `resume` all route
   through it. It now consults the per-thread slots first and falls back to
   `s.thread`. Thread ids are unique machine-wide, so consulting several
   sessions' slots cannot cross-match; the only failure it can produce is "not
   found".
3. **`regs`/`set_regs` take the thread.** They read `g;thread:<self.thread>;` —
   so `registers(thread)` resolved the right session and then returned a
   *sibling's* context under the right thread's name. This is a latent
   aliasing bug independent of the window, and it is fixed by the same waiver.
4. **The exec stop drains the whole map** rather than looking the window up by
   the stopping thread. An `execve` keeps one thread and discards the rest, and
   the survivor is reported under a thread id of the new image — so the thread
   stopping there need not be the one that entered the `execve`. Keying on it
   would have dropped the `ReturnKind::Exec` candidate and left the exec'd image
   half-mediated. The `fork-write`, `exec-write`, `posix-spawn-write` and
   `grandchild-write` fixtures all still capture.

### 2.3 The TID on `Pending` (D2)

`Pending` gains `thread: ThreadId`, recorded when the window opens.
`finish_return` attributes its `SyscallExit` from it.

The reason is not hypothetical. The supervisor removes its in-flight operation by
thread (`events.rs:1351`) and a miss is **not** an error — it takes the "an exit
for a call we never intercepted" branch (`events.rs:1352-1354`), lets the kernel
result stand, and abandons the rewrite without a word. Closing the registry hole
while attributing from `s.thread` would convert a loud escape into a silent one.

`resume()`'s own `SyscallExit` — the "PC moved past the `svc`" path — was changed
the same way, from `s.thread` to `command.thread`, for the same reason.

### 2.4 The tripwire, converted — and a second assertion beside it

`debug_assert!(self.pending.is_none(), …)` became
`debug_assert!(!self.pending.contains_key(&thread), …)`, the per-thread conversion
#135 sub-item 1 and D3 require. It no longer catches the between-threads collision
`mt_spawn` measured, because that collision can no longer happen; what it catches
now is a second window on a thread that already has one.

**A second `debug_assert!(self.pending.is_empty(), …)` was added beside it in round
1, and the reason is a false claim this document previously carried.** Round 0's
text — here, in `README.md` and at three places in `native.rs` — credited the
per-thread assertion with pinning the property `continue_absorbed` depends on: that
at most one window is open **process-wide**, which is what makes
`pending.values().next()` well-defined. It does not. Two windows on two *different*
threads pass a per-thread check untouched, and that is exactly the state that would
make the pick ambiguous.

What establishes the property is the **freeze**: the thread `return_stop` resumes is
the only one running, so nothing is left to reach `return_stop` and open a second
window. That is structural. The new assertion is a tripwire over it, not its source,
and all five sites now say so.

**Additive, not a replacement, and deliberately so.** Swapping the per-thread check
for `is_empty()` would undo the conversion that was ratified. The stronger condition
subsumes the weaker, so only the first to fail is reported — which is the right
order: a same-thread double-open gets the specific diagnosis, and a cross-thread one
means the freeze itself has broken. Measured both ways. On the shipped tree the new assertion fires **0 times** across
three full fixture runs (13 `CAPTURED` each), matching what the correctness reviewer
measured with an equivalent probe before it existed. Under **M1** — the freeze
deliberately broken by reverting `return_stop` to a bare `c` — it fires **3 of 3**,
with its own message, and `mt_spawn` fails on it.

**So it closed a detection gap, not only a documentation one.** Before it was added,
M1 failed `mt_write` alone and `mt_spawn` *passed with the freeze broken*; now M1
fails both. The correctness reviewer measured this at round 2 and it is re-verified
here, which is the sharper justification for the additive form than either the
ratification argument or mine.

### 2.5 Waiver (c), documented in the code

`continue_thread`'s doc comment carries it, as the ruling requires:

> The interposer routes `read`/`write`/`close`, so a routed call that blocks now
> blocks with its siblings held, where today's bare `c` would have let them run.
> It does not deadlock silently: `check_deadline` kills the tree at the session
> deadline, so the degradation is a **watchdog kill naming a timeout, not a
> hang**.

That is a real behaviour change from master's bare `c`, taken deliberately,
because the thing on the other side of it is the unmediated window — and that one
is silent.

---

## 3. New tests

All are new `#[test]` functions. **No test function that existed at the parent was
modified**, and that is the claim the test-surface rule is about — but it is not the
whole truth about this change's own history: round 1 edited the absorb pin, a test
this change itself created, to add the assertions it was missing. Both readings are
stated because the parent-relative one alone reads as exhaustive and is not.

The changes to `fixtures.rs` are the two `#[ignore]` removals **plus the two
doc-comment corrections recorded in §7** — the comments described the defect as
open and the fix as a prediction, and both are now measurements.

| Test | What it pins |
|---|---|
| `a_return_is_attributed_from_its_window_and_never_from_the_session_slot` | **M3.** `finish_return` reads `pending.thread` and contains no `s.thread`. |
| `a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings` | **M7.** `continue_absorbed` consults `pending` and calls `continue_thread`, **and** `stop()` routes through it twice and through `continue_run` never. |
| `the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread` | **M8.** The exec stop drains every window instead of looking one up by key. Added in round 1. |
| `the_fork_and_park_paths_still_refuse_a_multithreaded_tracee` | D3 item 3 stays deferred **by test**: `single_thread()` exists and guards exactly two sites. |

All but the first are source-text pins, the device `abi.rs` already uses for the
interposer's descriptor test, and each carries its own honest statement of what
it is worth: it pins a *decision*, not the wiring, and a reader who satisfies it
by renaming a variable has defeated it.

**The absorb pin gained its first half in round 1, and the reason is worth
recording.** As shipped in round 0 it asserted only that `stop()`'s two call sites
route through `continue_absorbed` — nothing asserted that `continue_absorbed` does
anything at all. The correctness reviewer gutted the helper to an unconditional
bare `c`, reintroducing the exact escape it exists to prevent, and **all thirteen
fixtures captured across five runs and all unit tests passed, including this pin
under its own name**. The most subtle decision in the change was the one nothing
defended, behind a test whose name claimed otherwise. Both halves are now asserted
and both mutations were re-run against them (§4.3).

> **A correction inside a correction, caught here rather than by a reviewer.**
> The first revision of the `single_thread` pin counted three call sites and
> failed. The third was the assertion message complaining about the count — the
> test quoted the code it counted, and `include_str!` found its own quote. That is
> lesson 28's generator exactly: the fixed point one level out from where it was
> looked for. The fix is `production()`, which searches only the text above
> `#[cfg(test)]`, and it is documented in the source at the point it bit.

---

## 4. Measurements

Fixture recipe per CI (`.github/workflows/ci.yml:264-268`): `clang -arch arm64`
of `experiments/fixtures/umbra-test-child.c`, `UMBRA_TEST_FIXTURE_PATH` and
`UMBRA_TEST_REDIRECT_ROOT` set, `--test-threads=1`.

### 4.1 Baseline reproduced on this exact parent, before any edit

```
mt_write  fixtures.rs:680  MISSED mt-write: the second thread's output reached the
                           host at …/umbra-rust-fixture-<pid>-mt-write-b/output,
                           holding Some("two\n")
mt_spawn  native.rs:571    a second intercepted syscall entered while one was
                           still in flight
```

Both exactly as #134 and the audit recorded.

### 4.2 After

Both `CAPTURED`, 3 runs of 3. The full direct-tracer suite is **13 `CAPTURED`,
0 `SKIP`, 0 ignored** — the verdict is the `CAPTURED` line, not the exit status
(lesson 23).

### 4.3 Mutation probes

Each mutation was applied to the shipped tree, **rebuilt** (lesson 24), run, and
reverted.

| # | Mutation | Predicted | **Measured** |
|---|---|---|---|
| M1 | `return_stop` resumes with a bare `c`, per-thread slots kept | `mt_write` fails | ✅ `mt_write` MISSED with the same `two\n` on the host; `mt_spawn` CAPTURED |
| M2 | every window keyed onto one shared slot, per-thread resume kept | `mt_spawn` fails | ❌ **both pass, 3/3** — see below |
| M3 | `finish_return` reads `s.thread` | a new test fails | ⚠️ no *runtime* test fails; the new source pin does — see below |
| M4 | drop the re-arm in `finish_return` | `mt_write` fails | ✅ both fail |
| M5 | `return_stop` resumes a thread that is not the trapping one | `mt_write`, or a watchdog hang | ✅ both fail through the watchdog, ≈25 s each |
| M6 | double-`Z0` every entry address | an existing fixture fails | ✅ `mt_write`, `mt_spawn` **and** `open_libc` all fail through the watchdog |
| M7 | gut `continue_absorbed` to an unconditional bare `c` | — (round 1; the reviewer's probe) | ⚠️ **nothing failed** as shipped in round 0 — 13/13 fixtures × 5 runs and every unit test, including the pin named after it. Fails the strengthened pin in round 1. |
| M8 | key the exec candidate lookup on the stopping thread | — (round 1; the reviewer's probe) | ⚠️ **13/13 fixtures pass** — no fixture discriminates twin adoption at all. Fails the new drain pin in round 1. |

M7 and M8 were re-run against the round-1 pins, rebuilt between each. Each fails
**only** its own pin and leaves the other green, so neither new pin is a tautology:

```
M7 → a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings  FAILED
     the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread  ok
M8 → the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread  FAILED
     a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings  ok
```

**M2 falsifies the audit's own prediction, and the correction matters.** With the
per-thread resume in place, a thread inside a window is the only thread running,
so no sibling is left to open a second transaction — the interleaving the slot
collision needed is gone. The per-thread `pending` is therefore **not** what keeps
`mt_spawn` green today; the resume closes both cases. The slots remain what makes
the invariant expressible, what `thread_index` resolves from, and what carries
D2's TID — but a reader looking for the guard that keeps `mt_spawn` green should
look at `return_stop`. Only the `pending` half was reverted in this probe;
`entry` stayed per-thread, so nothing is claimed about it either way. This is
recorded in `README.md` and in `mt_spawn`'s own doc comment, both of which
previously carried the prediction.

**M3 could not be a live test, and the reason is the fix.** The mutation was run:
`finish_return` was edited to read `s.thread`, the crate rebuilt, and both
fixtures still passed. They cannot catch it, and no runtime test on this backend
can, because the per-thread resume freezes every sibling for the whole of the
window — so nothing is running that could make the two values differ. The
equality is a *consequence of the freeze*, not a property of `finish_return`, and
it stops holding the moment anything resumes a sibling inside a window: the
deferred `single_thread()` removal, a `vCont` naming more than one thread, or a
future sibling-hold. M3 therefore ships as the source pin in §3, which **does**
fail under the mutation, and which says in its own doc comment that it makes the
swap impossible to make silently and proves nothing else. Ratifying D2 was still
right: it is the difference between a loud escape and a silent one the moment the
freeze is relaxed.

**M8 records a gap this arc did not create and does not close.** The exec drain is
measured *necessary* — at the exec stop the stopping thread is not the window
owner, because an `execve` replaces the address space and its threads. But keying
the lookup back on the stopping thread passes 13/13, because the only fixture that
reaches an exec stop re-execs the **same** twin: the candidate it drops equals the
value already there, so dropping it is a no-op. **No fixture in this crate
discriminates twin adoption at all.** That is a pre-existing #117/#134 gap, not one
this change introduced — but the per-thread rekeying created a natural-looking
wrong edit the suite would wave through, which is why the drain now carries a
source pin. A fixture that execs a *different* image would close it properly;
**that is deliberately not done here** — it closes a pre-existing gap and belongs
to its own arc, outside this ratification. Recorded as a follow-up candidate for
`merge_gate`.

Every source pin was verified to fail under its own mutation and to pass under the
others', so none is a tautology.

---

## 5. Guardrails

Every pin in audit §L was read back against the parent mechanically.

**The `z0`/`Z0` dance is untouched.** `jj diff --git` filtered for the release,
the gate plant, the hardware gate, the re-arm and both registry primitives
returns **nothing** — not one added or removed line. That is the ruling's safety
basis and it is intact by construction, not by review.

| Mechanism | Verdict |
|---|---|
| `install()`'s `self.parent.is_some()` inherited test | unchanged |
| `single_thread()` itself, and both call sites (`Delivery::Fork`, `WaitPlan::Park`) | unchanged; its line moved because code was added above it, which is why the pin matches on text and not on a line | 
| `TRANSIENT_SIGNALS` and its SIGCHLD-in / SIGSYS-out tests | unchanged |
| one table, two gates over `abi::TRACED_STUBS` | unchanged |
| `ReturnKind::Exec { twin }` candidate: raised at entry, adopted at the exec stop, discarded on a failed exec | unchanged in mechanism; the raise carries the TID, which is waiver (a) |
| `Session::allocate`'s `_M…,rw` scratch | unchanged |
| `task_for_pid` in `Task::acquire` | unchanged |
| exec-stop release loop before `breaks.clear()` (the closed M2 gap) | unchanged |
| `attach_child`'s `debug_assert!(child.breaks.is_empty())` | unchanged |
| descriptor fence (C half and Rust twin), interposer constructor prohibition | files not touched |

**Deferred and not touched:** #135 items 3 (`single_thread()` removal), 4b
(2-thread parent forking — explicit non-goal), 5 (suspended orphan leak — a
property of the error window, survives this fix), 6 (lost writer lease — debug
only, different trigger), 7 (lesson 28, process only). No new user setup, mounts,
drivers or privileged steps. Tests stay toy C plus standard utilities.

**Verification technique, as the audit required:** armed-ness is never asserted
with the `m` packet, which debugserver masks its own breakpoints in. No new
breakpoint-state assertion was added; M4 and M6 exercise arming through tracee
behaviour instead, which is stronger and needs no packet at all.

---

## 6. Local gates

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | no issues |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored |
| direct tracer suite | **13 `CAPTURED`, 0 `SKIP`** — qualified by the verdict line, not the exit status |
| `provider_ipc`, `sandbox_launch` | `CAPTURED open-libc provider IPC`; 4 passed |
| `umbra-platform-macos --lib` | 36 passed (round 0: 35; round 1 adds the exec-drain pin) |

**The `CAPTURED` verdicts come from the dedicated run, not from the workspace one.**
`cargo test --workspace` captures stderr, so a passing fixture's verdict line is
swallowed there; the qualification above is from
`cargo test -p umbra-platform-macos --test fixtures -- --nocapture --test-threads=1`.
That distinction is the whole of lesson 23 in one line: the workspace gate's `ok`
is an exit status, and an exit status is exactly what these cases are able to
produce without running (see the coverage note below).

**The 3 ignored cases are pre-existing, and round 0 named the wrong gate for them.**
They are the NFS **fault-injection** cases in `crates/umbra-storage-nfs/tests/mounted.rs`,
whose `#[ignore]` reasons read *"requires `UMBRA_TEST_NFS_FAULTS=1` and an idle
export"* — a different mechanism from `UMBRA_TEST_SKIP_NFS_MATRIX`, which produces a
runtime `SKIP` rather than an `#[ignore]`. The material claim is unchanged: they are
pre-existing, byte-identical at the parent, in a file not in this diff, and are not
newly ignored work.

**Real provider built and verified by symbols (lesson 27), not by exit status.**
Round 0 reported "45 raw-RPC/NFSv4 symbols" from a command it did not record, and
the reviewer could not reproduce that figure by any defensible pattern. **The count
is withdrawn** — lesson 27 is satisfied by the *named symbols*, not by how many
there are, and a count whose command is lost is not a measurement. One exact
command, reproducible, with its real output:

```
$ cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins
  warning: umbra-storage-nfs-userspace@0.1.0: libnfs raw binding: 16 functions emitted

$ nm target/debug/umbra-storage-nfs-userspace \
    | grep -E " _rpc_(connect_async|service|nfs4_compound_task)$"
00000001001244ec T _rpc_connect_async
0000000100128518 T _rpc_nfs4_compound_task
0000000100123464 T _rpc_service
```

Three named symbols from the linked C library, present as external text. **The
symbol round 0 cited as `_nfs4_compound_task` does not exist**; the binary exports
`_rpc_nfs4_compound_task` (and `_rpc_nfs4_compound_task2`), and round 0's grep
passed only because the cited name matched as a substring of the real one. The
substance — the real libnfs is linked, not a stub — was and remains confirmed.

> **Re-measured in round 1, because the artifact had been replaced.** The
> `--features transport-raw` binary is overwritten by any later default-feature
> `cargo build --workspace --bins`, which round 0 ran afterwards. The symbols above
> were re-measured after rebuilding with the feature, not copied forward.

The tracer provider, same discipline — the binary under test is the code in this
diff and not a stale build:

```
$ nm target/debug/umbra-platform-macos | grep -oE 'Session[0-9]+continue_(thread|absorbed)'
Session15continue_thread
Session17continue_absorbed
$ strings -a target/debug/umbra-platform-macos | grep -c '^vCont;c:$'
1
```

`third_party/libnfs` was absent from this worktree and was fetched at the pinned
commit `18c5c73e` from `libnfs.pin`, as `build.rs` instructs. That directory is
gitignored and is **not** in the diff.

**Two coverage facts that belong in the record rather than in a verdict.**

- **The two newly un-ignored fixtures report `ok` in CI's main `rust` job without
  executing.** `fixture_argv_locked` returns early with `eprintln!("SKIP …")` and
  the test still passes when `UMBRA_TEST_FIXTURE_PATH` / `UMBRA_TEST_REDIRECT_ROOT`
  are unset, unless `UMBRA_INTEGRATION_REQUIRED` is set — and that job sets none of
  them. Before this change they were `#[ignore]`d there and read as *ignored*,
  visibly not run; they now read as *passed* while skipping. Coverage holds, because
  the gate that actually qualifies them is `native-qualification`, which sets
  `UMBRA_INTEGRATION_REQUIRED=1` and makes a skip fatal. **A green `rust` job is not
  evidence that these two ran**, and that is the reason every figure in this
  document is qualified by a `CAPTURED` verdict rather than by an exit status.
- **Waiver (c)'s hazard is exercised nowhere in the suite.** The direct fixtures run
  `interpose: false`, so nothing routes; the routed `umbra run` matrix has no
  multithreaded case. The sibling freeze across a *blocking routed* syscall is
  therefore documented and reasoned about, not measured. It is also what rules out a
  waiver-(c) deadlock as the cause of the `101` below.

**One honest anomaly, undiminished.** One `cargo test --workspace --all-targets` run
exited `101` with its output suppressed, so there is no log of what failed. The
round-1 synthesis recorded it at **7 clean full runs against that 1** — counting
this round's own gates and both reviewers' independent runs — and every run since
has been clean, so the ratio only moves one way. The running total is not restated
here, because writing it down is what invalidates it; the synthesis's figure is the
established one and `cargo test --workspace --all-targets` is the authority on any
later count. Reproduction was attempted directly, including the identical
`clippy && test` chain, and failed.

A **waiver-(c) deadlock is ruled out** as its cause, on the coverage fact above: the
freeze hazard is exercised nowhere in the suite that could have produced it.
**Beyond that, no cause is asserted.** I could not reproduce it and will not invent
one.

**Not runnable here:** `memoria check` refuses this worktree — it requires a git
worktree and this is jj-only (`error [git_unavailable]`). `crates/umbra-platform-macos/README.md`
was updated in the same change as the code it documents, but the documentation
gate's ack state is CI's to judge, and this document does not claim it passed.

---

## 7. Lesson 28 — the self-sweep

This arc's corrections were swept before being handed on. The sweep caught
several things rather than zero — they are named rather than counted, because a
count of this arc's own corrections is precisely what remedy 4 tells this arc not
to state, and round 0 stated one here and got it wrong.

**Round 0's sweep:**

1. **The `single_thread` pin counted its own message.** §3. Caught by the test
   failing, fixed in the source, and documented at the point it bit.
2. **"the slots are not load-bearing for either fixture"** — written into both
   `README.md` and `mt_spawn`'s doc comment on the strength of M2. M2 reverted
   `pending` only; `entry` stayed per-thread. Both were tightened to name the
   `pending` half and to say explicitly that nothing is claimed about `entry`.
3. **"only the resume kept"** in `mt_spawn`'s comment read as though `entry` had
   been reverted too. Rewritten to describe what the probe actually did.
4. **This document cited `native.rs:498` for `single_thread()`** — the ratified
   location, correct at `d8a42def` and wrong in this tree, because the change
   itself moved the line. The added source and README text was swept for the same
   defect and carries **no** line-number citation at all; every reference there
   names a symbol. Remedy 3 again, and the one place it was needed was the
   document describing the change rather than the change.

**Round 1's sweep, over round 0's own corrections:**

- **`SYN-1` — and round 0's sweep did not catch it.** The claim that the converted
  per-thread tripwire pinned the process-wide one-window property was *introduced
  in round 0's correction pass* and propagated to five places across three of that
  round's deliverables. Both reviewers found it independently, by different
  methods. This is §J's generator running exactly to type — the fixed point one
  level out, the author not catching it, a reviewer catching it — and it is now
  again. The remedy applied was not only to correct the five
  sites but to make the property **asserted** (§2.4), so the next revision of the
  prose cannot quietly drift from the code again.
- **`SYN-8` — a count with no command behind it.** "45 raw-RPC/NFSv4 symbols" could
  not be reproduced by any defensible pattern. Withdrawn, not re-derived: §6 now
  records one exact command and its real output, and the claim rests on named
  symbols. Remedy 3, applied to a figure that should never have been a figure.
- **`SYN-9` — a symbol name that passed only as a substring.** `_nfs4_compound_task`
  does not exist; `_rpc_nfs4_compound_task` does. The check passed because `grep`
  found the cited name *inside* the real one — a verification that confirmed itself.
- **`SYN-7`, `SYN-5`, `SYN-6`** — a gate named wrongly, a summary that read as
  exhaustive, and a count that disagreed with its own list. All corrected in place.
- **The `101`'s run count**, caught in this round's own writing: the synthesis's
  figure is stated as the established one and the running total is not, because
  every gate run this document describes invalidates it.

Every one of these is the same shape, and it is the shape §J named: the correction
pass is where the new false claim gets made, and each was a claim about *this arc's
own other deliverable*. The remedies that worked are the recorded ones —
substitute the established value; where stating a value invalidates it, name the
stable part and point at the tool; and where prose claims a property, make the code
assert it so the two cannot drift.

The graph objective's "lesson 28 fired seven times" is not carried forward;
**four** is the figure, per D5, substituted rather than re-derived — as the count of
firings *before* this arc, which is the only reading that stays stable.

**This arc's own additions to it are named above and in `fix-r1.md` / `fix-r2.md`,
and are deliberately not totalled.** Every count of them this arc has attempted has
been wrong, in every round, including the ones written inside the fix for the
previous wrong count. That is remedy 4 arriving as a demonstration rather than as
advice, and the response is to stop producing the number rather than to produce a
better one.
