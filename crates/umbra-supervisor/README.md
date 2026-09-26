# umbra-supervisor

Synchronous run composition and the debug-control event loop, depending only on
core and the five contracts. This crate owns `src/lib.rs`, `src/run.rs`,
`src/events.rs`, `src/base.rs`, `src/sandbox.rs` and its manifest. It links no
backend and selects none: storage, journal, platform and agent arrive as
contracts, and backend choice stays in registry descriptors.

`umbra_supervisor::run(spec)` executes one command run end to end.
`umbra_supervisor::resume(spec)` — also reachable as `Supervisor::resume`, and as
`Supervisor::recover`, which is the same call — reopens an existing run and
reports whether what the last session left can be reconciled. Checkpoint and
handoff remain `NotImplemented`, and so does reconciliation itself: a reopen
diagnoses, it does not repair.

## `resume(ResumeSpec) -> Result<ResumeOutcome>`

`ResumeSpec` carries the provider registry, the run ID, the workspace as it was at
creation, and the persistence selection. There is nothing to launch, so there is
no `RunLaunch` and no observer.

Admission is **shared with `run`, not restated**. `admit_run` holds everything
that describes the run: registry validation, the strict-remote refusal, the
workspace checks, the refusal of any registry configuring a `namespace` role, the
storage descriptor's persistence-mode capability, and the presence of the journal
role. `validate` calls it and then adds what describes the *launch* — the
`--experimental` acknowledgement, the `RunLaunch` shape,
`experimental-open-rewrite-v1` and the two platform capabilities. `resume` calls it
and adds nothing, because it resolves nothing, rewrites nothing, launches nothing
and never connects the platform role; requiring capabilities it does not exercise
would turn an unqualified claim into a passing check.

That split is a correction, and worth stating as one: this path first re-derived
`run`'s admission and dropped a clause twice — the persistence capability, then the
namespace-role refusal — each time letting a reopen accept a registry `run`
refuses. One list means a third omission cannot be written.

The sequence is then `run`'s opening half with three differences. Storage is opened
with `OpenRunIntent::OpenExisting` rather than `CreateNew`. The journal gate is
narrower: `run` refuses *any* non-fresh journal, which is right for what it guards
— a `CreateNew` that comes back non-fresh is a storage/journal disagreement, not
recovery — while a reopen refuses only what `Overlay::bind` refuses, a checkpoint
or a torn tail, and lets `bind` classify the rest. And nothing launches, so
teardown is `fail_run` with `tree_terminated: true`: honest rather than
pessimistic, because no tree was ever started, and the only teardown a poisoned
session accepts.

`ResumeOutcome::recovery_required` is the verdict, read from the bound session
through `NamespaceSession::requires_recovery` rather than re-derived. `Ok` means
the reopen worked, **not** that the run is usable; a caller that ignores that
field has ignored the point of the reopen.

The writer epoch is not managed here. Every backend already advances it across a
reopen: `umbra-storage-{local,nfs,tar}` read the run's persisted epoch in
`acquire_writer`, add one and write it back durably on every acquisition, and
`umbra-storage-nfs-userspace` derives an `epoch_floor` in `open_run` that is zero
for `CreateNew` and the run's own recorded epoch otherwise, then admits at
`floor + 1` and treats a marker below the floor as the regression it is. A bump
here would be a fifth mechanism racing four.

## `run(RunSpec) -> Result<()>`

`RunSpec` carries the provider registry, a `RunLaunch` (a `CommandLaunch`, or an
`AgentLaunchRequest` that is refused as unimplemented), the approved workspace,
a `RunPersistence` selection, the experimental acknowledgement, and an optional
`RunObserver`. The observer is how status reaches a caller; this crate writes to
no stream and reads none.

The staged order is fixed, and each stage's failure decides what may be released:

1. Validate the request and the registry's declared capabilities. Nothing is
   opened until this passes, so an unqualified configuration fails first. A
   registry that configures a `namespace` role is refused outright at this stage,
   whatever its descriptor declares: stage 5 always binds the built-in overlay
   (`standard_namespace`) and never opens a namespace descriptor, so admitting one
   would silently discard it. `namespace-run-lifecycle-v1` names the capability
   such a provider will have to advertise, and an unqualified descriptor is told
   so first, but declaring it does not admit the run.
2. Inventory the approved workspace and derive the run's `ImmutableBaseContract`.
3. Connect storage, `open_run(CreateNew)`, then `acquire_writer` with
   `TakeoverPolicy::Refuse`. There is no stale-writer takeover.
4. Open the journal against the run's `control/` binding with the same run,
   writer identity and epoch, rejecting a non-fresh recovery state for CreateNew.
5. Bind the namespace over those already-open sessions, handing it the lease so
   one owner renews, releases and mutates.
6. Render enforcement from the run's own root, re-verify the workspace inventory,
   then connect the platform, require exactly one advertised architecture and one
   matching ABI capability `<platform>-<arch>-abi-v<decimal version>`,
   and launch stopped.
7. Drive events, then tear down in order: flush run data, append and flush a
   completion record, close the journal, release the writer, close storage.

`Ok(())` requires a successful root status **and** a clean teardown. A nonzero or
signalled child after a clean teardown is `ErrorKind::ProcessFailed`. A failure
after the lease is taken releases writer authority only when the supervised tree
is provably gone; otherwise the run is left recovery-required with the writer
marker retained, and the primary error is preserved with cleanup damage appended.
A failed launch releases writer authority only when the backend supplies explicit
`launch_tree_terminated` evidence; an error category is never evidence.

A provider bookkeeping error after all process exits preserves the run's success
semantics and is reported through `RunObserver::teardown_warning` (or tracing
when no observer is installed). A `finish_run` error enters the namespace failure
path, preserving the original error and appending cleanup failures.

Provider `timeout_ms` must be positive. Renewal is checked at every provider
boundary and scheduled at half the lease interval, with a 100 ms floor.
Configurations are rejected when the provider timeout consumes half the lease
or the floored interval leaves no timeout headroom before expiry.

## Event loop

`Supervisor::launch_prepared` takes ownership of the stopped tree.
`run`/`step`/`handle_event` maintain process, thread and exec-generation
inventory, service lease renewal between provider/namespace calls (including
ABI memory callbacks and before resuming), and process one syscall at a
time: decode through the negotiated ABI, resolve through the namespace, prepare
(which journals intent and performs copy-up or creation), ask the platform to
prepare the physical rewrite, apply it, then resume to the matching exit, observe
the outcome and commit or abort. A kernel errno is an observed outcome, not an
interception failure: it is aborted as `AbortReason::KernelRefused(errno)`, which
the namespace reconciles instead of poisoning, so the tracee is resumed and the
errno already sitting in its return register stands
([#53](https://github.com/invakid404/umbra/issues/53)). No register work is
needed there, because the rewritten syscall actually executed.

Any failure in that chain poisons the run: nothing resumes afterwards — including
an abort the namespace refuses to reconcile, which is its verdict to give and not
an error the supervisor may swallow. Two things can produce that refusal, and after
[#69](https://github.com/invakid404/umbra/issues/69) only two: an abort whose
claimed errno this session never observed, and a recorded undo the storage backend
would not perform. The overlay now records an undo for *every* prepare-time
logical creation — a shadow file, and so an ordinary creating open
([#55](https://github.com/invakid404/umbra/issues/55)); a materialised shadow
directory since [#64](https://github.com/invakid404/umbra/issues/64), whether an
explicit `mkdir`'s or an ancestor `parents` created over nothing; and since #69 a
logical symlink's backing index, target blob and placeholder as one entry, plus
the ancestors materialised for that placeholder. There is no third case, and in
particular no latch: `Pending.created` was retired with the last arm that set it.

The two exit-path refusal tests in `src/events.rs` drive two different ops and
script two different verdicts: `creating_open_under_an_absent_parent`
(`/newdir/fresh`, which the overlay rolls back down to the ancestor) reconciles,
and `a_symlink_whose_undo_the_backend_can_refuse`, whose four-object undo gives a
backend four chances to refuse, does not. The supervisor only ever *reads* that
verdict and never computes it, so the namespace double reports a scripted one and
models none of `Overlay::abort`'s rule; the op choice is a fidelity claim about
which real verdict each test stands for.

That claim is now checked rather than restated in prose. `tests/kernel_refusal.rs`
binds a real `Overlay` over `LocalStorage` on tempdirs into a real `Supervisor`
through `Supervisor::with_namespace` and drives real syscall entry/exit pairs
([#57](https://github.com/invakid404/umbra/issues/57)): a reconciled `Fchownat`
refusal that resumes the tracee and leaves its copy-up standing, an uncorroborated
abort claim that poisons and undoes nothing, a rollback the storage backend
refuses — which reports the backend's own `Denied`, never the engine's
`InvalidState` — and two transactions in one session reconciled independently.
Since [#60](https://github.com/invakid404/umbra/issues/60) and
[#61](https://github.com/invakid404/umbra/issues/61) it also drives the ownership
carry through the real stack: a write to a read-only base file, which copies up
and takes the base object's ownership, and a creating open beneath a base
directory, which materialises the ancestor with that directory's ownership.
Neither can *discriminate* a carry from a missing one on a single-user CI, where
the base tree is already owned by the test process — the engine's own tests
assert on the emitted `SetMetadata` for that — but they are the only place the
carry runs against a real filesystem through a real syscall pair.
Deleting that file un-pins `Overlay::abort`'s rule, because the double no longer
carries a copy of it.

Only write-mode `Open` and `Fchownat` are reachable end to end today, which is why
the double still carries the rest: `Overlay::resolve` refuses `Chmod` and `Write`
outright, and it emulates a logical symlink rather than letting the kernel execute
one, so the supervisor's `syscall_exit` never reaches a real refusal of that op.
What those tests pin is the supervisor's behaviour when the namespace refuses, not
that the op refuses today.

A `Deny`, reached only for
a whiteout-hidden non-mutating path, is answered here without
minting an operation: the supervisor asks the platform to emulate its errno, which
steps the tracee past the trapped syscall so the still-present base object stays
hidden ([#49](https://github.com/invakid404/umbra/issues/49)). A backend whose
`emulate_result` cannot skip the trap, such as the Linux stub, fails closed
instead. The remaining `Emulate` actions (`ReadLink`, a logical-symlink `Stat`)
still fail closed as a separate wiring gap, because rewriting return registers
alone would execute the very call the namespace refused. The loop ends when every
process has exited, not when the root
does. Fork preserves every inherited descriptor; only Exec drops close-on-exec
entries. An Exit for an uncaptured task poisons the run without reducing its live
process count, and duplicate exits do not count twice.

`Supervisor::new` and `with_namespace` take `Option<Box<dyn Agent>>`; a raw
command run has no adapter. Construction still performs no I/O and starts in
`RunLifecycle::Created`.

## Sandbox

`sandbox::render` embeds `experiments/seatbelt/umbra.sb` with `include_str!` and
substitutes exactly one `{{UMBRA_RUN_ROOT}}` token with a complete quoted Seatbelt
literal naming the opened run's root. It rejects a relative root, `/`, trailing
separators, control characters and non-UTF-8 bytes, and escapes quotes and
backslashes. The module is pure: it validates representation only. Preparation
resolves the run root first, because Seatbelt matches `subpath` against resolved
paths. Installation belongs to the platform backend.

## Base

`base::WorkspaceInventory` walks the approved workspace with bounded, safe
filesystem operations, refusing symlinks and non-regular entries. It is captured
before the run opens and re-verified immediately before launch, so a workspace
that moved underneath preparation fails the run. It detects change; it does not
preserve pre-run bytes, and a content-addressed snapshot under `control/base`
is not implemented.

`base::HostReadOnlyBase` serves the overlay's base layer as a read-only view of
the host filesystem, mapping logical paths back to host paths. Directory pages
skip unrepresentable special entries and advance past them; listings of `/dev`
and its descendants, including resolved aliases, are refused. Freezing is
enforced by the installed sandbox — a tracee may write only inside its own run
root — not by copying the host. One consequence is deliberate: when the namespace
resolves a non-mutating open to `NotFound` — a *truly-absent* target — the syscall
resumes unmodified, because with this base the kernel produces exactly the outcome
the namespace predicted. That equivalence holds only while the base is the host. A
whiteout-hidden target does not take this branch: the namespace resolves it to
`Deny` instead, which the supervisor emulates, so the host object the whiteout
hides is never revealed.

```sh
cargo check -p umbra-supervisor
cargo test -p umbra-supervisor
```
