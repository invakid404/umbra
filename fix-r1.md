# Fix — review round 1

Graph `dg-egt6apy1`, node `fix`, visit 1. Date 2026-09-28.
Input: `review-synthesis-r1.md` (from `review-correctness.md` + `review-scope.md`).
Change: **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark `feat/fork-lifecycle`,
parent `master` `7c3ecc8f`.

> **Every figure in this document is measured against that change id.** Per
> lesson 26, no working-copy commit id is cited: the one `impl.md` originally
> named (`fff2bae0`) has re-timestamped repeatedly and is now hidden. The
> existing change was **amended**, not replaced.

Remediation, not redesign. Both reviews confirmed the mechanism work is sound
and I have not revisited any of it.

---

## Disposition

| | Finding | Disposition |
|---|---|---|
| **R1** | Failed `execve` leaves `Session::twin` stale | **Fixed**, with a **live** regression test that fails before and passes after |
| **R2** | Three new prose sites assert the refuted pre-fix mechanism | **Fixed**, all three, plus a note in `impl.md` |
| **R3** | `events.rs` still calls `ChangedCwd` "the one thing that has ever moved cwd" | **Fixed** |
| **R4** | Interposer C header stale (`chdir` missing, `DORMANCY` superseded) | **Fixed**, prose only — **0 changed lines of C** |
| **R5** | `Chdir`'s `None` arm is a silent-success seam | **Fixed** — now `ProtocolMismatch` |
| **R6** | `impl.md`: "19 PASS lines" (really 20) and a working-copy commit id | **Fixed**, both |
| **R7** | Uncovered `chdir` operand shapes; `rollbackchild` comment attribution | **Fixed**, both (optional, taken) |
| **D1/D2/D3** | Deferred to `merge_gate` | **Untouched**, deliberately |

---

## R1 — the regression, and its test

### The fix

`intercept`'s `Delivery::Exec` arm committed `s.twin = twin` at the syscall
**entry**. A successful `execve` never returns, so that was correct for the case
it was written for. A **failed** `execve` returns — to `finish_return`'s
`ReturnKind::Exec` arm, which is the failed-exec return *by construction* — and
nothing put the old value back.

Took the smaller of the two ratified shapes: the resigned image is now carried as
a **candidate** on `ReturnKind::Exec { twin }` and adopted into `Session::twin`
only at the exec stop, where it was already being read. The failed-exec arm binds
and discards it (`twin: _candidate`, bound rather than `..` so a later edit has
to decide what a failed exec means for it rather than inheriting an answer). An
exec stop with no pending candidate leaves `twin` alone, which is what the code
did before the candidate existed.

### The regression test — live, not unit-level

The reviewer recorded an honest constraint: *"a resign-succeeds/exec-fails binary
could not be manufactured cheaply"*, and allowed a unit-level assertion as a
fallback. **A live trigger turned out to be reachable, so the fallback was not
used.** Finding it took two attempts and both are recorded, because the first
failure is itself the interesting part:

* **A dylib does not work**, though it is the shape F1's scenario names. It
  signs and `execv` fails `ENOEXEC`(8) — but `cache::resign` also runs
  `codesign -d --entitlements :-` and requires three keys back, and a dylib does
  not report them. The run therefore stops at `Io during codesign: missing twin
  entitlement`, **before** the operand is rewritten and before `twin` is touched.
  That is a different and entirely safe outcome, and a test built on it would
  have exercised nothing. Measured, not reasoned.
* **An arm64 executable with its execute bits cleared does work.** `codesign`
  cares about content, so signing and strict verification both pass and the
  entitlements read back; `fs::copy` preserves the mode into the twin, so the
  rewritten exec names an unexecutable file too. Measured: `execv` fails
  `EACCES`(13).

New fixture case `failedexec` (`umbra-userspace-edges.c`) execs that image,
observes the failure, then `fork`s and has the child write through routing;
driver `a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran`
reads both the child's and the parent's objects back **through the NFSv4
client**, because a surviving run whose child routed nothing would still exit 0.

### Before / after evidence

**Before** — the fix reverted to the pre-fix behaviour (`s.twin = twin.clone()`
restored at the exec entry, everything else identical), binaries rebuilt, live
fixture:

```
test a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran ...
panicked at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:526:32:
no child status in:
umbra: run ecba56cb-8322-4b64-87eb-d2783f778130 prepared
Io during tracee stop: fatal signal/exception: T0c…
  09:314d524152424d55; 0a:314d524152424d55; 10:52424d5500000000;
  … metype:5;mecount:2;medata:10003;medata:c
test result: FAILED. 0 passed; 1 failed; … finished in 4.48s
```

Decoded: `x16 = 0x554d4252` (`UMBRA_TRAP_NUMBER`), `x9`/`x10` = `"UMBRARM1"`
(`ARM_MAGIC`). **Armed control block, unbreakpointed trap, fatal undecoded
exception** — the #116 shape.

**After** — fix restored, binaries rebuilt, same fixture:

```
test a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran ... ok
test result: ok. 1 passed; 0 failed; … finished in 4.45s
```

**This upgrades R1's own evidence grade.** The review recorded the *consequence*
as CONFIRMED by execution (Mutation C, a hand-injected mismatched path) and the
*trigger* as PLAUSIBLE, reasoned from three lines. The run above reaches the
identical register signature through the **actual failed-`execve` trigger**, with
no mutation anywhere in the tree. The trigger is now confirmed by execution too.

---

## R2 — the refuted mechanism, corrected at all three sites

The audit predicted an unmediated exec'd child whose reads reach the host and
whose writes Seatbelt refuses, leaving the object absent. That is wrong, and
`impl.md`'s Step 0 already said so; three *new* prose sites repeated the
prediction anyway. The measured mechanism, which Mutation B settled independently
by reading `[]` back through the client:

> `install()` re-plants every `TRACED_STUBS` breakpoint after an exec, so the
> child's `open` **was** routed and did return a virtual descriptor. Only the
> interposer was un-armed, and `write`/`close` reach umbra through the interposer
> alone — so they went to libc carrying a number the kernel does not own and were
> answered **`EBADF`**. The object is **created and left empty** in the export.
> Nothing read the host; Seatbelt refused nothing.

Corrected at `README.md` (the `exec` row), `userspace_run.rs` (the `forkexec`
driver's doc comment) and `umbra-userspace-edges.c` (the `forkexec` case header).
Each now also says *why the driver asserts bytes rather than presence*: broken and
fixed both leave an entry at that path, and only the contents tell them apart.
That connects R2 to D3 — the reason `forkexec`'s discriminator is bytes is now
stated where a reader meets the test, rather than left as an unexplained
departure from the ratified "entry names" wording.

`impl.md` carries a round-1 note at the Step-0 paragraph recording that it was
right and the three other sites were wrong.

---

## R3 — the stale half-sentence

`events.rs` headed the `ChangedCwd` arm *"The one thing that has ever moved
`ProcessContext::cwd` after launch"* — false since `MovedCwd` landed in the same
change, and contradicted by the arm's own block a few lines down. Rewritten to
describe both movers honestly: `fchdir` was the first, `chdir`(12) is the second,
`GetCwd` is still inert and moves nothing; and the two differ *only* in where the
path comes from, which is forced by what each call is handed.

---

## R4 — the false invariant in a file with zero changed lines

`umbra_interpose.c`'s header documented behaviour that moved underneath it:

* the routed-call list omitted `chdir`(12) — added, with a pointer to
  `abi::TRACED_STUBS` as authoritative and an explicit warning that this list
  names the *shape* and will go stale again if read as membership;
* `DORMANCY` described a two-image world (installer + target) that P1a
  superseded — rewritten to say the armed set follows a session's execs and its
  attached children, that a *forked* child is the opposite case and is
  deliberately not re-armed, and that none of this changes anything in the file,
  which is the point.

**Prose only.** Verified mechanically: filtering the file's diff for added lines
that are not comments or blank yields nothing. Touching the C would have breached
the contract bound.

---

## R5 — the silent-success seam

`record_routed_effect`'s `Chdir` arm returned `Ok(())` when the namespace answered
`routed_cwd() == Ok(None)`, on the reasoning that `Overlay::resolve_routed_chdir`
sets the `Emulate` action and the resolved path together. True of *that* resolver;
not enforced at the seam — and the seam is a trait, so the resolver on the other
side is not necessarily that one. A provider answering `None` would have the
tracee told its `chdir` worked while `ProcessContext::cwd` stayed put: the split
anchor all of P0 exists to close, arriving by the one route P0 does not cover.

Now a `ProtocolMismatch` naming the fault, matching `Opened`'s shape — which
cross-checks the namespace's answer rather than trusting it. That asymmetry was
the defect.

---

## R6 — provenance, both defects

* **The count.** Re-measured mechanically rather than re-counted by eye:
  `PASS lines: 20 — of which crash: 3, of which local: 17`. The reviewer is
  right; my collapsed `/usr/bin/touch` notation merged `touch touched` and
  `touch seed.txt` into one. `impl.md` now reads 20 and enumerates both.
* **The tree.** `impl.md` cited `fff2bae0`, a working-copy commit that was not
  the reviewed tree and has since re-timestamped and gone hidden. It now cites
  the **change id**, with a note explaining why a working-copy commit is not a
  stable handle.

I have not re-audited the other figures; Scope re-ran them on the reviewed tree
and every one reconciled. What follows are fresh measurements on the amended tree.

---

## R7 — taken, both parts

* **`chdir` operand shapes.** New fixture case `chdirshapes` plus driver
  `a_chdir_answers_enoent_and_enotdir_to_the_tracee_and_anchors_a_relative_operand`
  closes all three gaps F6 named. The refusals matter for a reason particular to
  a routed run: umbra cannot fall back to resuming the tracee's syscall, because
  for a routed operation that syscall is umbra's reserved trap, which `nosys`
  answers `ENOSYS`(78) while posting `SIGSYS` — so `ENOENT` and `ENOTDIR` must be
  *produced*, and a suite that never asks for them cannot tell a correct refusal
  from a dead run. The relative half chains **two** `chdir`s so the assertion is
  not satisfiable by one: `leaf.txt` can only be two directories deep if the
  first move anchored the second. Asserted on the entry name, both directions.
* **`rollbackchild` attribution.** The comment credited the journal assertion
  with the structural claim. Rewritten to separate three claims: the status
  assertion proves least; the **journal** assertion pins *that the refusal
  fired*; the **middle** assertion — the child's object at the parent's
  `shadow_path` — is the one carrying the run-scoping claim. **Comment only. No
  assertion, fixture or declaration was changed** — see D1 below.

---

## Deferred findings — untouched, deliberately

* **D1, the `rollbackchild` narrowing.** Not pre-empted. The test's assertions,
  its fixture case and the declaration in `impl.md` and the doc comment are all
  unchanged. Only the *attribution* prose R7 names was corrected. The ratifier set
  the fixture shape and only the ratifier can change it.
* **D2, `routed_cwd()` beyond the literal bound.** Adjudicated required, not
  creep. Left as-is.
* **D3, `forkexec` discriminating on bytes.** Left as-is. R2's correction now
  states *why* bytes are the discriminator at the site a reader meets it, which
  should help the human's decision without pre-empting it.
* Still untouched: multithreaded fork, `vfork`, `posix_spawn` with non-null file
  actions, process-group waits, widening the `ENOTSUP` set, and the #117 item-3
  narrowing.

**Guardrails re-verified after R1 touched `native.rs`**, by diff against
`git show master:` rather than by inspection — all four byte-identical:

```
parent guard (self.parent.is_some()): BYTE-IDENTICAL   (1 site)
single_thread():                      BYTE-IDENTICAL
TRANSIENT_SIGNALS:                    BYTE-IDENTICAL
wait_plan():                          BYTE-IDENTICAL
```

---

## Gates — re-qualified on change `puyyxvmvmnpk…`

**Binaries built first, per lesson 24**, before every routed run:
`cargo build --workspace --bins`.

### fmt / clippy

`cargo fmt --check` — clean, exit 0.
`cargo clippy --workspace --all-targets -- -D warnings` — `No issues found`.

### `cargo test --workspace --all-targets`

`832 passed, 0 failed, 3 ignored`. **Not qualification**, and the lesson-23
signature is unchanged from round 0 — read from the per-suite lines:

```
tests/run_fixtures.rs   -> 10 passed, 0.00s
tests/fixtures.rs       -> 11 passed, 0.00s
tests/provider_ipc.rs   ->  1 passed, 0.00s
tests/sandbox_launch.rs ->  4 passed, 0.00s
tests/userspace_run.rs  ->  0 passed, 0.00s
```

All five at 0.00 s; the routed suite ran **zero** cases. So each was re-run with
its inputs and `UMBRA_INTEGRATION_REQUIRED=1`.

### Enforced macOS qualification — 12 `CAPTURED` verdicts read

```
CAPTURED argv0-check      CAPTURED open-libc
CAPTURED dirfd-rename     CAPTURED open-svc
CAPTURED dup-inherit-write CAPTURED posix-spawn-write
CAPTURED exec-write       CAPTURED symlink-cycle
CAPTURED fork-write       CAPTURED wnohang-wait
CAPTURED grandchild-write
test result: ok. 11 passed; 0 failed; … finished in 11.92s   (was 0.00s)
CAPTURED open-libc provider IPC
test result: ok.  1 passed; 0 failed; … finished in  1.77s
test result: ok.  4 passed; 0 failed; … finished in  8.12s   (sandbox_launch)
```

`exec-write`, `fork-write`, `grandchild-write`, `posix-spawn-write` and
`dup-inherit-write` are the direct regression evidence that R1's change to the
exec path did not disturb the rewrite-backed fork/exec lifecycle.
`sandbox_launch` doing 8.12 s of real work covers the installer handoff, which
now runs through the candidate-adoption path.

### CLI run-fixture matrix — 20 `PASS`, 2 declared `SKIP`

Counted mechanically, not by eye (this is R6's defect):

```
PASS lines: 20 — of which crash: 3, of which local: 17
SKIP lines: 2
test result: ok. 10 passed; 0 failed; … finished in 44.50s   (was 0.00s)
```

Both `SKIP`s are the NFS-mount matrices under `UMBRA_TEST_SKIP_NFS_MATRIX`, the
same opt-out CI's `native-qualification` job sets for the same documented TCC
reason. They are the only unqualified cases in this gate and are named rather
than counted as passes.

### Routed suite over the live NFSv4 client

Ganesha healthy on `127.0.0.1:12105`; `UMBRA_INTEGRATION_REQUIRED=1`, so a
missing fixture is a hard failure rather than a skip.

```
test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 90.01s
```

**22 cases executed live, 6 declared `SKIP`s** — all six the mutation probes,
enumerated from the output: `fstat`, `mkdir`, `read`, `readdir`,
`setattrlistat`, `write`. Those are skip-by-design in a baseline build (no two
probe features may be enabled at once), and
`every_mutation_probe_is_wired_into_the_userspace_job` passed, which pins that CI
runs all six.

**All six new/changed cases EXECUTED rather than skipped**, read by name:

```
a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran ... ok
a_chdir_answers_enoent_and_enotdir_to_the_tracee_and_anchors_a_relative_operand  ... ok
a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image        ... ok
a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against ... ok
a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it     ... ok
a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence ... ok
```

and the two pre-existing fork cases this change most risked:

```
a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes             ... ok
a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store    ... ok
```

### Mutation probe spot-check, and the lesson-24 discipline applied to it

Probe B (`write`) built and run: `mutation_probe_write_makes_the_read_back_come_up_short ... ok`,
28 passed. The unmutated binaries were then rebuilt **and the restoration verified
by execution** rather than assumed — `cargo build` reported "Finished in 1.51s",
which is exactly the shape lesson 24 warns about, so the baseline end-to-end case
was re-run and passed:

```
a_routed_run_creates_writes_reopens_reads_and_compares_end_to_end ... ok
```

---

## What a reader should still know

* The rewrite-backed `chdir` limit from round 0 is unchanged and still
  documented in three places: those runs resume `chdir` into the kernel, so their
  `ProcessContext::cwd` does not follow it.
* `memoria check` still cannot run here — it requires a Git worktree and this jj
  workspace has no `.git`. Unaffected by these fixes; CI runs it from the
  colocated checkout.
* The two review artifacts (`review-correctness.md`, `review-scope.md`) are part
  of this change because the reviewers wrote into the working copy. Left in, per
  this repo's convention — `master` itself carries `impl.md`, `fix-r*.md` and
  `ci-round*.md` at the root.
