# ci-fix-r1 — `dg-29vwer0f` / #121 / PR #128

**Input:** `ci-round1.md`. Head under test `e6618397`; CI red on
`Enforced macOS fixture qualification`, CodeRabbit `CHANGES_REQUESTED` with five
Minor inline comments.

**Outcome:** the CI failure is fixed and **a second instance of the same defect
was found in the same run** — `tests/provider_ipc.rs`, which CI had not reached
because `fixtures.rs` failed first. All three native-qualification suites are
green locally with the fixture env set: **16 passed, 0 failed**, against
**2 passed, 9 failed** before. All five CR items accepted and applied; none
rebutted. memoria re-acked from a throwaway git worktree.

---

## 1. The CI failure

### Reproduced first, then fixed

```
$ UMBRA_TEST_FIXTURE_PATH=… UMBRA_TEST_REDIRECT_ROOT=… \
  cargo test -p umbra-platform-macos --test fixtures --test sandbox_launch --test provider_ipc

fixtures       2 passed; 9 failed   <- unexpected operation Close { fd: TracedFd(3) }
```

Identical to CI, same panic, same nine cases.

### Cause and fix

`close`(6) and `__close_nocancel`(399) joined `TRACED_STUBS` in this slice so a
*virtual* dirfd could be released — `fts` closes its directory descriptor through
399, which is neither interposed nor previously breakpointed. The consequence is
the one `TRACED_STUBS`' own doc comment warns about: **routing one member of a
refused set makes the next member reachable for the first time.** Every `close`
in the process now traps, and dyld closes kernel descriptors before `main` in
every case this harness runs.

The harness documents this exact shape one stub earlier, for `fstat`, and argues
against a wildcard. I matched that: a named `FsOp::Close { .. } => {}` arm with
the same disposition — real kernel descriptor, no namespace in this driver, so
letting the kernel answer is the honest result, identical to what the
supervisor's descriptor fence does below the floor. The panic stays for
everything else.

### The second instance, found because the suite was finally run

With `fixtures.rs` fixed, `tests/provider_ipc.rs:272` failed on the same
operation. CI never showed this — `fixtures.rs` failed first and the job stopped
there, so fixing only what CI printed would have produced a second red run.

The `fstat` comment in that file had already named the shape: *"there are three
decode loops in this crate's tests and `sandbox_launch.rs` was already tolerant,
so these two are the pair that had to learn it."* The pair had to learn it again.
`provider_ipc.rs`'s `matches!` now admits `FsOp::Close { .. }` beside
`FsOp::Fstat { .. }`, with the reasoning recorded. `sandbox_launch.rs` needed
nothing, as its own comment predicted — verified by running it: 4 passed.

### `ReadDir`(461) and `Fchdir`(13): deliberately no arm, and why

Both joined the same stub list, so the question is fair. They **cannot reach
these harnesses**, and I checked rather than assumed:

- `experiments/fixtures/umbra-test-child.c` contains **zero** occurrences of
  `getattrlistbulk`, `fchdir`, `opendir`, `readdir` or `fts_` — it issues no
  directory read and no `fchdir`.
- Neither is issued by libSystem before `main` the way `fstat` and `close` are;
  that is the measured basis of the whole slice — only `fts` reaches them.
- Empirically: with the `Close` arm added and no arm for either, all 16 cases
  pass. Had either arrived, the panic would have fired.

So no arm was added for them. Inventing an empty arm for an operation this
driver has no plan for is precisely the wildcard the harness's own comment
refuses; if one ever does arrive, stopping the case loudly is the right outcome.
Both files say so, so the absence is explained rather than merely true.

### Before / after

| suite | before | after |
|---|---|---|
| `tests/fixtures.rs` | **2 passed, 9 failed** | **11 passed, 0 failed** |
| `tests/provider_ipc.rs` | (never reached in CI) **1 failed** | **1 passed** |
| `tests/sandbox_launch.rs` | not reached | **4 passed** |

---

## 2. Why every gate in this graph missed it — measured, and it is worse than "unset env"

`fixtures.rs` skips when `UMBRA_TEST_FIXTURE_PATH` is unset. The critical detail
is *how* it skips:

```
$ cargo test -p umbra-platform-macos --test fixtures        # no env
test result: ok. 11 passed; 0 failed; …; finished in 0.00s

$ UMBRA_TEST_FIXTURE_PATH=… UMBRA_TEST_REDIRECT_ROOT=… cargo test … --test fixtures
test result: ok. 11 passed; 0 failed; …; finished in 13.08s
```

**Identical counts. The only discriminator is elapsed time.** Each test body
returns early and reports `ok`, so the eleven cases are counted as passing
whether or not they ran. That is why no count reconciliation could have caught
this, and why `cargo test --workspace --all-targets` reported 826, 829 and 832
across three rounds with these nine cases never executing.

The crate README **already documents this** and has all along: *"when either env
var is unset, each test body skips via `eprintln` — the binary still reports the
cases as passed, so qualification requires the `CAPTURED <case>` stderr
verdict."* The documentation was right; the gate ignored it.

**Consequence for this graph's gate command:** the workspace gate must carry the
fixture env, or it is not testing this crate. With it set, the workspace run
takes 52s against 27s without — same 832 count, twice the work. Every gate figure
in this report was taken with the env set.

---

## 3. CodeRabbit — five items, five accepted, none rebutted

### 1. `umbra-userspace-listing.c` — the empty-vs-error hole (the only code item)

**Accepted, and it is the sharpest of the five** because it is this slice's own
theme inside its own proof fixture: an empty listing indistinguishable from a
served one.

`fts_children` returns `NULL` both for a genuinely empty directory (`errno`
untouched) and for one it could not read (`errno` set). The loop treated them
alike, and the trailing `fts_read` check cannot rescue it — a later `fts_read`
resets `errno`. So a **failed directory read produced an empty listing and exit
0**.

Fixed: `errno` cleared immediately before the call and read immediately after,
before any other libc call can overwrite it; a nonzero `errno` with a `NULL`
return is exit **8**, reported with the numeric errno as the fixture's own
convention requires. Exit 8 is documented beside the other seven.

**Measured before and after**, on a directory made unreadable (`chmod 000`):

```
BEFORE   exit=0   lines=0                                        <- silent
AFTER    fts_children /tmp/noread/sub: Permission denied (errno=13)
         exit=8   lines=0                                        <- loud
```

And the two non-error paths still behave: an empty directory exits 0 with zero
lines (**not** an error), a normal directory exits 0 with its names.

### 2. `fix-r3.md:37` — zero size qualified by descriptor status

**Accepted.** Row 5 of the input table said `EINVAL` for a zero size without
qualification. Measured, the kernel decides the descriptor first: unbound + zero
is `EBADF`, and `EINVAL` applies only after the descriptor validates. The row now
says that, which is also what the code does after round 3's reordering — the
table was describing the fix less precisely than the fix implements it.

### 3. `fix-r3.md:159` — the guard is a subset of the denial, not its equal

**Accepted.** I wrote that the N7 guard "fires exactly when
`!context.fds.contains_key(fd)`, which is **precisely** `routed_binding`'s
`Deny(EBADF)` condition". That equality is false: `routed_binding` denies on
three conditions — no binding, a binding with no `logical_path`, and a name that
no longer resolves — where the guard tests only the first.

**The unreachability argument survives**, and the correction says why rather than
just softening the wording: it needs the implication, not the equality. Guard
fires ⇒ no binding ⇒ `routed_binding` denies. Guard ⊆ denial is the direction
that matters and the direction that holds. A bound descriptor with no
`logical_path` simply does not take the guard's path; it goes through
`io_binding` and is denied `EBADF` inside `resolve` by the ordinary route.

### 4. `fix-r3.md:259` — the test delta

**Accepted, and it is the fourth quantitative self-report in this slice that did
not reconcile.** I claimed `+4, no test removed`. Round 3 added four and removed
one: `a_buffer_too_small_for_one_record_is_refused`, whose single `is_err()`
assertion `every_output_bound_refusal_carries_a_bindable_errno` supersedes with a
sweep. Net **+3**.

Verified independently rather than taken:

```
$ grep -rn '#\[test\]' crates/ --include='*.rs' | wc -l
908          # this change
887          # master@a85a8471
```

905 before round 3, 908 after. CodeRabbit, the round-4 scope review, the driver
and this count all agree.

**The structural half.** `impl.md` §6 already carried a sweep-not-a-count
discipline for test *edit* counts. It now covers *every* quantitative claim in
these records, naming all four failures (two edit counts, the `822`/`813`
baseline, this delta) and the command that produces the figure. The point is not
the number; it is that four numbers derived from memory of what a pass did were
all wrong, and every one was caught by someone re-deriving it.

### 5. `impl.md:255` — `-R` described as fail-closed

**Accepted.** The README was split during round 3 precisely because "fail-closed"
is false for recursion — when `fts` declines to descend it issues no syscall, so
there is nothing to refuse it *with*. `impl.md` still carried the old joint
wording. It now has its own row: not claimed, not tested, **not fail-closed**,
with the mechanism and a pointer to the README's full disposition. The
metadata-requesting modes keep the fail-closed claim, which is accurate for them.

---

## 4. memoria

Run from a throwaway git worktree (`rsync` excluding `target`, `.jj`,
`third_party`; `git init`; one commit), because this workspace has `.jj` and no
`.git` — #123's hazard, ratified as workflow-only.

Two boundaries flagged `input_changed`:

| README | changed inputs | disposition |
|---|---|---|
| `crates/umbra-platform-macos/README.md` | `tests/fixtures.rs`, `tests/provider_ipc.rs` | **no-update** — the README documents these drivers by purpose and env vars, not by the operations they match. Its skip warning is not only still true, it is the thing that describes §2's hazard exactly |
| `README.md` (root) | `ci-round1.md`, `fix-r3.md`, `impl.md`, `publish.md` | **no-update** — the root README neither references nor summarises the slice records, and carries no quantitative claim about them |

Both checked against the guidance's rule — *"if the README describes exported
names, modules, or behaviour that no longer matches source, edit the README
rather than acknowledging the drift"* — before acking. Neither describes anything
my changes falsified. `memoria check` now: **OK, 23 READMEs current.**

---

## 5. Measured versus inferred

**Measured:** the CI failure reproduced locally, identical to the job log; the
three native-qualification suites before and after; the second instance in
`provider_ipc.rs`; `sandbox_launch.rs` needing nothing; the absence of
`getattrlistbulk`/`fchdir`/`opendir`/`readdir`/`fts_` from the fixture child's
source; the skip-vs-run timing (0.00s against 13.08s at identical counts); the C
fixture's before/after on an unreadable directory and on an empty one; the
`#[test]` counts at both revisions; `memoria check` before and after acking; all
three local gates with the fixture env set; `userspace_run` 22/22 live;
`run_fixtures` 10/10; the probe and its negative control; 20 flag shapes
side-by-side against master.

**Inferred, not measured:** that `ReadDir`/`Fchdir` cannot reach these harnesses
on *CI's* machine specifically — I measured it here and argued it from the
fixture child's source and from libSystem's measured pre-`main` behaviour, but
the CI runner is a different host. If either ever arrives there, the panic is the
designed outcome and will say so by name.

**One environment note, not a finding:** the Ganesha fixture container had died
between rounds; the first `userspace_run` attempt failed 15 cases on "no prepared
run id" before I noticed `docker ps -a` was empty. Restarted from
`experiments/nfs-raw/docker-compose.yml`, after which the suite is 22/22. Nothing
in the tree was involved.

**Nothing rebutted this round.** All five CR items were correct, and item 1 found
a real hole in our own proof fixture.

---

## 6. Gates

**Figures below are at head `6a18b98f`** — the commit this pass produced. CI
round 2 tested `52fba1e1`, the driver's memoria re-ack on top of it, which
changed no code and no test. `impl.md` §7.1 is the canonical per-commit table;
832 still holds at the current head.

| gate | result, at `6a18b98f` |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` **with the fixture env** | **832 passed**, 3 ignored, 52s (27s without the env, same count — §2) |
| `--test fixtures --test sandbox_launch --test provider_ipc` | **16 passed, 0 failed** (was 2 passed, 9 failed) |
| `userspace_run` vs live Ganesha | **22 passed, 0 failed** |
| `run_fixtures` | **10 passed**, incl. `/bin/ls -l`, `/bin/ls -t` |
| `readdir` probe, mutated / **negative control** | passes / **fails on the names** |
| side-by-side vs `a85a8471`, 20 flag shapes | **0 divergences** |
| `memoria check` | **OK, 23 READMEs current** |

No test added or removed this pass; two test-harness helpers gained an arm each,
neither inside a `#[test]` body. The count stays 908.

**Still standing:** PAUSE BEFORE MERGING. The change is amended and pushed to
`feat/ls-userspace`; nothing is merged.
