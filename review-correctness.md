# Review — correctness, round 3 (confirmation pass)

Graph `dg-egt6apy1`, node `review_correctness`, visit 3. Date 2026-09-28.
Tree: change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark
`feat/fork-lifecycle`, parent `master` `7c3ecc8f`. Per lesson 26 the change id is
the only handle cited. Inputs since round 2: `review-synthesis-r2.md`,
`fix-r2.md`. Earlier reports are archived as `…-review-correctness.md` (r1) and
`…-review-correctness-r2.md` (r2).

**Scope, as instructed: this is a confirmation pass, not a third review.** I
confined myself to S1, S2, S3 and non-regression of what S1 touched. R1–R7,
the guardrails, the quantitative claims and D1–D4 were not re-derived.

---

## Verdict: **CLEAN PASS. Publish.**

All three items are closed. S1's chosen shape answers the finding I raised, and
the implementer's reasoning for choosing it is **correct and better than the
alternative I offered** — I say that as the person who offered the alternative.
S2 and S3 are fixed, and I re-read every prose line this round touched against
the code it describes: **no fifth instance of lesson 28.**

**No blockers. No follow-ups that are not already inside the ratified
multithreaded arc.** One observation is recorded below, flagged explicitly as
*not* a finding.

---

## 1. S1 — adjudicated on the merits

### Is the three-things claim true? **Yes. Verified by reading the helpers.**

`return_stop` (`native.rs:538-597`) does, in order: release the entry site,
plant a gate, then assign `pending` unconditionally. A second `return_stop`
before the first is consumed therefore loses all three riders, and I checked each
against the code rather than the argument:

* **`entry_breakpoint`** — `remove_breakpoint` (`native.rs:472-481`) sends
  `z0,<addr>,4`, removes the address from `self.breaks`, and **returns** the
  `Breakpoint` value. `finish_return` is the only thing that hands it back, via
  `install_breakpoint(pending.entry, pending.entry_breakpoint)`
  (`native.rs:461-471`). So losing the value leaves the site released and out of
  `self.breaks` — and `stop()`'s dispatch is `if s.breaks.contains_key(&pc)`, so
  that stub stops being intercepted for the rest of the run. **Claim true.**
* **`gate`** — the gate breakpoint is retired only in `finish_return`
  (`remove_breakpoint(pending.gate)`, or `z1` when hardware). After a clobber
  `pending.gate` names the *second* gate, so the first stays registered and is
  never retired. **Claim true.**
* **the `Exec` candidate** — losing it leaves `twin` naming the previous image,
  `retarget_interposer` pointing at it, `install()` matching nothing, and the
  exec'd image half-mediated. That is exactly the S1 mechanism I described in
  round 2. **Claim true.**

**And the corrected claim is the correct one.** The implementer's own re-read
caught its first draft saying the entry breakpoint is "simply leaked". Reading
`remove_breakpoint`: it sends `z0`, so debugserver restores the original
instruction and nothing leaks in memory; what is lost is the *value*
`install_breakpoint` needs to re-arm the site. The shipped comment
(`native.rs:546-552`) now says exactly that. I verified the correction against
the two helpers, not against the claim.

**One refinement that strengthens rather than contradicts the comment**, found
while checking it: the clobber is silent only when the two in-flight syscalls sit
at *different* stub addresses. If both threads are at the same stub, the second
`return_stop`'s `remove_breakpoint(pc)` finds the address already gone and
returns `Err("session does not own {address:x}")`, so the run stops loudly. So
there is a partial natural guard on the same-stub case. The comment's claims
remain true for the different-stub case, which is the one that matters.

### Is the reasoning for declining the dedicated field sound? **Yes, and it is a better argument than mine.**

I offered `exec_candidate: Option<PathBuf>` in round 2 as "clobber-proof *and*
failed-exec-proof". That framing was wrong in one respect, and the implementer
identified the respect precisely: a dedicated field would rescue **the newest of
three riders** while leaving the two older ones broken. The session's breakpoint
bookkeeping is already corrupt at that point — a stub silently un-intercepted and
a gate never retired — so the field is a partial repair of a corrupt state, not a
fix. It would also have been the only one of the three riders to get a rescue,
which is an odd shape to leave behind.

The assertion does the thing that is actually wanted: it names the precondition
**for all three riders**, at the one site that can violate it. That is the better
choice, and the implementer's reading of my "already broken by other means" note
as an argument against the field rather than for it is a fair reading of what I
wrote and a better conclusion than the one I drew from it.

### Is `debug_assert!` sufficient, or does the path deserve a release guard? **Sufficient for this slice.**

I weighed the release guard seriously, because "it's only a debug assert" is
exactly the excuse that would let a hollow hardening ship. Four things decide it:

1. **A release-mode refusal would be a behaviour change on a shipped path,
   inside a final confirmation pass.** The invariant is pre-existing — the slot
   carried `entry_breakpoint` and `gate` before R1 added the candidate — so a
   release guard would newly stop runs that limp today. Some multithreaded runs
   plausibly do limp: an un-re-armed stub the program never calls again costs
   nothing. Converting those to hard failures is not a confirmation-pass change.
2. **The only route is a multithreaded tracee**, and I re-confirmed the two
   supporting facts rather than taking them: `continue_run` sends `c`
   (`native.rs:493-497`), which resumes every thread, and `single_thread()` has
   **exactly two call sites** — `Delivery::Fork` (`native.rs:1922`) and
   `WaitPlan::Park` (`native.rs:1967`) — neither covering `Delivery::Exec` or a
   plain `Namespace` call.
3. **The multithreaded arc is ratified and scheduled**, and it must revisit this
   slot wholesale. An assertion waiting there is precisely what that arc wants.
4. **In release it compiles out**, so it cannot regress anything.

**What the assertion does not do, said plainly so nobody is surprised at
`merge_gate`: it detects and documents; it does not prevent.** In a release
build the S1 symptom — silent half-mediation of an exec'd image after a clobber —
remains reachable. That is **not a blocker and not a new follow-up**: it is
inside the multithreaded arc's scope by construction, and that arc is already a
sequenced commitment in the ratification record.

### Is the assertion hollow? **No — CONFIRMED by execution, independently.**

I did not take the implementer's negation probe on trust; I ran my own. Negated
`self.pending.is_none()` to `self.pending.is_some()`, rebuilt
(`cargo build --workspace --bins`), and ran the macOS fixtures suite:

```
occurrences of "a second intercepted syscall entered while one was still in flight": 11
test result: FAILED. 0 passed; 11 failed; 0 ignored; 0 measured; finished in 5.79s
```

Eleven firings across eleven cases. So the assertion is **reached**, is
**compiled in** under the test profile, and sits on a **hot path** rather than a
dead one. That also independently confirms the "no `[profile]` override in the
workspace `Cargo.toml`" claim — I checked the file too, and there is none — since
a compiled-out assertion could not have fired at all.

Restored, rebuilt, and **verified by execution** rather than assumed: `grep -c`
for the probe marker returns 0, the file's SHA-1 matches the pre-probe copy, and
the suite is green again with the assertion firing zero times:

```
CAPTURED argv0-check … CAPTURED wnohang-wait          (11 CAPTURED)
test result: ok. 11 passed; 0 failed; … finished in 11.88s
```

---

## 2. S2 and S3, and the lesson-28 re-read

### S2 — corrected, and consistent with both other statements in the file

`umbra-userspace-edges.c:739-748` now says the image is an ordinary arm64
executable with its execute bits cleared, that `execv` on it fails
**`EACCES (13)`**, and — pointing at the file's own `failedexec` header entry —
that a dylib *does* fail `execv` with `ENOEXEC` but umbra's resign refuses it
earlier, so the operand is never rewritten and nothing is exercised.

Checked against the code, not the claim:
* `unexecutable_image` (`userspace_run.rs:434-441`) sets
  `Permissions::from_mode(0o644)`. ✓
* `cache::resign(...)?` (`native.rs:1979-1983`) precedes the operand rewrite
  `set(&mut regs, slot, s.allocate(&bytes)?)?` in the same arm, so a refusal
  really does land before the register is touched. ✓

All three statements in the tree — header entry, case comment, driver — now
agree, and the near-miss is recorded rather than deleted, which is the right
outcome for a dead end that cost real time.

### S3 — corrected, and every clause matches what I measured

`userspace_run.rs:1498-1510` replaces the false "never on the tracee's exit
status" with an accurate account of what each assertion is worth. Every clause
checks out against my own round-1 measurements:

| New claim | My measurement |
|---|---|
| exit status is checked first | first `assert_eq!` in the body ✓ |
| for this defect it discriminates: 9 (`EBADF`) unfixed, 0 fixed | round-1 Mutation B: `Code(9)`, exit assert fired first ✓ |
| exit cannot establish whether bytes reached the store | ✓ |
| the broken tree leaves a name here too — an empty one | round-1 Mutation B read `left: []`; the `.expect` on presence did **not** fire ✓ |

### The lesson-28 check — no fifth instance

I read every prose line this round touched against the code, and I established
the boundary mechanically rather than trusting the file list. Diffing this
round's delta:

```
native.rs                      38 +   0 -      (assertion + comment only)
userspace_run.rs               16 +/-          (/// lines only)
umbra-userspace-edges.c        11 +/-          (one /* */ block only)
```

Isolating non-comment additions in `native.rs` yields exactly five lines — the
`debug_assert!` itself and nothing else. The other two files' deltas are entirely
inside comment syntax.

Spot-checks of `fix-r2.md`'s twelve-claim table (I checked seven rather than
accepting the table):

| Claim | Checked | Verdict |
|---|---|---|
| `pending` assignment unconditional | `return_stop` body | holds |
| entry site released before the assignment | `remove_breakpoint` sends `z0`, precedes it | holds |
| `gate` never retired on clobber | `finish_return` is the only retirer | holds |
| `single_thread()` does not gate `Exec`/`Namespace` | exactly 2 sites, `:1922` and `:1967` | holds |
| live in every `cargo test` | no `[profile]` in workspace `Cargo.toml`, **and** my negation fired 11× | holds |
| the corrected "not leaked, but never re-armed" | `remove_breakpoint` + `install_breakpoint` | holds |
| resign refuses the dylib before the rewrite | `native.rs:1979` precedes the `set(...)` | holds |

**One clause I would have phrased differently, and it is not a finding.** The
comment says moving the candidate to a field "would hide the violation rather
than surface it". Taken as a contrast between the two shapes that were actually
offered — a field with no assertion, versus an assertion with no field — that is
fair, because in the first the violation goes undetected. Taken literally on its
own, the candidate's loss is silent today too, so a field would not have hidden
anything currently surfaced. The surrounding sentences make the comparative
reading the natural one. Recording it so the phrasing is a choice rather than an
oversight; it needs no edit.

---

## 3. Non-regression of what S1 touched

This is the item the driver explicitly had not checked, so I checked it
mechanically rather than by inspection.

```
deleted lines in native.rs this round: 0
non-comment added lines in native.rs this round:
+        debug_assert!(
+            self.pending.is_none(),
+            "a second intercepted syscall entered while one was still in flight: \
+             this session's pending entry breakpoint, return gate and exec \
+             candidate would all be overwritten"
+        );
```

**`return_stop`'s runtime behaviour is otherwise unchanged.** Zero deletions, and
the only executable addition is an assertion that is a no-op unless violated —
and that compiles out entirely in release. Nothing in the ordering (release the
entry site → plant the gate → assign `pending` → `continue_run`) moved. The
`Pending` construction, the `hardware` branch and `continue_run` are untouched.

Behavioural confirmation, live: `exec-write`, `fork-write`, `grandchild-write`,
`posix-spawn-write` and `dup-inherit-write` all `CAPTURED`, and both at-risk
routed fork cases pass — below.

---

## 4. Live re-qualification

### Lesson 27 (corrected) reproduced, by size **and** symbols, before the run

```
after `cargo build --workspace --bins`                          : 5,096,720 bytes
after `-p umbra-storage-nfs-userspace --features transport-raw --bins`: 5,888,624 bytes
```

Both figures match `fix-r2.md` **to the byte**, so the trap is real and
reproducible: `--workspace --bins` genuinely reverts the provider to the
featureless build. Symbol counts on the `transport-raw` binary: 4,910 by
`nm | grep -ci nfs`, 10,310 by `nm -a | grep -ci nfs`. The implementer's 10,041
is the same order by a slightly different counting method — the two do not
disagree about the binary, and the byte sizes, which are exact, are what settle
it. The provider was confirmed to *be* the `transport-raw` build before the
routed run, not merely confirmed to exist.

### Routed suite, live Ganesha, verdicts read by name

Fixture: `umbra-m1-transport-raw-ganesha`, `Up 2 hours (healthy)`,
`127.0.0.1:12105`; `UMBRA_INTEGRATION_REQUIRED=1`.

```
test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 80.24s
22 executed live | 6 declared probe SKIPs | debug_assert fired: 0
```

**All six new/changed cases executed — none skipped:**

```
a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran   ... ok
a_chdir_answers_enoent_and_enotdir_to_the_tracee_and_anchors_a_relative_operand      ... ok
a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image            ... ok
a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against ... ok
a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it         ... ok
a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence     ... ok
```

**Both at-risk pre-existing fork cases executed:**

```
a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes          ... ok
a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store ... ok
```

Six `SKIP` lines, all declared mutation probes. **The `debug_assert` fired zero
times** — `grep -c` for its message across the whole routed log returns `0`,
across all live cases, on a build where I have independently proved it is live.

`cargo fmt --check`: clean, exit 0.

### Restoration verified by execution

The only mutation this round was my negation probe. Restored, and verified by
running rather than assuming: file SHA-1 back to the pre-probe value,
`grep -c "NEGATED PROBE"` = 0, macOS fixtures suite green with 11 `CAPTURED` and
zero firings, and the routed suite above run at the final tree state.

---

## Findings

**None.** No blockers and no follow-ups beyond what the ratified multithreaded
arc already owns.

One thing recorded for `merge_gate`, explicitly **not** a finding and requiring
no action before publish:

> The `debug_assert!` detects and documents the one-in-flight-per-session
> invariant; it does not enforce it. In a release build the S1 symptom stays
> reachable for a multithreaded tracee. This is correct for this slice — a
> release guard would change behaviour on a shipped path — and it is inside the
> scope of the multithreaded arc, which must revisit the `Pending` slot anyway.
> Worth one line in that arc's brief so the tripwire is found rather than
> rediscovered.

## Where my confidence is weakest

* I did not construct a multithreaded tracee to observe a real clobber. That
  remains the one link in S1 established by reading rather than by execution,
  unchanged from round 2 and unchanged by the fix — and building it *is* the
  next arc.
* Per the scoping I did not re-derive R1–R7, the four guardrails, the 832/20/12
  counts, or D1–D4. Round 2 closed R1–R7 against the code and `review_scope`
  cleared the quantitative claims; nothing this round touched them, which I
  confirmed by the zero-deletion, comment-only diff above.
* The symbol-count method differs from the implementer's; the byte sizes, which
  match exactly, are what I relied on.

---

## Summary

| Item | Status | Evidence |
|---|---|---|
| **S1** shape — assertion over dedicated field | **Closed. The reasoning is sound and better than my own suggestion** | three-riders claim verified against `remove_breakpoint` / `install_breakpoint` / `finish_return` |
| **S1** sufficiency — debug-only | **Sufficient for this slice** | pre-existing invariant; only route is multithreaded; `single_thread()` has exactly 2 sites; compiles out in release |
| **S1** non-hollowness | **CONFIRMED by my own execution** | negated → fired 11× across 11 cases; restored → 11 `CAPTURED`, 0 firings |
| **S2** `ENOEXEC` → `EACCES(13)` | **Closed** | `unexecutable_image` `0o644`; resign precedes the operand rewrite |
| **S3** "never on the exit status" | **Closed** | every clause matches my round-1 Mutation B measurements |
| **Lesson 28** — no fifth instance | **Confirmed** | 7 of 12 tabulated claims spot-checked against code; comment-only diff proven mechanically |
| **Non-regression** of `return_stop` | **Confirmed** | 0 deletions; 5 non-comment added lines, all the assertion |
| **Live re-qualification** | **Clean** | provider verified by size before the run; all 8 named cases `ok`; 6 declared probe SKIPs; assertion fired 0× |

**Clean pass. Publish.**
