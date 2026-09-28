# Publish — PR #129

Graph `dg-egt6apy1`, node `publish`, visit 1. Date 2026-09-28.

**PR: https://github.com/invakid404/umbra/pull/129**

* Branch `feat/fork-lifecycle` → `master`
* **No head SHA is recorded here, deliberately — see "the fixed-point problem"
  below.** Read it from the PR: `gh pr view 129 --json headRefOid`.
* jj change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`** (the stable handle; the
  working-copy commit re-timestamped ten times across this slice — lesson 26)
* Parent: master `7c3ecc8f`
* Both attribution trailers verbatim on the change; PR body ends with the Claude
  Code attribution and the session link, matching PR #128's convention.

**Nothing merged. `merge_gate` is next and needs the human.**

---

## #123 preflight

The 3-line `git rev-parse` preflight **fails from the workspace root**, and that
is expected rather than a defect:

```
$ cd ~/Coding/umbra-worktrees/fork-lifecycle
$ git rev-parse --show-toplevel >/dev/null 2>&1 || { echo 'no .git'; exit 1; }
no .git
```

A `jj workspace add` workspace carries no `.git`; only the colocated checkout at
`~/Coding/umbra` has one. This was flagged at `prep_workspace` rather than
discovered here. The preflight **passes** in the throwaway git worktree where
git-dependent tooling actually runs:

```
$ cd /tmp/graph-dg-egt6apy1/memoria-wt
PREFLIGHT PASS: /private/tmp/graph-dg-egt6apy1/memoria-wt
```

**Recommendation for the #123 preflight itself:** as written it cannot
distinguish "not a repository" from "a jj workspace whose git is elsewhere",
which are different conditions with different remedies. Worth a follow-up issue
after the human approves.

## memoria — six READMEs reviewed, not blind-acked

`memoria check` reported six pending. Its own guidance forbids acknowledging
drift (*"edit the README rather than acknowledging the drift"*), so each was
cross-checked against the code in its ownership boundary.

| README | result | why |
|---|---|---|
| `umbra-platform-macos` | **updated** | Two genuine drifts. Its traced-stub table was **missing the `chdir`(12) row** while the same section claims the table and `abi::TRACED_STUBS` *"read one source"* — a false invariant of exactly the parallel-list class the README itself recounts having suffered. Added the row beside `mkdir`(136) (both bare, cwd-anchored), which keeps the later *"the last four rows are the directory read"* sentence true. Also added item 12, the interposer's `exec` lifecycle, beside item 10's `fork` half |
| `umbra-storage-nfs-userspace` | **updated** | Already corrected in the implementation: the self-contradicting `exec` row replaced, and the combined `chdir`/`getcwd` row **split** so the shipped half and the inert half cannot drift into one claim |
| `umbra-supervisor` | no-update | Describes neither the effect enum nor logical cwd movement, so the `MovedCwd` addition falsifies no prose here |
| `umbra-overlay` | no-update | Names `routed_descriptor` only for the `Open` readback and never enumerated that family — `routed_stat` predates this change and is likewise unmentioned — so adding `routed_cwd` leaves its scoped claim true |
| `umbra-core` | no-update | The `lib.rs` change is the doc-comment fix removing `fstat` from the `EBADF` list; this README carries no such list |
| root `README.md` | **updated, acked last** | See ordering below |

Final state: `memoria check` → **`OK: 23 README(s) current, imports rendered, no
coverage or structure errors.`**

Note on that check's output: it prints 14 `no link or import path from the root
README reaches this README` advisories. Those are **pre-existing and present at
master**, which also reports `OK` — they are advisory within a passing check, not
errors this change introduced.

## Lesson 17's re-ack ordering, made concrete

The root README's inputs are **this graph's own root-level process documents**.
Verified from its review packet — `publish.md`, `ci-round1.md` and `done.md` are
all inputs:

```
publish.md    referenced-as-input: True
ci-round1.md  referenced-as-input: True
done.md       referenced-as-input: True
```

So the root README had to be acked **after** `publish.md` existed, or writing this
file would immediately re-pend it. That is what "re-ack ordering" means here, and
it explains the predecessor graph's `chore(memoria): re-acknowledge …` commits.

**Carried forward for the remaining nodes:** `wait_and_verify_ci` writes
`ci-round<visit>.md` and `done` writes `done.md`. **Each of those re-pends the
root README and needs a re-ack plus a push**, or CI's memoria gate will fail on a
state that is otherwise correct.

## The fixed-point problem, and why this file records no head SHA

Recording the final pushed SHA *in this file* is not possible, and the reason is
structural rather than clumsiness.

`publish.md` is an input to the root README (verified above). So writing it
re-pends the root README; acking that re-pends nothing but does change
`memoria.lock`; and amending the change to carry the new lock **moves the commit**.
Any SHA this file names is therefore stale the moment the file is committed.
Observed exactly that way: the branch went `cf89a75e` → `791aad2c` → `9f292308`,
each step a correction of the SHA the previous step had just invalidated.

The fix is to stop naming the moving value. This file pins the **change id**
`puyyxvmvmnpk…`, which is stable across every amend, and points at the PR for the
head SHA. Lesson 26 already said to pin by change id in jj; this is the sharper
form of it — **a document that is an input to a gated document cannot record its
own commit's identity.**

Recorded because the obvious alternative is worse: leaving a stale SHA in place
is exactly the provenance defect (`S-F2`, `R6`) that the scope review caught twice
in this slice, and it would have been self-inflicted here.

## Predecessor artifacts preserved

Nine inherited root documents (`impl.md`, `fix-r3.md`,
`review-synthesis-r3.md`/`-r4.md`, `ci-round1.md`/`-2.md`,
`ci-fix-r1.md`/`-r2.md`, `publish.md`) were copied to
`graph-audits/inherited-from-master-7c3ecc8f/` **before** this graph overwrote
them — closing the scope review's N2 finding in practice rather than only
recording it.

That surfaced something for `merge_gate`: after the merge the repo root will hold
a **mixed set** of process documents, some from this graph and some left stale
from its predecessor (`review-synthesis-r4.md`, the `ci-*.md` files). A
pre-existing wart of the root-document convention, not introduced here.

## One post-review edit, disclosed

The `umbra-platform-macos/README.md` edit above landed **after** the three review
rounds certified the tree. It is prose-only in a file whose code the reviews
already cleared, and it *removes* a false invariant rather than adding a claim —
but it was not reviewed, and `merge_gate` should say so rather than imply the
whole diff carries three rounds of sign-off. CI and CodeRabbit see it.
