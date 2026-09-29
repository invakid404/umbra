# Fix — round 2 (graph `dg-0ved1w0e`, #135 multithreaded closure, slice 1)

Node `fix`, round 2. Date 2026-09-29.
Change **`qqxtnynk`**, bookmark `feat/mt-closure`, parent `d8a42def`.
Input: `/tmp/graph-dg-0ved1w0e/review-synthesis-r2.md`, with the current
`review-correctness.md` and `review-scope.md` in the tree as its evidence.

> The commit id is not stated; writing this file moves it. `jj log -r qqxtnynk` is
> the authority.

**Both reviews pass and neither requires a code change.** `review_correctness` —
PASS, merge-ready, all 11 round-1 findings closed. `review_scope` — PASS, inside the
ratified envelope, all 6 closed. Both re-ran M7 and M8 themselves rather than
accepting `fix-r1.md`.

**This round's substance is a measurement, not an edit — and it did not come back
the way anyone expected.**

---

## 0. Headline: the intermittent is not a flake, it is a mechanism, and it is
## one-directional

`SYN2-1` asked whether unmodified master flakes the way the branch appeared to.
Measured, alternating, 15 runs each, full output of every run kept to its own file:

**Sequentially — which is how CI runs it — master and the branch are equally stable.
15/15 clean on each. The branch does not move master's flake rate.**

Nobody had asked the next question, and it is the one that mattered:

**Concurrently, master is stable and the branch is not — and the branch also breaks
a master run happening alongside it.** The cause is fully established, reproducible
on demand, and has nothing to do with the closure.

`StrayFixtureChildren`, #134's stray reaper, enumerates **every process on the
machine** whose executable file name equals the fixture's basename
(`fixture_named_processes`) and `SIGKILL`s any that appeared during its window and
was not in its `before` snapshot. It is constructed only by `mt_fixture` — that is,
only for `mt_write` and `mt_spawn`. **Those two are `#[ignore]`d at master, so it
never runs in a default suite run. This change un-ignores them, so it now runs in
every default suite run** — and it cannot tell another suite's live tracee from its
own stray, because they are the same binary at the same path.

Per the synthesis's own instruction — *"if master is **stable** and the branch is
not, **stop and report that**… it must not be written up as a disclosure"* — this is
**escalated, not disclosed**. §2 states the decision it needs.

---

## 1. Disposition per finding

| # | Disposition |
|---|---|
| **SYN2-1** | **MEASURED — and ESCALATED, not disclosed.** Sequential: no difference. Concurrent: a verified, reproducible mechanism. §2, §3 |
| SYN2-2 | **FIXED.** Count dropped entirely from `impl.md` §7 and `fix-r1.md`; no corrected number supplied. §4 |
| SYN2-3 | **FIXED.** `impl.md` §3 now true parent-relative *and* round-relative. §4 |
| SYN2-4 | **FIXED.** "17 mechanism pins" replaced by a pointer to audit §L; no count. §4 |
| SYN2-5 | **RECORDED** for `publish` and `merge_gate`. §5 |
| Lesson 28 | **Written up as a first-class synthesis of the arc's instances.** §6 |

---

## 2. SYN2-1 — the measurement, and the decision it needs

### 2.1 The four configurations

Every run's full output went to its own file and every figure below was read from
those files, never off a terminal — which is the artifact the scope reviewer said it
had lost. Each tree has its own fixture binary and its own redirect root, both built
from byte-identical sources (`diff -q` clean). The master tree is a throwaway **git**
worktree detached at `d8a42def`, verified unmodified: `pending` is still
`Option<Pending>` and both MT cases still carry their `#[ignore]`.

| configuration | runs | reaper active? | result |
|---|---|---|---|
| **master, sequential** | 15 | no | **15/15 clean** (11 passed each; the two MT cases are `#[ignore]`d there) |
| **branch, sequential** | 15 | yes, but alone | **15/15 clean** (13 passed each), and **0 `REAPED` lines in all 15** |
| **master ‖ master**, 6 pairs | 12 | no | **12/12 clean**, 0 `REAPED`, 0 failures |
| **master ‖ branch**, 8 pairs | 16 | branch only | **master 0/8 clean; branch 8/8 clean** |
| **branch ‖ branch**, 6 pairs | 12 | both | **5/12 clean, 7 failed** |
| | **70** | | |

The run column sums to **70**, which is the number of log files the experiment left
(`15 + 15 + 12 + 16 + 12`). Round 2 first wrote "forty-odd" and a later restatement
made it "42" — that is `15 + 15 + 12`, the two sequential sets plus one concurrent
set, with the other two concurrent sets' 28 runs dropped. The breakdown is given per
configuration here so the total is checkable rather than asserted, which is the only
form of a count this arc has managed to get right.

The sequential runs were **alternated** master/branch/master/branch rather than run
as two blocks, so machine-load drift cannot produce the difference — and there was
no difference to produce.

### 2.2 The captured identities — the artifact this round was asked for

**master ‖ branch** (only the branch has a live reaper, so the kills go one way):

```
master failing tests:   posix_spawn_write  8/8 runs
                        symlink_cycle      3/8 runs
master failure:         fatal signal/exception: T09      ← SIGKILL
branch, same pairs:     13/13 CAPTURED, every run
branch logs:            REAPED mt-write: stray fixture child pid …   (×11)
                        REAPED mt-spawn: stray fixture child pid …   (×3)
```

The branch emitted `REAPED` lines for children **it did not create** — the same
branch suite run sequentially produces **zero** `REAPED` lines in 15 of 15 runs. The
pids it reaped were the master suite's live tracees, and master's failure is the
`SIGKILL` arriving mid-syscall.

**branch ‖ branch** (both reapers live, so the kills go both ways):

```
failing tests:  mt_write   6     mt_spawn   6
failure:        fatal signal/exception: T09   ×9
REAPED lines:   1–3 per failing run
```

**master ‖ master**: zero `REAPED`, zero failures, 12/12. **Concurrency alone does
not break this suite.** The reaper does.

### 2.3 What this is, stated exactly

- **It is not a defect in the multithreaded closure.** No production tracer code is
  involved. The failures are external `SIGKILL`s delivered by a *sibling test
  process*, and they land on whichever tracee happens to be live — `posix_spawn_write`
  and `symlink_cycle` when master is the victim, `mt_write` and `mt_spawn` when
  another branch suite is.
- **It is not new code, and #134 did not merely document the hazard — it recorded the
  premise under which the design is safe.** `StrayFixtureChildren` shipped at master
  with #134, its machine-wide-by-name matching is unchanged in this diff, and
  `FIXTURE_LAUNCH`'s doc says why an interprocess lock was deliberately not taken:
  *"cargo runs each test target's executable in sequence… Nothing in this workspace
  supports two fixture test processes running at once… If that ever changes, this is
  the place that has to change with it."* **The baseline experiment did not discover an
  unknown defect — it deliberately violated a documented premise.** That is a better
  disposition than "latent debt nobody noticed", and it is why the repair belongs where
  #134 said it would.
- **It is newly reachable, and this change is what makes it so.** Removing the two
  `#[ignore]` attributes — the ratified success criterion — moves the reaper from
  "runs only under `--ignored`" to "runs in every default suite run".
- **CI is not affected, and that is now measured rather than reasoned.** The
  correctness reviewer sampled the process table during the real CI invocation:
  **max concurrent test binaries = 1 across 807 samples**, zero samples with two or
  more — so `cargo` serialises test executables and the three `--test` flags on one
  invocation cannot overlap. The main workspace job sets no fixture environment, so
  the reaper takes its reap-nothing path. And the scope reviewer added a ground
  neither the dispatch nor I had: the two macOS jobs **share** the self-hosted runner
  label, so "different jobs" would not have settled it by itself — but the reaper
  matches by *fixture basename*, and those jobs use `umbra-test-child` versus
  `umbra-userspace-toy`, so no cross-kill is possible even if they overlap. Both of
  those are the reviewers' measurements, not mine.
- **Developers and reviewers are affected, newly.** Two suites overlapping on one
  machine now break each other. That is exactly the situation two reviewers on one
  host were in.

### 2.4 The two unexplained intermittents — what I will and will not say

The mechanism above produces **exactly** the observed failure shape: a fixture tracee
`SIGKILL`ed mid-run, surfacing as `fatal signal/exception: T09` through
`next_event().unwrap()`, failing the test and exiting `101`.

- **The scope reviewer's 2 failures in 11 runs** are *consistent with* it in shape,
  signal and count. Whether that reviewer's runs actually overlapped another suite is
  **not established** and I have no way to establish it. **I do not assert it as the
  cause.**
- **The `101`** is likewise consistent — but `101` is the generic Rust test-harness
  failure code, so it is weak evidence on its own, and that run's output was lost.
  **I do not assert a cause for it either.**

What has changed is that the arc now has a **verified mechanism that produces this
failure mode**, where before it had two open questions. That is worth more than
either attribution would have been, and it is as far as the evidence goes.

### 2.5 The decision — the human's call, not mine

**Not a scope breach and not a correctness defect**; both reviews already pass on the
tree as it stands, and this changes neither verdict. The question is only whether to
publish with the hazard documented or to close it first.

| option | what it costs |
|---|---|
| **(a) Ship as-is**, document the hazard in the harness, file a follow-up | CI is unaffected; the cost falls on concurrent local runs, which is where it has already been paid once |
| **(b) Scope the reaper before publish** — restrict it to children this process created | Touches #134's shipped reaper, which is the **deferred item 5** (orphan-leak) machinery, so it is outside the ratified envelope. And the scoping is a design decision, not a one-liner — see the correction below |
| (c) Re-`#[ignore]` the two cases | Undoes the ratified success criterion. Not viable |

> **A correction to limb (b)'s stated reason, from the scope reviewer — it was too
> strong, and it sat in tension with the follow-up box below, which names a descendant
> check.** "Parentage is not a usable filter" is wrong as written. A sibling suite's
> tracee is *not* a descendant of this process, so a descent test **would** correctly
> spare it. The real difficulty is the other way round: the stray this reaper exists
> for has been **reparented to `launchd`** by the time it is reaped, so a descent
> filter fails in exactly the case the reaper is for. A descent filter therefore buys
> concurrency-safety at the cost of the reaper's actual job — a design decision on
> deferred item 5's machinery, not a mechanical fix. **The conclusion stands on firmer
> ground than the reason I first gave it.**

**My recommendation was (a)**, on the grounds that CI is unaffected, that the reaper is
pre-existing shipped code, and that item 5's machinery is explicitly deferred to its
own arc — the same reasoning that kept the exec fixture out of round 1. **Both
reviewers endorsed (a) at round 3 with no escalation**, and round 3 delivered the
harness note that (a) depends on: see `fix-r3.md`.

> **Tracking issue for `merge_gate` to file.** `StrayFixtureChildren` matches
> candidate strays by executable *name*, machine-wide, so it cannot distinguish a
> sibling suite's live tracee from its own stray. Not introduced by this change; newly
> reachable because of it. The issue should carry:
>
> - **Concurrent developer worktrees** — the real-world exposure, and this repository
>   is worked in `umbra-worktrees/`.
> - **The conditional case of two `native-qualification` jobs on one physical host** —
>   not reachable today, because those jobs use different fixture basenames, but the
>   basename is what makes it safe rather than the job boundary.
> - **Master's own revisit trigger** — `FIXTURE_LAUNCH`'s *"if that ever changes, this
>   is the place that has to change with it"*, which this arc is the first thing to
>   trip.
> - **The design decision**, stated above: a per-run marker in the spawned child, or a
>   descent check that would have to be reconciled with strays reparented to `launchd`.
>   Related to deferred #135 item 5.

---

## 3. What the measurement also settles

- **The `3/3` re-run claim is restated** in `fix-r1.md` §7 with the candour the `101`
  gets. It names both observations rather than picking one: it was true for me and
  false for a reviewer **on the same tree**, and neither of us was wrong about what we
  saw. What was wrong was the assumption behind reporting it — that repeated runs of
  this suite on this machine are independent observations. With a second suite
  running, they are not.
- **`mt_write` and `mt_spawn` are not implicated.** 15/15 sequential here, 8/8 under
  a concurrent master, and 12/12 in the reviewer's dedicated runs. Where they fail,
  they fail by receiving `SIGKILL` from another test process.

---

## 4. SYN2-2, SYN2-3, SYN2-4

**SYN2-2 — the count is gone, and no number replaces it.** `impl.md` §7 no longer says
"added to it twice" or "the second time"; `fix-r1.md` §0 no longer says "for the second
time in this arc". §7 now states that the pre-arc figure is **four** per D5 — the only
reading that stays stable — and that this arc's own additions are **named and
deliberately not totalled**, because every count of them has been wrong in every round,
including the ones written inside the fix for the previous wrong count. The synthesis
asked for no corrected number and none is supplied.

**SYN2-3 — true on both readings.** §3 said *"no existing test function was modified"*,
which is true against the parent and false against this change's own history: round 1
edited the absorb pin, a test this change created. It now states both, and says why the
parent-relative claim alone reads as exhaustive and is not.

> **Round 2 recorded this as "SYN-5's exact shape, in the paragraph where SYN-5 was
> fixed". That framing was wrong twice over, and §6 carries the corrected one.** The
> sentence is byte-identical at rounds 0 and 1 — the SYN-5 fix changed the sentence
> *next to* it, not this one — so the correction and the defect were never one
> sentence. What actually happened is §J's **original** generator: round 1's *other*
> deliverable joined the diff and silently falsified prose that had been true when
> written. A **code** edit — adding assertions to the absorb pin, a test this change
> created — made a sentence in a **different file** false, and no sweep of either file
> alone could see it.

**SYN2-4 — the count is replaced by the enumeration, not by a better count.**
"All 17 mechanism pins" is gone; `fix-r1.md` §7 now points at audit §L as the
enumeration and checks mechanism by mechanism without totalling. §L lists 12; the 17 was
my own working set with extra mechanisms folded in, and an unenumerated total is the
same defect as the symbol count withdrawn for SYN-8. The mechanisms themselves remain
verified — byte-identical against the parent in production source, re-checked this
round.

---

## 5. SYN2-5 — root process documents belong to other arcs

Recorded for `publish` and `merge_gate`, with **no action on the files**; they are not
this arc's to change.

The worktree root carries `review-synthesis-r1.md` (arc `dg-egt6apy1`), `publish.md`
(#129), and `review-synthesis-r2/r3/r4.md` and `ci-round1.md` from earlier arcs. All are
tracked at master and **none is in this diff**. This arc's syntheses exist only under
`/tmp/graph-dg-0ved1w0e/` and in `knowledge/umbra/graph-audits/dg-0ved1w0e-*`.

**`publish` must not read process documents from the worktree root.** `merge_gate`
should carry this as a live instance of #132, the root-document convention issue, which
is already open and out of scope here.

---

## 6. Lesson 28 — the arc's instances, as one finding

#134 called this *"plausibly this arc's most transferable finding"*. The evidence is now
strong enough that it outweighs either defect this arc measured, and it belongs in the
merge-gate record as a first-class finding rather than as a per-round footnote.

**The generator, restated from this arc's evidence.** The fixed point is not a document
editing itself. It is **the correction pass itself** — the act of restating an
established fact in new words is the act that manufactures a new false one. Every
instance below is a claim written *while fixing a different claim*, and in each the
author had just demonstrated they understood the underlying mechanism.

**This arc's instances, in order:**

| instance | shape |
|---|---|
| **SYN-1** | The claim that the per-thread tripwire pinned the process-wide one-window property. **Written in round 0's correction pass**, propagated to five sites across three of that round's own deliverables. The `native.rs` comment stated the correct mechanism in its **first sentence** and misattributed it in the next. Found by two reviewers independently, by reading and by measurement; by no author sweep. |
| **SYN2-3** | *"no existing test function was modified"* — true when written and false afterwards, because **round 1's code edit to the absorb pin falsified a sentence in another file**. §J's original generator: the round's own other deliverable joining the diff invalidates what the round just corrected. Neither file's sweep could see it alone. |
| **SYN2-2** | A count of this arc's own firings, wrong in every round that stated one — **including the rounds that were fixing the previous wrong count**. How many rounds that is, is itself a count of this arc's own corrections, and it is not supplied: round 2's "five consecutive rounds" was unsupported, it originated as "fourth" in a review and was escalated to "fifth" in a synthesis, and it reached a table whose subject is wrong counts past an author sweep, two reviewers and the driver. The right move is to stop producing the number. |
| **SYN-8 / SYN-9** | A symbol count with no recorded command, and a symbol name that passed its own check **as a substring of the real name** — a verification that confirmed itself. |
| **SYN-6 recurrence** | Fixed a count; wrote another one paragraph later. **Caught in-round by the author** — the first time in the arc that happened, and only because the sweep was run against the round's own new text rather than against the reviews. |
| **fix-r1's `fixture_argv`** | A function name wrong in `fix-r1.md` while **restating** two reviews that both had it right, and an `impl.md` that had it right. The restatement was the risky act, not the original. |
| **`review_scope`'s own two** | It nearly filed a **critical false finding** from a bad `grep` that appeared to show a keyed `remove` where production has `std::mem::take`; and it carried an unverified `impl.md` claim into round 1. Disclosed both itself. |
| **The dispatch's own** | Round 2's instructions pointed both reviewers at another arc's `review-synthesis-r1.md`. Caught by both reviewers; neither contaminated. |

**What that list shows that no single instance does.** It fired in the author, in both
reviewers, and in the dispatch — every role in the graph, including the ones whose job
is to catch it. It is not attributable to carelessness or to a weak participant. And
the instances that were caught before shipping were caught in one of two ways, and
neither is "the writer looked harder". **A different reader than the writer** caught
SYN-1, SYN2-3 and the dispatch's own mis-pointed synthesis. **Running the sweep against
the round's own new text rather than against its inputs** caught the SYN-6 recurrence
and `fix-r1.md`'s wrong function name. That is the actionable split — and this sentence
previously said "the two instances" and then named four, which is the catalogued defect
occurring inside the paragraph cataloguing it.

**The remedies that actually worked in this arc**, distinguished from the ones that
sounded good:

1. **Make the code assert what the prose claims.** SYN-1's five-site correction would
   have been just as driftable as the original. Adding `debug_assert!(pending.is_empty())`
   is what makes the next revision of that paragraph unable to go quietly wrong — and it
   turned out to close a real detection gap too. The correctness reviewer measured it and
   I re-verified it here, rebuilt, 3 runs of 3: under M1 the round-0 tree failed only
   `mt_write` — `mt_spawn` **passed with the freeze deliberately broken** — and the
   current tree fails **both**, `mt_spawn` on the new assertion's own message. The
   assertion written to make a comment honest turned out to be the only thing that
   detects a broken freeze on that path.
2. **Stop producing the number.** Not a better count — no count. Applied to the symbol
   figure (SYN-8), the pin total (SYN2-4), the arc's own firings (SYN2-2), and the clean-
   run tally. Each had been "corrected" at least once before being withdrawn, and the
   corrections were wrong too.
3. **Sweep the round's own new text, not its inputs.** The one author-caught instance
   came from re-reading what this round had just written. Re-reading the reviews finds
   nothing, because the reviews were right.
4. **Measure the thing nobody measured rather than disclosing it.** SYN2-1 was heading
   for a third careful disclosure of an unexplained intermittent. One throwaway worktree
   and **70 suite runs** turned it into a mechanism. The instinct to disclose honestly is
   not a substitute for the cheap experiment.

---

## 7. Gates, re-run

| Gate | Verdict |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace --all-targets -- --test-threads=1` | **exit 0**, 52 suites ok, 0 failed, 3 ignored (the pre-existing NFS fault-injection cases) |
| direct tracer + `provider_ipc` + `sandbox_launch` + `--lib` | exit 0; **14 `CAPTURED`, 0 `SKIP`** (13 fixtures + the provider-IPC verdict); 13 / 1 / 4 / 36 passed |

**Qualified by `CAPTURED` (lesson 23)** from the `--nocapture` run; the workspace run
captures stderr and its `ok` is exactly the signal SYN-10 shows these cases can emit
without running.

**Rebuild discipline (lesson 24), and the artifact hazard.** The `transport-raw`
provider was rebuilt **immediately before** its symbols were read, because a
default-feature `cargo build --workspace --bins` overwrites that path *and moves mtime
backwards*, so freshness cannot be judged from the file:

```
$ cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins
  warning: …: libnfs raw binding: 16 functions emitted
$ nm target/debug/umbra-storage-nfs-userspace | grep -E " _rpc_(connect_async|service|nfs4_compound_task)$"
00000001001244ec T _rpc_connect_async
0000000100128518 T _rpc_nfs4_compound_task
0000000100123464 T _rpc_service
```

The tracer provider, same discipline: `Session15continue_thread`,
`Session17continue_absorbed`, and the `vCont;c:` literal present in a binary built
immediately before reading it.

**Standing constraints re-verified.** `z0`/`Z0` byte-identical — the filter over the
whole diff returns nothing. `single_thread()` stays with both call sites. Deferred items
3 / 4b / 5 / 6 absent. No line numbers in new text. The throwaway master worktree is
outside the repo and is **not** in this diff.
