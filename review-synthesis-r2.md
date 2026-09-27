# review-synthesis — `dg-29vwer0f` / #121, round 2

**Inputs:** `review-correctness.md` round 2 (R1–R6) and `review-scope.md` round 2
(R1–R5, separate numbering), both against jj change `psvqmvlm` / `7bd1a95`.

**VERDICT: FINDINGS PRESENT → `fix`. One more pass, and it is small.**

The three round-1 HIGH defects are **genuinely fixed**, each re-verified by the
correctness reviewer's own measurement rather than by reading the fix report.
Scope is clean for the second consecutive round. But the H1 remedy introduced
**one new MEDIUM defect in itself** — a second exit that still ends the run — and
it is reachable by an ordinary program bug that works on every other backend.

---

## 1. Round 1's defects: closed

| round-1 finding | status in round 2 |
|---|---|
| F1/H1 — regression vs master | **FIXED**, verified by side-by-side against `a85a8471` |
| F3/H2 — silent empty second listing | **FIXED**, and it explains more than anyone realised (§3) |
| F2/H3 — run-stopping `StaleHandle` | **FIXED**, all four calls now answer `EBADF` |
| S1–S4 (scope) | **FIXED**, arithmetic re-derived from scratch |

---

## 2. Merged findings, ranked

| # | sev | source | finding | fix shape |
|---|---|---|---|---|
| **1** | **MED** | corr R1 | **New, in the H1 remedy.** `RequestedAttributes::decode` refuses through two constructors; the **header** arm (`bitmapcount != 5 \|\| reserved != 0`) uses `invalid()`, which carries **no errno**, so `unserved_directory_request` raises `Err` and **ends the run**. Reachable: `struct attrlist::reserved` holds stack garbage in any program that assigns fields without `memset`/`= {0}`. **The kernel serves that request** (`rc=5 errno=0`, measured); umbra kills the run with `errno: None`. `bitmapcount=3` is the same. | Give the header arm an errno — `EINVAL`(22) is the closest honest answer, or `ENOTSUP`(45) for consistency with its sibling. One constructor call. |
| **2** | LOW-MED | corr R2 | `SyscallAbi::directory_request`'s default body is `Ok(None)`, which makes the supervisor skip attribute validation **entirely and silently**. The sibling `io_buffer` returning `None` for the same operation is a hard `ProtocolMismatch`, and `encode_stat`'s default **refuses** for exactly this reason. No test double implements it, so neither direction is covered. | Make the default refuse, as `encode_stat`'s does; add a double that exercises both directions. |
| **3** | LOW | corr R3, R4 | Two doc claims assert the property finding 1 falsifies: `directory_request`'s comment ("answered `EFAULT` here rather than faulting the supervisor's read" — measured false, an unmapped pointer **does** fault and end the run) and the new ABI test's "Both refusals carry `ENOTSUP`, so the caller can answer the program rather than end the run". The second is finding 1's doc twin, **in the test that exists to pin the property**. | Correct both with finding 1. The `EFAULT` class is pre-existing and tree-wide (`fstat`/`open`/`read` measured the same) → tracking issue, not this slice. |
| **4** | LOW | scope R1 | A **third** existing-`#[test]` body edited (the C/Rust twin test's assertion **message**; asserted value byte-identical) while `impl.md` §6 and `fix-r1.md` §5 both say "two". Third instance of the same undercount, in the one guardrail enforced only through the self-report. | Two lines of markdown. |
| **5** | LOW | scope R2 | `impl.md` §6 — the section a human reads as the scope-and-guarantees statement — is **stale for this pass**: it does not record that a validation moved across the descriptor fence, nor that the ABI grew a method, and still says "three deviations" when there are four. Nothing is hidden (`fix-r1.md` has it all); the wrong document is authoritative. | Fold in, or state that `fix-r1.md` supersedes §6. |
| **6** | LOW | corr F4 + scope R3 | The README's new `-R` row records the round-1 discrepancy as **unresolved**. It is now resolved (§3). Keeping "unresolved" preserves a question that has an answer — a small false invariant of exactly the class this slice was waived to fix. | Rewrite the row with the actual cause. |
| **7** | LOW | corr R5 | On a descriptor umbra never issued, an unserved attribute set answers `ENOTSUP`(45) **before** `EBADF`(9); the kernel answers `EBADF`. Precedence cosmetic. | Optional; record if not changed. |
| **8** | LOW | corr R6 | `Overlay::directories` is evicted on `close` but not for a descriptor abandoned by `exec` (the key's `exec_generation` changes, the entry stays). Unbounded slow accumulation; not a correctness issue. | Evict on exec generation change, or record as accepted. |
| **9** | INFO | scope R4, R5 | The overlay README's corrected sentence kept "either" after a two→three fix; committed review artefacts describe the revision they reviewed, so every amend leaves a stale description inside the change. | No action. |

---

## 3. The F4 reversal — and I was wrong

Round 1's F4 (recursive truncation) is **withdrawn as an independent finding**,
but *not* for the reason I gave, and the correction is worth stating plainly
because I asserted the wrong cause with some confidence.

- **My explanation was wrong.** I proposed that the reviewer's fixture inherited
  `fts_set(…, FTS_SKIP)` from `umbra-userspace-listing.c`. The reviewer's
  `rec2.c` contains no `fts_set` and no `FTS_SKIP`; they showed the grep.
- **Their round-1 measurement was real**, not an artifact.
- **The cause was H2 itself.** Their fixture called `fts_children(FTS_NAMEONLY)`
  on every `FTS_D` *and* let `fts_read` descend — so it read each directory
  **twice**. The first enumeration ran to EOF on descriptor *N* and released it;
  `fts_read`'s own build of the same parent got *N* back and hit the cached empty
  remainder. `fts` saw an empty parent and never yielded the child.
- **My own fixture read each directory once**, which is why it could not exhibit
  it — the same reason the fix worker could not reproduce it. Two independent
  "cannot reproduce" results, both correct, both measuring the wrong shape.

So F4 was never a separate defect: it was H2 wearing a different hat, and fixing
H2 fixed it without anyone aiming at recursion. The eviction is load-bearing for
more than the case it was written for.

**This is the round's best result and it should not be lost in the finding
count.** It is also a caution about my own verification: a negative result from a
fixture I wrote is evidence about my fixture, not about the system.

---

## 4. The through-line, again

Round 1's lesson was *"the half of `resolve_directory` nobody re-read"*. Round
2's is narrower and sharper:

> **The H1 remedy moved a refusal across the fence and gave an errno to one of its
> two exits.**

Findings 1, 2 and 3 are all that one shape — a refusal path that can still end the
run, plus two comments and a test asserting it cannot. The fix was right in
design and incomplete in coverage, which is the same failure mode as round 1 one
level down. The `fix` pass should enumerate **every** exit from the relocated
validation and confirm each carries a bindable errno, rather than fixing the one
the reviewer found.

---

## 5. Routing decision

`review_synthesis → fix`, second traversal (budget 5, this is 2).

- Finding 1 is code and must land; findings 2 and 3 are its siblings and should
  land with it.
- Findings 4, 5, 6 are markdown and cheap.
- Findings 7, 8 are judgement calls: fix or record explicitly, either is
  acceptable, but **do not leave them undocumented**.
- Nothing re-opens `design_gate`. Both reviewers say so; the ratified objective is
  met, the deviations are sound, and the new ABI method is within the envelope on
  scope's reasoning (it *replaces* nothing the ratification protected).

**Expected next state:** if the `fix` pass closes findings 1–6, round 3 should be
a clean pass and the slice goes to `publish`. The remaining risk is concentrated
entirely in finding 1's blast radius — one constructor, one errno — so a third
review round should be cheap.

---

## 6. For the `fix` worker

- **Enumerate every exit** from `RequestedAttributes::decode` and from
  `unserved_directory_request`, and prove each carries an errno the supervisor can
  bind. Do not fix only the header arm.
- Re-run the correctness reviewer's **request-shape matrix** (ten shapes, one per
  process). It is the proof that finding 1 is closed, and it did not exist before
  this round.
- `reserved = 0x1234` and `bitmapcount = 3` are the two measured run-enders.
- Decide finding 2 deliberately: a fail-open default on a validation path is the
  same class as the parallel-admission-list defect this slice has cited four
  times.
- The `-R` README row now has a real answer — use it.
