# Fix round 4 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix`, visit 4, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
Input: `/tmp/graph-dg-nsw71bqq/review-synthesis-r4.md`. Pin at round start: commit
**`64e54782`** — the pin held a fourth consecutive round.

Two findings. No production source, guardrails byte-identical, `single_thread()` and
the `debug_assert!` untouched, slice 1 not started, nothing added beyond this document.

---

## R4-1 — P0. The counts are removed, not substituted · **FIXED**

**The severity call is right and my round-3 disclosure does not answer it.** I
disclosed the condition in `fix-r3.md` §1 with a remedy table and left it unfixed. The
deciding fact is that a reader of `impl.md` §8 never reaches `fix-r3.md` §1: §8 handed
them `jj diff -r wppwptxswssr --stat` and the command contradicted the sentence three
lines above it. A disclosure in another document does not repair a self-contradicting
section. Ship-blocking, as filed.

**Both reviewers converged on the remedy and it is the one applied: remove the
literals, keep what is stable, point at the tool.** Substituting seven → eight would
have been the fifth round of an operation that regenerates the defect — writing this
document would have made it nine before the round ended.

Kept, because neither changes when another `fix-rN.md` joins the diff:

- **four content files, named** — `README.md`, `tests/fixtures.rs`, `smoke.sh`,
  `umbra-test-child.c`, with their exact per-file line counts and the `527 / 1`
  subtotal, all re-quoted from `jj diff` this round;
- **zero production source in the diff.**

Removed as literals, everywhere they appeared:

| location | was | now |
|---|---|---|
| §0 path list | "returns **seven** paths — four content files and three process documents", seven-line block | four content files named in a block; everything else the tool lists identified as this arc's root `.md`, each a modification; no total |
| §0 (`:47`) | "**All three** process documents are modifications… `impl.md`, `fix-r1.md`, `fix-r2.md`" | folded into the sentence above, with no enumeration and no count |
| §0 | "The count is stated separately from the guarantee because round 1 caught…" | replaced by the reason the count is *absent*, naming the generator |
| §0 inventory | "This arc, **3**"; "Not this arc, **12** … **two** prior arcs" | "this arc's documents are the ones in the diff"; the two prior arcs kept, which is the stable part |
| §0 per-arc table | each row ended "— **6**" | row totals dropped; `fix-r3.md` removed from `dg-29vwer0f`'s row since this arc has taken it over |
| §0 (`:77-78`) | "the stale `fix-*` range … is `fix-r3.md` alone now"; "the total moved **13 → 12**" | replaced with "which `fix-rN.md` paths this arc has taken over is not stable, because each round takes over one more — `jj diff --name-only` answers that and no integer here does" |
| §0 (`:85`) | "deleting **twelve** unrelated documents belonging to two other arcs" | "deleting documents belonging to two other arcs" |
| §8 heading claim | "**Seven paths.** Four content files, **three** process documents, zero production source." | "**Four content files, and zero production source.**" |
| §8 table | three process-document rows without figures | dropped; the table is the four content files plus their subtotal, and the process documents are described in a following sentence |
| §8 | "Why the **last three rows** carry no numbers" | "Why neither a path total nor a per-document line count appears above", with both explained as the same self-reference |
| §8 | "a reader who counts **seven**" | "a reader who counts the paths and cannot tell which extras are not production code" — answered by naming the four |
| §8 bullet | "**All three** process documents are modifications… `impl.md`, `fix-r1.md` and `fix-r2.md`" | "Every process document in the diff is a modification", with no enumeration |
| §8 reproduce line | `--stat` only | `--stat` **and** `--name-only`, explicitly labelled the authority "not this section" |

That is the eleven R4-1 locations plus S4-1's three (`:47`, `:77-78`, `:85`), which my
round-3 remedy table had missed — applied literally it would have closed seven and left
three, and the synthesis is right that this would have been the sixth instance.

### One location beyond the fourteen

The stale-literal sweep found the same defect class in **§7.10**, which neither
reviewer enumerated: *"the repository root currently mixes this arc's **three**
documents with **twelve** belonging to **two** other arcs"* and *"moved the count
**13 → 12**"*. It would have gone stale this round like the rest, and it pointed at a
§0 that no longer states those numbers, so it was internally inconsistent as well.
Reformulated the same way. Flagged here because it means the enumeration was
fourteen-of-fifteen, not because I went looking for scope.

## R4-2 — LOW. The generalisation now claims less · **FIXED**

§7.7 said *"each of the first three recurrences came from recomputing something"*. It
does not hold, for both reasons given: the three examples include "a line number
inferred from a file growing", which the next sentence assigns to the internal-sweep
instances — double-counted, and no line-number error exists among the first three — and
**R2-1 was a causal misattribution, not a recomputation**.

Corrected to claim less: re-derivation accounts for S2-1, S3-1 and the two
internal-sweep instances, and explicitly **not** for R2-1, with a sentence saying the
earlier revision overstated it. The rule survives, and round 4 supplies evidence for it
from the other direction — S3-1's fix *was* pure substitution and still went stale,
because the input was not stable. So §7.7 now carries a companion rule, which is what
§0 and §8 implement: **where a value is invalidated by the act of stating it, do not
state it — name the stable part and point at the tool.**

Noted as the synthesis asked: the overstatement originated in the round-3 framing and I
carried it faithfully. Recording that is more useful than assigning it.

## Untouched, because it held up

§7.7's count of **four** recurrences and the four it names — correctness tried
specifically to break it and could not, so it is left exactly as written. The three
S3-1 substitutions were correct at `f7c15ee2`; R4-1 is that they went stale one commit
later, not that they were wrong, and nothing here re-litigates them. Items 6 and 7
remain `publish`'s, blocked by #123/#131, with item 6 still a ratified ship-gate. The
twelve-or-so other-arc root documents stay in place for the reasons every round has
given.

---

## The test that matters: would a hypothetical `fix-r5.md` invalidate anything left?

Run against the final tree with this document written and the change amended. Every
count-bearing claim remaining in §0 and §8, and whether adding a fifth round document
would falsify it:

| claim remaining | survives `fix-r5.md`? |
|---|---|
| "Exactly four content files are touched, and they are these" + the four paths | **Yes** — a new `fix-rN.md` is not a content file |
| §8's per-file `+/−` figures and the `527 / 1` subtotal | **Yes** — they are the four content files' own counts |
| "No production source file is in the diff" | **Yes** |
| "Everything else … is a root-level `.md` process document of this arc — `impl.md` and one `fix-rN.md` per review round" | **Yes** — stated as a rule over rounds, not a list |
| "Some of those paths already carry an earlier arc's document at `master`, so they appear as modifications; the later ones are new files … read off `jj diff --stat`" | **Yes** — and this replaced a claim the check itself caught: see below |
| "The parent commit *is* `master`, so every file not in that list is byte-identical" | **Yes** |
| §0's two prior-arc rows (`dg-egt6apy1`, `dg-29vwer0f`) | **Yes** — `fix-r5.md` is not among either arc's listed documents, and no row carries a total |
| "which `fix-rN.md` paths this arc has taken over is not stable … the tool answers that" | **Yes** — it asserts the instability rather than a value |
| §8's "substituting seven → eight, as round 3 correctly did for what was then true" | **Yes** — narration of what round 3 did, not a claim about the current diff |
| §7.10's reformulated #132 item | **Yes** — no integer |

**The check found three survivors, all introduced by this round's own edits, and all
three are now fixed.** That is the check earning its keep rather than rubber-stamping:

1. **§0 still enumerated two of the documents** — "Implementation documents this node
   authored (`impl.md`, `fix-r1.md`) ship in the diff". Replaced with "`impl.md` and
   each round's `fix-rN.md`".
2. **"each is a modification rather than an addition" was false as of this round.**
   `jj file show -r @- fix-r4.md` reports **absent**: `master` carries `fix-r1.md` and
   `fix-r2.md` (both `dg-egt6apy1`'s) and `fix-r3.md` (`dg-29vwer0f`'s), but no
   `fix-r4.md`. So this document is an **addition**, and the generalisation I wrote one
   hour earlier — in the very edit that removed the counts — was wrong for the first
   time at exactly this round. Both §0 and §8's bullet now say some paths are
   modifications and some are additions, and point at `--stat`, where a modification is
   the path that also shows deletions.
3. **§0's per-arc table implied its rows were the arcs' complete root inventories**,
   which cannot stay true as this arc takes over more `fix-r*.md` paths. The rows are
   now scoped to "documents at root this arc never writes" — no `review-*`, `ci-*` or
   `publish.md` is ever written here — with the `fix-r*.md` instability stated rather
   than enumerated.

After those three, **no remaining claim in §0 or §8 goes stale.** The only numbers left
in either section are the four content files' line counts, and a `fix-r5.md` does not
touch a content file. Verified by re-reading both sections whole rather than by grep
alone; the greps agree that no `**seven**`/`**eight**`/`**three** process`/`13 → 12`/
`twelve unrelated`/`fix-r3.md alone` remains anywhere in `impl.md`.

Worth stating plainly, because it is the round's most useful result: **two of the three
survivors were claims this round introduced while removing the literals.** The
reformulation that ends the count problem was itself a fresh site for a false claim —
lesson 28 at one more remove — and only a mechanical "would `fix-r5.md` break this?"
pass over the finished text found them. A sweep for the *old* wording would have passed.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

The C fixture was recompiled from this tree and `cargo build -p umbra-platform-macos
--tests` re-run after the last edit. This round touched no compiled file — only
`impl.md` and this document — and the gates were run in full anyway.

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

**The 832 figure keeps the qualification it has carried since round 0** and is still the
weaker figure: no `--nocapture` and no fixture environment, so every integration case in
it takes `fixture_argv`'s skip branch (`tests/fixtures.rs:47`) and reports `ok` with its
`SKIP` invisible. The rows beneath it are the qualification.

**The eleven `CAPTURED` verdicts, read by name from this round's own captured output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`. Zero `SKIP`, zero `MISSED`. The two `SKIP`s in `run_fixtures` are skips,
not passes, both on `UMBRA_TEST_SKIP_NFS_MATRIX`.

**Both `#[ignore]`d cases re-measured with `--ignored`**, same two verdicts as rounds
0–3; the `mt_write` assertion is still at `fixtures.rs:398`, measured rather than
assumed. **No new measurement was taken** — every figure in `impl.md` is rounds 0–2's,
restated.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
