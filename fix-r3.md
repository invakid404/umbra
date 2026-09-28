# Fix round 3 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix`, visit 3, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
Input: `/tmp/graph-dg-nsw71bqq/review-synthesis-r3.md`.
Pin at the start of this round: commit **`f7c15ee2`**, seven paths, no production
source — **the pin held a third consecutive round**, both reviewers confirming
independently.

**One finding, three sentence-level substitutions in `impl.md` §8, plus the instructed
item-6 addition. Nothing else.** Correctness round 3 is a clean pass; scope round 3 is
a clean pass on all four targets. No code change, no new measurement, no production
source, slice 1 not started. `single_thread()` stays (`native.rs:498`, call sites
`:1922`/`:1967`); the `debug_assert!` stays (`:571`); waivers 3 and 4 remain
unconsumed — and round 3 proved the last of those structurally rather than by
observation: the diff's only Rust file is a test, and `Session` lives in `native.rs`,
which is not in the diff, so waiver 4 *could not* have been spent even inadvertently.

---

## S3-1 — `impl.md` §8's prose lagged its own table by one round · **FIXED**

The table was exact. Three surrounding sentences were not. All three were pure
substitution of values already established and independently verified this round; **no
value was re-derived, no count was re-opened, and §8's arithmetic was not touched.**

| # | was | now |
|---|---|---|
| 1 | "Why the **last two rows** carry no numbers" | "**last three rows**" — the paragraph contradicted itself four sentences later with "the three process documents" |
| 2 | "**Both** process documents are modifications… `impl.md` and `fix-r1.md`" | "**All three** process documents… `impl.md`, `fix-r1.md` and `fix-r2.md`", plus the clause the finding asked for: `fix-r2.md` is the clearest illustration of the overwrite mechanism that bullet describes, since this round's copy overwrites a document belonging to another arc entirely |
| 3 | "its other **83** added lines" | "its other **89** added lines" — the README is `+90` with one deletion |

**Why this round ran at all rather than shipping the finding as residue.** Lesson 28
has fired four times in this arc, which argued for stopping. But every previous
recurrence came from **re-derivation or extrapolation** — a count recomputed while the
counted thing moved, a line number inferred from a file growing, an inventory
re-collapsed by hand. S3-1 has none of that shape: each item is a substitution over a
stable input, and none is self-referential. The `89` comes from the README's `+90`, and
editing `impl.md` does not change the README, so the fixed-point trap that defeated F3
does not apply. This is the arc's first correction that is pure mechanical
substitution, and — see the sweep below — the first that introduced nothing.

## Added to the item-6 payload: the arc's most transferable finding

`impl.md` §7.7 previously recorded three lesson-28 recurrences. It now records **four**,
with S3-1 as the fourth — the fix for S2-1 introducing a stale claim one section away
from the §0 note it had just corrected, which already said the right thing.

The framing the synthesis asked for is now the item's own wording: the finding is **not
"these documents had errors"** but that *in this codebase a documentation correction is
itself a likely site of a new false claim, so correction passes should be reviewed as
adversarially as code*. §7.7 adds that, since slice 0's product *is* a measurement
record, this may be worth more than the two defects the PR set out to record.

§7.7 also now names the cause specifically enough to act on — **re-derivation** — and
states the rule: *in a correction pass, substitute established values; do not
re-derive.* It lists the two further instances internal sweeps caught (the "30 of 30"
aggregate across run sets never counted together; the panic line extrapolated from a
file's length), because both have the same shape, and notes that S3-1 was the first
correction to break the pattern in both directions.

## Lesson-28 sweep over this round's edits

The failure mode of every previous round, so it was run again. **One thing found, and
it is not in the substitutions** — it is in the existence of this document. Details in
*Deliberately left alone* §1; summarised in the last row below.

The three substitutions themselves swept clean, which is the first time in this arc a
correction has introduced nothing, and is consistent with the synthesis's prediction
that substitution over stable inputs differs from re-derivation.

| swept | result |
|---|---|
| §8's three substituted sentences | Each matches the table it describes **as of the round-3 input tree**: three uncounted rows, three process documents named, README `+90 − 1`. No arithmetic re-opened; `527`, `90`, `247`, `149`, `41` left exactly as round 2 measured them. Items 1 and 2 are then falsified by this document joining the diff — see the last row. |
| §7.7's rewrite (item-6 addition) | Item numbering unchanged — it stays item 7, so items 8/9/10 keep their numbers and no cross-reference moved. |
| Every `§N` / `§N.M` reference across all four root documents | Re-checked mechanically. **UNRESOLVED: NONE.** |
| Commit description | Contains no §8 prose, no process-document count and no lesson-28 count, so S3-1 does not reach it. Verified by grep rather than assumed; left unamended apart from the trailers. |
| "two process documents" / "Both process documents" repo-wide | One match remains, in `fix-r2.md`'s own sweep table, where it is a **quotation** of the round-2 defect being reported. Correct as written. |
| **`impl.md`'s path-count and inventory prose** | **Found:** writing `fix-r3.md` takes the diff from seven paths to **eight** and the stale-document inventory from 12 to **11**, which lags §0 and §8 by one — including two of the three sentences this round just fixed. Reported with the full remedy and **left unfixed**, because substituting the count is the operation that produced S3-1 in the first place. |

## Deliberately left alone

Per the instruction that an unplanned edit is how the previous three recurrences
happened, two things were found and **not** fixed. The first is the more important, and
it is a correction to the synthesis's own reasoning.

### 1. Writing this document makes the diff **eight** paths, which falsifies two of the three sentences just fixed

`fix-r3.md` exists at `master` `e44d0db8` as `dg-29vwer0f`'s round-3 document, so this
round's copy is a **modification**, it joins the diff, and `jj diff -r @ --name-only`
now returns **8** paths — four content files and **four** process documents. That makes
the following stale by one, all in `impl.md`:

| location | says | correct value |
|---|---|---|
| §0 | "returns **seven** paths — four content files and three process documents", and a seven-line list | **eight**; add `fix-r3.md` to the list |
| §0's per-arc table | this arc **3**; `dg-29vwer0f` **6**; stale total **12** | this arc **4**; `dg-29vwer0f` **5**; stale total **11** |
| §8 | "**Seven** paths. Four content files, **three** process documents" | **eight** / **four** |
| §8 | "Why the last **three** rows carry no numbers" *(fixed this round)* | **four** rows |
| §8 | "**All three** process documents are modifications" *(fixed this round)* | **all four** |
| §8 | "a reader who counts **seven**" | **eight** |
| §8's table | three process-document rows | add a `fix-r3.md` row |

**This is a correction to the synthesis's justification for running this round, not a
complaint about it.** Its reasoning was that S3-1's three items need no derivation and
that *"none of the three is self-referential — the `89` derives from the README's
`+90`, and editing `impl.md` does not change the README."* That holds exactly for item
3, and item 3 is still correct. But items 1 and 2 **count process documents**, and the
act of documenting the round adds one. So they are self-referential after all — not
through `impl.md` editing itself, which is the trap F3 hit, but through the round's
*other* deliverable joining the diff. The fixed-point is one level out from where it
was looked for.

Left unfixed because the instruction is explicit that an unplanned edit is how the
previous three recurrences happened, and because "substitute seven → eight" would
reproduce round 2's exact mistake: round 2 performed that same six → seven substitution
and left §8's prose lagging, which *is* S3-1. A fourth round that fixes the count would
be the fifth instance unless whoever does it also re-reads §8 and §0 whole.

The remedy is mechanical and is listed above in full, so it is a one-pass edit for
`merge_gate` or a round 4 — **or** a decision that a document cannot state its own
diff's path count at all, which is the same conclusion §8 already reached for line
counts and would end the recurrence rather than postpone it. That is the recommendation
here, and it belongs on #132 with the overwrite-per-arc pattern.

### 2. `fix-r2.md:182`'s heading

- **It reads "Added to the item-6 payload: lesson 28 fired three
  times, as a process finding".** After this round §7.7 says *four*, so a reader
  following that heading to §7.7 finds three listed there and four stated. It is
  defensible as a dated round-2 record of what round 2 added — which is what it is —
  and editing a historical round document to track later rounds is its own hazard, of
  exactly the kind this arc keeps demonstrating. Reported rather than changed. If
  `merge_gate` wants round records to carry as-of scoping, that is a convention
  decision for #132 alongside the overwrite-per-arc pattern, not a one-line edit here.

### Also unchanged

Also unchanged, as in every round: the twelve other-arc root documents (F5/S2-1 — they
belong to two other arcs and deleting them would pre-empt #132); items 6 and 7, which
are `publish`'s and blocked here by #123/#131, with item 6 still marked a ratified
ship-gate; and the PR title, recommended at `impl.md` §7.9 for `publish` to set.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

The C fixture was recompiled from this tree and `cargo build -p umbra-platform-macos
--tests` re-run after the last edit, before any figure below was taken. This round
touched no compiled file at all — only `impl.md` and this document — and the gates were
run in full anyway.

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

**The 832 figure carries the qualification it has carried since round 0** and is still
the weaker figure: no `--nocapture` and no fixture environment, so every integration
case in it takes `fixture_argv`'s skip branch (`tests/fixtures.rs:47`) and reports `ok`
with its `SKIP` invisible. The rows beneath it are the qualification.

**The eleven `CAPTURED` verdicts, read by name from this round's own captured output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`. Zero `SKIP`, zero `MISSED` in that run.

**The two `SKIP`s are skips, not passes**, both on `UMBRA_TEST_SKIP_NFS_MATRIX` — the
opt-out CI sets for that job.

**Both `#[ignore]`d cases re-measured with `--ignored`**, giving the same two verdicts
as rounds 0–2. The `mt_write` assertion line is still `fixtures.rs:398` — measured, not
assumed, and unmoved because nothing in a compiled file changed this round:

```
thread 'mt_write' panicked at crates/umbra-platform-macos/tests/fixtures.rs:398:5:
MISSED mt-write: the second thread's output reached the host at …

thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
a second intercepted syscall entered while one was still in flight: …
```

**No new measurement was taken.** Every figure in `impl.md` is rounds 0–2's, restated.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
