# CI + CR fix round 3 — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `fix_from_ci_cr`, visit 3, continuing the `implement` session. Date 2026-09-28.
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`, parent `master` `e44d0db8`.
PR: https://github.com/invakid404/umbra/pull/134. Input:
`/tmp/graph-dg-nsw71bqq/ci-round3.md`, anchored on head `3e763c11`.

**CI green at the input head** — 5 passed, both Rust workspace jobs including the Memoria
documentation gate. Nothing to do on the CI side; the re-ack from round 2 held.

Three CR findings, all valid, all fixed. Plus the question the driver put to me, which I
answer first because the other three depend on it.

**Constraints held:** no production source, guardrails byte-identical, `single_thread()`
and the `debug_assert!` untouched, slice 1 not started, both waivers unconsumed.

---

## The judgement: keep the machinery, harden it, and delete one piece of it

**Asked for: whether the cleanup machinery should exist at all, given that three rounds
of review found three progressively worse hazards in it. My answer is keep it, with one
removal — and the reasoning is not "CodeRabbit was right three times".**

### Why the trend reads worse than it is

The severity trend is real and I do not dispute it. But severity is one axis and the
other one moves the opposite way:

| round | hazard | ever observed? | blast radius |
|---|---|---|---|
| 1 | `mt_spawn` leaks a suspended child holding inherited stdio | **yes — every single run**, and it hung a real command for >3 minutes in this session | the developer's terminal |
| 2 | the reaper could kill a concurrent test's child | no | an unrelated test fails |
| 3 | `Second`'s cleanup could delete a preexisting path | no | a directory under `TMPDIR/umbra-rust-fixture-*` |

So what is escalating is *hypothetical* consequence; what is falling is likelihood, and
the blast radius stays inside a temp path this test family owns by naming convention.
That is not a machinery that is diverging — it is CodeRabbit performing progressively
deeper static analysis of the same ~120 lines and finding progressively more remote
defects. A review trajectory that goes "observed bug → possible race → possible
mis-delete" is a *converging* one. If round 4 finds something in this code it will be
more remote still.

### The fact that decides it

**Removing the cleanup does not remove the risk, it relocates it onto a human.** These
two tests are `#[ignore]`d. Their documented invocation is `cargo test … -- --ignored`,
run by hand, repeatedly, by whoever works on slice 1 — which is precisely the situation
the leak hurts. The leaked child holds descriptors 0/1/2, so a captured pipe never
closes. That is not a prediction: **it happened in this session in round 0**, a
`cargo test` invocation hung past its timeout and had to be killed out of band, and I
spent the next several commands running the test binary directly to avoid it. Both
reviewers independently reaped orphans by hand after every round — nine in one, ten in
another.

Weigh the two failure modes as a developer meets them:

- **With cleanup, worst realistic case:** a stale `TMPDIR/umbra-rust-fixture-<pid>-*-b/`
  from a recycled pid is deleted. Recoverable, bounded, and now impossible (below).
- **Without cleanup, certain case:** every hand-run leaks a suspended process holding
  your terminal's pipe, silently, and the symptom — a command that never returns —
  points nowhere near its cause.

The second is worse, and it is certain rather than possible. "Harmless in CI" is true and
not the relevant test: the runner is ephemeral, but the tests are not primarily run in CI
— they are skipped there and exist to be run by hand.

### What I am conceding, because the driver's point is not empty

I am removing **`harness_directory`** outright. It was the one part that deleted
something *another function* created, at a path derived by copying `fixture_argv`'s
private naming. That coupling is exactly the shape that generates a round-4 finding, and
what it bought was the removal of an **empty** directory — no process, no descriptor, no
content. Dropping it removes the most speculative code in the machinery and the only
cross-function ownership claim in it. The empty directory `fixture_argv` leaves on a
failing case is `fixture_argv`'s business, shared by all thirteen cases, and not this
slice's to fix.

What stays, and why each piece earns it:

- **`StrayFixtureChildren`** — closes an observed, every-run, developer-facing defect.
- **`FIXTURE_LAUNCH`** — correct independently of the reaper: two concurrent tracer
  launches would contend for a debugger connection and the resigned-twin cache, which is
  not a supported configuration. It would be worth keeping even if the reaper went.
- **`Second`'s output cleanup** — two files this test provably created, now with
  ownership tracked in the type.

**A note on process, since this is the third round on the same code.** If a fourth
finding lands here, I would treat that as evidence for the driver's position rather than
for another patch, and I would say so then. The line I am drawing is: hazards that are
*observed* get fixed; hazards that are *derivable but unobserved* get fixed while the fix
shrinks the code, as this round's does, and get argued about when it would grow it.

---

## CR3-1 — `Second` removes only what it created · **FIXED**

The finding is right and the old code did not establish ownership at all:
`create_dir_all` succeeds whether or not the directory existed, and `remove_dir_all` then
deleted it wholesale. Process ids are recycled, so a directory left at that path by an
older binary is a real possibility.

Every point of the required fix, in order:

- **`fixture_launch_lock()` before setup**, not just before the launch. The exclusive
  creation below is the ownership proof, and a concurrent case observing or creating the
  same path would void it. One held region now covers setup → before-snapshot → run →
  cleanup.
- **Ownership tracked, not assumed.** `create_dir` replaces `create_dir_all`:
  `Ok(())` ⇒ this test made it, `AlreadyExists` ⇒ something else owns it and it survives
  the run, any other error panics with the path. The result is carried as
  `owned_directory: Option<PathBuf>`, so the type states the distinction.
- **Cleanup armed only after both absence checks pass.** `Second` is constructed after
  `assert!(!host.exists())` and `assert!(!shadow.exists())`, so arming cleanup and proving
  the outputs absent are the same event. A failing check leaves an empty directory rather
  than deleting a file the test cannot account for.
- **The destructor removes only owned outputs.** `host` and `shadow` — both proven absent
  before the value existed — and the directory only when `owned_directory` is `Some`, with
  `remove_dir` rather than `remove_dir_all`: with the output gone, a directory this test
  created is empty, and anything still in it is something this test did not put there.
  The call refusing a non-empty directory *is* the check.

**Demonstrated, not argued.** A directory was planted at the exact derived path with
unrelated content, using `sh -c 'mkdir …$$…; exec <test binary>'` so the planted path
matches the pid the binary actually runs under:

```
planted /var/…/T/umbra-rust-fixture-97636-mt-write-b with sentinel
thread 'mt_write' panicked at …/fixtures.rs:629:5:   (the verdict, unchanged)

after the run:
  -rw-r--r--  unrelated.txt
  sentinel: do-not-delete
```

The preexisting directory and its content survive; the escaped `output` file is still
removed; the measurement verdict is unchanged. And the owned path still cleans completely
— both `#[ignore]`d cases under default parallelism, twice: exactly one `REAPED` each,
**0** strays, **0** `-b` directories, **0** shadow files left.

## CR3-2 — the fixed-point claim narrowed · **FIXED**

`ci-fix-r2.md` claimed "the only integers either section states are the four content
files' line counts". False: §8 also tabulates the `+/−` figures and the subtotal. The
claim is now scoped to what the check actually assesses — the claims that vary with the
*number of paths in the diff*: path totals, process-document counts, the per-arc inventory
— and it says explicitly that §0/§8 do contain other integers, namely the content-file
measurements, which are stable under this check rather than exempt from it. The subtotal's
growth across the CR rounds (527 → 790 → 808) was noted as a measurement changing rather
than a claim going stale — a reading CI round 4 then corrected: a figure that moves every
time a review round edits the code is stale by the same mechanism as the path totals, and
it has since been removed rather than re-derived.

## CR3-3 — the two memoria runs distinguished · **FIXED**

This was the finding I most deserved. `impl.md` §6 described the gate as "not qualified
locally" — text written when that was true — while the gate had since been qualified by a
different run, and a reader could take the section as reporting on the passing one. Now
split explicitly:

- **Run A, in the jj workspace: FAILED**, `error [git_unavailable]`, exit 4, and
  qualified nothing. Earlier rounds reasoned around that failure rather than from a result.
- **Run B, in a throwaway git worktree at the pushed head: PASSED**, and this is what
  qualifies the gate, citing the recorded output —
  `OK: 23 README(s) current, imports rendered, no coverage or structure errors.`, 0 errors,
  14 `navigation_disconnected` warnings = master's baseline — corroborated by CI's own
  gate step on both platforms.

With the statement that neither run substitutes for the other, and that re-verifying run
B in a fresh worktree is now a standing per-push check.

---

## Suggested replies to the CR threads

**To CR3-1 (`fixtures.rs:573-599`):**

> Fixed as specified, and thank you — the old code established no ownership at all, since
> `create_dir_all` succeeds either way and `remove_dir_all` then took the directory
> wholesale. Now: `fixture_launch_lock()` is acquired before setup rather than before the
> launch, because the exclusive creation is the ownership proof and a concurrent case
> would void it; `create_dir` replaces `create_dir_all`, so `AlreadyExists` means
> something else owns the path and it survives the run, carried as
> `owned_directory: Option<PathBuf>`; `Second` is constructed only after both absence
> checks pass, so arming cleanup and proving the outputs absent are one event; and the
> destructor removes only `host` and `shadow` plus the directory when owned, with
> `remove_dir` not `remove_dir_all` — the call refusing a non-empty directory is the
> check. Verified by planting a sentinel at the exact derived path (`sh -c 'mkdir …$$…;
> exec <binary>'` so the pid matches): the preexisting directory and its content survive,
> the escaped output is still removed, the verdict is unchanged. I also removed
> `harness_directory` entirely — it deleted a directory another function created, from a
> copy of that function's private path derivation, to save an empty directory; that
> coupling was not worth its risk.

**To CR3-2 (`ci-fix-r2.md:277-279`):**

> Fixed. The claim is now scoped to what the check assesses — path totals,
> process-document counts and the per-arc inventory, i.e. everything that varies with the
> number of paths in the diff — and it states explicitly that §0/§8 do contain other
> integers, the four content files' `+/−` figures and their subtotal. Those are re-quoted
> from `jj diff` every round and are stable under this check rather than outside it; the
> subtotal's growth from 527 to 808 is a measurement changing as the CR fixes added test
> code, not a claim going stale. (Superseded in CI round 4: that figure moves on every
> round that edits the code, so it is stale by the same mechanism as the path totals and
> has been removed rather than re-derived.)

**To CR3-3 (`impl.md:922-930`):**

> Fixed, and this was the sharpest of the three. The section now separates the two runs:
> run A in the jj workspace **failed** with `error [git_unavailable]` and qualified
> nothing, and run B in a throwaway git worktree at the pushed head **passed** — `OK: 23
> README(s) current …`, 0 errors, 14 `navigation_disconnected` warnings matching master's
> baseline — with run B named as what qualifies the gate and CI's own gate step
> corroborating it on both platforms. It also says neither run substitutes for the other,
> since run A says nothing about the documentation and run B cannot be performed from
> inside this workspace.

---

## Gates, re-run against the final tree (lesson 24), verdicts read by name (lesson 23)

C fixture recompiled and the crate's tests rebuilt after the last edit.

| Gate | Result |
|---|---|
| `memoria --root . check`, throwaway worktree at the **pushed** head | **0 errors**, `23 README(s) current`, 14 `navigation_disconnected` warnings = master's baseline |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, **0** lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |
| `--test fixtures` (integration env, `--test-threads=1`) | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED`, **0** `REAPED` |
| both `#[ignore]`d cases, **default parallelism**, ×2 | exactly 1 `REAPED` each; 0 strays, 0 `-b` directories, 0 shadow files remaining |
| preexisting-directory probe | sentinel survives; escaped output removed; verdict unchanged |
| `--test provider_ipc` | 1 passed — `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS`, 2 declared `SKIP nfs_*_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh` untraced | **12 of 12 PASS** |

**The 832 figure keeps its standing qualification**: no `--nocapture` and no fixture
environment, so every integration case in it takes `fixture_argv`'s skip branch and
reports `ok` with its `SKIP` invisible.

**The eleven `CAPTURED` verdicts, read by name from this round's own output:**
`argv0-check`, `dirfd-rename`, `dup-inherit-write`, `exec-write`, `fork-write`,
`grandchild-write`, `open-libc`, `open-svc`, `posix-spawn-write`, `symlink-cycle`,
`wnohang-wait`.

**Both `#[ignore]`d cases re-measured**, same two verdicts as every round — the ownership
change did not move the defects. `mt_write`'s assertion is at `fixtures.rs:629`, measured
rather than assumed. §8's content subtotal was re-quoted as 808 at this round and has
since been removed entirely (`ci-fix-r4.md`).
**No new measurement of the defects was taken**; every enforced-run figure in `impl.md`
is rounds 0–2's, restated.

## The fixed-point check, run again

Writing this document adds a path. Nothing in §0 or §8 states a path total, a
process-document count or a per-arc integer, so a hypothetical `ci-fix-r4.md` falsifies
none of it; §0's "`impl.md`, one `fix-rN.md` per review round, one `ci-fix-rN.md` per CI
round" is a rule over rounds, and `memoria.lock` is named as a conditional deliverable.
What this check assesses is the claims that vary with the number of paths in the diff —
path totals, process-document counts and the per-arc inventory — none of which is stated
as a literal. It is not a claim that the sections contain no other integers; at the time
of writing §8 still tabulated the four content files' figures.

*CI round 4 note:* those figures proved size-dependent with respect to review iteration
rather than path count, and §8 now states no line counts at all — see `ci-fix-r4.md`.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
