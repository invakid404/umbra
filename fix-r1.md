# fix r1 — Memoria 0.4.0 → 0.7.0 upgrade

- **Graph:** `dg-t43fjdpv` · **Node:** `fix` (round 1) · **Date:** 2026-09-29
- **Input:** `knowledge/umbra/graph-audits/dg-t43fjdpv-review-synthesis-r1.md`, over `dg-t43fjdpv-review-correctness.md` (246 lines) and `dg-t43fjdpv-review-scope.md` (265 lines). All three read in full.
- **Reviewed commit:** `d9c1a046fb6a0fc3e48ea783d6339554918375c1`
- **Binary:** `/tmp/graph-dg-t43fjdpv/m070/bin/memoria` → `memoria 0.7.0`, invoked by absolute path. The `PATH` binary is 0.6.0 and was never used.

## What the reviews found

Both reviewers PASS the shipped mechanism and the gate. Every guardrail held, and **not one finding is a defect in what CI will run.** The six findings are three documentation/comment errors and three false statements inside `impl.md` — this arc's own record. Three further items (`I1` token discrepancy, `I2` `impl.md` supersession, `I3` the non-ancestor commit in the lock) were cleared by the reviewers and are deliberately left alone.

All six were applied as **one batched edit**, closed by **one ack**, per the synthesis instruction: `docs/memoria.md`, both workflows and `impl.md` are all root-README scope sources, so acking between them would only re-pend and burn cycles.

---

## M1 — MEDIUM — `docs/memoria.md:30` — dangling anchor this arc introduced

`../.claude/skills/memoria/SKILL.md#procedure-after-code-changes` pointed at a 0.4.0 heading that the 0.7.0 package deleted. Confirmed here before editing:

```
$ grep -ci "procedure after code changes" .claude/skills/memoria/SKILL.md
0
$ grep -n '^#' .claude/skills/memoria/SKILL.md
6:# Memoria skill        10:## Authority first   19:## Concepts     28:## Select
36:## Capture            44:## Inspect          53:## Reconcile    59:## Record
68:## Finish             72:## When to load the other files       78:## Limitation
```

The sentence promised three things, and in 0.7 they live in three different sections, so it now carries three links rather than one: **Select** (`data.next_action`, which document to review, never acknowledge an empty plan) is the review order; **Capture** (`memoria review <DOCUMENT> --save "$dir" --format json`) and **Record** (`memoria ack … --packet --reviewer --result --note`) are the exact commands and the packet and token rules.

```diff
-Use the [skill's procedure](../.claude/skills/memoria/SKILL.md#procedure-after-code-changes)
-for exact commands, review order, and packet/token rules. Review packets belong
-outside the repository so they do not become documentation inputs.
+Follow the skill's [Select](../.claude/skills/memoria/SKILL.md#select),
+[Capture](../.claude/skills/memoria/SKILL.md#capture) and
+[Record](../.claude/skills/memoria/SKILL.md#record) sections for review order,
+exact commands, and the packet and token rules. Review packets belong outside
+the repository so they do not become documentation inputs; `--save` refuses any
+directory inside the Git worktree for that reason.
```

The trailing clause is not decoration: `SKILL.md:42` states that `--save` refuses any directory inside the Git worktree *because a saved artifact there could become a review input*. That is the enforcement behind the sentence's existing claim, and it was worth naming once the anchor had to be rewritten anyway.

### The whole reference axis, swept

The reviewers were right that this axis had never been checked — H-1 enumerated *claims*, not *references*. Every relative link and every anchor in `docs/memoria.md` now resolves:

```
=== every relative target resolves ===
OK   ../.claude/skills/memoria/SKILL.md
OK   ../.github/workflows/memoria-auto-ack.yml
OK   ../.github/workflows/memoria.yml
OK   ../memoria.toml

=== every anchor resolves (against the target file's own headings) ===
OK   #capture in ../.claude/skills/memoria/SKILL.md
OK   #record in ../.claude/skills/memoria/SKILL.md
OK   #select in ../.claude/skills/memoria/SKILL.md
```

Five links before, seven after; four distinct targets, all present; three anchors, all matching a heading in the 0.7 package. There are no other anchors and no absolute or external links in the file.

---

## M2 — MEDIUM — `impl.md` §8 claimed finality for pre-closing-ack figures

The block headed *"Verification, on the committed tree"* asserted it was *"measured against the exact bytes of the change and not an intermediate working state"*, and then quoted `File bytes 7651` / `Revision 316` / `revision 50 … (updated)` — all of which belong to `3e41104c`, the commit *before* the closing ack. The one section claiming to describe published bytes was the one that did not.

Both remedies the synthesis offered were available; **re-labelling was chosen over re-taking the numbers, and deliberately so.** Re-taking them cannot terminate: the closing ack rewrites `memoria.lock` after the last edit, so any `state inspect` written into a tracked file is stale the moment the ack that seals the tree runs. Chasing the bytes would need a second ack to record the first one's output, and so on. Labelling the block for the commit it actually describes is stable and true at every future commit.

```diff
-## 8. Verification, on the committed tree
-
-… so what follows was measured against the exact bytes of the change and not an
-intermediate working state.
+## 8. Verification, at the pre-closing-ack commit `3e41104c`
+
+… so what follows was measured against committed bytes rather than a working state.
+
+**Read the `state inspect` figures below as commit `3e41104c` only.** They predate
+the closing ack that Deviation 1 describes, which rewrites `memoria.lock` — so at
+the published commit `File bytes`, `Revision`, and README's revision and result all
+move on by one cycle. What does hold at every commit from here on, including the
+published one, is the part that matters: `check` exit 0, `OK: 23 document(s)
+current`, and lock format 3.
```

The stale `+76 bytes` at `impl.md:383` is corrected in place rather than re-taken, for the same reason — the note now records the published delta as **+21 bytes** (7575 → 7596) as measured by `review_scope`, and flags that round r1 moves it again.

Per the synthesis's explicit warning, the transcript at `impl.md:316` showing `Recorded README.md revision 50 (updated)` was **left untouched** — that is genuinely the first ack and it is correct. Likewise `impl.md`'s reporting of token `mrv3.d7ef4246ab3d14a7` is untouched: `I1` resolved it in the record's favour.

One extra line was tightened for the same defect class — see Deviations.

---

## M3 — MEDIUM — `impl.md:49` recorded a false mechanism

The record claimed that editing `memoria.toml`'s `[documentation] guidance` array *"would change the guidance digest and invalidate all 23 documents, turning a 1-ack upgrade into a 23-ack one."* `review_scope` falsified that with two probes at `d9c1a046`, each with restore-and-reverify:

| probe | result |
|---|---|
| A — rewrite the stale vocabulary in place | `Reviews 23 current, 0 pending`; `check` exit 0; `review` tasks 0 |
| B — append an entirely new guidance rule | `Reviews 23 current, 0 pending`; `check` exit 0; `review` tasks 0 |

Editing the guidance block invalidates nothing. `guidance_changed` is an advisory flag raised on tasks that are *already* pending — which is exactly how `scripts/memoria-auto-ack.sh:125` consumes it — not an invalidation trigger. The immediate ack cost is **zero**.

`impl.md:49` now states the real reason: **the block is outside the ratified set.** Step 1 is `memoria.toml:1` and nothing else, so editing the array would have been an unratified change. That reason is sufficient on its own and does not depend on a cost figure.

The evidence line at `impl.md:52` — `guidance: {"changed_since_review": false, …}` — was dropped. It proved the block stayed put, which nobody disputed, and it was never evidence for the invalidation claim.

The one genuine consideration was added in its place, because it is the thing a future maintainer actually needs: a changed digest raises `guidance_changed` on subsequently-pending tasks, and the auto-ack script skips those, so a guidance edit wants pairing with a re-ack sweep that refreshes `reviewed_digest`. That is recorded as follow-up work, not as a cost that forecloses it.

**The guidance block itself was not edited**, per the hard constraint. `memoria.toml` is unchanged from `d9c1a046`.

---

## L1 — LOW-MEDIUM — `.github/workflows/memoria-auto-ack.yml:59` — stale cross-workflow reference

```diff
           # PAT (not GITHUB_TOKEN): a GITHUB_TOKEN-authored push is
           # suppressed from re-triggering downstream workflows, so the
-          # memoria gate in ci.yml would never re-run against the acked
+          # memoria gate in memoria.yml would never re-run against the acked
           # head. A fine-grained PAT with Contents:Read+Write on this
```

Only the filename changed, as instructed. The rationale survives intact: `memoria.yml` fires on `on: [push, pull_request]`, so a `GITHUB_TOKEN`-authored push would suppress it exactly as it would have suppressed the old in-`ci.yml` gate. The PAT is still required for the same reason.

```
$ grep -n 'ci.yml' .github/workflows/memoria-auto-ack.yml
(no ci.yml references remain)
```

---

## L2 — LOW — `docs/memoria.md:25`, `:101` — residual 0.6 vocabulary

```diff
-3. Cross-check the README's prose against its owned source and the project writing
-   rules. Edit claims that have drifted; refresh the review packet after edits.
+3. Cross-check the README's prose against the source in its scope and the project
+   writing rules. Edit claims that have drifted; refresh the review packet after edits.
```
```diff
-and `Cargo.lock` from owned inputs. Lockfile-only dependency updates do not require
+and `Cargo.lock` from review inputs. Lockfile-only dependency updates do not require
```

`review inputs` at `:101` is the same phrase the round-0 fix already used at `:85` (*"their scopes contain no review inputs"*), so the file is now internally consistent.

The sweep was re-run with the wider pattern the reviewers specified:

```
$ grep -niE 'own(s|ed|ership)' docs/memoria.md
57:version. Memoria generates and owns the file, recording the expected text in
70:acknowledge. 0.7.0 then replaced the ownership model with the scopes described
```

Two hits, both deliberate and both correct under 0.7: `:57` is memoria owning the *generated workflow file* (`integrations github`'s management record — real 0.7 behaviour, not the deleted scope model), and `:70` is the historical note that 0.7.0 *replaced* the ownership model, which has to name it to say it is gone.

---

## L3 — LOW — `impl.md:279` over-counted its own sweep

The claim was "two deliberate hits"; the narrow grep returns **one**. Re-measured at this round rather than patched, since L2 moves the number again:

```
$ grep -cE 'ownership|owns it|owned files|JSON v2|release tag|remain compatible' docs/memoria.md
1
$ grep -ciE 'own(s|ed|ership)' docs/memoria.md
2
```

The narrow pattern finds one (`:70`). The wide pattern finds two (`:57`, `:70`). The gap between them *is* the defect the reviewers identified: the original sweep was verified by an alternation narrower than the thing being swept for, so `owned source` and `owned inputs` could never have matched it. `impl.md` §6 now records that explicitly and defers the counts here instead of restating a fixed number that keeps moving.

---

## Verification

`memoria` still cannot run in the jj workspace — re-confirmed, unchanged since round 0:

```
$ /tmp/graph-dg-t43fjdpv/m070/bin/memoria --root . check
error [git_unavailable]
  git failed for .: not inside a Git worktree
exit status 4
```

So the same throwaway-git-worktree pattern was used: edits in the jj workspace → `jj describe`/amend → `git worktree add --detach` at the amended commit from `~/Coding/umbra` → memoria run there by absolute path → regenerated `memoria.lock` copied back → worktree re-pointed after each amend.

Both workflows still parse:

```
$ python3 -c "import yaml; …"
ok .github/workflows/memoria-auto-ack.yml
ok .github/workflows/ci.yml
ok .github/workflows/memoria.yml
```

### Guardrails, re-checked at this round

- `memoria.toml` — **unchanged from `d9c1a046`**, guidance block included.
- `.github/workflows/memoria.yml` — **not hand-edited**; still byte-identical to the generator output recorded in `.github/memoria-workflows/memoria.yml.json`, so memoria's management is intact and `integrations github status` stays `current`.
- `scripts/memoria-auto-ack.sh` — unchanged.
- Zero Rust, zero new `#[test]`, nothing outside the already-ratified file set: this round touches exactly three files, all of them already in the round-0 diff.

```
$ jj diff --stat --from d9c1a046
.github/workflows/memoria-auto-ack.yml |  2 +-
docs/memoria.md                        | 15 +++++++++------
impl.md                                | 24 +++++++++++++-----------
3 files changed, 23 insertions(+), 18 deletions(-)
```

Adding `fix-r1.md` makes four files in the round, and 20 paths in the change as published.

### A note on the closing figures, written the way M2 asks

`fix-r1.md` is itself a new root-level scope source — the same self-referential shape `impl.md` Deviation 1 describes — so writing it pends root `README.md`, and the node ends with one `memoria review README.md` → `memoria ack`. That ack rewrites `memoria.lock` *after* these bytes are fixed.

So, stating it plainly rather than repeating M2's mistake: **this file does not quote a `state inspect` or a lock byte count, because any figure it quoted would be one cycle stale by the time the tree is sealed.** What is asserted, and what is verifiable at the published commit, is the durable part — `memoria --root . check` exits 0 with `OK: 23 document(s) current, imports rendered, no coverage or structure errors`, and the lock stays format 3. That closing check is run from the git worktree against the final amended commit, and its output and the final commit id are reported in the node response.

---

## Deviations

1. **One line fixed beyond the six.** `impl.md:285` headed its `memoria status` transcript *"on the finished tree"* while quoting `Reviews 22 current, 1 pending`; the published tree reads `23 current, 0 pending`. `review_scope` noted this under S-5 as *"also noted, no action"* — but it is the identical misframing M2 exists to fix, one section away, and leaving it would have made the record self-inconsistent immediately after correcting §8. Reworded to *"taken once every edit was in place and before the first ack"*, which is what the transcript actually is. No figure changed.
2. **M2 resolved by re-labelling, not by re-taking the numbers.** Both were sanctioned; the reasoning for choosing the label is in M2 above — re-taking cannot terminate, because the sealing ack always writes after the last edit.
3. **M1 answered with three links rather than the two suggested.** The synthesis proposed `#capture` and `#record` as "the likely pair" and told me to read the file and choose. The sentence promises *review order* as well as commands and packet/token rules, and review order is `## Select`, so it takes three.

Nothing else departed from the synthesis. Not touched, as required: `memoria.toml` (guidance block included), `.github/workflows/memoria.yml`, `scripts/memoria-auto-ack.sh`, any Rust source, any test, `impl.md`'s first-ack transcript and its token reporting, and the three cleared INFO items.

## Out of scope — filed, not acted on

1. `memoria.toml` is now the only file in the repo still carrying `ownership boundary`, in two places (`:18`, `:19`), excluding the historical root artifacts. Cost is now known to be **0 acks**, with the `guidance_changed` pairing described in M3. A gap in the audit's own H-1 sweep, which enumerated files that *describe* memoria and missed the file that *configures* it.
2. Archive gap in `knowledge/` for `dg-0ved1w0e-impl.md` and `fix-r1.md` … `fix-r5.md`. Local, untracked, PR-neutral.

Both await human approval at `merge_gate`. Not pushed, no PR opened — that is the publish node's work.
