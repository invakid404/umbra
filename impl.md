# Implementation — Fork lifecycle over the umbra supervisor (#117 + #121's escalation)

Graph `dg-egt6apy1`, node `implement`, visit 1. Date 2026-09-28.
Baseline: `master` `7c3ecc8f`. Change: **`puyyxvmvmnpkwlmnnusmrqrvqkzsypnz`**,
bookmark `feat/fork-lifecycle`.
Contract: the RATIFICATION RECORD in `design-gate.md`.

> **Provenance, corrected in round 1 (R6).** This document originally cited
> `fff2bae0`, a jj **working-copy commit id**, which was not the tree the figures
> below were measured on and has since re-timestamped several times
> (`fff2bae0` → `14d16c4f` → `105023ee` → … ) and is now hidden. A working-copy
> commit is not a stable handle. The **change id** above is, and it is what every
> figure in this document is measured against. See `fix-r1.md` for the round-1
> remediation and its own re-qualified figures.

---

## Step 0 — the live fixture, and the failure reproduced before any fix

This was baked into scope and it was done first. No fix code was written until
the failure below had been observed live.

**The fixture.** `docker compose -f experiments/nfs-raw/docker-compose.yml up -d
--build --wait --wait-timeout 180` brought up `umbra-m1-transport-raw-ganesha`,
healthy, on `127.0.0.1:12105`:

```
CONTAINER ID   IMAGE                         STATUS                    PORTS                       NAMES
3e4d9b7894f6   umbra-nfs-ganesha-raw:local   Up 13 seconds (healthy)   127.0.0.1:12105->2049/tcp   umbra-m1-transport-raw-ganesha
```

The pinned libnfs source (`18c5c73e`, matching `libnfs.pin`) was fetched into
`third_party/libnfs`, because `transport-raw` — the feature the client read-back
needs — does not build without it.

**The failure, verbatim.** The new `forkexec` fixture case and its driver were
written first and run against that fixture on the **unmodified** tree:

```
thread 'a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image' (37926858) panicked at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:1475:5:
assertion `left == right` failed: a forked child's exec'd image did not complete a routed write:
umbra: run 98db47c6-8531-4425-9634-844c6f306fdc prepared
umbra: run 98db47c6-8531-4425-9634-844c6f306fdc finished: Some(Code(9)) after 2 process exits
ProcessFailed during run.child: supervised command finished with Code(9) (errno: None)

  left: 9
 right: 0
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 22 filtered out; finished in 5.91s
```

**The exit code is 9, `EBADF`, and it is a sharper diagnosis than the audit
predicted.** The audit expected the exec'd child's write to fail closed against
Seatbelt — `EPERM`. What actually happens is more specific, and it is worth
recording because it says exactly which half of the mediation survived the exec
and which did not:

* `install()` **does** re-run after the exec and **does** re-plant every
  `TRACED_STUBS` breakpoint, because that loop is unconditional. So the exec'd
  child's `open` still trapped at the libc stub, was routed, and was answered
  with a **virtual descriptor** — a number above the fence.
* The interposer was **not** re-armed, because the image was not the launch
  target. `write` and `close` reach umbra only through the interposer, and an
  inert interposer passes them to libc unchanged.
* So the child wrote to a virtual descriptor number through the kernel, which
  does not own it: `EBADF`.

That is a genuinely worse shape than "fails closed", and it is only visible
end-to-end. It is the concrete reason P1 must not have shipped against a test
that never ran.

**Round 1 note (R2).** This paragraph was right and three *other* new prose
sites were wrong, having repeated the audit's predicted mechanism -- "reads
reach the host, the write fails closed, the object never appears" -- instead of
this measured one. A review mutation settled it independently by reading `[]`
back through the client: the object is **present and empty**. All three sites
are corrected; see `fix-r1.md` R2.

The same run reproduced **P0's** failure too, once the `chdirchild` case existed:

```
thread 'a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against' panicked at crates/umbra-storage-nfs-userspace/tests/userspace_run.rs:1546:5:
assertion `left == right` failed: the exec'd child's chdir-then-relative-write failed:
umbra: run 551239f6-25fe-432b-8a2d-811ed671328a prepared
umbra: run 551239f6-25fe-432b-8a2d-811ed671328a finished: Some(Code(2)) after 2 process exits
  left: 2
 right: 0
```

Exit 2 is `ENOENT`, and it is the unintercepted `chdir` reaching the kernel: the
directory the case creates exists only in the run's shadow, so the *host* has no
such path and the host `chdir` fails. Precisely the split anchor (I) describes.

**Pre-fix state of all four new cases**, on the unmodified tree against the live
fixture:

| Case | Pre-fix | Meaning |
|---|---|---|
| `forkexec` | **FAIL** — child exit 9 (`EBADF`) | the (C′) exec gap |
| `chdirchild` | **FAIL** — child exit 2 (`ENOENT`) | the (I) `chdir` gap |
| `grandchild` | pass | new coverage of a working mechanism, as #117 asks |
| `rollbackchild` | pass | new coverage of an existing invariant, as (H) says |

The two that passed pre-fix are stated as such rather than presented as fixes.
They are coverage, not repairs, and the audit said so in advance.

---

## What changed

### P0 — route `chdir`(12)

**Measured first.** The audit's claim that `_chdir` needs no new mechanism was
re-verified on this host before the row was written, by reading the stub exactly
as `install()`'s verifier reads it (`dlsym(RTLD_DEFAULT, "chdir")`, then the last
`movz x16` before the `svc`):

```
chdir -> chdir in /usr/lib/system/libsystem_kernel.dylib
  +00  d2800190   <-- movz x16, #12
  +04  d4001001   <-- svc #0x80
```

One `movz x16, #12` immediately before the `svc`. The existing machinery plants
and verifies it unchanged.

* `crates/umbra-platform-macos/src/abi.rs` — the row `("chdir", 12,
  Delivery::Namespace)`, beside `fchdir`'s, and the decoder arm `12 =>
  FsOp::Chdir { dir: DirRef::Cwd, path: read_path(..., x0) }` modelled on bare
  `mkdir`(136), which is the other no-dirfd path call.
* `crates/umbra-overlay/src/engine.rs` — `resolve_routed_chdir`, the sibling of
  `resolve_routed_fchdir`. It differs in one way and the difference is forced:
  `fchdir` is handed a descriptor whose binding already carries a logical path,
  while `chdir` is handed a path operand that has to go through the same
  resolution every other path operand does — symlink expansion, whiteout
  traversal, the logical-root containment check. Two refusals, both answered *to
  the tracee* so the run survives: `ENOENT` through `hidden_or` for a name that
  does not resolve, `ENOTDIR` (20) for one that resolves to a non-directory.
* `crates/umbra-overlay/src/lib.rs` — `routed_cwd()`, the third member of the
  `routed_descriptor` / `routed_stat` family, defaulted to a refusal exactly as
  they are. The resolved absolute logical path has to leave the namespace beside
  the action, because the working directory belongs to the caller's
  `ProcessContext` while *which* directory the operand names is the namespace's
  answer.
* `crates/umbra-supervisor/src/lib.rs` — **one additive `RoutedEffect` variant**,
  `MovedCwd(BytePath)`. It carries the path rather than a descriptor, which is
  the one way it differs from `ChangedCwd`: `fchdir` names something already in
  `ProcessContext::fds`, so the path can be read at the moment the move is
  applied; `chdir` binds nothing, so there is no later place to read it from.
* `crates/umbra-supervisor/src/events.rs` — recording and applying it, with the
  same absolute-path check `ChangedCwd` makes and for the same reason.

**`intercept()` was not touched**, and that is the point of one-table-two-gates.
It is exhaustive over `abi::Delivery`, so the new row is routed without a second
admission edit — which is exactly the #116 `admit_run` drift this structure
exists to prevent. Both `TRACED_STUBS` invariant tests
(`every_traced_stub_is_classified_by_the_decoder`,
`every_traced_stub_carries_the_delivery_its_number_implies`) iterate the table,
so they cover the new row without being edited.

**A rewrite-backed run resumes `chdir` into the kernel, unchanged.** The row
breakpoints `chdir` on *every* registry, so a gate was needed. The four
descriptor-relative calls gate on `umbra_owns_descriptor`; `chdir` names a path
and binds nothing, so the equivalent question is whether the run routes at all,
and the same `descriptor_floor` answers it — `Some` exactly for a routed run, by
`RunBudget`'s own definition. Emulating it on a rewrite-backed run would be
strictly worse: umbra would report success while the kernel's working directory
stayed put, and every relative path handed to a call umbra does *not* intercept
would resolve against the old one.

**The limit that leaves is stated, not hidden.** A rewrite-backed run's
`ProcessContext::cwd` still does not follow the tracee's `chdir`. Closing that
needs a second mechanism — `RoutedEffect` is recorded only for a routed run, by
construction, which is why "one additive `RoutedEffect` variant" bounds this
slice to the routed path — so it is out of this slice rather than half-done
inside it. It is said in the code comment, in the README row, and here.

**`getcwd`(326) is untouched and stays inert**, by ratified decision. The comment
at `events.rs` that used to say "`Chdir` and `GetCwd` stay declared and inert"
was rewritten rather than left to rot: it now says `Chdir` is routed, why, and
that `GetCwd` is unserved rather than in agreement with `ProcessContext::cwd`.

### P1a — the interposer follows the exec

`install()` gated on `image_path(pid) == target`, where `target` was assigned
once in `launch_traced` and never reassigned. The **comparison is unchanged**;
what changed is that its right-hand side now means "the image this session is now
running" and is maintained per session.

* `Session::interposer`'s second element is redocumented from "target image" to
  "current image", with the gap it used to cause spelled out.
* `Session::retarget_interposer(&Path)` canonicalizes an image and points the
  requirement at it. Called from exactly two places:
  * the **exec stop**, before `s.install()`, from `s.twin` — which `intercept`
    already set to the resigned image the tracee is execing. The ordering is
    forced: after `install()`, `install()` would already have decided from the
    previous image's name that this run routes nothing here.
  * **`attach_child`**, from the `twin` it is handed — the parent's image for a
    `fork`, the spawned twin for a `posix_spawn`. This is what also closes the
    spawn half, which had the same defect.

**It is deliberately *not* called for a freshly attached root, and that is what
keeps the sandbox installer inert.** The installer is a `sandbox-exec` twin with
the interposer loaded into it by `DYLD_INSERT_LIBRARIES`, stopped and installed
before dyld has mapped anything at all. Retargeting it to its own image would
make `install()` match, look for a library that is not mapped yet, and fail the
launch inside trusted bootstrap code. Its copy stays dormant because nothing arms
it — the mechanism since round 1 — and that is unchanged. Verified by the
`sandbox_launch` suite passing with real work (4 cases, 8.18 s; see gates).

### P2 — four fixture cases

In `experiments/fixtures/umbra-userspace-edges.c` (new cases only; no existing
case body edited) with drivers in
`crates/umbra-storage-nfs-userspace/tests/userspace_run.rs` (**new `#[test] fn`
bodies only**; no existing test fn edited). Every one asserts **through the NFSv4
client** via `read_through_client`, never on the tracee's exit status.

* **`forkexec`** — fork, exec a different binary, which writes `execchild`.
  P1's regression test. The helper is the edge fixture copied to a **different
  basename**, and that detail is load-bearing: a twin is cached at `<sha256 of
  contents>/<basename>`, so a copy with the *same* basename resigns to the same
  twin path and would have been armed for the wrong reason, reporting a pass
  against the defect it exists to catch. The driver's doc comment says so.
* **`chdirchild`** — `mkdir` through routing, fork, exec, `chdir`, **relative**
  write. P0's regression test, asserted on **entry names in both directions**:
  the bytes are at `out.txt.d/leaf.txt`, and `leaf.txt` beside the workspace root
  **does not exist**. The second half is what makes it discriminating — a wrong
  anchor does not lose a write, it puts it somewhere else, so asserting only the
  first would pass against an implementation that wrote to both. The directory is
  created through routing so it lives only in the shadow; there is deliberately
  nothing on the host for a stray `chdir` to land in.
* **`grandchild`** — two generations, three separate objects, all read back.
* **`rollbackchild`** — see the deviation recorded below.

`assert_host_write_root_empty` is asserted by every new case, and
`run.destination.exists()` is refused where the case has a host destination.

### P3 — the two documentation defects

* `crates/umbra-storage-nfs-userspace/README.md` — the `exec` row said "Not
  claimed … an exec'd image routes nothing" while the forked-child row said the
  new image is "loaded and armed from scratch". Both now state the post-P1a
  behaviour, each naming the tests that cover it, and the forked-child row now
  says explicitly that `fork` and `exec` are *opposites* — one inherits an armed
  block and must not be re-armed, the other gets a zeroed one and must be — and
  that the `parent` test in `install()` is what keeps them apart. The
  `chdir`/`getcwd` row was split in two, because after P0 the two calls no longer
  share a verdict.
* `crates/umbra-core/src/lib.rs` — `LaunchPolicy::descriptor_limit` no longer
  names `fstat` among the calls receiving `EBADF` on a virtual descriptor. The
  correction also fixes the *reason* the list was wrong: it said "everything the
  interposer does not implement", and the interposer's set stopped being umbra's
  set once calls were routed at the libc stub. It now points at
  `abi::TRACED_STUBS` as authoritative rather than restating membership that can
  rot again.

---

## Deviation from the ratified wording — `rollbackchild`

Ratified as "parent rolls back, child's writes are gone". **That literal
assertion is not expressible against the shipped surface, and asserting it would
pin a behaviour this codebase does not have.**

What I found: there is no run-level rollback. `umbra stop` and `umbra checkpoint`
are `not_implemented`. The only rollback is per-operation `abort`, reached from
`OperationOutcome::Failure`, which on a *routed* run is unreachable from a
fixture — every routed operation is emulated to `Success`, and a `Deny` is
answered before `prepare` and allocates no operation slot. And `abort` is
explicitly documented as reconcile-or-abandon that **does not claim arbitrary
writes were undone**. A failed run does not delete shadow objects.

So I implemented the **structural half**, which is what audit (H) and (G)
actually claim and which *is* observable: there is one `NamespaceSession` per
**run**, not per process, and `track_process` clones the parent's context into
the child rather than opening a transaction of its own, so no per-process
transaction exists that could commit independently. The case has the child write
through routing, the parent reap it and then issue an `O_APPEND` routed open —
refused as unsupported, which stops the run — and the driver asserts:

1. `umbra` reports failure (`run.status != Some(0)`);
2. the child's object **is** in the parent's run shadow, read back through the
   client (the premise the next assertion depends on, measured rather than
   assumed);
3. the run carries **no `RunCompleted` record**, read through the real
   `FileJournal` by the existing `journal_records_completion` helper.

The test's own doc comment states this bound in full, so a reader meets it at the
test rather than only here. **This is a narrowing of a ratified item and is
flagged for the review nodes rather than absorbed silently.** If the intended
assertion was the literal one, it needs a run-level rollback surface first, which
is a different slice.

---

## How each named mechanism was preserved

* **`native.rs` `if self.parent.is_some()` — the human's hard guardrail.**
  **Not loosened.** It is byte-identical to `master` and now sits at
  `native.rs:585`; `grep -n 'self.parent.is_some()'` returns exactly that one
  site. Nothing in this change touches the condition, its body, or the
  `image_base` / `wait_for_image` split it selects between. It could not have
  needed loosening: a forked child that *then* execs reaches the fresh-image
  branch on its own, because the exec cleared the image list that guard reads, so
  `image_base` answers `None` and the existing `None => wait_for_image` arm does
  the work. Both directions are proven live — `forkexec` passes (the exec'd child
  is armed) **and** `a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes`
  still passes (the plain forked child is *not* re-armed and is not resumed to
  `main`).
* **Arming-after-breakpoints ordering.** Untouched. `arm_interposer` is still the
  last call in `install()`, after `install_image(main)` and
  `install_image(base)`. Retargeting happens strictly *before* `install()` runs,
  so it changes which image is armed, never when.
* **No constructor in the interposer.** `umbra_interpose.c` is not modified at
  all in this change.
* **`TRANSIENT_SIGNALS`, `SIGCHLD` in and `SIGSYS` out.** Untouched;
  `the_gate_absorbs_sigchld_and_never_absorbs_sigsys` passes in the
  `umbra-platform-macos` unit suite (32 passed).
* **The descriptor fence and its Rust/C twin predicate.** Untouched.
  `the_interposers_descriptor_test_is_the_one_the_supervisor_applies` passes.
  The new `chdir` gate is deliberately a *separate* test rather than an extension
  of the four-call `umbra_owns_descriptor` block, because `chdir` has no
  descriptor to ask about — merging them would have made the twin predicate mean
  two things.
* **One-table-two-gates over `abi::Delivery`.** Preserved and relied upon:
  `intercept()` has no edit, and both table-iterating invariant tests cover the
  new row without modification.
* **No wire-format change.** `UMBRA_TRAP_NUMBER` and the 16-byte arm-block layout
  are untouched. No new user setup, mounts, drivers or privileged steps: the
  fixture is CI's existing compose file and the helper binary is a copy the test
  harness makes for itself.

---

## Gates, and the evidence behind the figures

Lesson 23 applied throughout: **no figure below is read from an exit status or a
case count.** Where a suite's own verdict lines exist, they are what was read.

### `cargo fmt --check`

Clean, exit 0, no diff output.

### `cargo clippy --workspace --all-targets -- -D warnings`

Clean: `cargo clippy: No issues found`, exit 0.

### `cargo test --workspace --all-targets`

`832 passed, 3 ignored (52 suites, 31.68s)` — **and that figure is not
qualification, which is the whole of lesson 23.** Reading the per-suite lines
shows what the green actually covered:

```
Running tests/userspace_run.rs   test result: ok. 0 passed; 0 failed; ... finished in 0.00s
Running tests/fixtures.rs        test result: ok. 11 passed; 0 failed; ... finished in 0.00s
Running tests/run_fixtures.rs    test result: ok. 10 passed; 0 failed; ... finished in 0.00s
Running tests/sandbox_launch.rs  test result: ok. 4 passed; 0 failed; ... finished in 0.00s
```

The routed suite ran **zero** cases (cfg'd out without `transport-raw`), and
every live suite took its `input()` skip — **11 cases in 0.00 s is exactly the
shape lesson 23 names**. So each was re-run with its inputs provided and
`UMBRA_INTEGRATION_REQUIRED=1`, and the verdict lines read.

### Enforced macOS fixture qualification — `CAPTURED` verdicts read

`cargo test -p umbra-platform-macos --test sandbox_launch --test provider_ipc
--test fixtures`, with `UMBRA_TEST_FIXTURE_PATH` / `UMBRA_TEST_REDIRECT_ROOT`
built as CI builds them and `UMBRA_INTEGRATION_REQUIRED=1`:

```
CAPTURED argv0-check          CAPTURED open-libc
CAPTURED dirfd-rename         CAPTURED open-svc
CAPTURED dup-inherit-write    CAPTURED posix-spawn-write
CAPTURED exec-write           CAPTURED symlink-cycle
CAPTURED fork-write           CAPTURED wnohang-wait
CAPTURED grandchild-write
test result: ok. 11 passed; 0 failed; ... finished in 10.85s      (was 11 in 0.00s)
CAPTURED open-libc provider IPC
test result: ok. 1 passed;  0 failed; ... finished in 1.49s
test result: ok. 4 passed;  0 failed; ... finished in 8.18s       (sandbox_launch)
```

**Eleven `CAPTURED` lines, one per case, in 10.85 s.** These are the figures, and
`exec-write`, `fork-write`, `grandchild-write`, `posix-spawn-write` and
`dup-inherit-write` among them are the direct regression evidence that P1a's
retargeting did not disturb the rewrite-backed fork/exec lifecycle. The
`sandbox_launch` suite doing 8.18 s of real work is the evidence that the sandbox
installer still hands off correctly — the one thing retargeting could have
broken.

### CLI run-fixture matrix — per-case `PASS` lines read

`cargo test -p umbra-cli --test run_fixtures --test resume_cli`, same environment:

```
PASS local open-libc / open-svc / fork-write / posix-spawn-write / exec-write
PASS local grandchild-write: 11 exact bytes, host absent, lease released, RunCompleted, elapsed=2.507s
PASS local dup-inherit-write: 4 exact bytes, host absent, lease released, RunCompleted, elapsed=2.170s
PASS local /usr/bin/touch touched / touch seed.txt / /bin/mkdir made / /bin/rm seed.txt
PASS local /bin/cat seed.txt / /bin/cat absent.txt
PASS local /bin/ls <workspace> / absent / -l / -t: host untouched, lease released, RunCompleted
SKIP nfs_fixture_matrix: UMBRA_TEST_SKIP_NFS_MATRIX set (real NFSv4 mount unavailable in this environment)
SKIP nfs_utility_matrix: UMBRA_TEST_SKIP_NFS_MATRIX set (real NFSv4 mount unavailable in this environment)
test result: ok. 10 passed; 0 failed; ... finished in 44.77s      (was 10 in 0.00s)
resume_cli: 3 passed, 0.79s;  umbra-supervisor reopen: 7 passed, 1.34s
```

**20 `PASS` lines in 44.77 s, and 2 declared `SKIP`s** — both the NFS-mount
matrices, skipped by `UMBRA_TEST_SKIP_NFS_MATRIX`, which is the same opt-out CI's
`native-qualification` job sets and for the same documented TCC reason. Those two
are the only unqualified cases in this gate and they are named rather than
counted as passes. The `/bin/ls`, `ls -l` and `ls -t` lines are #121's directory
work, still green.

### Routed suite over the live NFSv4 client

`cargo test -p umbra-storage-nfs-userspace --features transport-raw --test
userspace_run`, with `UMBRA_NFS_RAW_FIXTURE=127.0.0.1:12105` and
`UMBRA_INTEGRATION_REQUIRED=1` so a missing fixture is a hard failure, not a skip:

```
test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 74.29s
```

**20 cases actually executed against the live fixture in 74.29 s**, and **6
declared `SKIP`s**, all of them the mutation probes, each naming the cargo
feature its binary would have to be built with:

```
SKIP: set UMBRA_MUTATION_PROBE=fstat / mkdir / read / readdir / setattrlistat / write
```

Those six are skip-by-design in a baseline build — no two probe features may be
enabled at once — and `every_mutation_probe_is_wired_into_the_userspace_job`
**passed**, which is what pins that CI runs all six. The four new cases are among
the 20 that ran:

```
a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image ... ok
a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against ... ok
a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it ... ok
a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence ... ok
```

and so are the two pre-existing fork cases this change most risked:

```
a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes ... ok
a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store ... ok
```

### Mutation-probe machinery, spot-checked

To confirm the probe matrix still discriminates after this change, probe A was
built and run: `cargo build -p umbra-cli --features mutation-probe-read`, then
the suite with `UMBRA_MUTATION_PROBE=read`:

```
test mutation_probe_read_makes_the_toy_reject_the_bytes_it_read ... ok
test result: ok. 26 passed; 0 failed; ... finished in 3.15s
```

The unmutated binaries were then rebuilt and the baseline suite re-run to prove
the tree was left unmutated.

**No new cargo-feature probe was added**, and that is a deliberate reading of the
contract bound ("chdir(12) row + one additive `RoutedEffect` variant, and nothing
else"; "no new user setup"). Two new features would have needed two new CI
invocations. The ratified requirement — "mutation-verified on **entry names**" —
is met by the assertion shape instead: `chdirchild` asserts the entry name in
both directions, so a broken cwd propagation is caught by a wrong *name* rather
than a nonzero exit, and the pre-fix failures recorded in Step 0 are the
unmutated-tree evidence that both new assertions actually discriminate.

---

## What was left out, and why

Everything on the ratified escalation list, untouched:

1. **Multithreaded fork.** `single_thread()` (`native.rs:1788`, `:458-464`) is
   byte-identical to `master` and still refuses a multithreaded tracee's fork
   with `fork/deferred wait requires one thread`. Deferred by ratification and
   scheduled as the immediate next arc. **Completing P1 does not by itself make
   real agents work**, and this change does not claim otherwise.
2. `vfork`(66) — measured never called; `___vfork` has no `svc` at all.
3. `posix_spawn` with non-null file actions / attributes — still refused. Named
   in the README's new `exec` row as explicitly not claimed.
4. Process-group / `WUNTRACED` / `WCONTINUED` waits.
5. Widening the `ENOTSUP` path-operation set.
6. Cross-host resume, `StatEncoder` consolidation.
7. **#117 item 3, as narrowed and ratified:** no per-descendant fd-disjointness
   assertions were written. One enforcement point — the hard-lowered
   `RLIMIT_NOFILE` at `native.rs:1414-1423` — and `forkfd` already measures
   inheritance-with-offset. To be recorded on #117 at merge.

Two further things a reviewer should know:

* **`memoria check` could not run here.** It requires a Git worktree and this jj
  workspace has no `.git`; it fails `git_unavailable` before inspecting
  anything. It is unaffected by the change and CI runs it from the colocated
  checkout.
* **The rewrite-backed `chdir` limit** described under P0 is a real, remaining
  gap. It is documented in three places rather than left for a reader to
  discover, and closing it needs a mechanism outside this slice's contract bound.
