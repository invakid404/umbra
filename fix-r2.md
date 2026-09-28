# Fix — review round 2

Graph `dg-egt6apy1`, node `fix`, visit 2. Date 2026-09-28.
Input: `review-synthesis-r2.md` (from round-2 `review-correctness.md` +
`review-scope.md`).
Change: **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**, bookmark `feat/fork-lifecycle`,
parent `master` `7c3ecc8f`.

> Per lesson 26, the change id is the only handle cited. The working-copy commit
> has now re-timestamped eight times (`14d16c4f` → … → `b4767294` → `cd4c61ea`),
> which is the whole argument for not naming one. The existing change was
> **amended**, not replaced.

Short final pass: three items, no new mechanism, one assertion.

---

## Disposition

| | Finding | Disposition |
|---|---|---|
| **S1** | R1's fix moved the exec candidate into the clobberable `Pending` slot | **Fixed** — `debug_assert!` in `return_stop`; shape and sufficiency argued below |
| **S2** | `edges.c` case comment says `ENOEXEC`; header and driver say `EACCES(13)` | **Fixed** |
| **S3** | `forkexec` prose says "never on the tracee's exit status"; the driver asserts it | **Fixed** (prose, not the assertion) |
| **D1–D4** | Deferred to `merge_gate` | **Untouched** |

---

## S1 — which shape, and why an assertion is sufficient

### The choice

The synthesis offered two shapes: a dedicated `exec_candidate: Option<PathBuf>`
field, or `debug_assert!(self.pending.is_none())` in `return_stop`. I took the
**assertion**, and the reason came out of reading `return_stop` rather than from
a preference:

```rust
let entry_breakpoint = self.remove_breakpoint(pc)?;   // site released with z0
...
self.temporary_breakpoint(gate)?;                     // new gate planted
self.pending = Some(Pending { … });                   // unconditional
```

A second `return_stop` before the first is consumed loses **three** things, and
the candidate is the least of them:

* `entry_breakpoint` — the site was already released with `z0` a few lines up,
  and `finish_return` is what hands the value back to `install_breakpoint`.
  Losing it means that stub is **never re-armed** and stops being intercepted
  for the rest of the run.
* `gate` — the first return gate stays registered and is never retired.
* the `Exec` candidate — S1's symptom.

So a dedicated field would rescue the candidate **while the breakpoint state
around it is already corrupt**. That is not a fix; it is removing the one
symptom that is currently visible from a run that is broken anyway, which makes
the remaining corruption harder to diagnose, not easier. The reviewer's own note
says as much — *"a run that reaches this state was already broken by other
means"* — and I read that as an argument against the field rather than for it.

The assertion instead names the real precondition at the one site that can
violate it, and it names it for all three consumers rather than just the new one.

### Why a debug-only check is sufficient here

Four reasons, and they are specific to this invariant rather than general
excuses:

1. **It is not a new invariant.** The single `Pending` slot has required
   one-in-flight-per-session since before this change; R1 added a third rider to
   a slot that already had two. A release-mode guard would be new behaviour on a
   shipped path, which is more than "cheap hardening" and is the kind of change
   that turns a limping case into a stopped run.
2. **The only route to violating it is a multithreaded tracee** — `continue_run`
   sends `c`, which resumes every thread, and `single_thread()` gates
   `Delivery::Fork` and `WaitPlan::Park` but **not** `Delivery::Exec` or a plain
   `Namespace` call (verified: exactly two call sites, both checked). That is the
   ratified, explicitly-scheduled next arc, which will have to revisit this slot
   wholesale. The assertion is precisely the tripwire that arc wants waiting for
   it.
3. **It is live where it matters.** `cargo test` builds with the `test` profile,
   which inherits `dev`; the workspace `Cargo.toml` has **no `[profile]`
   override**, so `debug_assertions` is on for every unit, integration and live
   routed case.
4. **In release it costs nothing and changes nothing**, so it cannot regress a
   shipped path — which is the property that made it safe to add inside a final
   pass.

Explicitly **not** attempted: making the session multithread-safe. That is the
ratified next arc.

### Proving the assertion is not vacuous

An assertion that is never reached, or compiled out, is worth nothing — and
"it's only a debug assert" is exactly the excuse that would let a hollow one
ship. So I checked it the way I would check a test: **negated it** to
`debug_assert!(self.pending.is_some())`, rebuilt, and ran the fixtures suite.

```
a second intercepted syscall entered while one was still in flight: this session's
pending entry breakpoint, return gate and exec candidate would all be overwritten
   … (fires repeatedly)
```

So the assertion is reached, is live in the test profile, and is on a hot path
rather than a dead one. Restored, rebuilt, and **verified by execution** rather
than assumed — 11 `CAPTURED` with none firing.

On the real tree it fires **zero** times across the full workspace suite and all
22 live routed cases, which is also the first evidence anyone has gathered that
the one-in-flight invariant actually holds today.

---

## S2 and S3 — the corrections

**S2.** `case_failedexec`'s comment said the failed exec *"returns ENOEXEC"*,
contradicting the same file's header and `unexecutable_image` (mode `0o644`),
which say `EACCES(13)` and rule the `ENOEXEC`/dylib route out explicitly. The
comment now says `EACCES(13)`, names the actual shape (an arm64 executable with
its execute bits cleared), and points at the header entry for **why not a
dylib** — a dylib does fail `execv` with `ENOEXEC`, but `cache::resign` refuses
it earlier, so nothing is exercised. Both halves of the file now say the same
thing and the near-miss is recorded rather than deleted.

**S3.** The `forkexec` driver's prose claimed it asserts "never on the tracee's
exit status" while its first assertion is `assert_eq!(run.child_exit(), 0, …)`.
The prose was wrong, so the prose moved. It now says what each assertion is
actually worth: the exit status is checked first, is the most legible failure,
and for *this* defect does discriminate (measured: 9/`EBADF` unfixed, 0 fixed) —
but it cannot establish the claim the case is named for, because a tracee's own
exit says nothing about whether bytes reached the store. And the read-back is on
**bytes** rather than presence because the broken tree leaves a name here too,
an empty one.

---

## Lesson 28 — the adversarial re-read, and what it caught

Lesson 28 says the pass that fixes a false invariant is a likely place to
introduce one. It was right, and it caught me inside this pass.

I re-read **every** prose line I touched against the code it describes, rather
than re-reading the edit. Claim by claim:

| Claim | Checked against | Verdict |
|---|---|---|
| S1: `pending` assignment is unconditional | `native.rs` `return_stop` body | holds |
| S1: entry site released before the assignment | `remove_breakpoint` sends `z0`, precedes it | holds |
| S1: `gate` never retired on clobber | `finish_return` is the only retirer | holds |
| S1: `single_thread()` does not gate `Exec`/`Namespace` | grep: exactly 2 call sites, `Delivery::Fork` and `WaitPlan::Park` | holds |
| S1: `continue_run` resumes every thread | sends `c` | holds |
| S1: live in every `cargo test` | no `[profile]` override in workspace `Cargo.toml` | holds |
| S1: *"the first one is simply **leaked**"* | `remove_breakpoint` sends `z0`, so debugserver **does** restore the instruction | **WRONG — corrected** |
| S2: image is an executable with execute bits cleared | `unexecutable_image`, `from_mode(0o644)` | holds |
| S2: `EACCES(13)` / dylib `ENOEXEC(8)` | both measured directly | holds |
| S2: resign refuses the dylib *before* the operand is rewritten | `cache::resign` precedes `set(&mut regs, slot, …)` in the `Exec` arm | holds |
| S3: exit status is the first assertion | driver body, first `assert_eq!` | holds |
| S3: broken tree leaves an empty name | round-1 Mutation B read `[]` | holds |

**The one it caught.** My S1 comment said the first `entry_breakpoint` is
*"simply leaked"*. Reading `remove_breakpoint` rather than assuming, it sends
`z0` — debugserver restores the original instruction, so nothing is leaked in
memory. What is actually lost is the value `install_breakpoint` needs to
**re-arm** the site, so that stub silently stops being intercepted. Corrected to
say that.

That is the same two-halves-disagree shape as R3, R4 and S2 — the fourth
instance in this slice, and this time inside the comment written to harden
against the class. Lesson 28 earned itself again; the re-read is what stopped it
shipping.

---

## Deferred — untouched, and checked rather than asserted

* **D1** `rollbackchild`. This round edited three files: `native.rs` (S1),
  `umbra-userspace-edges.c` (the `case_failedexec` comment only) and
  `userspace_run.rs` (the `forkexec` doc comment only). The `rollbackchild`
  fixture case, its driver and its declaration were not touched. Still the
  ratifier's.
* **D2** overlay `routed_cwd()`. `engine.rs` and `lib.rs` last modified 12:40 and
  12:52, before this round began — **0 files changed**, as in the fix round.
* **D3** `forkexec` discriminating on bytes. Unchanged; S3's correction now
  states *why* bytes are the discriminator, which should inform the decision
  without pre-empting it.
* **D4** `ReturnKind::Exec { twin }`. Not enlarged — S1 deliberately added no
  field and no variant payload. Adjudicated inside the bound; left exactly as it
  was.
* Still untouched: multithreaded fork, `vfork`, `posix_spawn` with non-null file
  actions, process-group waits, widening the `ENOTSUP` set, #117 item-3.

**Standing guardrails re-verified after S1 touched `native.rs`**, by diff against
`git show master:` rather than by inspection:

```
parent guard (self.parent.is_some()): BYTE-IDENTICAL   (1 site)
single_thread():                      BYTE-IDENTICAL
TRANSIENT_SIGNALS:                    BYTE-IDENTICAL
wait_plan():                          BYTE-IDENTICAL
```

---

## Gates — re-qualified on change `puyyxvmvmnpk…`

### Lessons 24 and 27 applied, and 27 reproduced

`cargo build --workspace --bins` is **not** sufficient. Measured this round,
exactly as the reviewer described:

```
after `cargo build --workspace --bins`:                    5,096,720 bytes  (featureless)
after `-p umbra-storage-nfs-userspace --features transport-raw --bins`: 5,888,624 bytes
libnfs symbols in the provider binary: 10041
```

So the provider was rebuilt with `transport-raw` and **checked to be that build**
— by size *and* by symbol count — before every routed run, not merely checked to
exist.

### fmt / clippy / workspace

* `cargo fmt --check` — clean, exit 0.
* `cargo clippy --workspace --all-targets -- -D warnings` — `No issues found`.
* `cargo test --workspace --all-targets` — **832 passed, 0 failed, 3 ignored**.

Unchanged at 832, and the round-2 scope review already resolved why: the six new
cases are integration-gated and report `0 measured` in a default run, so they
contribute zero by construction. The total that *did* move is the routed one
(22 live + 6 probe SKIPs = 28).

Lesson-23 signature, unchanged and still read rather than trusted:

```
tests/run_fixtures.rs   -> 10 passed, 0.00s
tests/fixtures.rs       -> 11 passed, 0.00s
tests/provider_ipc.rs   ->  1 passed, 0.00s
tests/sandbox_launch.rs ->  4 passed, 0.00s
tests/userspace_run.rs  ->  0 passed, 0.00s
```

All five at 0.00 s; the routed suite ran **zero** cases. Hence the qualified runs
below.

### Enforced macOS qualification — 12 `CAPTURED`, read by name

```
CAPTURED argv0-check       CAPTURED open-libc
CAPTURED dirfd-rename      CAPTURED open-svc
CAPTURED dup-inherit-write CAPTURED posix-spawn-write
CAPTURED exec-write        CAPTURED symlink-cycle
CAPTURED fork-write        CAPTURED wnohang-wait
CAPTURED grandchild-write
test result: ok. 11 passed; 0 failed; … finished in 8.32s   (unqualified: 0.00s)
CAPTURED open-libc provider IPC
test result: ok.  1 passed; 0 failed; … finished in 1.16s
test result: ok.  4 passed; 0 failed; … finished in 5.92s   (sandbox_launch)
```

`exec-write`, `fork-write`, `grandchild-write`, `posix-spawn-write` and
`dup-inherit-write` all CAPTURED — the rewrite-backed fork/exec lifecycle is
undisturbed by S1. `sandbox_launch` doing 5.92 s of real work covers the
installer handoff, which runs through the candidate-adoption path.

### CLI run-fixture matrix — counted mechanically

```
PASS: 20 (crash 3 + local 17)
SKIP: 2
test result: ok. 10 passed; 0 failed; … finished in 29.40s   (unqualified: 0.00s)
```

20, matching R6's correction. Both `SKIP`s are the NFS-mount matrices under
`UMBRA_TEST_SKIP_NFS_MATRIX`, the same opt-out CI sets — named, not counted as
passes.

### Routed suite over the live NFSv4 client

Ganesha healthy on `127.0.0.1:12105`; `UMBRA_INTEGRATION_REQUIRED=1`.

```
test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 92.15s
executed ok: 22 | probe SKIPs: 6 | debug_assert fired: 0
```

**All six new/changed cases executed, read by name:**

```
a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran ... ok
a_chdir_answers_enoent_and_enotdir_to_the_tracee_and_anchors_a_relative_operand     ... ok
a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image           ... ok
a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against ... ok
a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it        ... ok
a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence    ... ok
```

**Both at-risk pre-existing fork cases executed:**

```
a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes          ... ok
a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store ... ok
```

The six `SKIP`s are all declared mutation probes (`fstat`, `mkdir`, `read`,
`readdir`, `setattrlistat`, `write`), skip-by-design in a baseline build.

### Mutation restored and verified by execution

The only thing mutated this round was the S1 assertion itself, for the
non-vacuity probe above. It was restored and the restoration **verified by
execution** (11 `CAPTURED`, none firing), then the full routed suite re-run at
the final tree state — the 92.15 s figure above is from that post-restore run,
not the pre-probe one.

---

## Unchanged notes from earlier rounds

* The rewrite-backed `chdir` limit stands and is documented in three places.
* `memoria check` still cannot run here (needs a Git worktree; this jj workspace
  has no `.git`). CI runs it from the colocated checkout.
* The review artifacts remain part of the change, per this repo's root-document
  convention.
