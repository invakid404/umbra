# ci-fix-r2 — `dg-29vwer0f` / #121 / PR #128

**Input:** `ci-round2.md`. Head under test `52fba1e1`; **CI fully green on all
five checks**, CodeRabbit `CHANGES_REQUESTED` with one actionable Minor item.

**No code changed this pass.** One documentation item, fixed as a class rather
than as two numbers: every quantitative total in every report is now bound to the
commit that produced it, and the canonical table is `impl.md` §7.1.

---

## 1. The item, and why neither figure was wrong

> `impl.md` reports **826 passed** while `publish.md` reports **832** for the same
> command, and `publish.md` names `e6618397` while the reviewed head is
> `52fba1e1`. State which commit produced each run, and correct any stale total.

**Valid, and the interesting part is that both figures are accurate.** 826 was
true at `7bd1a95c`; 832 was true at `e6618397` and is still true here. Neither
said which commit it described, so a reader at this head could not tell a stale
figure from a current one.

That is the property worth naming, and it generalises past these two numbers: **a
count without its commit is not wrong, it is uncheckable** — which is worse,
because wrongness can be caught and uncheckability cannot. This is the fifth
quantitative-reconciliation item in the slice, after the master baseline (822 vs
813), the test-body edit count (twice), and the test delta (+4 vs +3). Every one
was harmless in substance and every one was caught by a sweep or a reviewer
rather than by reading the report.

`impl.md` §6 already carried the right remedy for edit counts — *name the sweep
that finds the number rather than asserting the number*. This pass extends it to
totals, which needs the commit, because unlike an edit count a total is only
meaningful relative to a tree state.

---

## 2. The per-commit table

Added as `impl.md` §7.1, and reproduced here. Each row is one head of change
`psvqmvlmktvv`.

| commit | pass | `cargo test --workspace --all-targets` | `#[test]` attrs | fixture env set? |
|---|---|---|---|---|
| `a85a8471` | master (parent) | **813** passed, 3 ignored | 887 | n/a |
| `9f94b8f3` | implement | **825** passed | 901 | no |
| `7bd1a95c` | fix round 1 (`fix-r1.md`) | **826** passed | 902 | no |
| `20ccf7db` | fix round 2 (`fix-r2.md`) | **829** passed | 905 | no |
| `3480713b` | fix round 3 (`fix-r3.md`) | **832** passed | 908 | no |
| `e6618397` | PR #128 opened; CI round 1 **red** | 832 | 908 | no |
| `6a18b98f` | CI/CR fix round 1 (`ci-fix-r1.md`) | **832** passed | 908 | **yes** |
| `52fba1e1` | memoria re-ack; CI round 2 **green** | 832 | 908 | yes |
| **this head** | CI/CR fix round 2 (this document) | **832** passed, 3 ignored | 908 | **yes** |

### What is measured now, and what is contemporaneous

The `#[test]` attribute column is **re-derived at this head for every row**, not
carried forward from the reports:

```
$ jj diff --from a85a8471 --to <commit> --git 'glob:crates/**/*.rs' \
    | grep -cE '^\+\s*#\[test\]'
```

The path filter matters and its absence is how I got it wrong on the first
attempt: without `glob:crates/**/*.rs` the diff also counts `#[test]` appearing
*inside the markdown reports*, which quote the token while discussing it. That
produced 913/918/917/920/923 — numbers that look plausible and are pure artefact.
Filtering to Rust sources gives the table above.

The **passed** totals are contemporaneous; they cannot be re-run at a hidden
commit without checking it out. But each reconciles independently against the
re-derived attribute count:

```
reported = 813 + (attrs - 887) - 2
  901 -> 825 ✓    902 -> 826 ✓    905 -> 829 ✓    908 -> 832 ✓
```

The `- 2` is the two tests in `userspace_run.rs`, behind
`#![cfg(all(feature = "transport-raw", target_os = "macos", target_arch =
"aarch64"))]`, which compile to nothing under a default-feature workspace run.
Four independently-measured counts reproducing four independently-reported totals
is a stronger check than either alone, and it is the check a reader can now run.

### The column that is not a number

Every total up to and including `3480713b` was measured **without**
`UMBRA_TEST_FIXTURE_PATH`. The count is identical either way — that is exactly
what made CI round 1's failure invisible to three rounds of local gates — but the
eleven `umbra-platform-macos` fixture cases reported `ok` without executing. So
the figure was right and the run behind it was weaker than it looked, and a
per-commit table that carried only the number would have preserved the same
false confidence in tidier form. The condition is a column.

---

## 3. What changed, per file

| file | change |
|---|---|
| `impl.md` | §7 gate table bound to `7bd1a95c` with its 826 marked **superseded**, pointing at §7.1. New **§7.1**: the canonical table, the re-derivation command, the reconciliation formula, and the fixture-env condition |
| `fix-r1.md` | gate table bound to `7bd1a95c`; master named as `a85a8471` |
| `fix-r2.md` | gate table bound to `20ccf7db`; prior heads named by commit rather than "round 1" |
| `fix-r3.md` | gate table bound to `3480713b`, with the note that its 832 predates the fixture-env discovery |
| `ci-fix-r1.md` | gate table bound to `6a18b98f`, noting CI round 2 tested `52fba1e1` on top of it with no code or test change |
| `publish.md` | figures bound to `e6618397`, marked superseded twice, with 832 confirmed still current and the fixture-env condition carried |

**Nothing was silently updated.** Where a figure is superseded it says so and
keeps its original value, because the history of these numbers has been
informative twice — the 822 baseline was wrong when written, and the round-3
delta was wrong in a way that revealed a superseded test. Overwriting 826 with
832 would have destroyed the only record that round 1 measured a different tree.

---

## 4. Measured versus inferred

**Measured this pass:** the current head's totals (`832 passed, 3 ignored`, 908
attributes, 53.70s with the fixture env); the attribute count at all eight prior
heads by diff against master; the reconciliation formula against all four
reported totals; the evolog mapping every review and CI head to a commit id
(`9f94b8f3`, `7bd1a95c`, `20ccf7db`, `3480713b`, `e6618397`, `6a18b98f`,
`52fba1e1` all present as ancestors of this change); all three local gates.

**Inferred, not measured:** the passed totals at the seven superseded heads — as
above, they are contemporaneous reports, corroborated by the formula but not
re-executed. I did not check out hidden commits to re-run them; the corroboration
is what makes that unnecessary rather than merely inconvenient.

**Nothing rebutted.** The one item was correct.

**Second CR comment:** acknowledged by the report as not a finding — CodeRabbit
marking the `routed_binding` thread resolved. No action, and I did not reply to
it; thread replies are the driver's.

---

## 5. Gates

**Figures below are at this head** — the commit this pass produced. Documentation
only, so nothing was expected to move, and nothing did.

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` **with the fixture env** | **832 passed**, 3 ignored, 53.70s |
| `#[test]` attributes | **908** (master `a85a8471`: 887) |
| `memoria check` | see §6 |

No test added, removed or edited; no Rust file changed. `impl.md` §6's
three-edit disclosure and the 908 count both stay accurate.

---

## 6. memoria, run last

Run from a throwaway git worktree as the **final step before push**, after this
document existed. That ordering is the fix for a real failure last round: I
re-acked and *then* wrote `ci-fix-r1.md`, and the new root-owned file
re-triggered the root README's review, so `memoria check` was red on the pushed
head and the driver had to re-ack and re-push. A root-owned document is itself an
input to the root README, so acking before writing it acks a tree that is about
to change.

Result is recorded in `memoria.lock`; `memoria check` passes on the pushed tree.

**Still standing:** PAUSE BEFORE MERGING. Amended and pushed to
`feat/ls-userspace`; nothing merged.
