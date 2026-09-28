# ci-round2 — `dg-29vwer0f` / #121 / PR #128

**Head SHA under test: `52fba1e157270d508e30f4264653effe36e3a27b`**

**VERDICT: CI fully GREEN and verified by log. CR still `CHANGES_REQUESTED`,
with one new Minor item → `fix_from_ci_cr`.**

---

## 1. Checks — all pass

| check | result | |
|---|---|---|
| **Enforced macOS fixture qualification** | **pass** | 1m1s — *was the round-1 failure* |
| Rust workspace (ubuntu-latest) | pass | 2m58s |
| Rust workspace (macos-14) | pass | 2m27s |
| Raw NFS transport (ubuntu-latest) | pass | 3m8s |
| Userspace-routed run over the live NFSv4 client | pass | 1m59s |
| auto-ack | skipping | as designed |

## 2. The green was verified against the README's own required proof

Round 1 established that this job's eleven cases report `ok` whether or not they
run, discriminated only by elapsed time — so "pass" alone proves nothing here.
The crate README states the real bar: *"qualification requires the `CAPTURED
<case>` stderr verdict."*

Read out of `runs/36357407028/job/108727769160`:

```
CAPTURED argv0-check      CAPTURED dirfd-rename     CAPTURED dup-inherit-write
CAPTURED exec-write       CAPTURED fork-write       CAPTURED grandchild-write
CAPTURED open-libc        CAPTURED open-svc         CAPTURED posix-spawn-write
CAPTURED symlink-cycle    CAPTURED wnohang-wait
test result: ok. 11 passed; 0 failed; finished in 4.08s
```

**All eleven verdicts present, and 4.08s rather than 0.00s.** The cases executed.
This is the first time in this graph that claim has been checked rather than
assumed.

The userspace job likewise re-verified on this head: **seven proof steps**,
`PASS userspace /bin/ls <workspace>/`,
`mutation_probe_readdir_makes_the_listed_names_wrong ... ok`, and
`a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client ... ok`.

## 3. CR verdict, anchored on commit ids rather than on the latest review type

Eight reviews on the PR. Anchored:

| author | state | commit |
|---|---|---|
| coderabbitai | `CHANGES_REQUESTED` | `e6618397` (round 1) |
| invakid404 | `COMMENTED` ×5 | `6a18b98f` (my thread replies) |
| **coderabbitai** | **`CHANGES_REQUESTED`** | **`52fba1e1` — the current head** |
| coderabbitai | `COMMENTED` | `52fba1e1` |

**`APPROVED` reviews: zero.** `reviewDecision: CHANGES_REQUESTED`. So the
`merge_gate` edge's condition ("CI green **and** CR APPROVED") is not met, and
the CodeRabbit *check* showing `pass / Review completed` is not the verdict —
the review state is.

## 4. New CR items on this head: one actionable

| # | file:line | claim | disposition |
|---|---|---|---|
| 1 | `impl.md:382` | **Bind each test total to its commit.** `impl.md` reports 826 passed while `publish.md` reports 832 for the same command, and `publish.md` names `e6618397` while the reviewed head is `52fba1e1`. State which commit produced each run and correct any stale total. | **Valid.** Both figures were true when written and neither says so. |
| 2 | `fix-r3.md` | Not a finding — CodeRabbit acknowledging the `routed_binding` reply and marking the thread **resolved**. | No action. |

Four of the five round-1 items are resolved on CodeRabbit's side; the fifth
produced item 1 above rather than a re-raise.

**Item 1 is the fifth quantitative-reconciliation item in this slice** (master
baseline; test-body count ×2; test delta; now per-commit totals). Every one has
been harmless in substance and every one was caught by a sweep or a reviewer
rather than by reading the report. The remedy already in `impl.md` §6 — naming
the sweep instead of asserting a number — is the right shape and item 1 is the
argument for extending it to *every* figure: **a count without its commit is a
claim that cannot be checked.**

## 5. Route

`fix_from_ci_cr`, second traversal. One documentation item. CI needs nothing.
