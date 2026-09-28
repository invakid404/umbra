# ci-round1 — `dg-29vwer0f` / #121 / PR #128

**Head SHA under test: `e6618397a94e465a8f37a4bf42ba275d23eb2992`**

**VERDICT: CI RED + CR `CHANGES_REQUESTED` → `fix_from_ci_cr`.**

---

## 1. Check results

| check | result | |
|---|---|---|
| Rust workspace (ubuntu-latest) | **pass** | 2m59s |
| Rust workspace (macos-14) | **pass** | 3m41s |
| Raw NFS transport (ubuntu-latest) | **pass** | 3m13s |
| Userspace-routed run over the live NFSv4 client | **pass** | 2m2s |
| **Enforced macOS fixture qualification** | **FAIL** | 26s |
| CodeRabbit | **`CHANGES_REQUESTED`** | 5 inline comments |
| auto-ack | skipping | as designed |

`reviewDecision: CHANGES_REQUESTED`. No `APPROVED` review exists, so there is
nothing to anchor on a commit id yet.

## 2. The green was read from the log, not the status

The #120 lesson is that a green status can hide a skipped proof, and this slice
added proofs CI had never run. Read out of
`runs/36355653853/job/108722739339`:

- **All seven proof steps executed**, including the new
  `Proof 7 -- mutation probe F, directory reply names corrupted`.
- `PASS userspace /bin/ls <workspace>/: store holds it, host untouched`
- `test mutation_probe_readdir_makes_the_listed_names_wrong ... ok`
- `test a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client ... ok`

So the userspace job's pass is **real**, and the slice's own proofs ran in CI.

## 3. The failure — a genuine regression, and one our gates could not catch

`crates/umbra-platform-macos/tests/fixtures.rs:154`:

```
thread 'wnohang_wait' panicked at crates/umbra-platform-macos/tests/fixtures.rs:154:30:
unexpected operation Close { fd: TracedFd(3) }
test result: FAILED. 2 passed; 9 failed
```

Failing: `argv0_check`, `dup_inherit_write`, `exec_write`, `fork_write`,
`grandchild_write`, `open_libc`, `open_svc`, `posix_spawn_write`, `wnohang_wait`.

**Cause.** This slice added `close`(6) and `__close_nocancel`(399) to
`TRACED_STUBS`, so `FsOp::Close` now reaches this tracer harness. Its `match` has
named arms and `other => panic!("unexpected operation {other:?}")`, and no `Close`
arm.

**The harness documents this exact shape, one stub earlier.** The comment above
the panic records that `fstat` joined the stub list, that libSystem issues it on a
kernel descriptor before `main` in every process, and that an `FsOp::Fstat { .. }
=> {}` arm was added for it — explicitly *"named rather than folded into a
wildcard: an operation it has no plan for should still stop the case loudly."*
The panic is working as designed. We added four stub rows and updated no harness.

**Why local gates missed it, stated plainly because it is my own verification
hole.** `fixtures.rs` needs `UMBRA_TEST_FIXTURE_PATH` and debugger permission; my
`cargo test --workspace --all-targets` runs did not set it, so the whole file
**skipped**. Every gate I reported green — 826, 829, 832 — excluded these nine
cases. That is the "green gates silently skip proofs" lesson firing on the driver,
not on CI, and CI is what caught it.

## 4. CodeRabbit — 5 inline comments, all Minor, all assessed valid

| # | file:line | claim | disposition |
|---|---|---|---|
| 1 | `experiments/fixtures/umbra-userspace-listing.c:104` | `fts_children` returns `NULL` with `errno == 0` for an empty directory and nonzero on error; the loop ignores the distinction, and a later `fts_read` can reset `errno`, so the fixture can write an empty listing and return zero **after a directory-read error**. | **Valid, and apt** — this slice's whole theme is that an empty listing is indistinguishable from a served one. Fix: check `errno` immediately, return nonzero and report it. |
| 2 | `fix-r3.md:37` | Qualify the zero-size result by descriptor status: unbound + zero-size is `EBADF`; `EINVAL` applies only after descriptor validation. | Valid doc fix. |
| 3 | `fix-r3.md:159` | The missing-fd guard is not the full `routed_binding` denial condition — a bound descriptor with no `logical_path` is also denied. | Valid; the round-4 correctness review established the same fuller condition. |
| 4 | `fix-r3.md:259` | Net test change is `+3` with one superseded, not `+4` with none removed. | **Valid — independently corroborates the scope review and my own count** (908 vs 905; four added, one removed). |
| 5 | `impl.md:255` | Do not describe `-R` as fail-closed; separate it from the modes refused by name. | Valid — the README was split during round 3, `impl.md` still carries the old wording. |

No comment disputes the mechanism. Four of five are documentation; the one code
item is on our own test fixture.

## 5. Replies

Deferred until the fix lands, deliberately: a reply that says "will fix" ages
worse than one that says what was done and in which commit. The driver owns these
replies (carried lesson) and will post them against `fix_from_ci_cr`'s head.

## 6. Route

`fix_from_ci_cr`. Work order:

1. **CI first** — add the `Close` arm to `fixtures.rs`'s match, in the same
   deliberate style as the `Fstat` arm beside it, with the same reasoning recorded.
   Check whether `ReadDir`/`Fchdir` can reach this harness too and handle them
   the same way rather than waiting for a second red run.
2. **Run that suite locally with `UMBRA_TEST_FIXTURE_PATH` set** — it has never
   been run in this graph, by anyone, at any point.
3. CR items 1–5.
