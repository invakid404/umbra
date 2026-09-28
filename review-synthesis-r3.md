# review-synthesis — `dg-29vwer0f` / #121, round 3

**Inputs:** `review-correctness.md` round 3 (N3‑1 … N3‑3) and `review-scope.md`
round 3 (**clean, no findings**), both against `psvqmvlm` / `20ccf7db`.

**VERDICT: FINDINGS PRESENT → `fix`. Two MEDIUMs, one of them created by the
previous pass. This is the third traversal of a budget-5 edge and the routing
rationale below is written with that in mind.**

---

## 1. What is now settled

| | |
|---|---|
| Scope | **clean, third consecutive round.** G36 finally passes; nothing out-of-scope; #123 leaves no repo trace. Nothing outstanding. |
| Round 1's three HIGH defects | closed, re-measured |
| Round 2's N1 (header arm) | **closed where it was found**, verified by my own five-shape probe |
| `encoder_fault` | **provably unreachable from a tracee request** — all three callers traced |
| The two new sweep tests | genuinely discriminate; no `is_err()`-shaped hole |
| The renamed constructor | sound, and a real improvement |

Scope review is **done**. Everything below is correctness.

---

## 2. Findings

| # | sev | source | finding |
|---|---|---|---|
| **1** | **MED** | N3‑1 | `dirents::encode`'s "buffer cannot hold one record" arm is an **inline errno-less `UmbraError::new`**, driven by the tracee's `max_bytes`. **Data-dependent run-ender.** Confirmed by my own probe: same binary, same 140-byte buffer, `short/` served and `withlong/` (one 200-char name) **kills the run**, while the host serves both. |
| **2** | **MED** | N3‑2 | **Regression from this pass's own N7 fix.** The N7 guard skips the whole `io_binding` branch for an unbound descriptor, removing the `io_buffer` empty-buffer `EINVAL` that had been *shadowing* `resolve_directory`'s errno-less `max_bytes == 0` check. `unbound` + zero-length buffer answered `EINVAL`(22) in round 2 and **ends the run** now. |
| **3** | LOW | N3‑3 | The new seven-exit enumeration concludes "None of them is reachable by a tracee's choice of request" — true of that *function*, false of the *request*, because the buffer size is a tracee's choice and reaches findings 1 and 2. |

---

## 3. The pattern, and it is now three for three

This is the part that matters more than the individual findings.

> **Every remedial pass has introduced or left a new instance of the same class
> on the same path.**

- Round 1's **H1 fix** introduced **N1** — the header arm with no errno.
- Round 2's **N7 fix** introduced **N3‑2** — by removing a guard that was
  *accidentally shadowing* an errno-less check nobody knew was there.
- **N3‑1** was live the entire time and was never reached, because every audit so
  far enumerated **constructors** or **exits of one function**, never the
  **tracee-controlled inputs**.

The class is precise: *a tracee-supplied value reaching an `Err` with no errno on
the directory path.* There are exactly two tracee-supplied values in a
`getattrlistbulk` request — **the attribute list** and **the buffer size**. Three
rounds have hardened the first and never enumerated the second.

**So the fix instruction for this round is not "fix these two".** It is:

> Enumerate the **tracee-supplied inputs**, not the constructors and not the
> exits. For each, trace every path it can reach and prove each terminates in a
> bindable errno or is provably unreachable.

That is a different axis from the one the last two passes used, and it is the
axis on which both remaining findings live. If that enumeration is done properly,
this class closes; if it is done per-finding again, expect a fourth instance.

---

## 4. Severity and the merge question

Neither finding is a silent wrong answer — both are **run-enders**, loud and
fail-closed, on an unrouted host and a routed run alike. Neither is a regression
against **master** (master refuses the directory open outright, so neither shape
is reachable there). Finding 2 is a regression against **round 2 of this slice**,
which is a real but narrower claim.

Against that: finding 1 needs **no malformed request** — a long filename that
sorts early and an ordinary modest buffer, which is a legitimate paging pattern.
That is a foreseeable shape in normal use, not an adversarial one.

**Routing to `fix` rather than to `publish`.** A data-dependent run-ender that a
200-character filename can trigger is not something to hand to a human at
`merge_gate` as "known and accepted" when the remedy is the same one-line shape
applied twice already.

---

## 5. Budget

`review_synthesis → fix` is **traversal 3 of 5** after this. Stated plainly so the
constraint is visible rather than discovered: two remediation rounds remain. If
round 4 produces a fourth instance of this class, the right move is **not** a
fifth attempt — it is to pause and put the pattern to the human, because at that
point the evidence is that per-finding remediation is not converging and the
decision to keep going belongs to them.

I judge round 4 likely to close it, because the axis is now named and the
enumeration is bounded: there are two tracee-supplied inputs and one has already
been done thoroughly.

---

## 6. For the `fix` worker

- **Enumerate the tracee-supplied inputs, not the exits.** `max_bytes` is the one
  never enumerated. The attribute list already has a sweep; give `max_bytes` the
  same treatment.
- Finding 1's remedy is the one this slice has already chosen twice: bind an
  errno. `ERANGE`(34) is what the kernel answers for a too-small buffer — measure
  it rather than assuming, as the `ENOTSUP` premise taught.
- **Consider whether the sort order itself is worth a note.** `merged()` is
  byte-sorted while the kernel's enumeration order is not, which is why the long
  record lands first. That is not a defect, but it is why the threshold is
  data-dependent, and it belongs in a comment.
- Finding 2 is a *shadowing* bug: a guard was removed and exposed a check that
  had never been reached. When you fix it, check whether any **other** errno-less
  check was being shadowed by the same guard.
- Finding 3 is one sentence, and it should be corrected to say what is actually
  true rather than deleted.
