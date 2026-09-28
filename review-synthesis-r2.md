# Review synthesis — round 2

Graph `dg-egt6apy1`, node `review_synthesis`, visit 2. Date 2026-09-28.
Inputs: `review-correctness.md` (round 2), `review-scope.md` (round 2).
Round-1 copies preserved as `dg-egt6apy1-review-{correctness,scope}.md`.
Tree: change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark
`feat/fork-lifecycle`, parent master `7c3ecc8f`. The working-copy commit moved
four more times during this round alone (`0c30dc08` → `f3de291d` → `bc94d98c` →
`b4767294`), which is lesson 26 earning itself twice over.

---

## Verdict: **one more `fix` pass**, then publish

Round 1's seven findings are **all closed and verified against the code rather
than the disposition**. Scope is a clean pass on every guardrail item and every
quantitative claim. Correctness is a clean pass on R1-R7.

But correctness's sibling hunt — the thing I asked it to prioritise — found that
**the R1 fix traded one hazard for a narrower one**, and the fix round introduced
**two fresh instances of the exact defect class it was fixing**. All three are
cheap. The `review_synthesis → fix` budget is at 1 of 5, so there is room, and
the alternative is knowingly publishing a new hazard plus two false invariants
that both reviewers can already name.

**This is deliberately not a perfectionist loop.** S1-S3 are the last items: no
new mechanism, no new tests beyond one assertion, and nothing that can cascade.
If a third round were to surface another cosmetic slip I would publish over it.

---

## R1's evidence grade rose twice under scrutiny — worth recording

R1 entered round 1 as *consequence CONFIRMED, trigger PLAUSIBLE* — confirmed via
a hand-injected path (Mutation C). During `fix` the implementer built a **live**
regression case instead of the unit-level fallback I had permitted, and in round
2 correctness **independently reproduced it**: reverted only
`s.twin = twin.clone()`, rebuilt *both* binaries, and the case FAILED with the
identical signature (`x9`/`x10` = `"UMBRARM1"`, `x16 = 0x554d4252`, `metype:5`),
then passed on restore in 4.42s — reached through the **real failed-`execve`
trigger with no mutation in the tree**.

So R1 is now **CONFIRMED in both halves by execution**. It also checked the test
cannot be hollow: it returns 85/86 if the exec never happened, and would fail on
the *fixed* tree if `cache::resign` had refused the image.

The path there is worth keeping. A **dylib** — exactly the shape the finding
named — turns out **not** to work as a trigger: `cache::resign` demands three
entitlement keys back, a dylib does not report them, so the run stops at
`missing twin entitlement` *before* the operand is rewritten. A test built on the
obvious trigger would have exercised nothing and looked green.

---

## Open items → `fix` (round 2)

### S1 — LOW · PLAUSIBLE · **the R1 fix traded one hazard for a narrower one**
*(correctness, priority 2)*

Correctness swept every durable session write in `intercept`: `entry`, `waiting`,
`scratch` and `exec_generation` are **clean** on the failed-return axis, and
`twin` is gone from the entry entirely — so the R1 class is genuinely closed.

But the exec candidate now lives in `Pending`, and `return_stop`
(`native.rs:550`) overwrites `pending` **unguarded**, while `stop()` dispatches
to `intercept` regardless (`:2125`) and `continue_run` resumes all threads
(`:493`). A multithreaded tracee whose thread B completes an intercepted syscall
between thread A's `execve` entry and the exec stop **loses the candidate** →
`_ => None` (`:2036-2045`) → exec'd image half-mediated, EBADF-ing silently.
Pre-fix, the entry commit survived that clobber.

**Bounded, and honestly so.** It sits behind the pre-existing
one-in-flight-syscall-per-session assumption and the *deferred* multithreaded
escalation. One caveat the reviewer was right to note: `single_thread()` gates
`Fork` and `Wait`, **not** `Exec`, so a multithreaded tracee can reach an exec.

**Fix: take the cheap hardening, not the redesign.** A dedicated
`exec_candidate` field, or a `debug_assert!(self.pending.is_none())` in
`return_stop`. Either is a few lines. Do **not** attempt to make the session
multithread-safe — that is the ratified next arc.

### S2 — INFO · must fix · false invariant **introduced by the round that fixed R3**
`umbra-userspace-edges.c:740` says the failed exec returns `ENOEXEC`, while the
same file's header (`:184`) and `unexecutable_image`
(`userspace_run.rs:434-441`, mode `0o644`) say **`EACCES(13)`** and explicitly
rule the `ENOEXEC`/dylib route out.

This is the same two-halves-disagree shape as R3, committed by the very pass that
fixed R3 — the third instance in this slice after R3 and R4. Worth a lesson (28
below) rather than just a correction.

### S3 — INFO · must fix · false invariant
`userspace_run.rs:1499` says the `forkexec` driver asserts "never on the tracee's
exit status", but `:1513` does exactly that — and it is what fires first under
mutation. Either the prose or the assertion should move; the prose is wrong, so
correct the prose.

---

## Closed and verified — `fix` must not churn these

| Round-1 finding | Verification in round 2 |
|---|---|
| **R1** twin lifecycle | Reproduced **by execution** both directions; trigger upgraded to CONFIRMED |
| **R2** three refuted-mechanism prose sites | All three now state the measured mechanism (routed open → virtual fd; write/close interposer-only → EBADF; object present and empty) |
| **R3** `events.rs` cwd heading | Fixed |
| **R4** interposer C header | **Proven** prose-only: comments/blanks stripped, **101 identical code lines** both sides. DORMANCY and the routed-call list now accurate for the multi-image world, and the list **demoted to point at `abi::TRACED_STUBS`** rather than duplicating it — which also retires a future parallel-list hazard |
| **R5** `MovedCwd` cross-check | `ProtocolMismatch`, matching `Opened`'s shape |
| **R6** PASS count / provenance | Re-measured mechanically: exactly **20** (3 crash + 17 local), both `/usr/bin/touch` lines present; cites the change id |
| **R7** `chdirshapes` | Covers all three shapes, and the chained relative moves are **unreachable** rather than merely unsatisfiable by one. Mutation-checked that the two chdir cases fail through **different channels**, so neither borrows the other's evidence |

**Scope: clean pass on everything.** Contract bound re-derived from scratch
(`^pub const TRACED_STUBS`: master 32 → 33, set-diff exactly the `chdir` row;
`RoutedEffect` 4 → 5, only `MovedCwd`). All 8 ratified escalations PASS against
`git show master:`. Standing guardrails byte-identical, parent guard still one
site. Confinement verified **at the parent** (`7c3ecc8f` carrying the `master`
bookmark), anchor checkout clean, default workspace untouched.

Three pieces of scope work deserve naming because they are stronger than what
was asked:

* **It refused the weaker proof I offered it.** I told it its round-1 technique
  (zero deletions in test files) was invalidated by R2/R7's comment edits. It
  showed my premise was half-wrong — deletions are still zero, because those
  edits touched lines *this change itself added* — and then declined to rely on
  it anyway, proving the guardrail two stronger ways: all 22 pre-existing test
  fns byte-identical to master **including their doc comments**, and 0 of 26
  test fns with any non-comment change across the fix round.
* **It caught a check that would have produced a false FAIL.** `intercept()` is
  *not* byte-identical; a naive guardrail comparison would have flagged it. Its
  only change is 2 lines in the `Delivery::Exec` arm (R1); the
  `Delivery::Namespace` arm admitting the `chdir` row is untouched and no
  wildcard was added, so one-table-two-gates is **verified, not assumed**.
* **It resolved the `832`-unchanged puzzle rather than excusing it.**
  Integration-gated cases report `0 measured` in the default run, contributing
  zero by construction — and the six new cases *are* accounted for in a total
  that **did** move, by exactly six (22 live + 6 probe SKIPs = 28 routed). Its
  own framing: had the routed suite reported 27, the unchanged 832 would have
  been hiding a missing case.

**Live qualification, both reviewers independently:** routed suite 28 passed / 0
failed (86.74s correctness, 82.52s scope), 6 SKIPs all declared probes, **all six
new/changed cases plus both at-risk fork cases executed by name**; 12 CAPTURED
across fixtures/provider_ipc; `sandbox_launch` 4 passed with real work — and
correctness noted the installer handoff now runs through the candidate-adoption
path, so P1a is exercised on the launch path too.

---

## Still the human's at `merge_gate` — unchanged, and verified unspent

* **D1** `rollbackchild` narrowing, where the shipped test asserts the **converse**
  of the ratified text. Scope confirms the fixture case is **byte-identical**
  across the fix round and only attribution prose moved, so the decision is
  genuinely still the ratifier's.
* **D2** the purposive reading of the contract bound (overlay `routed_cwd()`).
  Scope reports D2's overlay is **0 files changed** across the fix round.
* **D3** `forkexec` discriminating on bytes rather than entry names.
* **D4 (new, informational)** `ReturnKind::Exec { twin }` is a fourth additive
  surface element. Scope adjudicated it **inside** the bound and argued it:
  **zero added `pub` items anywhere in the diff**, `enum ReturnKind` is private
  to the `native` module, it implements the fix shape the synthesis quoted, and
  it commits *less* state than master. Listed for completeness, explicitly **not**
  a decision alongside D2.

## New lessons this round

* **(27) Lesson 24 generalises past `umbra-cli` to every provider binary, and
  its symptom can mislead.** Correctness's first round-2 routed run gave 21
  failures from a stale *provider* binary (`umbra-storage-nfs-userspace`, 4.9M
  featureless vs 5.6M `transport-raw`). It failed loudly rather than silently —
  but the error text names a capability handshake and points nowhere near the
  build.
* **(28) The pass that fixes a false invariant is a likely place to introduce
  one.** R3 and R4 were two-halves-disagree defects; the round that fixed them
  produced S2, the same shape, in a file it was editing for exactly that reason.
  A doc fix deserves the same adversarial re-read as a code fix.
* **(29) A finding's obvious trigger may be unreachable.** R1 named a dylib;
  a dylib cannot reach the code path, because `cache::resign` refuses it earlier
  for an unrelated reason. Verify the trigger is *reachable* before building a
  regression test on it, or the test passes while exercising nothing.

## Instruction to `fix` (round 2)

Fix **S1, S2, S3** in the existing `impl` session (`s-0gaqvxvw91`). S1 takes the
cheap hardening — a dedicated `exec_candidate` field or a `debug_assert!` — and
explicitly **not** multithread safety, which is the ratified next arc. S2 and S3
are one-line prose corrections; re-read them adversarially per lesson 28 rather
than pattern-matching the edit.

Do **not** touch D1-D4. Re-qualify live with `UMBRA_INTEGRATION_REQUIRED=1`,
build **all** binaries first per lessons 24 and 27, and cite the **change id**.
