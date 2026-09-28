# review-synthesis — `dg-29vwer0f` / #121, round 4

**Inputs:** `review-correctness.md` round 4 (N4‑1, N4‑2 — both LOW) and
`review-scope.md` round 4 (scope holds; one bookkeeping finding), both against
`psvqmvlm` / `3480713b`.

**VERDICT: CLEAN PASS → `publish`.**

No HIGH. No MEDIUM. Nothing that ends a run. Nothing silent. **No fourth
instance of the errno-less class** — which was the question this round existed to
answer, and the one that would have triggered a pause instead.

---

## 1. The class is closed

The correctness reviewer did not accept the fix report's enumeration table; it
walked the axis itself, across **five files in three crates**, and tabulated
every error construction on the `getattrlistbulk` path against whether a
tracee-supplied value can reach it.

- Every tracee-supplied value terminates in an errno the supervisor can bind, or
  a `Deny`.
- Every remaining errno-less exit judges **umbra's own output** — provider/wiring
  faults, the encoder's own bytes, or umbra contradicting itself.
- The rule committed in round 3 ("inside `umbra-overlay` the errno must become
  `Deny` before leaving `resolve`") was verified **true of every path, not
  aspirational**: the conversion happens before `self.planned` is set, and the
  only errno that can arrive there is `ERANGE`, so encoder self-contradiction
  stays correctly fatal.

**And it was tested on a shape no previous round used: 15 conjunctions of two bad
arguments each — zero run-enders, 15/15 answered.** That matters because N3‑2
*was* a combination defect, and the single-input matrices of rounds 2 and 3 could
not have caught it by construction.

The fix report's one remaining *inferred* item is now confirmed rather than
assumed: `routed_binding` denies `EBADF` both when the descriptor is absent and
when its logical path is `None`, so the N7 guard's condition is a strict subset.

---

## 2. The deliberate divergence is acceptable, on measured grounds

At a small buffer on a directory whose longest name sorts early, the kernel
serves what fits and umbra answers `ERANGE`. The reviewer gave three measured
grounds rather than an opinion, and one of them is new to this round and
decisive:

> **It is unreachable by `/bin/ls` and every fts-based utility**, which no
> earlier round established.

`ls` sizes its own buffer. A growing paging consumer was measured host-vs-routed
across two directories and got the same entries, the same growth and the same
final capacity. So the divergence is real, honest (a bindable errno, run
survives), and outside the ratified objective's reach.

---

## 3. Findings, and both are optional

| # | sev | source | finding | disposition |
|---|---|---|---|---|
| 1 | LOW | N4‑1 | The table enumerates the **request**'s six values and six is complete. A **seventh tracee-*influenced* input** — the directory's own entries, created via routed writes — reaches `encoder_fault`, errno-less. **Measured unreachable**, so *not* an instance of the class; but it sits outside the rule's stated scope ("a value in this table"). | Optional: widen the rule's wording, or record why entries are excluded. |
| 2 | LOW | N4‑2 | Under conjunctions of two bad arguments, umbra's errno **precedence** differs from the kernel's on 7 of 15 shapes (e.g. `filefd-zerocap`: kernel `ENOTDIR`, umbra `EINVAL`). Every shape answers a real errno and the run survives. | **Reviewer recommends NOT fixing, and I agree.** It is descriptor *kind* vs *existence*, one level below N7, and chasing it risks the shadowing shape that produced N3‑2. |
| 3 | LOW | scope | `fix-r3.md` §7 says "+4, no test removed"; measured **+3, one removed** (a round-1 test superseded by the broader sweep). Guardrail unaffected; gate figure 832 correct. | One line. |

Both reviewers state explicitly that they have no reservation about publishing.

---

## 4. The durable lesson, worth carrying beyond this slice

Rounds 1–3 each enumerated a **structure** and each structure was bounded by a
crate:

| round | axis enumerated | bounded by | result |
|---|---|---|---|
| 1 | — (per-finding) | — | H1 fix created N1 |
| 2 | **constructors** | one crate | missed row 5 |
| 3 | **exits of one function** | one crate | N7 fix created N3‑2 |
| 4 | **inputs** | **follows data across crates** | class closed |

> **Inputs are the only one of the three axes that follows the data across crate
> boundaries.** That is why the first two enumerations kept leaving instances
> behind and the third did not.

A second lesson, from the four self-report discrepancies this slice produced
(master baseline; test-body count, twice; test delta): **every one was caught by
a sweep and none by reading the report.** The remedy that landed in round 3 — 
`impl.md` §6 naming the *sweep that finds them* rather than asserting a count —
is the right shape, and finding 3 is the argument for extending it to the other
counts rather than correcting one more number.

---

## 5. Routing

`review_synthesis → publish` (fallback edge, first traversal).

The `review_synthesis → fix` edge finishes at **3 of 5**. My standing commitment
— pause rather than dispatch a fifth remediation round on a fourth instance —
does not fire, because there is no fourth instance.

**Carry to `merge_gate`, not to `fix`:** findings 1–3, the deliberate divergence
(measured acceptable, and `ls`-unreachable), tracking issues #125/#126/#127, and
the four-round history. None blocks publication.

**Not re-opening `design_gate`.** The ratified objective is met and was never in
question after round 1.
