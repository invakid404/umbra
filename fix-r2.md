# Fix round 2 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix`, visit 2, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
Input: `/tmp/graph-dg-nsw71bqq/review-synthesis-r2.md`.
Pin at the start of this round: commit **`f1b57a67`**, six paths — **both reviewers
independently confirmed the tree did not move under them this round**, which is the
no-root-writes policy from F4 working as intended.

**A deliberately small round, and it was kept small.** Round 1's twelve findings are
all closed. Scope round 2 is a clean pass on all six targets; correctness round 2
found exactly one gating item. Both reviewers judged this too small for a full round.
So: exactly the listed edits, one lesson-28 sweep over them, gates re-run for the
record, stop.

**No code change, no new measurement, no production source, slice 1 not started.**
`native.rs`, `abi.rs`, `rsp.rs`, `umbra_interpose.c`, `journal.rs`, `events.rs` and
`run_fixtures.rs` remain byte-identical to master. `single_thread()` stays
(`native.rs:498`, call sites `:1922`/`:1967`); the `debug_assert!` stays (`:571`).
Waivers 3 and 4 remain unconsumed.

---

## Disposition

| id | Disposition |
|---|---|
| **R2-1** | **Fixed** — and the identical defect was found in **two** further places the synthesis did not flag: the `mt_spawn` doc comment and the commit description |
| **S2-1** | **Fixed** — inventory re-derived from every document's own header; all three errors corrected, and a fourth caught |
| Cosmetic 1 | **Fixed** — `sandbox.rs` / `run.rs` crate-qualified |
| Cosmetic 2 | **Fixed** — `native.rs:2386` → `:2387`, all five occurrences |
| Guard figures | **Replaced** with the mechanism and the range; the load-bearing claim moved off the guard entirely |
| §2.5 order | **Restructured** to lead with the structural argument, runs as corroboration |
| Lesson 28 ×3 | **Added** to the item-6 payload as a process finding |

---

## R2-1 — the README attributed `mt-spawn`'s non-silence to a guard that fires 0/10 · **FIXED**

The clause was *"the run does not end silently: the supervisor holds an independent
per-thread guard…"*, sitting in the paragraph about `mt-spawn`. On `mt-spawn` that
guard fired **0 of 10**. What actually ends that case in release is the path decode
refusing the overwritten operand — `Io during path: null or overflowing pointer`
(`EFAULT`) — which my own round-1 runs measured at **5 of 5** release runs, each
exiting non-zero.

The README paragraph now says that, and the guard has been moved into a parenthetical
that states what it *is* good for (it is why the supervisor layer is not single-slot)
and explicitly disclaims the role it was given: *"It is not what makes this case
non-silent, and the paragraph above used to say it was."*

`impl.md` needed no change here and got none on this point: §2.3 already said
"detectable" and "is timing", and §2.5's table already attributed release failures to
`ProcessFailed` / `Io during path` without citing the guard. The synthesis's scoping
was right.

**Beyond the finding, and the reason this round's sweep mattered: the same sentence was
in two more places.** `tests/fixtures.rs`'s `mt_spawn` doc comment carried *"The run
does not fail silently in release… The supervisor holds a second tripwire that ships"*
followed by the 10-run figure, and the **commit description**'s `mt-spawn` bullet
carried *"the run does not fail silently: the supervisor holds an independent
per-`ThreadId` guard"*. Identical misattribution, identical position, all three written
in the same round-1 pass — so a finding filed against one file was really a finding
about one sentence copied three times. All three now name the EFAULT refusal as this
case's mechanism and demote the guard to a parenthetical with its 0/10 rate.

## S2-1 — the stale-document note was wrong three ways · **FIXED**

That note is F5's entire mitigation for leaving thirteen other-arc documents in the
tree, so being wrong inside it was worse than the thing it mitigated. Re-derived by
reading every root document's own header rather than by pattern-matching filenames:

| arc | PR | documents at `f1b57a67` |
|---|---|---|
| **`dg-nsw71bqq`** (this arc) | — | `impl.md`, `fix-r1.md` — **2** |
| `dg-egt6apy1` (#117 + #121's escalation) | #129 | `fix-r2.md`, `publish.md`, `review-correctness.md`, `review-scope.md`, `review-synthesis-r1.md`, `review-synthesis-r2.md`, `review-synthesis-r3.md` — **7** |
| `dg-29vwer0f` (#121) | #128 | `ci-round1.md`, `ci-round2.md`, `ci-fix-r1.md`, `ci-fix-r2.md`, `fix-r3.md`, `review-synthesis-r4.md` — **6** |

All three of the synthesis's corrections confirmed:

1. **Double-counted `fix-r1.md`.** §0 claimed it as this arc's and then listed
   `fix-r1..r3.md` as previous-arc — it counted a file it had itself just written as
   somebody else's. The stale `fix-*` range was `fix-r2..r3.md`.
2. **Collapsed two prior arcs into one.** Seven documents are `dg-egt6apy1`'s; six
   are `dg-29vwer0f`'s. The note attributed all of them to `dg-egt6apy1`.
3. **"eight-plus" understated it.** The count was **13**.

**And a fourth error, which this round's own sweep caught: the number is not stable.**
This round's `fix-r2.md` overwrites `dg-egt6apy1`'s, so on this commit the inventory is
**3 this arc's** and **12 stale** — 6 and 6 — and the stale `fix-*` range is
`fix-r3.md` alone. §0 now carries the per-arc table, states the 13 → 12 movement and
why, and §7.10 puts the instability itself on #132: *the inventory is not stable
across rounds of the same arc.* Writing the note changes what the note describes,
which is the same fixed-point problem §8 already has for its line counts.

## Cosmetic 1 — ambiguous `sandbox.rs` / `run.rs` paths · **FIXED**

Confirmed ambiguous, and both intended files are the `umbra-supervisor` ones:

```
crates/umbra-cli/src/commands/run.rs          crates/umbra-core/src/sandbox.rs
crates/umbra-supervisor/src/run.rs            crates/umbra-supervisor/src/sandbox.rs
```

Every reference is now crate-qualified: `crates/umbra-supervisor/src/sandbox.rs:42-47`
(the `occurrences != 1` hard-fail, verified at `:43`) and
`crates/umbra-supervisor/src/run.rs:1218-1234` (the `write_root` derivation through
`sandbox::render`, verified at `:1218`, `:1233`, `:1234`). `impl.md` §2.5 additionally
now cites `crates/umbra-supervisor/src/sandbox.rs:18` for the `include_str!`, which is
the fact that makes the argument one about *shipped* policy.

## Cosmetic 2 — `native.rs:2386` should be `:2387` · **FIXED**

Verified: `s.return_stop(ReturnKind::Syscall)` is at `native.rs:2387`; `:2386` is the
`if get(&regs, PC)? == pc {` above it. Corrected in all five places that carried it —
`impl.md` ×3 (§1's per-arm table, §1's correction note, §3), `fix-r1.md` ×2, and
`tests/fixtures.rs`'s module header ×1.

## The `events.rs:307` discrepancy — resolved as a race, and the claim moved off it

Round 1 left two figures attributed but unreconciled: the reviewer's 5/5 in release
against my 0/20. Round 2 resolved it by holding everything constant but the
environment — same commit, same fixture binary (`02fb7ad8`, byte-identical across both
rounds because the `umbra-test-child.c` diff is comment-only):

| case | condition | `InvalidState` fires |
|---|---|---|
| `mt-write` | idle | **1/20** |
| `mt-write` | 10 spinners / 10 cores, loadavg 3.65 | **5/20** |
| `mt-spawn` | idle | **0/10** — always `Io during path` |

It is a race on machine load. The reviewer sampled while a parallel reviewer occupied
the same cores; I sampled a quiet machine. **Neither figure was wrong**, and my "the
denial kills the child first" was the *usual* case rather than the only one —
`impl.md` §2.3 now says exactly that instead of presenting two numbers and leaving the
reader to reconcile them.

**The load-bearing claim is now off the guard entirely, because it never needed it.**
Release non-silence rests on two witnesses that hold every time:

- the **denial** — guaranteed by the structural argument below, not by sampling;
- the **non-zero exit** — 20 of 20 in the enforced runs measured for `impl.md`.

Round 2's contention runs are counted separately and not folded in: the review reports
`InvalidState` frequency for them, not exit codes, so they corroborate the guard's
range and nothing else. An earlier draft of this section aggregated them into a "30 of
30" exit figure, which was wrong twice over — round 2 ran 50 runs, not 10, and their
exit codes were never reported. Caught by this round's sweep; see the table below.

The guard is 1/20–5/20 on `mt-write` and 0/10 on `mt-spawn`: a contention-dependent
extra witness. It stays in the document for the two reasons that do not depend on its
frequency — it is why the "silent in release" sentence was wrong, and its
per-`ThreadId` keying is the asymmetry that localises the defect and tells us slice
1's shape is the architecturally consistent fix.

## §2.5 now leads with the structural argument · **RESTRUCTURED**

Round 2's correctness review judged the policy argument to outrank the run counts and
asked that the document lead with it. It now does, as four checkable facts before any
run is mentioned:

1. `TEMPLATE` is `include_str!(".../experiments/seatbelt/umbra.sb")`
   (`crates/umbra-supervisor/src/sandbox.rs:18`) — the argument is over the policy the
   binary ships, not a file a deployment supplies.
2. Exactly **one** line matches `^(allow file-write` in that template (`umbra.sb:13`),
   beside `(deny default)` and `(allow file-read*)`, with the template's own closing
   line disclaiming `/tmp` and `/private/var/folders` carve-outs.
3. `render()` **hard-fails** unless the token appears exactly once
   (`crates/umbra-supervisor/src/sandbox.rs:42-47`) — no path renders a profile with a
   second or missing write allowance.
4. The one allowance is `<store>/<run-id>/root`
   (`crates/umbra-supervisor/src/run.rs:1218-1234`), and an escaped write is by
   definition **unrewritten**, so it targets the tracee's workspace path — outside the
   sole allowance. The run root is a fresh UUID directory created during preparation,
   so no argv operand can name it.

A `(deny default)` profile whose only write allowance is a path the escaped write
cannot be addressing must deny that write. The 20 runs are labelled *"The
corroborating runs"* and framed as confirming the construction behaves as read, not as
establishing it.

## Added to the item-6 payload: lesson 28 fired three times, as a process finding

Now `impl.md` §7.7, recorded as process rather than code:

1. the original README invariant that slice 0's own README edit corrected;
2. **the fix for (1) introduced R2-1** — a false mechanism attribution inside the very
   paragraph written to remove one;
3. **the fix for F5 introduced S2-1** — a stale attribution inside the note written to
   stop stale attributions misleading readers.

Each was caught only because a reviewer swept the *corrected text* adversarially
rather than the change as a whole. The recorded conclusion: in this codebase a
documentation correction is itself a likely site of a new false claim, so correction
passes should be reviewed as adversarially as code, and a fix worker should sweep its
own corrections before handing them on. §7.7 also records the fourth instance, caught
inside this round by that sweep (the 13 → 12 count).

---

## Lesson-28 sweep over everything edited in this round

The named failure mode of this round, so it was run deliberately over each edit rather
than over the change as a whole. **Nine issues found; all nine fixed** — and only one
of them is the finding the synthesis filed. The other eight were in round 1's
corrections or in this round's own, which is precisely the pattern §7.7 records.

Three of the nine are the *same* defect as R2-1 in three different places: the README
(the filed finding), the `mt_spawn` doc comment, and the commit description. All three
were written in the same round-1 pass, so a finding scoped to one file was really a
finding about one sentence that had been copied three times.

| swept | found |
|---|---|
| README `mt-spawn` paragraph (R2-1 fix) | **Found:** the identical misattribution in `tests/fixtures.rs`'s `mt_spawn` doc comment, which the synthesis had not flagged. Fixed. |
| §0 stale-document note (S2-1 fix) | **Found:** the corrected count is itself unstable — writing this round's `fix-r2.md` moves it 13 → 12. Stated in §0 and put on #132 (§7.10). |
| §7's new process item (renumbering) | **Found:** inserting it as item 7 silently invalidated four cross-references — `impl.md` §7.9→§7.10 and `fix-r1.md` §7.8→§7.9, §7.9→§7.10. All repointed and every `§7.N` reference re-checked against an existing item. |
| §7.9's PR-title item | **Found:** it still said *"the commit description stays as written"*, which stopped being true when round 1 amended the body to drop the silent-in-release claim. Reworded to say the type and framing are what both reviews ratified, and that the body was amended. |
| §0/§8 path counts | Six → **seven** paths and three process documents, since `fix-r2.md` joins the diff. §8's content subtotal is unchanged in kind: the four content files' counts are the stable ones and are restated from the tool, while the process documents stay named-but-uncounted for the fixed-point reason §8 gives. |
| §2.5 restructure | No new claim introduced: every one of the four structural facts was re-verified against source in this round (directive count by `grep -c '^(allow file-write'` = 1; `include_str!` at `sandbox.rs:18`; `occurrences != 1` at `:43`; `write_root` at `run.rs:1218`/`:1233`/`:1234`). |
| Cosmetic path/line fixes | Verified by re-grepping for the old forms afterwards: zero remaining `:2386`, zero unqualified `sandbox.rs:42-47` or `run.rs:1218-1234`. |
| The "non-silence rests on X" claim | **Found:** the draft aggregated my 20 enforced runs with round 2's contention runs into "30 of 30 exited non-zero". Wrong twice: round 2 ran **50** runs (20 idle + 20 loaded `mt-write`, 10 `mt-spawn`), not 10, and the review reports `InvalidState` frequency for them rather than exit codes. Corrected to **20 of 20**, with the two sets kept separate in `impl.md` §2.3 and §2.5. |
| Every `§N` cross-reference in all three documents | **Found:** `§0` resolved to nothing — `impl.md`'s headline section was unnumbered while three documents referred to it as `§0`. Heading renamed to `## 0. Headline: …`. Every `§N`/`§N.M` reference in `impl.md`, `fix-r1.md` and `fix-r2.md` now resolves to an existing heading or item, checked mechanically. |
| `fix-r1.md`'s description of `§8` | **Found:** it still claimed `§8` "carries per-file line counts including the two process documents", which stopped being true when `§8` was restructured around the fixed-point problem in the same round. An as-of note now scopes round 1's six-path figure to `f1b57a67` and states what `§8` does instead. |
| The **commit description** | **Found:** the identical R2-1 misattribution, a *third* instance — its `mt-spawn` bullet said "the run does not fail silently: the supervisor holds an independent per-`ThreadId` guard". Amended to name the EFAULT refusal, with the guard demoted to a parenthetical stating its 0/10 rate. The `mt-spawn` severity paragraph was also reordered to lead with the structural policy argument, matching `impl.md` §2.5. |
| **This document's own panic-line claim** | **Found:** the draft said the `mt_write` assertion line moved 398 → 406, extrapolated from the file growing rather than measured. It is still **398** — every edit this round landed after it. Corrected, and left recorded in the gates section as the fifth instance. |

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

The C fixture was recompiled from this tree and `cargo build -p umbra-platform-macos
--tests` re-run **after** the last edit, before any figure below was taken. The only
compiled artefact this round could affect is `tests/fixtures.rs`, and only its
comments changed.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED` |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS` verdicts, 2 declared `SKIP nfs_fixture_matrix` / `SKIP nfs_utility_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS**, both new arms included |

**The 832 figure carries the same qualification it has carried since round 0 and is
still the weaker figure**: no `--nocapture` and no fixture environment, so every
integration case in it takes `fixture_argv`'s skip branch (`tests/fixtures.rs:47`) and
reports `ok` with its `SKIP` invisible. The rows beneath it are the qualification.

**The eleven `CAPTURED` verdicts, read by name from this round's own captured
output:** `argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`,
`fork-write`, `grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`,
`symlink-cycle`, `wnohang-wait`. Zero `SKIP`, zero `MISSED` in that run.

*On the `PASS` count:* correctness round 2 noted its own first count came back 15
rather than 20 and reported it as a grep artefact rather than a finding — five verdicts
share a line with the `test <name>` prefix under `--nocapture`. Counting distinct
`PASS <case>` occurrences rather than lines gives 20, which is what the table above
reports and what round 1 reported.

**The two `SKIP`s are skips, not passes**, both on `UMBRA_TEST_SKIP_NFS_MATRIX` — the
same opt-out CI sets for that job. Nothing about a live NFS mount is qualified here.

**The five `#[ignore]`d tests, by name:** three pre-existing NFS-fault cases in
`umbra-storage-nfs/tests/mounted.rs`, plus `mt_write` and `mt_spawn`.

**Both `#[ignore]`d cases re-measured with `--ignored` against this build**, giving the
same two verdicts as rounds 0 and 1 — this round changed what the documents claim, not
what the tracer does:

```
thread 'mt_write' panicked at crates/umbra-platform-macos/tests/fixtures.rs:398:5:
MISSED mt-write: the second thread's output reached the host at …

thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
a second intercepted syscall entered while one was still in flight: …
```

The `mt_write` assertion line did **not** move this round: it is still
`fixtures.rs:398`, because every edit this round landed *after* it (the `mt_spawn` doc
comment sits further down the file). `impl.md` §5.5's record of the earlier 373 → 375
→ 398 movement therefore stands unchanged.

*This paragraph is the round's fifth lesson-28 catch, and it was mine.* The draft of
this document asserted the line had moved to 406 — extrapolating from "the file grew"
without running it. Running it gave 398. Recorded rather than quietly corrected,
because it is the identical error class this round exists to sweep for: a plausible
number written into a correction pass without being measured.

**No new measurement was taken.** Every enforced-run figure in `impl.md` is round 1's,
restated; round 2's contention table is the reviewer's, attributed to it.

---

## Not done, and why

- **Slice 1**, any code change, any production-source edit, any new measurement — out
  of scope by instruction and by both reviewers' judgement that this round is small.
- **Removing the twelve other-arc root documents** (F5/S2-1). They belong to two other
  arcs and deleting them would pre-empt #132; noted precisely instead, which is the
  mitigation S2-1 exists to keep honest.
- **Items 6 and 7** — `publish`'s obligations, blocked here by #123/#131. Item 6
  remains marked a ratified ship-gate.
- **The PR title** — `publish`'s to set; recommendation recorded at `impl.md` §7.9.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
