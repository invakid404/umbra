# Implementation — Multithreaded tracee, slice 0 (graph `dg-nsw71bqq`)

Node `implement`, visit 1. Date 2026-09-28.
Baseline: `master` `e44d0db8` (merge commit of PR #129).
Change: **`wppwptxswssr`**, bookmark `feat/mt-fork`.
Contract: the `RATIFICATION` section at the end of
`/tmp/graph-dg-nsw71bqq/design-gate.md`, with
`/tmp/graph-dg-nsw71bqq/audit.md` as its evidence.

> **Pinned by change id, not by commit id.** A jj working-copy commit id
> re-timestamps on every `jj describe` and every snapshot; the change id does
> not. Every figure below was measured against `wppwptxswssr`. Its commit id moved
> four times while round 0 was being written (`55861f2c8a51` → `b06ff325e2a2` →
> `d8b5d690ae68` → `3efced79`), twice more when the correctness reviewer's
> root-level write was snapshotted into it (`220e6ff8` → `6343991c`), and again for
> every round-1 edit. Both round-1 reviews pinned `3efced79`; neither pin is the
> commit you are reading. That is the whole argument, and round 1 demonstrated it
> the hard way. Cite the change id; resolve it with `jj log -r wppwptxswssr`.

---

## 0. Headline: the slice 0 measurement fired the ratified stop condition

Design-gate item 2 was ratified **CONDITIONAL**:

> if the slice 0 measurement shows live corruption on master, split slice 0 into
> its own PR **ahead of** feature work. *The measurement decides.*

It showed live corruption on a shipped path. **So this PR is slice 0 alone.
Slice 1 is not implemented, and no production file is touched** — `native.rs`,
`abi.rs`, `rsp.rs`, `umbra_interpose.c`, `journal.rs` and
`umbra-cli/tests/run_fixtures.rs` are byte-identical to master.

**Exactly four content files are touched, and they are these:**

```
crates/umbra-platform-macos/README.md            documentation
crates/umbra-platform-macos/tests/fixtures.rs    test
experiments/fixtures/smoke.sh                    test harness
experiments/fixtures/umbra-test-child.c          test fixture
```

Everything else `jj diff -r wppwptxswssr --name-only` returns falls into exactly
two other categories, and **no production source file is in the diff**:

- **This arc's root-level process documents** — `impl.md`, one `fix-rN.md` per
  review round, one `ci-fix-rN.md` per CI round. Some of those paths already carry
  an earlier arc's document at `master`, so they appear as modifications; the later
  ones are new files. Which is which is read off `jj diff --stat`, where a
  modification is the path that also shows deletions.
- **`memoria.lock`**, when the documentation gate required a re-ack. It is a
  generated binary artifact, not source and not prose, and it is a **deliverable**:
  it carries the `memoria ack` records without which CI's documentation gate fails.
  Named explicitly because a category this partition does not name is a category
  someone treats as residue — which is exactly how this arc destroyed two acks
  once, recorded in `ci-fix-r2.md`.

Run the command for the current list; it is the authority and this document does
not restate it.

The parent commit *is* `master`, so every file not in that list is byte-identical
to master by construction — a stronger guarantee than a per-file audit, and the
one this section actually rests on.

**Why this is phrased as "four content files plus whatever the tool lists" rather
than as a number.** Five rounds of review kept finding this sentence one commit
stale, and the generator was finally located in round 4: the fixed point is not
`impl.md` editing itself, which is the trap round 1's F3 hit, but **the round's own
other deliverable joining the diff.** Every `fix-rN.md` written to correct the
inventory invalidates the inventory it just corrected, so substituting the number
regenerates the defect while removing the number ends it. The two claims kept above
are the ones that *are* stable under another round — four named content files, zero
production source — and they are also the two a reviewer actually needs. §8 reached
the same conclusion for line counts and is where the per-file figures live.

**Which root-level documents belong to this arc, and which do not.** Re-derived by
reading every root document's own header, because round 1's version of this note was
wrong three ways — and that note is the *entire* mitigation for leaving these files
in place, so being wrong in it was worse than the thing it mitigates.

This arc's documents are the ones in the diff: `impl.md` and one `fix-rN.md` per
review round. Everything else at the repository root belongs to an **earlier** arc and
is inherited unchanged from master — and to **two** earlier arcs, not one:

| arc | PR | document families it left at root |
|---|---|---|
| `dg-egt6apy1` (#117 + #121's escalation) | #129 | `impl.md`, `fix-r*.md`, `review-*.md`, `publish.md` |
| `dg-29vwer0f` (#121) | #128 | `ci-round*.md`, `ci-fix-r*.md`, `fix-r3.md`, `review-synthesis-r4.md` |

**What is stable is the arcs and their families; what is not stable is which of those
paths this arc has taken over.** Every node here that writes a document — each review
round's `fix-rN.md`, each CI round's `ci-fix-rN.md` — overwrites one more path that
belonged to one of those two arcs, so any list of "still theirs" is falsified by the
next node. This arc's documents are, by definition, the ones `jj diff --name-only`
returns; everything else at root is an earlier arc's. That command answers it and no
list or integer here does. Round 1's version of this note got the arcs and the
arithmetic wrong in three separate ways, rounds 2 and 3 corrected the arithmetic, and
round 4 established that correcting the arithmetic was the wrong move: **writing the
correcting document changes the inventory it describes.**

A reader at this commit who opens `review-scope.md` gets `dg-egt6apy1`'s, and one who
opens `ci-round1.md` gets `dg-29vwer0f`'s. That is the overwrite-per-arc pattern issue
**#132** exists for; it is not introduced here, and it is not fixed here — deleting
documents belonging to two other arcs is not a slice-0 measurement change. Flagged for
`merge_gate` rather than left to be discovered, along with round 4's structural
suggestions: consolidate the round documents into one, or move them out of root to
`docs/graphs/dg-nsw71bqq/`.

**Review documents are deliberately not in this diff.** During round 1 the
correctness reviewer wrote `review-correctness.md` into the worktree root, which
jj snapshotted straight into the change under review. That has been restored to
master's bytes. The policy, and the reason: in jj a root-level write *is* a
mutation of the change under review, which is exactly why the scope reviewer wrote
only to its scratchpad and to the anchor's archive, and why the previous arc's
round 3 recorded a change being mutated under a live reviewer. So review
documents live beside the audit and the design gate in
`knowledge/umbra/graph-audits/` — both of this round's do, at
`dg-nsw71bqq-review-correctness.md` (sha256 `f551cbca…`, byte-identical to the
copy that was removed) and `dg-nsw71bqq-review-scope.md`. Implementation
documents this node authored — `impl.md` and each round's `fix-rN.md` — ship in the diff, per the
scope review's ruling (i) and master's own precedent. Shipping one review and not
the other was the only option that was defensible on neither ground.

The measurement also **re-sizes slices 1–3**, and that is the more consequential
half of the finding: one of the two defects it exposes is *not* closed by the
per-thread slot work slice 1 describes. Section 3 states why, and section 7 says
what slice 1 should become.

---

## 1. What was built

Two new `#[test] fn`s and two new `strcmp` arms. Nothing else.

| Artefact | Where | Shape |
|---|---|---|
| `--mt-write` arm | `experiments/fixtures/umbra-test-child.c` | two pthreads, rendezvous, each `open`/`write`/`close`s a distinct destination |
| `--mt-spawn` arm | same | one thread `posix_spawn`s, the other writes concurrently |
| `mt_write`, `mt_spawn` | `crates/umbra-platform-macos/tests/fixtures.rs` | `#[test]`, `#[ignore]`d, driven through `fixture_argv` |
| `mt_fixture` helper | same | owns the second destination and the `CAPTURED` line |
| untraced baseline | `experiments/fixtures/smoke.sh` | proves the fixture itself, with no tracer involved |
| case table + defect note | `crates/umbra-platform-macos/README.md` | the table said "all eleven cases are enabled"; it now says thirteen, eleven enabled |

### Why these two cases and not the brief's three

`single_thread()` (`native.rs:498`) has exactly two call sites — `native.rs:1922`
(`Delivery::Fork`) and `native.rs:1967` (`WaitPlan::Park`). Four dispatch arms are
ungated. They do **not** all write the two single-element slots (`Session::pending`
at `:234`, `Session::entry` at `:236`), and round 1 was right that saying they do
is false. Per arm, read off `native.rs`:

| arm | gated? | `entry` | `pending` | measured by |
|---|---|---|---|---|
| `Delivery::Namespace` | no | set at `:1913` | one hop later, at `:2387` | **`mt-write`** |
| `Delivery::Fork` | **yes** | — | `return_stop` `:1931` | — (refused) |
| `Delivery::Exec` | no | — | `return_stop` `:2002`/`:2008` | **`mt-spawn`** |
| `WaitPlan::Native` | no | — | `return_stop` `:1970` | **nothing** |
| `WaitPlan::Park` | **yes** | — | — (sets `s.waiting`) | — (refused) |
| `WaitPlan::Poll` | no | — | — (**neither** slot) | — (nothing to collide) |

Two corrections round 1 made to this table's earlier form, both verified here:

- **`WaitPlan::Poll` writes neither slot.** It rewrites `x0`/`CPSR`/`PC`, calls
  `set_regs` and continues. There is no transaction to collide with, so it is
  ungated *and* harmless, and counting it among the slot writers overstated the
  exposure.
- **`Delivery::Namespace` does not call `return_stop` itself.** It records the
  entry PC and emits the event; the gate is planted one hop later, when the caller
  resumes the thread — `resume()` takes `s.entry` and calls
  `s.return_stop(ReturnKind::Syscall)` at `native.rs:2387`. `mt-write` reaches the
  window through that hop. The conclusion is unchanged; the path is one hop longer
  than the first write-up implied.

**`WaitPlan::Native` is a third ungated slot writer and slice 0 does not measure
it.** Said plainly rather than left to be inferred from a two-row table. It writes
`pending` exactly as `Exec` does, so it is *expected* to collide the way `mt-spawn`
does — a non-blocking `wait4` beside a concurrent namespace call is not an exotic
shape — but expectation is not measurement and no fixture drives it. A case for it
is named in §7 for the item-6 issue. The same statement now appears in
`tests/fixtures.rs`, `umbra-test-child.c` and the crate README, which round 1
found disagreeing with each other about the size of this enumeration.

`mt-spawn` deliberately does **not** reap its child. A blocking wait over a live
child is `WaitPlan::Park`, which *is* gated, so reaping would refuse the run
before the spawn path had been measured at all. The harness's own event loop sees
the child exit instead.

### Additive by construction

- `fixture_argv` (`tests/fixtures.rs:33`) is one-destination-per-case, and so is
  `matrix()` in `crates/umbra-cli/tests/run_fixtures.rs`. Neither is reshaped.
  The second destination is an **extra argv operand the case builds for itself**
  — the `--dirfd-rename` shape, where the child is handed a root of its own
  rather than a single output path. `mt_fixture` creates that destination,
  passes it through `fixture_argv`'s `argv` closure, and asserts it afterwards.
- The second destination lives **outside** the directory `fixture_argv` creates
  and removes. Inside it, a host-side leak would surface as a failed
  `remove_dir` naming a directory; outside it, it surfaces as an assertion
  naming the defect. That is the one assertion the whole slice turns on.
- No existing test function, helper signature or call site is edited.

### Oracle: entry names and bytes, never the journal

Each case writes two destinations differing in **name and in bytes** (`one\n` /
`two\n`, or `libc\n` / `two\n`). A rewrite that reached the wrong thread shows up
three ways: wrong content in a shadow file, a shadow file absent, or bytes on the
host under the unrewritten name.

The journal is not a valid oracle here and is not used as one. `JournalRecord`
carries `format_version`, `sequence`, `operation_id`, `writer_epoch`, `payload`
and **no task or thread field**, so cross-thread corruption still journals
`Prepare`/`ObservedResult`/`Commit` triples that pair correctly by
`OperationId`. A journal-based assertion would have passed on a corrupted run.

Raw `open`/`write`/`close` throughout, never stdio — open issue **#127** records
that a buffered stdio write reaches a routed descriptor as zero bytes with exit
0, which would let a corrupted run read as a clean one.

The rendezvous is a bounded spin on an `atomic_int`, not a mutex, condvar or
pipe: those issue syscalls of their own, and one of them landing between the
barrier and the call under measurement is the interleaving the case is trying to
produce, not an ingredient of it. The bound exists so a host that starves one
thread falls through instead of spinning to the 25 s session deadline.

---

## 2. The slice 0 measurement, verbatim

All runs: darwin 26.5.1 (25F80) / arm64, Apple clang 21.0.0, rustc/cargo 1.98.1,
debug profile (so `debug_assert!` is live), `--test-threads=1 --nocapture`,
`UMBRA_TEST_FIXTURE_PATH` and `UMBRA_TEST_REDIRECT_ROOT` both set, against the
tree described above — production code byte-identical to `master` `e44d0db8`.

### 2.1 Untraced baseline first: the fixture itself is correct

`experiments/fixtures/smoke.sh`, no tracer involved, 12 of 12:

```
PASS open-libc
PASS open-svc
PASS fork-write
PASS posix-spawn-write
PASS exec-write
PASS grandchild-write
PASS dup-inherit-write
PASS --wnohang-wait
PASS --dirfd-rename
PASS --symlink-cycle
PASS --mt-write
PASS --mt-spawn
```

Both new cases write both destinations with the right bytes in the right file
when nothing is tracing them. Everything in 2.2 and 2.3 is therefore the tracer,
not the fixture.

### 2.2 `mt-write` — live corruption, 3 runs of 3

```
thread 'mt_write' panicked at crates/umbra-platform-macos/tests/fixtures.rs:398:5:
MISSED mt-write: the second thread's output reached the host at
/var/folders/8p/h_y4038x1cl3y7_q1y6g5sx00000gn/T/umbra-rust-fixture-61707-mt-write-b/output
```

Reproduced 3/3 before the `#[ignore]` was added and again after every rebuild.
Read back from the leaked host file:

```
$ od -c .../umbra-rust-fixture-38460-mt-write-b/output
0000000    t   w   o  \n
0000004
```

No shadow file was ever created for that destination. `fixture_argv`'s own
assertions on the *first* destination passed — `one\n` in the shadow, nothing on
the host — so exactly one of the two threads was mediated.

The event stream says which, and it is the decisive evidence. Per-thread event
counts for one run (`native_id` of `ThreadId`):

```
SyscallEntry   task=40058 thread=40127713  x10      <- main thread, pre-main libSystem
SyscallExit    task=40058 thread=40127713  x10
SyscallEntry   task=40058 thread=40127734  x2       <- one worker: its open and its close
SyscallExit    task=40058 thread=40127734  x2
ThreadStarted  task=40058 thread=40127713  x1
```

**The second worker thread does not appear at all.** Not misattributed to the
wrong thread — *not intercepted*. Its `open`, `write` and `close` produced no
`SyscallEntry`, so no rewrite was ever planned, and the tracee's own path
reached the kernel.

### 2.3 `mt-spawn` — the tripwire fires, 3 runs of 3

```
thread 'mt_spawn' panicked at crates/umbra-platform-macos/src/native.rs:571:9:
a second intercepted syscall entered while one was still in flight: this
session's pending entry breakpoint, return gate and exec candidate would all be
overwritten
```

Reproduced 3/3. This is the `debug_assert!` at `native.rs:571-576` that master's
38-line comment on `return_stop` planted *for this arc*, reached exactly the way
that comment predicts. Per-thread event counts:

```
SyscallEntry   task=43597 thread=40141970  x10      <- main thread, pre-main
SyscallExit    task=43597 thread=40141970  x10
SyscallEntry   task=43597 thread=40142017  x1       <- writer's open, no exit
```

`Delivery::Exec` emits no `SyscallEntry` of its own — it goes straight to
`return_stop` — so the spawn is invisible as an event while holding `pending`.
The writer thread's `open` entry is delivered, the harness rewrites it, and its
`resume` opens a second transaction on the same slot. One `SyscallEntry`, no
matching `SyscallExit`: the panic is between them.

`debug_assert!` is compiled out of a release build, so **the backend** takes that
overwrite in release: the spawn's return gate, its entry breakpoint and the pid
pointer `finish_return` would have attached the child by are all lost.

**But "silent in release" — which this document asserted in round 0 — is false,
and the correction matters more than the sentence.** It rested on
`native.rs:571` being the only tripwire, and it is not. The supervisor holds a
second one, a layer up, that ships:

```rust
// crates/umbra-supervisor/src/events.rs:307, in syscall_entry
if self.operations.contains_key(&thread) {
    return Err(error(
        ErrorKind::InvalidState,
        "supervisor.syscall_entry",
        "entry received while an operation is still awaiting its exit",
    ));
}
```

Read in source for this fix: that is a real `Err`, not a `debug_assert!`, so it is
**live in release**, and `operations` is declared
`BTreeMap<ThreadId, OperationId>` (`umbra-supervisor/src/lib.rs:332`) — it is keyed
**by thread**. Two consequences, and the second is the one nobody had written
down:

1. The violation is *detectable* in release on the shipped path, not silent. §2.5
   measures what actually happens.
2. **The supervisor already models in-flight operations per `ThreadId`. Only the
   platform backend's `Session::pending`/`Session::entry` are single-slot.** The
   audit's section D concluded "the only inherently per-thread state is the
   in-flight syscall transaction" without noting that one layer already models it
   correctly and the other does not. That asymmetry localises the defect exactly,
   and it is independent evidence that slice 1's shape is the architecturally
   consistent fix rather than a guess. It belongs in the audit and on the item-6
   issue.

**How often the guard actually fires: it is a race, and the axis is machine load.**
Round 1 left two attributed figures unreconciled — the correctness reviewer's 5/5 in
release against my 0/20. Round 2 resolved it by varying only the environment, at the
same commit and against the same fixture binary (`02fb7ad8`, byte-identical across
both rounds because the `umbra-test-child.c` diff is comment-only):

| case | condition | `InvalidState` fires |
|---|---|---|
| `mt-write` | idle machine | **1/20** |
| `mt-write` | 10 spinners on 10 cores, loadavg 3.65 | **5/20** |
| `mt-spawn` | idle machine | **0/10** — always `Io during path` |

Neither figure was wrong; they are opposite ends of one contention range. The
reviewer's round-1 sample ran while a parallel reviewer occupied the same cores;
mine ran on a quiet machine. Reaching the guard needs the escaped thread to issue a
*second* traced call while the sibling's operation is still open, so "the denial
kills the child first" is the **usual** case, not the only one.

**So nothing load-bearing rests on this guard, and this document does not let it.**
The guard is 1/20–5/20 on `mt-write` and 0/10 on `mt-spawn` — a contention-dependent
extra witness, not a mechanism anyone should plan around. What actually makes the
shipped path non-silent is structural and holds every time: the **denial**, which
§2.5 shows is guaranteed by the rendered policy, and the **non-zero exit**, 20 of 20
in the enforced runs measured here. The guard is recorded because it is real, because
it is the reason the "silent in release" sentence was wrong, and above all for its
*second* consequence — the per-`ThreadId` asymmetry — which does not depend on how
often it fires at all.

### 2.4 One measured side effect of 2.3, and it is not about the tripwire

The tripwire fires *after* the `posix_spawn` syscall has already run. The child
exists, is held by `POSIX_SPAWN_START_SUSPENDED`, and has not been attached to
any session yet — so nothing owns it: neither `Session::drop` nor the watchdog
kills it. Measured: it survives the panic in state `T`, still holding the
inherited descriptors 0/1/2, and held a captured pipe open until killed. (The test
harness now reaps it — see the note at the end of this section — but the enforced runs
this measurement comes from do not go through that harness, and the defect is
untouched.)

```
40289  TN  .../twins/02fb7ad8.../umbra-test-child open-libc /var/.../output
```

This is master's behaviour on an error at that point, not something slice 0
introduced, and no production change is in scope here to fix it. It is recorded
in `mt_spawn`'s doc comment so the next person running it by hand is not
surprised, and it is a reason the fix PR should not stop at the assertion.

**Round 0 asserted that this generalises beyond the tripwire — "reachable from any
error between the spawn's `svc` and `finish_return`". It is now measured, and it
holds.** Under enforced `umbra run` (§2.5), `mt-spawn` leaked exactly one suspended
orphan in **10 of 10** runs:

| profile | runs | orphans | error that ended the run |
|---|---|---|---|
| debug | 5 | 1 each | the `debug_assert!` at `native.rs:571` |
| **release** | 5 | 1 each | `Io during path: null or overflowing pointer` (`EFAULT`), an **unrelated** refusal with the assertion compiled out |

The release column is the proof: the assertion is not present, the error is a
different one entirely, and the orphan still leaks. So the leak is a property of
the **error window**, not of the tripwire — which is what makes it worth its own
issue rather than a footnote to slice 1. All ten were reaped by hand.

**CR round 1 asked for the harness to stop leaking, and it no longer does.** The two
`#[ignore]`d tests now run under a `Drop` guard that snapshots the fixture's processes
before the traced run and kills whatever appeared once it unwinds, printing a `REAPED`
line naming the pid; verified with no external `pkill`, zero strays remaining, and zero
`TMPDIR`/shadow residue. **That is a harness fix and changes nothing about the defect
above** — the enforced `umbra run` measurements do not go through this harness, the
10/10 figure stands, and the follow-up issue's description is unchanged. A `REAPED`
line is evidence the error window is still open.

### 2.5 What this costs on the shipped path

Round 0 reasoned about this section instead of running it, and offered two branches
without saying which occurs. Round 1 was right to refuse that. The severity is
*lower* than round 0 implied, and the argument for that is **structural first**,
with the runs as corroboration — which is the order round 2 asked for, on the
grounds that a policy property outranks any number of samples of it.

#### The structural argument: the severe branch is unreachable under the shipped policy

Four facts about the shipped configuration, each checkable without running anything:

1. **The policy is compiled in, not configured.** `TEMPLATE` is
   `include_str!("../../../experiments/seatbelt/umbra.sb")`
   (`crates/umbra-supervisor/src/sandbox.rs:18`), so the argument below is over the
   policy this binary ships, not over a file a deployment might supply.
2. **The template grants exactly one write.** It is `(deny default)` plus
   `(allow file-read*)` plus a single `(allow file-write* (subpath
   {{UMBRA_RUN_ROOT}}))` — one line matching `^(allow file-write`, at
   `umbra.sb:13`. Its closing line is explicit: *"No /tmp, /private/var/folders, or
   other persistent write carve-outs."*
3. **The renderer hard-fails rather than degrade.** `render()` counts the token and
   returns `Err` unless it appears exactly once
   (`crates/umbra-supervisor/src/sandbox.rs:42-47`), and rejects any other
   unresolved token. There is no path on which a profile is rendered with a second
   or missing write allowance.
4. **The one allowance is never a path an escaped write can name — and which path it
   is depends on the run's shape.** `root_path` is chosen at
   `crates/umbra-supervisor/src/run.rs:1200-1203`, on `routed =
   binding.root.physical_path.is_none()` (`:952`), and the two cases must not be
   conflated:

   - **Routed run** (`interpose` true): the allowance is
     `<state_root>/<run-id>/host` — an **empty per-run host directory** that exists
     only so the template's one write rule names a real path. It is not where the
     run's data goes; every routed operation reaches the store through the userspace
     client. `HostState::host`'s own documentation (`run.rs:548-562`) states the
     property this argument needs: it is "strictly less than the grant a kernel-path
     run receives, and **nothing in the tracee's logical namespace resolves to it**".
   - **Kernel-path run** (`interpose` false): the allowance is the canonicalized
     `binding.root.physical_path` (`run.rs:1218-1234`) — this run's own root inside
     the store. An escaped write is by definition *unrewritten*, so it names the
     tracee's **workspace** path, which is not inside this run's root: the root is
     created during preparation, after the workspace has been inventoried, so a path
     that already existed cannot lie inside a directory that does not yet exist.

   **The run-root UUID is no part of this argument.** An earlier revision of this
   document claimed the root is "a fresh UUID directory created during preparation,
   so no argv operand can point at it", which reads as an unguessability property
   and a UUID does not provide one. What does the work is **disjointness** — stated
   in source for the routed case, and **creation order** for the kernel-path case.
   CR round 5 struck the claim; `ci-fix-r5.md` records the check confirming that the
   reclassification below never rested on it.

So a `(deny default)` profile whose only write allowance is a path the escaped write
cannot be addressing must deny that write. **The severe branch is excluded by
construction.** The runs below are what confirm the construction behaves as read;
they are not what establishes it.

#### The corroborating runs

`fixture_argv` launches with `SandboxRequirement::UnsandboxedExperiment`,
`interpose: false` and no descriptor fence. That is deliberate — the harness
measures interception, not the sandbox boundary — and it is why the escaped write
is visible there as bytes on the host. It is not what a real run does.

**20 enforced `umbra run` executions, 10 of them release.** Registry built exactly
as `run_fixtures.rs::matrix()` builds it (real `umbra-platform-macos` +
`umbra-journal-file` + `umbra-storage-local` providers), `--experimental
--local-dev`, Seatbelt in force before the target's first instruction, both
destinations placed **inside the workspace** so the worst branch had its best
chance.

These are **kernel-path** runs, in the sense fact 4 distinguishes:
`umbra-storage-local` exposes a kernel path for the run root, so `routed` is false,
`interpose` is false, and the allowance was this run's own root inside the store —
which is what the shadow paths below show. The routed case is the *narrower* one,
so it is not measured here and does not need to be: its allowance is an empty
directory nothing in the tracee's namespace resolves to.

| configuration | runs | exit | host `output2` | how it failed |
|---|---|---|---|---|
| debug, `mt-write` | 5 | 1 ×5 | **never** | `umbra-test-child: open: errno=1 (Operation not permitted)` → child exit 1 → `ProcessFailed during run.child` |
| **release**, `mt-write` | 5 | 1 ×5 | **never** | identical |
| debug, `mt-spawn` | 5 | 1 ×5 | **never** | provider panic at `native.rs:571` → `StorageUnavailable during provider.transport: provider disconnected` → see §2.5.1 |
| **release**, `mt-spawn` | 5 | 1 ×5 | **never** | `Io during path: null or overflowing pointer` (`errno 14`, `EFAULT`) |

Signature sweep across all 21 enforced scratch directories (20 runs plus one
discarded `ProtocolMismatch` attempt while the registry was still wrong): 10
stderr logs naming `Operation not permitted`, 5 naming `native.rs:571`, 5 naming
`LeaseLost`, 5 naming `Io during path`, and **0 host `output`/`output2` files in
total**.

Shadow inventory shows the asymmetry rather than a clean run: the run root holds
`output` at 4 bytes (`one\n` — thread A was mediated) and **no `output2` at all**,
in neither store nor host. The escaped write was denied, so nothing landed
anywhere.

Two witnesses hold across every one of those runs, and they are the ones the
non-silence claim rests on: the **denial** — guaranteed by the structural argument
above — and the **non-zero exit**, **20 of 20**. The `events.rs:307` guard is *not*
among them (§2.3): it is contention-dependent, 1/20–5/20 on `mt-write` and 0/10 on
`mt-spawn`, and nothing here needs it.

*The counts are kept separate on purpose.* The 20 above are the runs measured for this
document, and both witnesses are reported for all of them. Round 2's contention runs
(§2.3) are the correctness reviewer's, and what that review reports for them is
`InvalidState` frequency, not exit codes — so they corroborate the guard's *range* and
are not folded into any exit-code total here.

**Reclassification, stated once and carried into the PR body and the tracking
issue.** The interception escape is real and confirmed. On the shipped path it is
an **availability and correctness** defect — a multithreaded tracee aborts its run,
loudly, with a named error, in release — and **not** a namespace escape. It must
not be filed under confidentiality or integrity.

*The residual caveat, bounded rather than waved at:* reaching the severe branch
needs a rendered profile that grants a host-writable path the tracee also names.
In the shipped product that is not a configuration knob — it would take an edit to
`umbra.sb` adding a second `allow file-write*` line, which the renderer's
one-token check does not prevent. Nobody has run that, and nothing here claims it.

#### 2.5.1 New finding: an enforced `mt-spawn` loses the writer lease

Not in the audit, the design gate, or round 0 of this document. Under an enforced
run in **debug**, the provider-side panic takes the provider process down, and the
supervisor's cleanup cannot prove the tree is gone:

```
StorageUnavailable during provider.transport: provider disconnected
  (cleanup also failed: LeaseLost during overlay: run left recovery-required:
   supervised tree termination unproven)
```

Measured 5/5, and the store is left with `<store>/<run-id>/writer.lock` **still
present** — the run is recovery-required with its writer lease lost. That is a
materially worse operational outcome than the direct harness shows.

**It is debug-only, and the distinction is measured.** In release the same case
fails through an ordinary `Err` return (`Io during path`, above) rather than a
panic, the provider stays up, and the lease is released — 0 of 5 release runs left
a `writer.lock`. So the lost lease is a consequence of the *panic*, i.e. of the
`debug_assert!`, while the orphan leak of §2.4 is not. Both go on the item-6
issue (§7), as separate items, because they have different triggers.

### 2.6 A control, so the two new tests are known to be non-vacuous

A red test that can never go green is indistinguishable from a broken one. The
correctness review added this control and it is reproduced here rather than cited.

A **serialized** copy of the fixture was compiled into scratch — same file, same
arms, same oracle, with `mt_write`'s two threads started and fully joined one at a
time so no two namespace transactions can overlap — and `UMBRA_TEST_FIXTURE_PATH`
pointed at it. No tree file was touched.

```
SyscallEntry thread=40561533 x2     worker A: open + close   <- both mediated
SyscallEntry thread=40561542 x2     worker B: open + close
CAPTURED mt-write
test result: ok. 1 passed; 0 failed
```

Serialized, **both** workers are intercepted, both destinations are rewritten, and
the test **passes with its `CAPTURED` verdict**. This rules out the competing
explanations — that worker threads are never breakpointed, that thread creation
loses the registry, that the case is unsatisfiable — and leaves the overlap window
as the cause. It also establishes what the failing tests alone cannot: the oracle
is sound and the passing assertion is reachable, so `mt_write` and `mt_spawn` are
real measurements rather than failing stubs.

---

## 3. Why slice 1 as ratified does not close `mt-write`

This is the finding that re-sizes the arc, so it is stated mechanically.

Slice 1 is "make `Session::pending` and `Session::entry` per-thread, thread the
trapping TID explicitly, convert the `debug_assert!` to a per-thread invariant".
That closes **2.3**. It does not touch **2.2**, because 2.2 is not the slot.

`return_stop` (`native.rs:539-...`) must let a thread stopped *on* a breakpointed
`svc` execute that `svc`. It does so by releasing the entry breakpoint, planting
the return gate one instruction later, and resuming:

```rust
let entry_breakpoint = self.remove_breakpoint(pc)?;   // z0: the svc is restored
if hardware { /* Z1 at gate */ } else { self.temporary_breakpoint(gate)?; }
self.pending = Some(Pending { entry: pc, entry_breakpoint, gate, kind, hardware });
self.continue_run()                                   // bare `c`: every thread
```

`finish_return` is what puts it back — `install_breakpoint(pending.entry,
pending.entry_breakpoint)`. Between those two points:

1. the stub carries no breakpoint, and a debugserver `Z0` is **per-process**, not
   per-thread; and
2. `continue_run()` sends a bare `c`, which resumes **every** thread.

So the window is open for the whole process. Any sibling thread that reaches the
same stub inside it executes the real syscall unmediated. Both workers in
`mt-write` call `open`, which is one stub at one address, so the second worker
walks through the hole the first worker's return gate opened. Making `pending`
per-thread does not narrow that window by one instruction: the hole is in the
**shared breakpoint registry**, not in the slot.

`Delivery::Namespace` reaches `return_stop` one hop later than the code above
suggests, and the correction is round 1's: the arm itself only records the entry
PC, and the gate is planted when the caller resumes the thread — `resume()` takes
`s.entry` and calls `s.return_stop(ReturnKind::Syscall)` at `native.rs:2387`. The
window and its consequence are unaffected.

**The cleanest statement of why slice 1 closes one case and not the other** —
round 1's, and it is better than anything in round 0. The two cases collide on
*different shared resources*, decided by whether their threads share a stub:

| case | threads' stubs | shared resource they collide on | closed by per-thread slots? |
|---|---|---|---|
| `mt-write` | both `open` — **one address** | the process-wide `Z0` registry, via the `z0` window | **no** |
| `mt-spawn` | `__posix_spawn` and `__open_nocancel` — **two addresses** | the single `pending` slot | **yes** (expected) |

Neither un-arms the other's stub in `mt-spawn`, so that collision has nowhere to
land but the slot; in `mt-write` the shared address means it never reaches the slot
at all.

Master's own comment on `return_stop` mentions the `z0` — but as a consequence of
*losing* the pending value ("the site was already released with `z0` … so losing
the value means it is never re-armed"). The release *window itself*, with the
pending value intact and correctly consumed, is not recorded anywhere in the
audit or in the design gate. It is what slice 0 was for.

Closing it needs the entry site to stay armed while another thread could reach
it. **Round 0 presented two shapes as exhaustive and it was wrong: there are
three, and the cheapest needs no new tracer wire surface at all.** That changes
what the human is being asked to re-ratify, so it is stated as a costed table
rather than prose.

| shape | mechanism | new RSP surface | new dependency | notes |
|---|---|---|---|---|
| **Mach sibling-hold** | over the task port this backend **already holds**: `task_threads` to enumerate, `thread_suspend`/`thread_resume` to hold every thread but the trapping one across the return | **none** | **none** | `Task::acquire` takes the port via `task_for_pid` (`native.rs:45-51`) and `Session` holds it as `Arc<Task>` (`:225`) for the session's life. `mach2` is already this crate's dependency (`Cargo.toml:49`, lock 0.7.0) and exposes both calls (`mach2::task::task_threads`, `mach2::thread_act::thread_suspend`/`thread_resume`). The audit's own section F probe already used `task_threads`, so the technique is in the graph's toolkit. |
| **per-thread step** | leave the `Z0` planted and single-step the trapping thread over the `svc`, so no window exists | `vCont;s:<tid>` | none | the deferred waiver |
| **per-thread resume** | resume only the trapping thread while a return is in flight | `vCont;c:<tid>` | none | the deferred waiver |

And one shape recorded because it is the option that *is* genuinely blocked, so the
next arc does not re-derive it: an **out-of-line `svc` trampoline** (leave the site
armed and execute the syscall from scratch memory) would also close the window, but
`Session::allocate` requests `_M<size>,rw` (`native.rs:435`) — read/write, **not
executable** — so it needs new executable-scratch work on a platform with W^X and
`MAP_JIT` constraints.

**One caution for whichever shape is chosen**, from round 1 and verifiable in this
crate's own README ("Closed M2 gap: `exec-write` post-exec breakpoint loop"):
debugserver's `Z0` registrations are **reference counted**, and a duplicate `Z0`
makes the matching `z0` decrement without restoring the instruction *while still
answering `OK`*. Any design that keeps the entry site armed across a return is
operating directly on that refcount, and that is where this crate has already been
bitten once.

**So the consequence for re-ratification is narrower than round 0 stated.**
`mt-write` is not reachable by slice 1, and the design gate's ordering — "slices 2–3
without 1 are not safe" — now has a companion: *slice 1 without the window fix does
not close what slice 0 measured.* But the ask put to the human should **not** be
"grant the deferred `vCont` waiver": the Mach sibling-hold closes the window with no
wire surface, so the decision is a **choice between three costed shapes**, and that
is a materially cheaper decision than the one the design gate deferred. Nobody has
costed them against each other yet; that is the re-ratification, and §7 carries it.

---

## 4. Guardrails — every #55–#117 mechanism preserved

Byte-identical, and verified by the file being unmodified rather than by reading
the diff. `jj diff -r @ --name-only` lists no production file (section 0). The
pinned sites were then read back by line number in the committed tree:

| Mechanism | Pinned at | Read back |
|---|---|---|
| `self.parent.is_some()` in `install()` | `native.rs:644` | `let inherited = if self.parent.is_some() {` |
| `return_stop`'s `debug_assert!` — **kept, not deleted** | `native.rs:571` | `debug_assert!(` |
| `single_thread()` itself | `native.rs:498` | `fn single_thread(&mut self) -> Result<()> {` |
| its two call sites | `native.rs:1922`, `:1967` | `s.single_thread()?;` twice |
| `TRANSIENT_SIGNALS` | `native.rs:2202` | `const TRANSIENT_SIGNALS: [i32; 8] = [` |
| its SIGCHLD-in / SIGSYS-out tests | `native.rs:2550`, `:2554` | the two `assert!`s on `super::TRANSIENT_SIGNALS` |
| one table, two gates over `abi::TRACED_STUBS` | `native.rs:669`, `:1884-1891` | the `for (name, number, _) in abi::TRACED_STUBS` loop and `intercept`'s "disposition comes from `abi::TRACED_STUBS`" comment |
| `ReturnKind::Exec { twin }` candidate | `native.rs:1875`, `:2008` | `ReturnKind::Exec { twin: _candidate } =>` and `s.return_stop(ReturnKind::Exec { twin })?` |
| descriptor fence, C half | `umbra_interpose.c:406-409` | `umbra_owns` — `umbra_active() && fd >= 0 && (uint64_t)fd >= umbra_control.floor` |
| its Rust/C twin predicate | `abi.rs` | `the_interposers_descriptor_test_is_the_one_the_supervisor_applies` at `abi.rs:2405` |

Also, per ratification and deliberately:

- **`single_thread()` stays.** `Delivery::Fork` and `WaitPlan::Park` continue to
  refuse a multithreaded tracee. The gate is not what this work removes; it is
  what this work makes unnecessary later — and after section 3, later means
  after slice 2, not after slice 1.
- **No `vCont`.** The waiver was DEFERRED and nothing here needs it. Section 3
  is the argument for granting it when slice 2 is ratified, not an exercise of it.
- **Interposer unchanged.** No `pthread_atfork`, no TSD, no locks, no mutable
  globals — the audit's section G, re-confirmed by the file being untouched.
- **Journal unchanged.** `JournalIntent` is process-independent; section 1
  explains why that makes it useless as this slice's oracle.
- **Waiver 4 (`Session` shape) is granted but unused.** It licenses slice 1,
  which this PR does not contain.

---

## 5. Gate figures, every one qualified by the verdicts it was read from

### 5.1 The three local gates

| Gate | Command | Result |
|---|---|---|
| format | `cargo fmt --all -- --check` | exit 0, no diff |
| lint | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, 0 lines matching `^(warning\|error)` |
| tests | `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** |

**The 832/0 figure is qualified and it is weaker than it looks.** That invocation
has no `--nocapture` and none of the fixture environment variables, so every
integration case in it takes `fixture_argv`'s skip branch (`tests/fixtures.rs:47`),
prints `SKIP <case>` to a stderr nobody captures, and is reported as `ok`. Read
on its own it proves the suite compiles and the unit tests pass. **It proves
nothing about interception.** That is why 5.2 exists; a skip is not a pass.

The 5 ignored are 3 pre-existing plus the 2 added here, enumerated by name with
`-- --ignored --list`:

```
provider_drop_after_deadline_miss_returns_promptly          pre-existing (NFS faults)
scratch_server_killed_after_flush_preserves_export_bytes    pre-existing (NFS faults)
scratch_transport_outage_recovers_to_local_without_remote_claim  pre-existing (NFS faults)
mt_spawn                                                    added here
mt_write                                                    added here
```

The three pre-existing ones need `UMBRA_TEST_NFS_FAULTS=1` and an idle export
(`crates/umbra-storage-nfs/tests/mounted.rs`). Master had 3; this change makes 5.

### 5.2 Integration qualification, verdicts read by name

All with `UMBRA_INTEGRATION_REQUIRED=1`, `UMBRA_TEST_FIXTURE_PATH`,
`UMBRA_TEST_REDIRECT_ROOT`, `--nocapture --test-threads=1` — the CI
`native-qualification` job's own invocations.

**`-p umbra-platform-macos --test fixtures` → 11 passed, 0 failed, 2 ignored.**
Eleven `CAPTURED` lines, read by name from the captured output, zero `SKIP`,
zero `MISSED`:

```
CAPTURED argv0-check          CAPTURED open-libc
CAPTURED dirfd-rename         CAPTURED open-svc
CAPTURED dup-inherit-write    CAPTURED posix-spawn-write
CAPTURED exec-write           CAPTURED symlink-cycle
CAPTURED fork-write           CAPTURED wnohang-wait
CAPTURED grandchild-write
```

and the two new cases reported as ignored, with their reason:

```
test mt_spawn ... ignored, slice 0 measurement: fails on master by design, on the
                  `debug_assert!` at native.rs:571. ...
test mt_write ... ignored, slice 0 measurement: fails on master by design; the
                  unmediated entry-breakpoint window it names is not closed by
                  per-thread slots alone. ...
```

**`--test provider_ipc` → 1 passed.** Verdict `CAPTURED open-libc provider IPC`.

**`--test sandbox_launch` → 4 passed, 0 failed.** This suite prints no
`CAPTURED` line of its own; its assertions are the verdict.

**`-p umbra-cli --test run_fixtures` → 10 passed, 0 failed.** Twenty `PASS`
verdicts read by name. The seven fixture cases, unchanged and in the same
one-destination-per-case table:

```
PASS local open-libc      PASS local exec-write
PASS local open-svc       PASS local grandchild-write
PASS local fork-write     PASS local dup-inherit-write
PASS local posix-spawn-write
```

ten standard-utility cases (`/bin/cat seed.txt`, `/bin/cat absent.txt`,
`/bin/ls <workspace>`, `/bin/ls -l <workspace>`, `/bin/ls -t <workspace>`,
`/bin/ls absent`, `/bin/mkdir made`, `/bin/rm seed.txt`,
`/usr/bin/touch seed.txt`, `/usr/bin/touch touched`) and three crash cases
(`crash exit 41`, `crash signal 9`, `crash signal 11`).

**Two `SKIP`s in that suite, and they are skips, not passes**:
`SKIP nfs_fixture_matrix` and `SKIP nfs_utility_matrix`, both on
`UMBRA_TEST_SKIP_NFS_MATRIX set` — the same opt-out CI sets for this job, for
the TCC `SystemPolicyNetworkVolumes` reason recorded in `ci.yml`. Nothing about
a live NFS mount is qualified here.

**`-p umbra-cli --test resume_cli` → 3 passed.**
**`-p umbra-supervisor --test reopen` → 7 passed.**

### 5.3 The real provider, `transport-raw`, verified by symbols

`third_party/libnfs` was cloned and checked out at the pin
`18c5c73ee88bb7dc8da0d55dc95164bb77e49dc6` (matching
`crates/umbra-storage-nfs-userspace/libnfs.pin`), which `build.rs` verifies with
`git rev-parse HEAD` in that checkout. `/third_party/` is gitignored, so it is
not part of the change.

```
cargo build -p umbra-storage-nfs-userspace --features transport-raw --bins        exit 0
cargo build -p umbra-storage-nfs-userspace --features transport-raw --all-targets exit 0
```

`--all-targets` matters more than `--bins` for this crate: it has no `[[bin]]`,
and its `transport-raw` surface is four whole test files behind
`#![cfg(feature = "transport-raw")]` that `--bins` never compiles.

**Exit 0 is not the verification.** The generated binding was read by symbol name
(`out/libnfs_raw.rs`), and so was the static library's build:

```
build.rs:  libnfs raw binding: 16 functions emitted
emitted `pub fn` count: 16

allowlisted raw-RPC entry points, each exactly once:
  rpc_init_context 1   rpc_destroy_context 1   rpc_connect_async 1
  rpc_nfs4_compound_task 1   rpc_nfs4_read_task 1   rpc_nfs4_write_task 1
  libnfs_authunix_create 1

managed lifecycle symbols, each absent:
  nfs_init_context 0   nfs_mount 0   nfs_open 0   nfs_close 0   nfs_creat 0
```

and `out/libnfs-build/lib/libnfs.a` exists at 488 944 bytes. The allowlist held.

The provider executables were verified the same way rather than by exit code:

```
cargo build --workspace --bins   exit 0
  target/debug/umbra                 14 275 736 bytes
  target/debug/umbra-platform-macos   5 958 168 bytes

umbra-platform-macos carries, by symbol/string:
  __DATA,__umbra_arm                     1   (the interposer's arming section name)
  libumbra_interpose                     9   (the embedded dylib)
  __open_nocancel __openat_nocancel __fork __posix_spawn __wait4_nocancel
  __renameatx_np getattrlistbulk setattrlistat   all present (TRACED_STUBS)

capability ids, both binaries:
  experimental-userspace-interpose-v1    macos 1, umbra 1
  experimental-syscall-rewrite-v1        macos 1, umbra 1
```

### 5.4 The release build, used by §2.5 and gated the same way

`cargo build --workspace --bins --release` → exit 0.

```
target/release/umbra                 3 702 400 bytes
target/release/umbra-platform-macos  1 761 104 bytes
```

This is the build §2.5's ten release runs were driven through, and it is what makes
the "loud in release" and "no host bypass in release" claims measurements rather
than readings of the source. `--bins` reported nothing to rebuild after the round-1
edits, which is the freshness check rather than a shortcut: every round-1 edit was
to a test file, a fixture, a README or a process document, and none of them is
compiled into a binary target.

### 5.5 Rebuilt between mutations

Every figure above was taken after the mutation it describes, not carried over.

Round 0: the `mt_write`/`mt_spawn` verdicts were re-measured three times — before
`#[ignore]` was added, after it was added and the crate rebuilt, and again after
the README edit and the final `cargo test --workspace --all-targets`. Identical
each time; only the panic line number moved (373 → 375) as the helper's doc comment
grew. Round 1 moved it again, to **398**, for the same reason — the block above
quotes the current line, and the `assert!` it names is at `fixtures.rs:398`.

Round 1 added four more mutations — the `fixtures.rs` per-arm table and doc
comments, the C fixture's header comment, the README's per-arm table and severity
paragraphs, and this document. The crate's tests were rebuilt and the C fixture
recompiled from this tree after them, and everything was re-run against that build.

Round 2 was documentation only: the README's `mt-spawn` paragraph, the `mt_spawn` doc
comment, this document's §0/§2.3/§2.5/§7/§8, and three cosmetic reference fixes. The
tests were rebuilt and the C fixture recompiled again anyway, and every gate re-run —
"documentation only" is a claim about the diff, not a licence to skip the gates that
would catch it being wrong. The `mt_write` assertion line did **not** move this round
(still `fixtures.rs:398`): every round-2 edit to that file landed after it, and that
was measured rather than assumed.

### 5.6 Round 1: every gate re-run against the final tree

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, 0 lines matching `^(warning\|error)` |
| `cargo test --workspace --all-targets` | exit 0; **52 suites, 832 passed, 0 failed, 5 ignored** — unchanged, and carrying the same §5.1 qualification: no `--nocapture`, no fixture env, so every integration case in it is a silent skip |
| `--test fixtures`, integration env | **11 passed, 0 failed, 2 ignored**; **0** `SKIP`, **0** `MISSED` |
| `--test provider_ipc` | 1 passed, `CAPTURED open-libc provider IPC` |
| `--test sandbox_launch` | 4 passed |
| `-p umbra-cli --test run_fixtures` | 10 passed, **20** distinct `PASS` verdicts, 2 declared `SKIP nfs_fixture_matrix` / `SKIP nfs_utility_matrix` |
| `-p umbra-cli --test resume_cli` | 3 passed |
| `-p umbra-supervisor --test reopen` | 7 passed |
| `smoke.sh`, untraced | 12 of 12 `PASS`, both new arms included |

The eleven `CAPTURED` names, read by name from this run's captured output and not
carried over from round 0: `argv0-check`, `dirfd-rename`, `dup-inherit-write`,
`exec-write`, `fork-write`, `grandchild-write`, `open-libc`, `open-svc`,
`posix-spawn-write`, `symlink-cycle`, `wnohang-wait`.

Both `#[ignore]`d cases were re-run with `--ignored` against the same build and give
the same two verdicts: `MISSED mt-write: the second thread's output reached the
host` (now at `fixtures.rs:398:5`) and the `debug_assert!` panic at
`native.rs:571:9`. The round-1 edits changed what the documents claim, not what the
tracer does.

---

## 6. Environment constraints hit, and what they cost

- **`git` and `gh` do not work in this workspace** (issues #123/#131): it has
  `.jj` and no `.git`. No git or GitHub operation was attempted from here. The
  colocated anchor `/Users/inva/Coding/umbra` is 55 PRs behind master (the
  audit's lesson 4/21), so it is not a substitute for anything measured here.
- **`memoria --root . check` cannot run *in this workspace*, and the gate is
  nonetheless qualified — by a second, different run. The two must not be
  conflated.**

  **Run A, in the jj workspace: FAILED, and never qualified anything.**
  `error [git_unavailable] … not inside a Git worktree`, exit 4, because
  `memoria --root` requires a git worktree root and this workspace has `.jj` and
  no `.git` (#123/#131). Every statement about the documentation gate in earlier
  rounds of this document rested on reasoning around that failure, not on a result.

  **Run B, in a throwaway git worktree cut from the colocated anchor at the
  pushed head: PASSED, and this is what qualifies the gate.** Recorded result:
  `OK: 23 README(s) current, imports rendered, no coverage or structure errors.`
  — **0 errors**, with 14 `navigation_disconnected` warnings, which is master's
  own baseline. CI's own "Memoria documentation gate" step agrees on both
  platforms. The procedure and the two `memoria ack` records it depends on are in
  `ci-fix-r2.md`; re-verifying it in a fresh worktree is now a standing check on
  every push rather than a one-off.

  Neither run is a substitute for the other: run A says nothing about the
  documentation, and run B cannot be performed from inside this workspace.
  `memoria.toml` ignores `experiments/**`, so two of the four content paths are
  outside its inputs; `tests/fixtures.rs` is inside
  `crates/umbra-platform-macos`'s boundary, and that README has been corrected
  and re-acked as its inputs moved.
- **`impl.md` is overwritten**, not appended. The copy this replaces is the
  previous arc's (`dg-egt6apy1`, fork lifecycle) and is preserved in history at
  `master` `e44d0db8`. §0 records which other root documents are this arc's and
  which are that one's, and §7.10 raises the pattern on #132.
- **The enforced runs in §2.5 were driven by hand, not through `run_fixtures.rs`.**
  `matrix()`'s case table is one-destination-per-case and reshaping it is out of
  scope, so the registry was rebuilt by a scratch script that mirrors what
  `matrix()` builds — same three real providers, same `--experimental --local-dev`
  flags, same store layout. The one consequence worth stating: those runs are
  reproducible from the script in the session scratchpad, not from a test in the
  tree. A standing CI probe for them would be the item-6 issue's business, and
  open issue **#130** already records that there is no standing mutation probe of
  this kind.

---

## 7. What the next PR should be, given the measurement

Not a recommendation about scope — a consequence of section 3.

1. **Slice 1 (per-thread slots) still stands and is still worth its own PR.** It
   closes `mt-spawn` and converts the `debug_assert!` to a per-thread invariant,
   exactly as ratified item 8 requires. Un-ignore `mt_spawn` in that PR. Leave
   `mt_write` ignored, with its reason updated to say the remaining defect is the
   entry-breakpoint window.
2. **`mt-write` needs the window fixed, and the decision is a choice between
   three shapes — not a `vCont` waiver.** §3's table costs them: the Mach
   sibling-hold needs **no** new RSP surface and no new dependency, because the
   backend already holds the task port and `mach2` already exposes `task_threads`
   and `thread_suspend`. So re-ratification should be put as *"pick a shape"*, not
   as *"grant the deferred waiver"*. This is the correction round 1 made to round
   0's framing and it is the item most likely to change what the human decides.
3. **Measure `WaitPlan::Native`.** It is the third ungated `return_stop` caller
   (§1), it writes `pending`, and slice 0 does not touch it. A non-blocking `wait4`
   beside a concurrent namespace call is an ordinary shape. Expected to behave like
   `mt-spawn`; unverified.
4. **The suspended-child leak (§2.4) wants an owner, and it is now measured.**
   10 of 10 enforced runs leaked exactly one `POSIX_SPAWN_START_SUSPENDED` child
   that neither `Session::drop` nor the watchdog kills, **including 5 release runs**
   where the tripwire is compiled out and the error is unrelated. Property of the
   error window, not of the assertion. Its own issue.
5. **The lost writer lease (§2.5.1) is a separate item with a different trigger.**
   Debug-only: the provider-side panic takes the provider down, cleanup cannot prove
   the tree is gone, and the store keeps `writer.lock` with the run
   recovery-required (`LeaseLost during overlay`). 5/5 debug, 0/5 release. File
   beside item 4, not merged into it — 4 survives the panic being removed and 5 does
   not.
6. **Ratified item 6 — file the tracking issue. This is a ship-gate, not a
   nice-to-have.** The ratification says FILE ON SHIP and this arc closes nothing;
   #117 was multi-*process* fork and is closed by #129. `publish` must not mark the
   arc complete without it. The issue should carry §2's measurements verbatim, §2.5's
   **reclassification** (availability/correctness, *not* a namespace escape) with the
   structural argument that establishes it, the `events.rs:307` supervisor guard and
   its per-`ThreadId` keying (§2.3) — flagged as a contention-dependent race, not a
   guarantee — and items 3, 4 and 5 above. Plus one **process** finding, below.
7. **A process finding for the same issue, and plausibly this arc's most
   transferable one: in this codebase a documentation correction is itself a likely
   site of a new false claim.** Lesson 28 fired **four times** here, and never in
   fresh prose — every instance was inside a correction, and every instance was found
   by a reviewer rather than by the author:

   1. the original README invariant that slice 0's own README edit corrected;
   2. **the fix for (1) introduced R2-1** — the corrected paragraph attributed
      `mt-spawn`'s non-silence to the supervisor guard, which fires 0/10 on that
      case, in the very text written to remove a false mechanism claim. The same
      sentence turned out to have been copied into three files;
   3. **the fix for F5 introduced S2-1** — the note written to stop a stale
      attribution misleading readers was itself stale three ways, including
      double-counting the file it was written in;
   4. **the fix for S2-1 introduced S3-1** — §8's prose fell one round behind its
      own table, one section away from the §0 note that had just been corrected and
      already stated the right thing.

   Each was caught only because a reviewer ran a *deliberate lesson-28 sweep over the
   corrected text* rather than over the change as a whole. The finding to record is
   **not** "these documents had errors": it is that a correction pass is a
   high-probability site for a new false claim, so correction passes should be
   reviewed as adversarially as code, and a fix worker should sweep its own
   corrections before handing them on. Given that slice 0's own product is a
   measurement record, that may be worth more than the two defects this PR set out to
   record.

   **One cause accounts for several of them, though not for all: re-derivation.**
   Recomputing a value rather than substituting a known one explains S2-1 (an inventory
   re-collapsed by hand), S3-1 (a count corrected while the counted thing moved), and
   two further instances internal sweeps caught — a "30 of 30" exit figure aggregated
   across run sets that were never counted together, and a panic line extrapolated from
   a file's length instead of being run. It does **not** explain R2-1, which was a
   causal misattribution rather than an arithmetic one: the corrected paragraph named
   the wrong mechanism, and no recomputation was involved. An earlier revision of this
   item claimed re-derivation explained *every* recurrence; it does not, and the
   overstatement is corrected here rather than left standing.

   The rule that follows is still worth having — **in a correction pass, substitute
   established values; do not re-derive** — and round 4 supplied fresh evidence for it
   from the opposite direction. S3-1's fix *was* pure substitution over inputs that
   looked stable, and it still went stale one commit later, because the input was not
   stable: the round's own document joined the diff it was counting. So the rule needs
   a companion, which is what §0 and §8 now implement: **where a value is invalidated by
   the act of stating it, do not state it — name the stable part and point at the
   tool.**
8. **Ratified item 7 — correct PR #129's Scope claim**, on the new issue *and*
   as a comment on #129: Python is single-threaded at `fork` (measured 1 thread),
   and Node is multithreaded but on the *ungated* `posix_spawn` path, so removing
   the gate would not unblock it. Slice 0 sharpens this: Node is not blocked
   today, it is **unmediated** today, and `mt-write` is the shape of what that
   costs — bounded by §2.5 to availability and correctness.
9. **Ratified item 1's rename lands across two places, not on this PR's title
   alone.** The arc is renamed to the thread-safe slot model; this PR ships only its
   slice 0. Per round 1's scope ruling, the PR title should name both:
   `test(tracer): measure the ungated multithreaded paths before the thread-safe
   slot model (slice 0)`. The commit description keeps the type and framing both
   round-1 reviews ratified; its body was amended in round 1 to drop the
   silent-in-release claim F1 refuted. The arc rename itself belongs on the graph and
   on the item-6 issue.
10. **Raise the root-document convention on #132.** `impl.md` and every `fix-rN.md`
   are overwritten per arc, so the same path means different things at different
   commits, and the repository root mixes this arc's documents with those of two other
   arcs (§0). Not fixable inside a slice-0 measurement PR.

   The sharper version, and the one worth putting on #132, is that the inventory is not
   stable even across rounds of the *same* arc: each round's `fix-rN.md` takes over one
   more previously-stale path, so **any document that states the count is falsified by
   the document stating it.** Four review rounds were spent substituting that number
   before the generator was identified — the fixed point is not the file editing itself
   but the round's own other deliverable joining the diff. §0 and §8 now state no such
   count at all, which is the only formulation that terminates rather than iterates.
   Round 4's structural suggestions are the durable fix: consolidate the round
   documents into one, or move them out of root to `docs/graphs/dg-nsw71bqq/`.

The two GitHub actions (items 6 and 7) are blocked from this workspace (section 6)
and belong to `publish`. Recording them in a file that ships in the diff is the
handoff; item 6 being a ship-gate is the part that must not be lost.

---

## 8. Files changed

**This section states no line counts at all — only properties that a further round
cannot move.** Three claims, each checkable in one command:

1. **Exactly four content files are touched, and they are these:**

   | content file | what it is | edit character |
   |---|---|---|
   | `crates/umbra-platform-macos/README.md` | documentation | one line deleted — the stale eleven-cases sentence — and additions |
   | `crates/umbra-platform-macos/tests/fixtures.rs` | test | **insertion-only** |
   | `experiments/fixtures/umbra-test-child.c` | test fixture | **insertion-only** |
   | `experiments/fixtures/smoke.sh` | test harness | **insertion-only** |

2. **No production source file is in the diff.** Any path the tool lists that is not
   one of the four above is a root-level `.md` process document of this arc —
   `impl.md`, one `fix-rN.md` per review round, one `ci-fix-rN.md` per CI round — or
   `memoria.lock` when the documentation gate required a re-ack.

3. **Nothing pre-existing was edited in the three insertion-only files**, which is a
   stronger guarantee than matching names: no pre-existing test, helper or doc comment
   can have changed if no line was removed.

Check all three with `jj diff -r wppwptxswssr --stat`: the four paths appear with
`0` deletions except the README's `1`, and no `crates/**/src/**` path appears at all.

**Why no line counts, and why the last of them went this round.** The boundary between
what this section asserts and what it routes to the tool has moved twice, each time
after a stated figure went stale, and the pattern is one generator seen from three
distances: **the act of performing a round changes the number that round states.**

| figure | invalidated by | withdrawn at |
|---|---|---|
| grand total of all files | writing the total (1858 → 1885 in the same edit) | round 1 |
| path total, process-document counts, per-arc inventory | the round's own `fix-rN.md` / `ci-fix-rN.md` joining the diff | fix round 4 |
| per-file content figures and their subtotal | **the round's own code fix editing a content file** — `fixtures.rs`'s additions rose at four separate rounds and `umbra-test-child.c`'s at two, moving the subtotal every time | CI round 4 |

The third row was the gap. Fix round 4's partition assumed the content files were
stable, which held against the threat it was built for — added process documents — and
failed against one nobody modelled: **review rounds that change the code.** The
partition's shape was right and its boundary was one category too narrow.

**Why the figures were removed rather than derived once at a final commit**, which was
the alternative on the table. "The final commit, when no further round can move it" is
not knowable from inside a round: every round of this arc has believed it was the last,
and round 3's S3-1 was precisely a substitution that was correct at the tree it was
taken on and stale one commit later — argued at the time to be safe because its inputs
looked stable. Deferring the derivation keeps the same failure mode and only narrows the
window in which it can fire. Removing the figure ends it.

Nothing is lost by the removal. What round 1's F3 actually protects is a reader who
counts the paths and cannot tell which extras are not production code, and the three
claims above answer that directly — they say which files, what kind, and that nothing
pre-existing moved. A subtotal never said any of that.

Reproduce with `jj diff -r wppwptxswssr --stat` for the full list and
`jj diff -r wppwptxswssr --name-only` for the paths; those commands are the authority,
not this section.

Two notes the claims need, so a reader is not left inferring them:

- **The one deleted line is the README's stale "All eleven direct tracer cases are
  enabled" sentence**, replaced by the thirteen-case table and the sections beneath it.
- **Some process documents in the diff are modifications, some are additions, and
  that too is why no count of them is stated here.** `master` `e44d0db8` carries an
  earlier arc's document at several of those paths — which is where their deletion
  counts come from, and which is the overwrite-per-arc pattern §0 flags for #132 —
  while later rounds' documents are new files. A path showing deletions in
  `jj diff --stat` is one of the overwrites; overwritten content is preserved in
  history at that commit.

Change **`wppwptxswssr`** on `feat/mt-fork`, parent `master` `e44d0db8`.
