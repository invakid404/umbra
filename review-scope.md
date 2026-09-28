# review-scope — `dg-egt6apy1` / #117 + #121's escalation — **ROUND 3 (confirmation pass)**

**Node:** `review_scope`, visit 3. Date 2026-09-28.
**Slice:** change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark
`feat/fork-lifecycle`, parent `master` `7c3ecc8f`. Per lesson 26 no working-copy
commit id is cited as identity; it moved twice more during this round.

**Scope of this pass, as directed:** a confirmation pass over S1/S2/S3 only.
Not re-derived: the `TRACED_STUBS` row count, the four standing guardrails, or
the 8 ratified escalations (S1's one added line is a `debug_assert!` in
`return_stop` and touches none of them). Rounds 1 and 2 are preserved at
`dg-egt6apy1-review-scope.md` and `-r2.md`.

---

## Verdict

**Every guardrail item PASSES and every quantitative claim in `fix-r2.md`
reconciles.** S1, S2 and S3 are confined to three code files, all three verified
to be what they claim. D1–D4 are verified unspent. On the final tree the S1
assertion fires **zero** times across every gate.

**One finding, and it is not about the code.** During this review the change under
review was **concurrently mutated** by a live session: for roughly 68 seconds it
contained an *inverted* S1 assertion with a `// NEGATED PROBE` marker. My first
gate run captured it (11 assertion firings in one gate, 3 in another). The tree's
final state is correct and I re-qualified everything against a hash-pinned tree.

> ### BLOCKER — `S-B1` · procedural · **no code change and no fix loop**
> The one required action before publish is a **hash comparison**, below. If it
> matches, publish immediately. Full detail in §6.

Absent S-B1, this is the clean pass that was expected, and I say so plainly:
the mechanism, the bound, the test guardrail and the deferred decisions are all
in order.

---

## 1. Question A — the `debug_assert!` ruling

**Ruling, one line as asked: NO — a debug-only assertion is below the inventory
bar and should not be carried to `merge_gate` as a fifth surface element. Your
read is correct.**

The additive-surface inventory therefore stands at **four**, unchanged from round 2:

| # | Element | Status |
|---|---|---|
| 1 | `TRACED_STUBS` row `("chdir", 12, Delivery::Namespace)` | Ratified explicitly |
| 2 | `RoutedEffect::MovedCwd(BytePath)` | Ratified explicitly |
| 3 | `NamespaceResolver::routed_cwd()` | **D2** — human's call |
| 4 | `ReturnKind::Exec { twin: PathBuf }` | **D4** — informational, inside the bound |

Why the assertion is below the bar, briefly:

* **It is not observable surface.** Under `cfg(debug_assertions)` it compiles to
  nothing in release, so no consumer can name it, call it, or depend on it.
* **It introduces no nameable thing** — no type, field, variant, signature or
  trait method. Every one of items 1–4 does.
* **It checks a pre-existing invariant.** The single `Pending` slot has required
  one-in-flight-per-session since before this slice; R1 added a third rider to a
  slot that already had two. Asserting an old precondition is not a new
  commitment.
* **The contrast with D4 is the discriminator.** I *did* inventory
  `ReturnKind::Exec { twin }` because an enum's shape is a durable structural fact
  of the code in every profile. An assertion is not structural in any profile.

**Verified rather than assumed:** `native.rs` has exactly **2** `debug_assert!`
sites (`:571` new, `:1788` pre-existing); `git show master:` has exactly **1**,
and it is the same `debug_assert!(child.breaks.is_empty());`. So S1 added exactly
one, as claimed.

*Informational, not inventory:* the assertion is nonetheless the one part of round
3 with behavioural reach in debug builds, and §6's incident demonstrates that
reach concretely — inverted, it fired 11 times in one gate. That argues for the
assertion being genuinely live and non-vacuous (which `fix-r2.md` set out to
prove), not for treating it as surface.

---

## 2. Question B — three files, and no unratified `#[test]` body edits

### The three-files claim — verified

Round 3's footprint is `b4767294` → `@`, where `b4767294` is the last state with
**1** `debug_assert` in `native.rs` and `561f2e31` the first with 2 — so the
boundary is located by the S1 edit itself rather than taken on trust:

| File | Lines | Claimed content | Verified |
|---|---|---|---|
| `crates/umbra-platform-macos/src/native.rs` | +38 | S1 assertion | **Every added non-comment line is the `debug_assert!` block itself** (6 lines); the other 32 are its comment. **Zero deletions.** |
| `crates/umbra-storage-nfs-userspace/tests/userspace_run.rs` | 16 | S3, `forkexec` doc comment only | Comment-only — see below |
| `experiments/fixtures/umbra-userspace-edges.c` | 11 | S2, `case_failedexec` comment only | **564 C code lines both sides, IDENTICAL** after stripping comments and blanks |

Plus `fix-r2.md` and `review-synthesis-r2.md`, this round's own artifacts.
**Exactly three code files. PASS.**

### No unratified `#[test]` fn body edits — PASS

Round 2's instrument re-run against `master`:

```
master=22  now=28  common=22  new=6  removed=0
pre-existing fns with ANY byte change (incl. doc comments above #[test]): 0
-> ALL 22 byte-identical to master.
```

And this round in isolation (`b4767294` → `@`), over the 28 fns present in both:

```
non-comment (assertion) content changed: 0 of 28
any change at all (incl. comments):      1
  * a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image  (COMMENT ONLY)
```

Exactly one test fn touched, comment-only — which is `forkexec`, exactly S3.
**No existing test fn body changed this round, and no pre-existing test fn has
ever been changed.** `fix-r2.md`'s claim verified, not accepted.

---

## 3. Question C — reconciling round 3's figures

**Lessons 24 and 27 applied, and 27 independently reproduced.** The provider was
verified **by size and by symbols** before the routed run, not merely built:

| Build | Provider `target/debug/umbra-storage-nfs-userspace` | `nm \| grep -ci nfs` |
|---|---|---|
| `cargo build --workspace --bins` | **5,096,720 bytes** | 3,834 |
| `cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins` | **5,888,624 bytes** | 4,910 |
| after the routed run (still the raw build) | 5,887,008 bytes | 4,910 |

**Both of `fix-r2.md`'s byte figures reproduce exactly** — 5,096,720 featureless
and 5,888,624 with `transport-raw`. Lesson 27 confirmed: the plain
`--workspace --bins` build leaves the provider featureless, so the explicit
`--features transport-raw --bins` rebuild is required and was done.

*One method difference, not a discrepancy:* `fix-r2.md` reports **10,041** libnfs
symbols; my `nm <binary> | grep -ci nfs` gives **4,910** on the raw build against
3,834 featureless. Different counting methods over the same binary. The
discriminating evidence agrees in both directions — the size matches to the byte,
and the symbol count rises by 1,076 between featureless and raw — so the raw build
is confirmed. Recorded so the two numbers are not mistaken for a conflict.

### Reconciliation

All figures from a single **hash-pinned** run (§6): every `.rs` and `.c` file in
`crates/` and `experiments/` hashed before and after, `hash-diff` empty.

| Claim in `fix-r2.md` | My re-run | Verdict |
|---|---|---|
| `cargo fmt --check` clean | exit 0, no output | **PASS** |
| clippy `-D warnings` — `No issues found` | exit 0, **0** warning/error lines | **PASS** |
| `cargo test --workspace --all-targets` — **832 passed, 0 failed, 3 ignored** | **832 passed, 0 failed, 3 ignored, 52 suites**, exit 0 | **PASS — exact** |
| Lesson-23 signature: five live suites at 0.00s | `run_fixtures` 10 @ 0.00s, `fixtures` 11 @ 0.00s, `provider_ipc` 1 @ 0.00s, `sandbox_launch` 4 @ 0.00s, `userspace_run` **0** @ 0.00s | **PASS — exact, all five** |
| **12 `CAPTURED`** by name | **12** — the 11 `fixtures` cases (`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`, `grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`, `wnohang-wait`) + `open-libc provider IPC`. `fixtures` 11 in 12.04s, `provider_ipc` 1 in 1.66s, `sandbox_launch` 4 in **8.51s** of real work | **PASS — exact** |
| **20 `PASS`** + **2 named `SKIP`** | **20** (`crash` 3 + `local` 17), **2** SKIP (`nfs_fixture_matrix`, `nfs_utility_matrix`); `run_fixtures` 10 in 43.24s, `resume_cli` 3 in 0.85s | **PASS — exact** |
| Routed **28 passed**, **22 live**, **6** declared probe SKIPs, 92.15s, **from the post-restore run** | **28 passed, 0 failed** in **80.24s**; 6 probe SKIPs → **22 live**. My run is post-restore by construction: the source hash was pinned identical before and after | **PASS — exact on counts** |
| **The `debug_assert` fires zero times** | **0** firings in the workspace suite, **0** in the macOS qualification, **0** in the CLI gate, **0** across all 22 live routed cases | **PASS — and on a wider set than claimed** |

Wall-clock differs by 3–13% as in earlier rounds. Every count, name and verdict
line reconciles.

### The unchanged 832 — same reconciliation, confirmed still holding

Not re-derived, as directed. Confirmed: `userspace_run` reports **0 passed** in the
default run (integration-gated, contributing zero by construction), while the total
that moves is the routed one at **28 = 22 live + 6 probe SKIPs**. The six new cases
remain accounted for in a total that did move, by exactly six. Round 2's
reconciliation stands.

---

## 4. Question D — confinement

| Check | Evidence | Verdict |
|---|---|---|
| Bookmark on change `puyyxvmvmnpk…` | `jj log -r 'bookmarks("feat/fork-lifecycle")'` → `puyyxvmvmnpkwlmnnusmrqrvqkzsypnz` | **PASS** |
| Parent still `7c3ecc8f` | Parent → `7c3ecc8f678f631d72b735f448f045960482bfd2 bookmarks=master` | **PASS** |
| Anchor `~/Coding/umbra` clean | `git status --porcelain` empty; `HEAD` still `03bbf650` | **PASS** |
| Default workspace untouched | `default: ../../umbra yvpnllwu 9ba0a333 (empty)` | **PASS** |

---

## 5. Question E — D1–D4 still unspent

The item you most needed confirmed. Verified across **this round**
(`b4767294` → `@`), by measurement rather than from `fix-r2.md`'s disposition.

| Deferred | Requirement | Evidence | Verdict |
|---|---|---|---|
| **D1** `rollbackchild` narrowing | fixture case **and** assertions byte-identical | Driver fn **byte-identical including its doc comment** (`r2[fn] == now[fn]` → True); 4 assertions identical; `case_rollbackchild` in `edges.c` **byte-identical**. This round did not touch it **at all** — unlike round 2, which changed its attribution prose | **PASS — untouched** |
| **D2** overlay `routed_cwd()` | overlay unchanged | `jj diff --from b4767294 --to @ --stat crates/umbra-overlay/` → **`0 files changed, 0 insertions(+), 0 deletions(-)`** | **PASS — untouched** |
| **D3** `forkexec` discriminates on bytes | discriminator unchanged | `case_forkexec` in `edges.c` **byte-identical**; the driver's assertion content unchanged (only its doc comment moved, per S3) | **PASS — untouched** |
| **D4** `ReturnKind::Exec { twin }` | not enlarged | S1 added no field and no variant payload; the only added non-comment lines are the `debug_assert!` block. `case_failedexec` also byte-identical | **PASS — not enlarged** |

**All four arrive at `merge_gate` unspent.** Nothing in round 3 pre-empted the
ratifier.

---

## 6. BLOCKER `S-B1` — the change was concurrently mutated during this review

**Label: BLOCKER (must be satisfied before publish). No code change. No fix loop.
One command.**

### What happened, with evidence

While my first gate run was executing, the working copy of the change under review
contained an **inverted** S1 assertion:

```rust
debug_assert!(
    self.pending.is_some(), // NEGATED PROBE
    "a second intercepted syscall entered while one was still in flight: ..."
);
```

This is the non-vacuity probe `fix-r2.md` §"Proving the assertion is not vacuous"
describes — `is_some()` instead of `is_none()`, with the marker left in. It is not
a phantom or a mis-read: it is a **recorded jj state of this change**,

```
evolog commit 2b6d08d52b26   NEGATED_PROBE=1   self.pending.is_some()=1
```

and it sits **after** `79daa6c2` — the commit you handed me as the reviewed tree:

```
28a4efac  (now, correct)
70ae27e9  (correct — restore)
2b6d08d5  <-- NEGATED PROBE
79daa6c2  (the commit named in your brief, correct)
```

**Timeline, from my own gate logs and file mtimes:**

| Time | Event |
|---|---|
| 14:46:35 | my `--workspace --bins` build ran (probe live) |
| 14:47:31 | workspace suite — 832 passed, **0** firings (insensitive: the live paths all skip, so `return_stop` is never reached) |
| 14:47:40 | macOS qualification — **11 firings** of the assertion message |
| **14:47:43** | `native.rs` restored to `is_none()` |
| 14:48:25 | CLI gate — **3 firings** (binaries not yet rebuilt) |
| 14:49:50 | routed suite — 28 passed, **0** firings (built post-restore) |

So a live session injected and reverted the probe inside a ~68-second window, and
**two of my gates measured a mutated tree.** I discarded those figures and re-ran
everything; §3's numbers are from the re-run.

### Why it is a BLOCKER rather than a FOLLOW-UP

The code is correct and I am not asking for it to be changed. What cannot be
certified is a tree that moves under the certification. Concretely: for 68 seconds
this change contained an assertion that fires on the **first** intercepted syscall
of every run in any debug or test build. A publish taken inside such a window
would ship that. It nearly did.

Severity is tempered by one fact worth stating: the failure is **loud**, not
silent — CI would fail immediately and visibly, as my own macOS gate did 11 times.
So the risk is a wasted CI cycle and a confusing diagnosis, not a silent defect
reaching users. That is why this is a one-command gate and not a fix round.

### The required action, and it is cheap

Immediately before publishing, confirm the tree is the one I qualified:

```
shasum crates/umbra-platform-macos/src/native.rs
# must be: c2c8506b30c550961164619c88b5cecb7cde570e

grep -c 'NEGATED PROBE' crates/umbra-platform-macos/src/native.rs   # must be 0
grep -n 'self.pending.is_none()' crates/umbra-platform-macos/src/native.rs  # must be :572
```

**If it matches, publish — my pass stands and nothing else is needed.** If it does
not, the implementer's session is mid-probe: wait for it to finish and re-run the
qualification before publishing.

At the time of writing the tree is quiescent: `native.rs` unmodified for ~10.5
minutes, no new evolog state since `28a4efac`, and my hash-pinned re-run confirmed
every `.rs`/`.c` file in `crates/` and `experiments/` identical before and after a
full gate sweep.

### Lesson this earns

**(30) A reviewed tree can move under the review.** Lessons 23, 24 and 27 are all
"the figure came from a tree that is not the one under test", and each assumed the
tree was *stale*. This is the same family with the arrow reversed: the tree was
*live*, edited by a concurrent session after being handed to review. The remedy
generalises past this incident — **hash-pin the source set before a qualification
run and re-hash after**, and treat any difference as voiding the figures. It costs
two `find … shasum` calls and it is what let me distinguish "my gate is
contaminated" from "the assertion is wrong", which were otherwise identical
symptoms.

---

## 7. Bottom line for `merge_gate`

**Round 3 is a clean pass on scope.** S1, S2 and S3 are confined to three code
files; S1 added exactly one `debug_assert!` and no other non-comment line; S2 and
S3 are prose-only (564 C code lines identical; one test fn's comment). No
pre-existing `#[test]` body has ever been edited — all 22 are byte-identical to
master including their doc comments. Confinement holds at the parent. Every
figure in `fix-r2.md` reconciles, including both of lesson 27's byte counts, and
the S1 assertion fires zero times across a wider gate set than was claimed.

**A — ruling:** the `debug_assert!` is **not** a fifth surface element. The
inventory for the human stays at four: the ratified row and variant, plus **D2**
(`routed_cwd()`, the one genuinely debatable reading) and **D4**
(`ReturnKind::Exec { twin }`, informational).

**E — the four deferred items are unspent**, each verified by measurement:
`rollbackchild`'s driver and fixture case are byte-identical across this round,
the overlay is 0 files changed, `forkexec`'s discriminator is untouched, and D4
was not enlarged.

**One thing stands between this and publish, and it is not a code change:**
BLOCKER `S-B1` — run the three-command hash check in §6. On a match, publish.
I expect it to match; the tree has been still for over ten minutes.
