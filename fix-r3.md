# Fix — round 3 (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `fix`, round 3. Date 2026-09-29.
Change **`qqxtnynk`**, bookmark `feat/mt-closure`, parent `d8a42def`.
Input: `/tmp/graph-dg-0ved1w0e/review-synthesis-r3.md`, with the current
`review-correctness.md` and `review-scope.md` in the tree as its evidence.

> The commit id is not stated; writing this file moves it. `jj log -r qqxtnynk` is
> the authority. The worktree-root `review-synthesis-r*.md` files belong to **other
> arcs** — this arc's syntheses live only under `/tmp/graph-dg-0ved1w0e/` and in
> `knowledge/umbra/graph-audits/dg-0ved1w0e-*`.

**Both reviews pass. Both endorse option (a) on the reaper with no escalation.
Neither requires a code change or re-ratification.** `native.rs` and `fixtures.rs`
were byte-identical to the round-2 tree at line level, so the mechanism was settled
before this round began and carries forward by construction.

**This round is one harness comment plus the corrections listed in §2.** The one
thing that changes a certified file is stated up front, in §1, so `review_scope` knows
to re-verify it.

---

## 1. SYN3-1 — option (i). **`fixtures.rs` is edited, and it is comment-only.**

`fix-r2.md` committed to documenting the hazard in the harness and the diff did not
contain that text. Scope's argument is the right one: process documents ship to
master, but nobody reads them before running a suite, so a developer would never see
the warning. **Document and delivery must agree**, and the cheaper of the two fixes is
to deliver.

**What changed.** The doc comment on `FIXTURE_LAUNCH` in
`crates/umbra-platform-macos/tests/fixtures.rs`. That is where #134 said the notice
would have to go — *"if that ever changes, this is the place that has to change with
it"* — and it is the comment a developer reading the harness passes on the way to the
reaper.

The note records: what the reaper reaches (every process on the machine matching the
fixture basename); why it now runs when it did not before (the two `#[ignore]`
removals); the measured symptom (`SIGKILL` surfacing as `fatal signal/exception:
T09`, versus zero reaps when the suite is run twice in a row); the practical
instruction — **do not run two fixture suites concurrently on one machine**, and if a
case dies with `T09` look for the other suite before looking at the tracer; why CI is
unaffected, measured; and why it is not repaired here.

**For `review_scope`'s re-verification, the two invariants it certified:**

- **Comment-only.** Stripping comments and blank lines from `fixtures.rs` and diffing
  against `d8a42def` still returns **exactly six changed lines**, and they are the two
  `#[ignore]` attributes' six lines. No code, no behaviour, no test.
- **The reaper's logic is untouched.** `StrayFixtureChildren`, its `Drop` and
  `fixture_named_processes` are byte-identical to master, as both reviewers verified.
  Deferred item 5's machinery was not opened.

**One thing the note also fixes, and it is a find rather than a chore.** The same doc
comment ended with *"`mt_write` and `mt_spawn` panic **by design** on every run, so a
poisoned mutex is the normal state after them"*. That was true at master and **this
change falsified it** — they pass now. It is the identical shape as SYN2-3 (§3): a
code edit silently making prose false, in a file whose sweeps were all checking that
it had not *changed*. Both reviewers certified `fixtures.rs` byte-identical and both
were right; byte-identity is not truth-preservation when the thing that moved is
somewhere else. The sentence now states the guard's purpose without resting on a
premise this change removed.

---

## 2. Disposition per finding

| # | Disposition |
|---|---|
| **SYN3-1** | **FIXED by delivering, option (i).** Harness note written into `fixtures.rs`; comment-only; reaper logic untouched. §1 |
| **SYN3-2** | **FIXED. No replacement number supplied.** §3 |
| **SYN3-3** | **FIXED — mechanism restated** as §J's original generator. §3 |
| **SYN3-4** | **FIXED — the instances are named, not counted.** §3 |
| **SYN3-5** | **FIXED — 70**, with the per-configuration breakdown so it is checkable. §3 |
| Reaper grounds | **FOLDED IN** — #134's recorded premise, the measured CI result, scope's limb-2 correction, and the tracking-issue contents. §4 |
| Reviewer disclosures | **RECORDED** for the merge-gate record. §5 |

---

## 3. The four counts, corrected — and recorded as having been wrong

All four sat in `fix-r2.md` §6, the section whose subject is wrong counts. The
corrections are made **in place, with the error left visible beside them**, rather
than producing a clean-looking section that hides its own history. That is the same
choice the driver made in annotating synthesis r2 rather than rewriting it.

**SYN3-2 — "wrong for five consecutive rounds".** Unsupported: it exceeds the number
of rounds this arc has authored, and the evidenced instances are two rounds of
handed-on text plus one caught in flight. **No replacement figure is supplied** — both
reviewers deliberately withheld one and were right to, and a figure stated here would
be one more count of this arc's own corrections written into a document that is itself
one of them. The row now says that the number is itself a count of
this arc's own corrections, records that "five" originated as "fourth" in a review and
was escalated in a synthesis, and records that it reached a table about wrong counts
past an author sweep, two reviewers and the driver.

**SYN3-3 — the mechanism was misnamed, twice.** Round 2 called SYN2-3 *"SYN-5's exact
shape, in the paragraph where SYN-5 was fixed"*. Wrong on both limbs: the sentence is
byte-identical at rounds 0 and 1, and the SYN-5 fix changed the **adjacent** sentence.
The accurate mechanism is §J's **original** generator — *the round's own other
deliverable joining the diff invalidates what the round just corrected*. Here
specifically: a **code** edit (round 1 adding assertions to the absorb pin, a test
this change created) silently falsified prose **in a different file**. No sweep of
either file alone could have caught it, which is what makes it worth recording. It is
also, exactly, the shape of the `FIXTURE_LAUNCH` poisoning sentence found in §1 — the
generator firing twice on the same change by the same route.

**SYN3-4 — "the two instances that were caught", then four named.** The catalogued
defect occurring inside the paragraph cataloguing it. The instances are now **named**,
and the split they illustrate is kept because it is the actionable part: a **different
reader than the writer** caught SYN-1, SYN2-3 and the dispatch's own mis-pointed
synthesis; **sweeping the round's own new text rather than its inputs** caught the
SYN-6 recurrence and `fix-r1.md`'s wrong function name. Neither is "the writer looked
harder". The sentence now records that it previously said two and named four.

**SYN3-5 — "forty-odd", and a later "42". The total is 70.** Counted from the log
files the experiment left, which is the artifact, not a memory:

| configuration | runs |
|---|---|
| master, sequential | 15 |
| branch, sequential | 15 |
| master ‖ master, 6 pairs | 12 |
| master ‖ branch, 8 pairs | 16 |
| branch ‖ branch, 6 pairs | 12 |
| **total** | **70** |

`42` is `15 + 15 + 12` — the two sequential sets plus one concurrent set, with the
other two concurrent sets' 28 runs dropped. `fix-r2.md` §2.1 now carries the run
column, so the total is checkable rather than asserted. That is the only form of a
count this arc has managed to get right: one that shows its own arithmetic.

---

## 4. The reaper write-up, on better grounds

Three corrections folded into `fix-r2.md` §2, all of which strengthen option (a):

1. **#134 recorded a premise, not just a hazard.** `FIXTURE_LAUNCH`'s doc explains why
   an interprocess lock was deliberately not taken — *"cargo runs each test target's
   executable in sequence… Nothing in this workspace supports two fixture test
   processes running at once… If that ever changes, this is the place that has to
   change with it."* **The baseline experiment deliberately violated a documented
   premise** rather than discovering an unknown defect. That is a materially better
   disposition, and it is why §1's note belongs exactly where it was put.
2. **CI-unaffected is measured, not reasoned.** Correctness sampled the process table
   during the real CI invocation: **max concurrent test binaries = 1 across 807
   samples**, zero with two or more. Scope added a ground neither the dispatch nor I
   had: the two macOS jobs **share** the self-hosted runner label, so "different jobs"
   would not have settled it — but the reaper matches by **fixture basename**, and
   those jobs use `umbra-test-child` versus `umbra-userspace-toy`, so no cross-kill is
   possible even if they overlap. The main workspace job sets no fixture environment,
   so the reaper takes its reap-nothing path. **Both measurements are the reviewers',
   not mine**, and are attributed as such.
3. **Limb (b)'s stated reason was too strong, and scope corrected it — including a
   tension inside my own document.** I wrote that "parentage is not a usable filter"
   while the follow-up box two paragraphs later proposed a descendant check. The
   precise version: a sibling suite's tracee is **not** a descendant, so descent
   **would** correctly spare it — but the reaper's real target is **reparented to
   `launchd`** once its tracee exits, so descent fails *exactly* in the case the reaper
   exists for. A descent filter buys concurrency-safety at the cost of the reaper's
   job. **The conclusion stands on firmer ground than the reason I gave it.**

**Tracking issue for `merge_gate` to file**, contents now carried in `fix-r2.md`:
concurrent developer worktrees (this repo is worked in `umbra-worktrees/`); the
conditional case of two `native-qualification` jobs on one physical host — not
reachable today, and it is the *basename* that makes it safe rather than the job
boundary; master's own revisit trigger, which this arc is the first thing to trip; and
the design decision itself, a per-run marker versus a descent check reconciled with
strays reparented to `launchd`. Related to deferred #135 item 5.

---

## 5. The reviewers' own disclosures — evidence, for the merge-gate record

These belong in the record because they are part of the finding in §6, not despite
being the reviewers' errors but because of it.

- **`review_correctness` self-attributed N1.** The "consecutive rounds" count it
  reported as a finding had originated in its own visit-2 wording as "fourth"; the
  synthesis then escalated it to "fifth".
- **`review_scope` caught two of its own near-miss false findings before reporting
  them.** An interim waiver-(a) signature count of 6, which came from a `grep`
  conflation; and a failed `grep` for audit §L that missed because the text reads *"the
  audit's §L"*. Either would have been filed as a finding against this change.
- **`review_scope` recorded a third item of a different kind, and kept it separate.** A
  transient bad `shasum` / `diff -q` read reported `native.rs` as differing from the
  pin, where `diff -u` and repeated digests say it does not. **No cause asserted.** It
  is logged as a reviewer **tooling** instance, distinct from the reasoning ones — a
  distinction worth preserving, because a tooling misread and a reasoning slip need
  different remedies.
- **What each reviewer explicitly did not verify.** Scope notes that remedy 1's M1
  measurement is correctness's and not its own. Recording the boundary of a review's
  own evidence is the practice that makes the rest of it trustworthy, and it is why the
  M1 result is attributed rather than absorbed.

---

## 6. Lesson 28 — the point, stated plainly

The arc's instances are catalogued in `fix-r2.md` §6, corrected this round and left
showing their own history. What this round adds is the conclusion those instances
support, which is stronger than the one #134 could draw:

**Lesson 28 has now fired on the author, on both reviewers, and on the driver** —
including *inside the document arguing that correction passes are where false claims
are born*, and including *in the paragraph doing the arguing*. **Three of round 3's
five findings are the driver's own**, self-recorded in the synthesis rather than
quietly fixed — and among the five are corrections *to corrections* that two reviewers
and an author sweep had already passed.

**No single reviewer caught all of it.** Correctness caught what scope missed and the
reverse; the driver caught what both missed; both reviewers caught what the driver
mis-pointed. Every role in the graph both produced an instance and caught one.

That is the transferable finding, and it is not an embarrassment to be tidied. A
section on wrong counts that turned out to contain them — and a misattribution
besides — is better evidence than a clean one would have been, which is why the
corrections above are made in place with the errors left visible, and why the driver
annotated synthesis r2 rather than rewriting it. **A record that hides its own history
cannot support the finding it is making.**

**This document caught one of its own while being written**, which is the remedy
working rather than a reason to hide it: §3 first explained SYN3-2 by stating how many
rounds this arc has authored — a number the act of writing this round invalidates, in
the very paragraph explaining why no number should be supplied. It now describes the
bound without naming it.

**And the first attempt to record that catch stated how many times the remedy has
worked**, which is another count of this arc's own corrections, written into the
sentence congratulating the remedy for catching one. It is removed rather than
corrected. The instances are named in `fix-r2.md` §6; what they have in common is that
each was a claim written *in the sentence doing the correcting*, which is the part
worth carrying forward.

The remedies that this arc actually demonstrated remain the four in `fix-r2.md` §6,
and §1 of this document is the fourth one applied once more: the commitment to
document a hazard was itself a claim that the diff had to keep, and keeping it was
cheaper than restating it.

---

## 7. Gates, re-run

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored (the pre-existing NFS fault-injection cases) |
| direct tracer + `provider_ipc` + `sandbox_launch` + `--lib` | exit 0; **14 `CAPTURED`, 0 `SKIP`**; 13 / 1 / 4 / 36 passed |

**Qualified by `CAPTURED` (lesson 23)**, from the `--nocapture` run; the workspace
run captures stderr, and its `ok` is exactly the signal SYN-10 shows these cases can
emit without running.

**Rebuild discipline (lesson 24), and the mtime hazard.** The `transport-raw` provider
was rebuilt **immediately before** its symbols were read, because a default-feature
`cargo build --workspace --bins` overwrites that path *and moves mtime backwards*, so
freshness cannot be judged from the file:

```
$ cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins
  warning: …: libnfs raw binding: 16 functions emitted
$ nm target/debug/umbra-storage-nfs-userspace | grep -E " _rpc_(connect_async|service|nfs4_compound_task)$"
00000001001244ec T _rpc_connect_async
0000000100128518 T _rpc_nfs4_compound_task
0000000100123464 T _rpc_service
```

The tracer provider, same discipline and built immediately before reading:
`Session15continue_thread`, `Session17continue_absorbed`, and the `vCont;c:` literal
present.

**Standing constraints re-verified after this round's edit.** The `z0`/`Z0` filter
over the whole diff returns **nothing**. `single_thread()` stays with both call sites.
Deferred items 3 / 4b / 5 / 6 absent — including item 5's reaper, which this round
documented without touching. No line numbers in new text; the harness note names
symbols only. Both attribution trailers intact.
