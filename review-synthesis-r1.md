# Review synthesis — round 1

Graph `dg-egt6apy1`, node `review_synthesis`, visit 1. Date 2026-09-28.
Inputs: `review-correctness.md` (1003 lines), `review-scope.md` (525 lines).
Tree under review: change **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark
`feat/fork-lifecycle`, parent master `7c3ecc8f`.

> **Identity note, and it changes how everything below should be cited.**
> The commit id `14d16c4f` that earlier reports named is a jj *working-copy*
> commit and has re-timestamped repeatedly since — `14d16c4f` → `105023ee` →
> `a13a7cfb` → `9996732f` at the time of writing. The stable handle is the
> **change id `puyyxvmvmnpk`**. `review-scope.md`'s N3 claims
> `jj diff --from 14d16c4f --to @` is empty; re-measured, it is **not** — the
> tree now differs by `review-correctness.md` and `review-scope.md`, because
> both reviewers wrote into the jj working copy. That is consistent with this
> repo's convention (master itself carries `impl.md`, `fix-r*.md`,
> `ci-round*.md`, `publish.md`, `done.md` at the root), so it is not a defect,
> but `publish` must expect the PR to carry the process artifacts.

---

## Verdict: **FINDINGS PRESENT → route to `fix`**

Not a clean pass. One genuine **new regression** (`R1`) in the same defect
family the ratified guardrail exists to protect, five documentation false
invariants, one silent-success seam, and two provenance defects in `impl.md`.

Both reviews independently agree the *mechanism* is sound: P0, P1a, P2 and P3 do
what they claim, the ratified bound held, and all 8 escalations are untouched.
The findings are real but bounded, and none of them argues for redesign.

**What makes this round unusually trustworthy, and worth saying before the
findings:** both reviewers re-qualified live rather than believing `impl.md`,
and each caught something the other could not have. Correctness confirmed
consequences *by execution* (three separate mutations) instead of reasoning
about them. Scope proved the no-test-edit guardrail *structurally* — zero
deletions in both test files, so editing an existing body is impossible — which
is stronger than any spot-check. Correctness also caught **itself** producing a
false pass against a stale binary, and Scope disclosed a flaw that applies to
its own artifact rather than exempting itself. That is the behaviour that makes
the rest of their reporting credible.

---

## Merged findings, ranked

### R1 — MEDIUM · **must fix before publish** · new regression
*(correctness F1; no scope counterpart)*

**A failed `execve` leaves `Session::twin` stale, and the next `fork` points the
child's interposer requirement at the wrong image.**

`native.rs:1936` commits `s.twin = twin` at the syscall **entry**. A successful
`execve` never returns, so the value is consumed correctly at the exec stop
(`:2020`). A **failed** `execve` does return, and `finish_return`'s
`ReturnKind::Exec` arm (`:1807-1809`) — reachable *only* on a failed exec — does
nothing but `continue_run()`. It never restores the previous value. `s.twin` is
then read at `:1782` and handed to `attach_child`, which under P1a now calls
`child.retarget_interposer(&image)` at `:1727`.

**Why this is new:** before P1a the `twin` argument had no bearing on interposer
matching, so a stale value was harmless. P1a made it load-bearing.

**Failure shape:** child's `current` names the wrong image → `image_path` does
not match → `install()` takes `_ => None` → no interposer trap sites
breakpointed, **but the control block is armed because `fork` copied it** →
first routed `open` issues `svc #0x80` with `x16 = 0x554d4252` that no
breakpoint covers → `SIGSYS`, run dies undiagnosed. **This is the #116 defect
shape the ratification record names as the one thing not to reopen.**

**Evidence quality — consequence CONFIRMED by execution, trigger PLAUSIBLE.**
Mutation C handed `attach_child` a mismatched-but-real path and the *existing*
plain-fork case died with `x16 = 0x554d4252` and `x9`/`x10` = `"UMBRARM1"`:
armed block, unbreakpointed trap. What stays reasoned is only that a failed
`execve` is *a way* to reach that state, which is plain from the three cited
lines.

**Ratified fix shape (take the smaller one):** do not commit `s.twin` at the
exec entry — carry the candidate in the `Pending`/`ReturnKind::Exec` value and
assign only at the exec stop, where it is already read. Restoring the previous
value in the `ReturnKind::Exec` arm is the acceptable alternative.

**Regression test is required, not optional.** R1 must ship with a case that
fails before the fix. Note the honest constraint the reviewer recorded: a
resign-succeeds/exec-fails binary could not be manufactured cheaply, so if a
live trigger stays out of reach, a unit-level assertion on the twin's lifecycle
across a failed-exec return is acceptable — but the *absence* of a test is not.

### R2 — LOW-MEDIUM · must fix · **my audit's error, now in the tree**
*(correctness F2)*

Three **new** prose sites assert the refuted pre-fix mechanism — "reads reaching
the host", "the object simply never appeared in the store", "that absence is
what the Rust side reads back" — at `README.md:682`,
`userspace_run.rs:1454`, `umbra-userspace-edges.c:97`. `impl.md`'s own Step 0
contradicts them.

**Provenance is mine.** The audit predicted an unmediated child whose reads
reach the host and whose writes Seatbelt refuses. Measured reality: `install()`
re-plants every `TRACED_STUBS` breakpoint after exec, so the child's `open` *was*
routed and returned a virtual descriptor; only the interposer was un-armed, so
`write` went to libc with a number the kernel does not own → **EBADF**. Mutation
B settled it independently: the client read back `[]` — object **present and
empty** in the export. Nothing read the host; Seatbelt refused nothing.

Correct all three sites to the measured mechanism: *the object is created by the
routed `open` and left empty because the interposed `write` was not armed.*

### R3 — LOW · must fix · false invariant
*(correctness F3)*

`events.rs:1198` still heads the `ChangedCwd` arm *"the one thing that has ever
moved `ProcessContext::cwd`"* — contradicted by `MovedCwd` at `:1264` and by its
own block at `:1218`. Exactly the stale-half-of-a-sentence defect the dispatch
warned about, in the one place a reader has nothing but the comment to check
against.

### R4 — LOW · must fix · false invariant in an *untouched* file
*(correctness F4)*

The interposer C header is stale: the routed-call list omits `chdir`(12)
(`:165`), and `DORMANCY` (`:94`) describes a two-image world that P1a
superseded. **"0 changed lines in `umbra_interpose.c`" is exactly how this went
unnoticed** — and both reviews cited that zero as evidence of scope compliance.
It is: the *code* is untouched. But an untouched file is not an unaffected file
when its prose documents behaviour that moved. Worth carrying as a lesson.

### R5 — LOW · should fix · silent-success seam
*(correctness F5)*

`Chdir`'s `None` arm in `record_routed_effect` is a silent-success seam across
the provider trait: `Opened` has a cross-check, `MovedCwd` has none. A provider
that reports success without supplying a path leaves the logical cwd unmoved
while the tracee believes it moved.

### R6 — LOW · fix in place · provenance
*(scope S-F1 + S-F2, merged — same root cause)*

Two `impl.md` provenance defects, and they are the same mistake twice:
* **"19 PASS lines" is wrong; the actual count is 20.** The collapsed
  `/usr/bin/touch` notation merged two distinct cases (`touch touched`,
  `touch seed.txt`). Arithmetic traced exactly: 16 enumerated + 3 unenumerated
  crash lines = the reported 19.
* **`impl.md` cites change `fff2bae0`, which is not the reviewed tree** (differs
  by 7 comment-only lines of `userspace_run.rs`, plus `impl.md` itself).

Substance is untouched — Scope re-ran everything on the reviewed tree and every
other figure reconciled, much of it exactly (832 passed / 3 ignored / 52 suites;
all four lesson-23 qualification lines 0/11/10/4 at 0.00s; 11 CAPTURED with
identical case names; both NFS SKIPs verbatim; routed 26 passed, 20 live, 6
probe SKIPs). But this is precisely the *"counted rather than read"* class the
document claims to avoid, and the provenance line must name the tree the numbers
came from. Given N3, `impl.md` should cite the **change id**, not a working-copy
commit id.

### R7 — INFO · optional
*(correctness F6/F7)*

Only the absolute-operand `chdir` is tested; the relative / `ENOENT` / `ENOTDIR`
paths were read and are correct, just uncovered. `rollbackchild`'s comment
credits the journal assertion with a claim the shadow-path assertion actually
carries. Cheap to close while R1-R6 are open; not a blocker.

---

## Deferred to `merge_gate` — the human's call, not `fix`'s

### D1 — `rollbackchild` narrowing, **with a converse assertion**
Both reviews reach the same place from different directions, and Scope's framing
is the sharper one, so it governs.

The ratified fixture shape said *"parent rolls back, child's writes are gone."*
That turned out not to be expressible, and **both reviewers verified the
inexpressibility independently** rather than accepting it: `umbra stop` and
`checkpoint` are `not_implemented`; the only `abort` is per-operation from
`OperationOutcome::Failure` and is unreachable on a routed run because `Deny` is
answered before an `OperationId` is minted; and `abort` explicitly disclaims
undoing writes.

So the substitution is sound engineering and is **declared** in both `impl.md`
and the test's own doc comment. Two things keep it from being `fix`'s to absorb:

1. **In one particular the shipped test asserts the converse of the ratified
   text** — it confirms the child's object *is* in the shadow, where the
   ratified wording says those writes are *gone*.
2. It is a **narrowing**, so the FOLD-IN POLICY's expansion trigger is not met —
   which is exactly why it could pass unnoticed.

Scope names the failure mode precisely: *absorbing it silently because it is
well-argued.* I agree. The ratifier set the fixture shape; only the ratifier can
change it. **Carried to `merge_gate` as a decision, not a note.**

### D2 — the literal-versus-purposive reading of the contract bound
*(scope N1)*

The bound was ratified as "a `chdir`(12) `TRACED_STUBS` row + one additive
`RoutedEffect` variant, and nothing else". The implementation also adds
`routed_cwd()` to the overlay (`engine.rs` +87, `lib.rs` +24). Scope adjudicated
this **required, not creep**, and the reasoning holds: the ratified P0 text
itself says the effect "takes the absolute logical path from the resolved
operand", and only `resolve()` produces that; both files are 0-deletion; and
`routed_cwd()` is the third member of a family (`routed_descriptor`,
`routed_stat`) that already existed at master.

I accept that judgement and am **not** sending it to `fix`. But the bound was
read purposively rather than literally, and that is the human's to bless at
`merge_gate` rather than mine to wave through.

### D3 — `forkexec`'s discriminator is bytes, not entry names
The ratification required mutation verification *on entry names*. Re-derived
rather than trusted: `chdirchild` genuinely discriminates on the entry name
(failure is a missing entry at `userspace_run.rs:1562`). For `forkexec` the
discriminator is **bytes** — the object is present and empty — so the ratified
"entry names" wording is satisfied literally only by `chdirchild`. The
verification is real and arguably stronger for this mechanism; the wording is
what does not fit. Human should know at `merge_gate`.

---

## Accepted as sound — recorded so `fix` does not churn them

* **Ratified guardrail intact.** `self.parent.is_some()` — one site,
  byte-identical to master, verified by both reviewers and by me. P1a *never
  needed* to loosen it: a fork+exec child reaches the fresh-image branch on its
  own, because the exec clears the image list that guard reads.
* **Arming states disjoint.** `arm_interposer` is reachable only with a
  demonstrably-unarmed block; no overlap with the inherited-arming path.
* **Lesson 20 (parallel admission) clear in code.** `intercept()` exhaustiveness
  verified by reading; `path_operands`' exclusion of 12 shown *unreachable*
  rather than merely documented, traced on both run kinds. The only stale list
  is prose — R4.
* **All 8 escalations untouched**, each checked against `git show master:`:
  `single_thread()` and `wait_plan()` byte-identical; no `vfork` in `abi.rs`;
  spawn refusal intact; `WaitPlan::Unsupported` 4/4; the five ENOTSUP path
  symbols unchanged; **zero** fd-disjointness assertions.
* **No unratified test edits, proven structurally:** 4 added `#[test]` attrs
  across the whole diff, 0 removed, and zero deletions in both test files.
  Edges-fixture exit codes 70-82 byte-identical, 83/84 appended, dispatch order
  preserved.
* **All four new cases executed live** against Ganesha — none skipped. Routed
  suite 26 passed / 0 failed; the six SKIPs are all declared mutation probes.
* **Bonus fix nobody asked for:** P1a also repairs `fork` *after* a successful
  exec, where the old code still named the launch target and left the whole
  subtree half-mediated.

## New lessons this round earned

* **(24) A test run that does not rebuild the binary under test can report a
  false pass.** `cargo test -p umbra-storage-nfs-userspace` does not rebuild
  `umbra-cli`; correctness's first mutation run passed against a stale
  `target/debug/umbra` and it caught itself. CI is immune because it runs
  `cargo build --workspace --bins` first. Same family as lesson 23: a green
  result from a tree that is not the one under test.
* **(25) An untouched file is not an unaffected file.** `umbra_interpose.c` has
  0 changed lines and both reviews correctly cited that as scope compliance —
  while its header prose went stale (R4). Zero-diff proves the code did not
  move, never that the documentation still holds.
* **(26) In jj, pin a reviewed tree by change id, not commit id.** A
  working-copy commit re-timestamps under you; `14d16c4f` moved three times
  mid-review and is now hidden. Quantitative self-reports must cite the change
  id.

## Instruction to `fix`

Fix **R1 through R6** in the existing `impl` session (`reuse=impl`, worker
`s-0gaqvxvw91`, which still holds the full implementation context). R7 is
optional and cheap. Do **not** touch D1, D2 or D3 — they are the human's at
`merge_gate`, and pre-empting D1 in particular would spend the ratifier's
decision for them.

R1 is the only one that can hurt a user: ship it with a test that fails before
the fix. Re-qualify on the live Ganesha fixture with
`UMBRA_INTEGRATION_REQUIRED=1`, build `--workspace --bins` first per lesson 24,
and cite the **change id** for every figure per lesson 26.
