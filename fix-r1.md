# Fix — round 1 (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `fix`, round 1. Date 2026-09-29.
Change **`qqxtnynk`**, bookmark `feat/mt-closure`, parent `d8a42def`.
Input: `/tmp/graph-dg-0ved1w0e/review-synthesis-r1.md`, with `review-correctness.md`
and `review-scope.md` in the tree as its evidence. All three read end to end before
any edit.

> **The commit id is not stated.** It advances on every snapshot, and writing this
> file is a snapshot. `jj log -r qqxtnynk` is the authority; the stable identifiers
> are the change id and the bookmark. Both reviewers hit this and handled it the
> same way.

**Both reviews passed.** `review_correctness` PASS WITH FINDINGS (7),
`review_scope` PASS — inside the ratified envelope (6). Neither found a live defect
in the shipped tree and neither found a scope breach. This round fixes one false
claim in shipped source, adds the two guards that were missing, and corrects a set
of figures and wordings.

---

## 0. Headline: the blocking finding was introduced by the previous correction pass

`SYN-1` is not a defect in the mechanism. It is a **false justification**, written
during round 0's own correction pass, propagated to five places across three of
that round's deliverables, and caught by two reviewers independently — by reading
and by measurement — and by neither author sweep.

That is §J's generator running exactly to type, again.
So the remedy is not only to correct the five sites. **The property the prose
claimed is now asserted in code**, so the next revision of the prose cannot drift
from it silently again.

---

## 1. Disposition of every finding

| # | Sev | Disposition |
|---|---|---|
| **SYN-1** | MED, blocking | **FIXED**, both parts. Five sites corrected; a second `debug_assert!` added **alongside** the per-thread one. §2 |
| **SYN-2** | MED | **FIXED.** The absorb pin now asserts the helper's body, and M7 was re-run against it. §3 |
| SYN-3 | LOW-MED | **RECORDED**, plus a cheap source pin on the drain. No exec fixture added — that is deliberate. §4 |
| SYN-4 | LOW | **FIXED** (wording). §5 |
| SYN-5 | LOW | **FIXED** (wording). §5 |
| SYN-6 | LOW | **FIXED** (wording) — and it recurred once during this round's own edit, caught here. §5 |
| SYN-7 | LOW | **FIXED** (wording). §5 |
| SYN-8 | LOW | **FIXED** (figure) — the count is **withdrawn**, not re-derived. §5 |
| SYN-9 | LOW | **FIXED** (figure). §5 |
| SYN-10 | LOW/INFO | **RECORDED** in `impl.md` §6. §6 |
| SYN-11 | INFO | **RECORDED** in `impl.md` §6, and it is what rules out a waiver-(c) deadlock as the `101`'s cause. §6 |

---

## 2. SYN-1 — the false claim, and the assertion that now stands behind it

### Part A — the five sites

The converted tripwire is `debug_assert!(!self.pending.contains_key(&thread), …)`:
**per-thread**. Five places credited it with pinning *"at most one window is open at
a time"* **process-wide** — which is the property `continue_absorbed`'s
`pending.values().next()` needs to be well-defined. It does not pin that. Two
windows on two *different* threads pass a per-thread check untouched, and that is
precisely the state that would make the pick ambiguous.

What establishes the property is the **freeze**: the thread `return_stop` resumes is
the only one running, so nothing is left to reach `return_stop` and open a second
window. All five now say so:

| # | Site | Now says |
|---|---|---|
| 1 | `native.rs`, `return_stop`'s comment | the per-thread check does *not* establish it; the freeze does; the new assertion is a tripwire over that structure |
| 2 | `native.rs`, `Session::pending` field doc | "the freeze is what makes that true … a structural consequence of which threads run, not of any check" |
| 3 | `native.rs`, `continue_absorbed`'s doc | names `values().next()` explicitly, credits the freeze, and **records that an earlier revision of that comment got it wrong** |
| 4 | `README.md` | same correction, and states what a per-thread check does and does not catch |
| 5 | `impl.md` §2.4 | rewritten around the correction rather than patched |

The scope reviewer's observation that the `native.rs` comment *"states the correct
mechanism in its own first sentence and then misattributes it in the next"* was the
right read, and the corrected text builds on that first sentence rather than
replacing it.

### Part B — the assertion. **Additive form taken. No deviation to flag.**

```rust
debug_assert!(!self.pending.contains_key(&thread), …);   // ratified, per-thread
debug_assert!(self.pending.is_empty(), …);               // what continue_absorbed needs
```

**I did not replace the per-thread check, and I do not judge replacement better.**
The synthesis left the choice open with a preference; the preference is correct on
its own terms, and on the merits too:

- #135 sub-item 1 and ratification D3 require the tripwire **converted to a
  per-thread invariant**. Swapping it for `is_empty()` would undo exactly what was
  ratified, to gain a condition that can simply be added instead.
- The two conditions have **different diagnoses**, and both are reachable. The
  stronger subsumes the weaker, so only the first to fail is reported — which is the
  right order: a same-thread double-open is a transaction-slot bug and gets the
  specific message about the entry breakpoint, gate and exec candidate; a
  cross-thread one means **the freeze itself has broken**, which is a different and
  worse failure, and its message says so.
- The per-thread form is also the one that stays meaningful after the deferred
  `single_thread()` removal, when a second concurrent window becomes reachable and
  `is_empty()` would have to be relaxed.

**So there is nothing for `merge_gate` to put to the human on this item.**

**Measured.** The new assertion fired **0 times** across three full fixture runs
(13 `CAPTURED` each, 0 `SKIP`), which matches what the correctness reviewer measured
with an equivalent probe before it existed. The property holds; it is now asserted
rather than only reasoned about.

---

## 3. SYN-2 — the subtlest decision now has a guard, and the guard discriminates

As shipped in round 0, `a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings`
asserted only that `stop()`'s two **call sites** route through `continue_absorbed`.
Nothing asserted that `continue_absorbed` does anything. The correctness reviewer
gutted it to an unconditional `self.continue_run()` — reintroducing the exact escape
it exists to prevent — and **13/13 fixtures captured across five runs and every unit
test passed, including that pin under its own name.**

The test now asserts the helper's body as well: that it consults `pending`, and that
it calls `continue_thread`. Both assertions carry messages naming what their absence
costs.

**Verified to discriminate, rebuilt between mutations (lesson 24).** M7 and M8 were
each re-run against the round-1 pins. Each fails **only** its own pin:

```
M7  (continue_absorbed → unconditional bare `c`)
    a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings ... FAILED
    the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread ... ok
    test result: FAILED. 35 passed; 1 failed

M8  (exec candidate looked up by the stopping thread)
    the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread ... FAILED
    a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings ... ok
    test result: FAILED. 35 passed; 1 failed
```

Neither pin is a tautology, and neither catches the other's mutation. That check
exists because M3 taught this arc that a pin which cannot fail is worse than no pin.

---

## 4. SYN-3 — recorded, pinned cheaply, and deliberately not closed

The exec drain is **measured necessary**: at an exec stop the stopping thread is not
the window owner, because an `execve` replaces the address space and its threads.

But M8 — keying the candidate lookup back on the stopping thread — passes **13/13**,
because the only fixture that reaches an exec stop **re-execs the same twin**
(`before == adopted`, measured by the reviewer). The candidate it drops equals the
value already there, so dropping it is a no-op and the documented consequence never
materialises. **No fixture in this crate discriminates twin adoption at all.**

- **Pinned cheaply**, as the synthesis permits: a new
  `the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread`
  asserts the drain and refuses a keyed lookup. Its doc comment states plainly that
  it stops the natural-looking wrong edit the per-thread rekeying made available,
  and proves nothing about twin adoption itself.
- **No exec fixture was added.** A case that execs a *different* image would close
  this properly, and that closes a **pre-existing #117/#134 gap** — its own arc,
  outside this ratification. Not done here.

> **Follow-up candidate for `merge_gate`:** a direct-tracer fixture that execs an
> image other than its own twin, to discriminate `ReturnKind::Exec` candidate
> adoption. Pre-existing gap; not introduced by this change; newly worth filing
> because the per-thread rekeying created a plausible wrong edit that this suite
> would wave through.

---

## 5. SYN-4 to SYN-9 — wordings and figures

**SYN-4.** `Session::pending`'s doc justified the map by *"which thread owns a window
has to be answerable"*. A single slot answers that through `Pending::thread` — M2
demonstrated it, with both fixtures green. Rewritten to what is actually true: the
map is what lets the per-thread invariant be **stated**, and it becomes load-bearing
at the deferred `single_thread()` removal. `impl.md` §2.2 gained the same correction
so the two do not disagree.

**SYN-5.** `impl.md` §3's *"the two fixture changes are the removal of two
`#[ignore]` attributes"* now reads *"the two `#[ignore]` removals plus the two
doc-comment corrections recorded in §7"*.

**SYN-6.** §7's "three things / four items / All three" is gone. The items are
**named, not counted** — a count of this arc's own corrections is what remedy 4 tells
this arc not to state, which is how the defect arose.

> **It recurred inside this round's own edit.** Adding the SYN-4 item to `impl.md`
> §2.2 left its intro saying *"Three are worth naming"* over four numbered items.
> Caught in this round's sweep, before handing on. Same generator, one paragraph
> later — which is the most concrete evidence available that §J's finding is about a
> mechanism and not about carelessness.

**SYN-7.** The 3 ignored cases are NFS **fault-injection** cases gated by
`UMBRA_TEST_NFS_FAULTS=1`, not by `UMBRA_TEST_SKIP_NFS_MATRIX` (which produces a
runtime `SKIP`, not an `#[ignore]`). Verified at source in
`crates/umbra-storage-nfs/tests/mounted.rs`. The material claim — pre-existing,
byte-identical at the parent, file not in this diff — is unchanged.

**SYN-8. The count is withdrawn rather than re-derived.** "45 raw-RPC/NFSv4 symbols"
came from a command round 0 did not record, and the reviewer got 51 / 43 / 48 by
three defensible patterns. Lesson 27 is satisfied by the **named symbols**, not by
how many there are, and a count whose command is lost is not a measurement.
`impl.md` §6 now records one exact command and its real output.

> **The artifact had to be rebuilt to re-measure it**, and that is worth recording:
> the `--features transport-raw` binary is overwritten by any later default-feature
> `cargo build --workspace --bins`, which round 0 ran afterwards. Checking the
> symbols at the end of round 0 would have found **zero** — not because the build
> failed, but because the file had been replaced. Lesson 27's own failure mode,
> arriving by a route it does not name.

**SYN-9.** `_nfs4_compound_task` does not exist. The binary exports
`_rpc_nfs4_compound_task` and `_rpc_nfs4_compound_task2`; round 0's `grep` passed
because the cited name matched as a **substring** of the real one — a verification
that confirmed itself. Full names now cited, in a command that anchors them with
`$`.

---

## 6. SYN-10 / SYN-11 — records, and the `101`

Both are now in `impl.md` §6 as coverage facts rather than verdicts.

**SYN-10.** The two newly un-ignored fixtures report `ok` in CI's main `rust` job
**without executing**: `fixture_argv_locked` — which `fixture_argv` is a
lock-taking wrapper over, and which `mt_fixture` calls directly — returns early
after `eprintln!("SKIP …")` when
the two fixture env vars are unset, and the test passes unless
`UMBRA_INTEGRATION_REQUIRED` is set — which that job does not set. Before this change
they were `#[ignore]`d there and read as *ignored*, visibly not run; they now read as
*passed* while skipping. Verified at source. **Coverage holds** — the gate that
qualifies them is `native-qualification`, which sets `UMBRA_INTEGRATION_REQUIRED: "1"`
and makes a skip fatal. Recorded plainly: **a green `rust` job is not evidence these
two ran.**

**SYN-11.** Waiver (c)'s hazard is exercised nowhere in the suite. The direct
fixtures launch with `interpose: false`, so nothing routes; the routed matrix has no
multithreaded case. The sibling freeze across a *blocking routed* syscall is
documented and reasoned about, **not measured**. Verified at source.

**The `101` disclosure is not softened.** The synthesis's figure — **7 clean full
runs against that 1** — is carried as the established value. The running total is not
restated, because writing it down is what invalidates it; every gate run this arc
describes moves it. A **waiver-(c) deadlock is ruled out** as its cause, on SYN-11:
the freeze hazard is exercised nowhere that could have produced it. **Beyond that no
cause is asserted**, and reproduction was attempted directly, including the identical
`clippy && test` chain.

---

## 7. Gates, re-run

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, no issues |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored (the pre-existing NFS fault-injection cases) |
| direct tracer suite | **13 `CAPTURED`, 0 `SKIP`** on every run I made — but see the note below: this figure did **not** reproduce for a reviewer on the same tree, and round 2 found out why |
| `provider_ipc` / `sandbox_launch` | `CAPTURED open-libc provider IPC` / 4 passed |
| `umbra-platform-macos --lib` | 36 passed (35 in round 0, plus the exec-drain pin) |

> **The `3/3` in this table's original form was true for me and false for a
> reviewer on the same tree, and naming only one of those would have been the
> dishonest form.** The scope reviewer saw 13/13, then 5 failed, then 2 failed, then
> eight clean runs. Round 2 measured the difference rather than disclosing it:
> `fix-r2.md` carries the result. The short version is that the runs were not
> independent of each other — a second suite running concurrently on the same
> machine is not a neutral observer of the first — and that neither figure was
> wrong about what its author saw.

**Qualified by `CAPTURED`, not by exit status (lesson 23).** The verdict lines come
from `cargo test -p umbra-platform-macos --test fixtures -- --nocapture
--test-threads=1`; the workspace run captures stderr and swallows them, and its `ok`
is exactly the signal SYN-10 shows these two cases can produce without running.

**Rebuild discipline (lesson 24).** Every mutation in §3 was applied, rebuilt with a
confirmed `Compiling umbra-platform-macos` line, run, and reverted. The transport-raw
provider was rebuilt before its symbols were re-read, for the reason in §5.

**Guardrails re-verified after this round's edits.** The audit's §L pin set is
byte-identical against the parent in production source, checked mechanism by
mechanism rather than as a total — §L is the enumeration and this document does not
restate it as a count, for the same reason SYN-8's figure was withdrawn. The
`z0`/`Z0` filter over the whole diff still returns **nothing**: the second assertion
added in §2 sits above the dance and does not touch it.

---

## 8. Lesson 28 — this round's own sweep

Applied to this round, since a correction pass over a correction pass is the highest-
risk site in the arc and the arc's record is that the author does not catch these.

- **The SYN-6 recurrence, caught.** §5. One paragraph after fixing a count, this
  round wrote another. Fixed before handing on, and recorded rather than quietly
  corrected, because the recurrence is the evidence.
- **The `101` run count, not stated.** Round 0's disclosure would have become false
  the moment this round ran its gates. The synthesis's established figure is cited;
  the current total is left to the tool. Remedy 3.
- **SYN-8's count withdrawn, not replaced with a better count.** The temptation in a
  correction pass is to produce the right number. The right move was to notice the
  number was never the evidence.
- **A wrong function name in this document, caught by that re-verification.** §6
  first said `fixture_argv` returns early; the early return is in
  `fixture_argv_locked`, which `fixture_argv` wraps. Both reviews had it right and
  `impl.md` had it right — this document introduced the error while restating them.
  Which is the point: the restatement is the risky act, not the original.
- **Every claim re-verified at source rather than taken from the reviews.** SYN-7's
  `#[ignore]` reasons, SYN-9's symbol names, SYN-10's early-return and the CI job
  that sets `UMBRA_INTEGRATION_REQUIRED`, SYN-11's `interpose: false` — each read
  from the tree in this round, because a review is as likely a site for a new false
  claim as a correction is, and both reviewers said so about their own documents.
- **No line numbers were added.** The source and README text added across both
  rounds carries none; every reference names a symbol. The line citations in
  `impl.md` are all qualified to `d8a42def` or to files this change does not touch.
