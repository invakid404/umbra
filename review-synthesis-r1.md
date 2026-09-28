# review-synthesis — `dg-29vwer0f` / #121, round 1

**Inputs:** `review-correctness.md` (690 lines, 9 findings F1–F9) and
`review-scope.md` (437 lines, 7 findings S1–S7), both round 1, both against
jj change `psvqmvlm` / `9f94b8f3` (`feat/ls-userspace`), parent `a85a8471`.

**VERDICT: FINDINGS PRESENT → `fix`. Not publishable.**

Two independent reviews, one clean on its own axis and one not. The deciding
facts are three confirmed HIGH defects in the correctness review, one of which
is a **measured regression against master on registries this slice was not
supposed to touch**, and one of which is a **silent wrong answer** — the exact
disposition this codebase refuses on principle.

---

## 1. The verdict in one paragraph

The ratified objective is **met**: `ls` genuinely serves directory reads on a
virtual descriptor, proven by execution and by a mutation probe whose negative
control fails on the *names*. Scope is **clean** — nothing on the out-of-scope
list landed as mechanism and #123 left no trace in the repo. But the slice
regresses `ls -l`/`-t`/`-i`/`-p`/`-S`/`-F`/`-s`/`-n` on the rewrite-backed
registries from exit 0 to a stopped run, it stops the run on any
`getattrlistbulk` against a descriptor umbra does not own, and a second read of
the same directory in one process returns **zero entries with exit 0**. None of
these is caught by any gate we have, including the ones I ran myself.

---

## 2. Merged findings, ranked

Deduped across both reviews. Severity is mine, not either reviewer's, and is
assigned by *what a user experiences*, not by how hard the fix looks.

| # | sev | source | finding | fix shape |
|---|---|---|---|---|
| **1** | **HIGH** | F1 | **Regression vs master.** The 461 attrlist is validated in `decode_entry` (`abi.rs:991`) which runs *before* the descriptor-floor test (`events.rs:387`), so `ls -l/-t/-i/-p/-S/-F/-s/-n` now **stop the run** on `--local-dev` and kernel-`nfs`, where master exited 0. Fires on descriptors umbra does not own, on registries umbra does not route. | Move the validation after the floor test, **or** carry the refusal as a bindable `ENOTSUP`(45) — what the kernel itself answers — instead of an `Err`. The second form also fixes the routed case. |
| **2** | **HIGH** | F3 | **Silent wrong answer.** `Overlay::directories` is never evicted on close and descriptor numbers are reused, so the **second** enumeration of a directory in one process hits a cached *empty remainder* and returns zero entries, **exit 0**. `/bin/ls <dir> <dir>` reaches it. Invisible because the tracee's stdout is `Stdio::null()`. | Evict the `directories` entries in `RoutedEffect::Closed`, or key them on something a reused descriptor cannot reproduce. |
| **3** | **HIGH** | F2 | `resolve_directory` resolves its descriptor with a raw `context.fds.get().ok_or(StaleHandle)` instead of `routed_binding`, so `getattrlistbulk` on an unbound virtual fd **stops the run** where `fstat`/`fchdir`/`close` all answer the tracee `EBADF`. | Route it through `routed_binding`, exactly as `resolve_routed_fchdir` — added by this slice, twenty lines away — already does. |
| **4** | MED | F4 | Recursive `fts` (`ls -R`, `find`) **silently truncates** to the top directory, exit 0, no `FTS_ERR`, `errno` 0 — while `README.md:687` calls that row "Not claimed, and **fail-closed**". Out-of-scope is respected; the *disposition* is misdocumented. | Doc decision: stop claiming fail-closed, or make the descent fail loudly. |
| **5** | MED | F1 + S-corollary | **The missing proof.** `run_fixtures`'s `/bin/ls` rows carry no flag variants, on any registry, so finding 1 is invisible to a green CI run. This is a *missing* proof, not a skipped one. | Add at least one metadata-requesting flag variant to the rewrite-backed matrix. |
| **6** | MED | S2 | `umbra-overlay/README.md:267` still says "A routed `Open` of a directory is refused: directory reads are not routed" — **this slice made that false**, and the file is absent from the diff. | One-line correction. In-scope maintenance, not new waiver territory. |
| **7** | MED | F1-adjacent | `events.rs:382-386`'s comment states the false invariant verbatim: "all four calls resume into the kernel exactly as they did when they were not breakpointed at all." True for three, false for 461. | Correct with the fix for finding 1. |
| **8** | LOW | S3 | Inside the **granted waiver**, `umbra_interpose.c:182` lists `close`(6) among calls "rather than anything this file interposes" while `:189` says it *is* interposed. Self-contradiction about the waiver's own subject. | Comment-only. |
| **9** | LOW | S1 | A **second**, undisclosed existing-`#[test]`-body edit (`abi.rs` `process_control_stubs_are_not_delivered_to_the_namespace`). The edit is fine; the self-report says "the one edit". | Correct `impl.md` §6. |
| **10** | LOW | S4 | `impl.md` §7's baseline is wrong: it says master is 822 (+3). **Measured: master is 813, so the delta is +12.** Reconciles exactly — 14 new tests minus the 2 needing `--features transport-raw`. | Correct the figure. |
| **11** | LOW | F6 | D1's "nothing was given up" overstates: the engine lost its only binding of the tracee-visible return value to an **engine-derived** quantity, and the compensating walk lives inside the encoder being validated. | Soften the claim in `impl.md`. |
| **12** | LOW | F5 | `dirents.rs`'s `PACKING` comment cites offset `0x24` as 4-aligned evidence; re-measured it is `0x28` and *is* 8-aligned. The rule stated is true; the evidence offered does not demonstrate it. | Comment-only. |
| **13** | LOW | F7 | `abi.rs:515,520` keep private `ATTR_BIT_MAP_COUNT`/`ATTRLIST_BYTES` beside `dirents`'s new public ones — one parallel-constant instance. | Use the shared constants. |
| **14** | LOW | F8, F9 | Two stale doc enumerations: the `Deny` "six things reach it" list omits `resolve_routed_fchdir`; the README's unclaimed-modes list names `-l/-la/-@/-R` while the mechanism also refuses `-t -S -F -s -n -p -i`. | Comment-only. |
| **15** | LOW | S5 | Three stale comments in edited files: "the three it added" (now four), "five mutation probes" (now six), the C/Rust twin test's message naming only `FsOp::Fstat`. | Comment-only. |
| **16** | LOW | S6 | `nfs-userspace/README.md:683` cites "impl.md §1.5"; this `impl.md` has no §1.5. Pre-existing dangle, now resolving to a present file lacking the section — the more misleading failure mode. | One line. |
| **17** | INFO | S7 | `memoria check` was not run and **cannot** be run from this workspace (`.jj`, no `.git`, exits 4). Two new source files sit inside documented ownership boundaries. | Run from a git worktree before publish — which the `publish` node already does. |

**Dropped as non-findings:** none. Every item from both reviews is carried.

---

## 3. What the two reviews agreed on, and where only one looked

**Agreed:** the ratified objective is met; D2 and D3 are correct and stay; the
mutation probe genuinely discriminates; every #55–#120 mechanism is intact; the
fd fence is preserved; nothing out of scope landed as mechanism; #123 left no
repo trace; nothing merged or pushed.

**Only correctness looked at** the newly-reachable surface, and that is where all
three HIGH defects live. Worth stating plainly: **a scope review cannot find
these.** Findings 1–3 are each a *correct-looking* diff that traces cleanly to a
ratified item. S-review passed G32 ("descriptor fence preserved") on the
strength of `events.rs:387`'s floor test — which is genuinely there and
genuinely correct, and which finding 1 shows is simply reached too late for one
of the four calls.

**Only scope looked at** the self-report's completeness (S1), the untouched-file
sweep for claims this slice falsified (S2), and the arithmetic (S4). None of
those is visible from a correctness posture either.

The two reviews are complementary rather than overlapping, and the fan-out earned
its cost: **each found things the other structurally could not.**

---

## 4. The through-line worth naming

Findings 1, 2, 3 and 7 are one shape: **`resolve_directory` and the 461 decode
path were written before anything could reach them, and this slice is the first
thing that does.**

- The return-value identity was wrong for the only ABI umbra has (that is D1, and
  it was caught and corrected).
- The descriptor resolution never went through `routed_binding` (finding 3).
- The entry cache was never evicted because nothing ever filled it twice
  (finding 2).
- The decode's placement relative to the fence never mattered because the decode
  could not fail (finding 1).

D1 was caught because the implementer had to make the check pass. The other three
were not, because nothing forced them. The correctness review's own phrasing is
exact: *"the half of `resolve_directory` nobody re-read."* That is the lesson for
`fix`: **the same argument that justified D1 applies to the whole of the
newly-reachable path, and it was applied to one line of it.**

---

## 5. Routing decision

`review_synthesis → fix`. Three HIGH defects, one of them a regression against
master and one a silent wrong answer, are disqualifying for `publish` under any
reading.

**For `fix`, in priority order:** findings 1, 2, 3 are code and must land.
Finding 5 (the missing flag-variant proof) must land with finding 1 — a
regression fixed without a proof that would have caught it leaves the same hole.
Findings 4, 6, 7 are documentation-or-decision and should land in the same pass.
Findings 8–16 are cheap corrections; batch them.

**Nothing here re-opens `design_gate`.** Both reviewers say so independently, and
I agree: the ratified objective is met, the deviations are sound, and every
defect is on the *implementation* of a ratified item rather than on its choice.
`merge_gate` should still see findings 1–4 explicitly, because the human's
standing condition 3 was "preserve every setup / recovery / cleanup / validation
guarantee" and finding 1 breached it on registries the slice was not meant to
touch.

**Do not re-run the full audit.** The audit's premises survived; two of its
inferences were corrected by measurement (already recorded), and nothing in
either review contradicts its structural conclusions.

---

## 6. For the `fix` worker, stated once

- Findings 1–3 each have a fix shape proposed by the correctness reviewer. They
  are observations, not prescriptions; measure before adopting.
- Finding 1's second form (bindable `ENOTSUP`(45)) fixes the routed case too and
  is what the kernel itself answers. Prefer it if it measures out.
- Re-run the **side-by-side against master** after fixing finding 1. That is the
  only check that proves the regression is gone, and it is not in CI.
- Finding 2's fix is twenty lines from a correct implementation of the same
  pattern written by this slice.
- After any fix touching the encoder or the cache, re-run the mutation probe
  **and its negative control**. The control failing is the proof.
- `cargo test --workspace --all-targets` at master is **813**, not 822.
