# Review synthesis — round 3

Graph `dg-egt6apy1`, node `review_synthesis`, visit 3. Date 2026-09-28.
Inputs: `review-correctness.md` (round 3), `review-scope.md` (round 3).
Rounds 1 and 2 preserved in the archive with `-r1`/`-r2`-equivalent names.
Tree: change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark
`feat/fork-lifecycle`, parent master `7c3ecc8f`.
`native.rs` pinned at **`c2c8506b30c550961164619c88b5cecb7cde570e`**.

> **Note on this file.** It overwrites a predecessor graph's
> `review-synthesis-r3.md`, which existed at master. That copy — and eight other
> inherited root documents — were preserved first under
> `graph-audits/inherited-from-master-7c3ecc8f/`, closing scope's round-1 N2
> finding in practice rather than merely recording it.

---

## Verdict: **CLEAN PASS → `publish`**

Both reviewers say publish. Every finding raised across three rounds is closed:
R1-R7 in round 1, S1-S3 in round 2, and round 3 found **no code defect at all**.

The one blocker raised — scope's `S-B1` — was **procedural, not a code defect**,
and I have discharged it. Details below, including the part that is my fault.

Loop budget spent: `review_synthesis → fix` 2 of 5. Stopping here is the
judgement I recorded in synthesis r2 — the remaining items are the human's
decisions, not remediable findings.

---

## `S-B1` — discharged, and the root cause is the driver's, not a worker's

**What scope found.** While its first round-3 gate run was executing, the change
briefly contained an **inverted** S1 assertion —
`self.pending.is_some(), // NEGATED PROBE`. It proved this as a *recorded jj
state* (evolog commit `2b6d08d52b26`, sitting after the `79daa6c2` I handed it)
rather than inferring it from symptoms. Two of its gates measured the
contaminated tree: **11** assertion firings in the macOS gate, 3 in the CLI gate.
It discarded those figures, re-ran everything hash-pinned, and verified every
`.rs`/`.c` file identical before and after.

**What it asked for, and what I did.** One command before publish:
`shasum crates/umbra-platform-macos/src/native.rs` must be
`c2c8506b30c550961164619c88b5cecb7cde570e`.

Run: **it matches exactly.** I additionally confirmed the assertion is the
correct polarity (`self.pending.is_none()` at `:571`), that no `NEGATED_PROBE`
marker exists anywhere under `crates/` or `experiments/`, and that the tree has
been quiescent for 13+ minutes (`native.rs` mtime `14:47:43`, now `15:01:01`).
**S-B1 is satisfied and scope's pass stands.**

**The cause was my orchestration, and it should be recorded as such.** I
dispatched `review_correctness` and `review_scope` **concurrently into a single
shared worktree**, and then explicitly instructed correctness to *"verify the
non-hollowness claim yourself"* — which necessarily means negating the assertion
and rebuilding, mutating the very tree scope was certifying. Scope had already
flagged the artifact-level version of this in round 1 (*"review-correctness.md is
also now modified — the sibling node writing concurrently"*); this is the same
defect reaching source. **The exposure existed in rounds 1 and 2 too**, since
correctness ran mutations A/B/C concurrently with scope on both occasions. We
were lucky, not safe.

Worth keeping in proportion, as scope itself did: the failure mode is **loud**.
A contaminated gate fails visibly — its did, 11 times — rather than passing
quietly. That is why this was a one-command gate and not a fix round.

---

## Round 3 findings: none. Both passes, item by item.

### Correctness — clean pass, "publish"

**`S1` adjudicated by the reviewer who raised it, ruling against its own
proposal.** It verified the three-things claim against the *helpers* rather than
against the implementer's argument: `remove_breakpoint` (`native.rs:472-481`)
sends `z0`, drops the address from `self.breaks`, and returns a value only
`finish_return` hands back — so losing it leaves that stub **permanently
un-intercepted**; the first gate is retired nowhere but `finish_return`; and the
candidate's loss is the S1 mechanism it described in round 2.

It then **withdrew its own suggestion**: it had offered `exec_candidate` as
"clobber-proof and failed-exec-proof", and says that framing was wrong in the way
the implementer identified — the field rescues the *newest of three riders* while
the two older ones stay broken, which is a partial repair of a corrupt state. The
assertion names the precondition for all three at the one site that can violate
it.

So S1's full arc: **raised** by correctness (r2) → **answered with a
code-derived counter-argument** by the implementer, declining both the
reviewer's and my proposed shape → **ratified on the merits by the original
finder** (r3). Both the reviewer and I were corrected by someone actually reading
`return_stop`. That is an adversarial loop working rather than a compliant one.

### Scope — clean pass on every guardrail and every figure

* **The ruling I asked for: a `debug_assert!` is *not* a fifth surface element.**
  It agreed with my read and supplied a better discriminator than I had: an
  enum's shape is structural in **every profile**; an assertion is not. Verified
  exactly one site added (master 1, workspace 2, the pre-existing one identical).
  **The human's inventory stays at four: D1-D4.**
* **Exactly three code files**, with the boundary located *by the S1 edit itself*
  rather than taken on trust. `native.rs`'s only added non-comment lines **are**
  the assertion block (6 of 38, zero deletions). S2 proven prose-only: **564 C
  code lines identical**. Round-2 instrument re-run: all **22** pre-existing test
  fns byte-identical to master **including doc comments**; this round changed
  **one** test fn, comment-only (S3's `forkexec`).
* **Lesson 27's byte figures reproduced exactly** — 5,096,720 featureless,
  5,888,624 with `transport-raw`. It flagged one discrepancy rather than papering
  over it: the "10,041 libnfs symbols" I relayed vs its 4,910 is a
  **counting-method difference, not a conflict** — size matches to the byte and
  symbols rise by 1,076, so the build is confirmed either way. My relayed figure
  should not be quoted as authoritative.
* Everything else reconciles: 832/0/3; 12 CAPTURED; 20 PASS + 2 named SKIP;
  routed 28 with 22 live; assertion fires **zero** times across a wider gate set
  than `fix-r2.md` claimed.
* **Confinement holds at the parent** (`7c3ecc8f`), anchor checkout clean at
  `03bbf650`, default workspace untouched.

---

## The human's four decisions — verified unspent, carried to `merge_gate`

Scope confirmed all four are intact, which is the check that mattered most: a
pass that quietly resolved one would have spent the ratifier's decision for them.

* **D1 — `rollbackchild` narrowing.** The ratified fixture shape said *"parent
  rolls back, child's writes are gone."* Both reviewers independently verified
  that is **not expressible** (`umbra stop`/`checkpoint` are `not_implemented`;
  the only `abort` is per-operation from `OperationOutcome::Failure` and
  unreachable on a routed run because `Deny` is answered before an `OperationId`
  is minted; `abort` explicitly disclaims undoing writes). The substitute
  asserts the structural claim (G)/(H) actually makes. **But in one particular
  the shipped test asserts the converse of the ratified text** — it confirms the
  child's object *is* in the shadow. It is a *narrowing*, so the FOLD-IN POLICY's
  expansion trigger never fires, which is precisely how it could pass unnoticed.
  Scope verified the fixture case is byte-identical across round 3 (untouched
  entirely, unlike round 2). **This is the one that needs judgement rather than
  assent.**
* **D2 — purposive reading of the contract bound.** The overlay's `routed_cwd()`
  goes beyond the literal "row + one variant". Adjudicated *required, not creep*;
  0 files changed across both fix rounds.
* **D3 — `forkexec` discriminates on bytes, not entry names.** The ratification
  said entry names. `chdirchild` does discriminate on the entry name; `forkexec`'s
  object is present-and-empty, so bytes are the discriminator. Arguably stronger
  for this mechanism; the *wording* is what does not fit.
* **D4 — `ReturnKind::Exec { twin }`** as a fourth additive surface element.
  Adjudicated **inside** the bound: zero added `pub` items anywhere in the diff,
  the enum is private to the `native` module, and it commits *less* state than
  master. Informational only.

## Lessons carried out of this slice (24-30)

* **(24)** A test run that does not rebuild the binary under test can report a
  false pass. Correctness's first mutation run passed against a stale
  `target/debug/umbra`, and caught itself.
* **(25)** An untouched file is not an unaffected file. `umbra_interpose.c` had
  zero changed lines — correctly cited as scope compliance — while its header
  prose went stale (R4).
* **(26)** In jj, pin a reviewed tree by **change id**, not commit id. The
  working-copy commit re-timestamped **nine** times across this slice.
* **(27, corrected)** `cargo build --workspace --bins` is **not** sufficient: it
  leaves the provider featureless at 5,096,720 bytes. Only
  `--features transport-raw --bins` gives 5,888,624. **Verify by size and
  symbols, not by existence** — and the symptom misleads, naming a capability
  handshake nowhere near the build.
* **(28)** The pass that fixes a false invariant is a likely place to introduce
  one. Four instances in this slice (R3, R4, S2, and one inside the S1 comment
  written to harden against the class).
* **(29)** A finding's obvious trigger may be unreachable. R1 named a dylib; a
  dylib cannot reach the path, because `cache::resign` refuses it earlier for an
  unrelated reason. Verify a trigger is *reachable* before building a regression
  test on it.
* **(30)** A reviewed tree can move **under** the review. Lessons 23/24/27 assume
  a stale tree; this is the same family with the arrow reversed. Reviewer-side
  hash-pinning is the **detection**; **driver-side isolation is the
  prevention** — do not run concurrent reviewers in a shared worktree when either
  is asked to mutation-verify.

## Instruction to `publish`

Publish the change. Note two things the audit trail already knows:

1. **The #123 `git rev-parse` preflight will fail from the workspace root** —
   this jj workspace has no `.git`; only `~/Coding/umbra` is colocated. Use a
   throwaway git worktree for `memoria` per lessons 5+9, and run the preflight
   somewhere git-capable. Flagged at `prep_workspace`, still true.
2. **After the merge the repo root will hold a mixed set** of process documents —
   some from this graph, some left stale from its predecessor
   (`review-synthesis-r4.md`, the `ci-*.md` files). Pre-existing wart of the
   root-document convention, not introduced here; the inherited copies are
   preserved in the archive. Mention it at `merge_gate`.
