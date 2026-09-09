# umbra-supervisor

Synchronous run composition and the debug-control event loop, depending only on
core and the five contracts. This crate owns `src/lib.rs`, `src/run.rs`,
`src/events.rs`, `src/base.rs`, `src/sandbox.rs` and its manifest. It links no
backend and selects none: storage, journal, platform and agent arrive as
contracts, and backend choice stays in registry descriptors.

`umbra_supervisor::run(spec)` executes one command run end to end. Checkpoint,
resume, recovery and handoff remain `NotImplemented`.

## `run(RunSpec) -> Result<()>`

`RunSpec` carries the provider registry, a `RunLaunch` (a `CommandLaunch`, or an
`AgentLaunchRequest` that is refused as unimplemented), the approved workspace,
a `RunPersistence` selection, the experimental acknowledgement, and an optional
`RunObserver`. The observer is how status reaches a caller; this crate writes to
no stream and reads none.

The staged order is fixed, and each stage's failure decides what may be released:

1. Validate the request and the registry's declared capabilities. Nothing is
   opened until this passes, so an unqualified configuration fails first.
2. Inventory the approved workspace and derive the run's `ImmutableBaseContract`.
3. Connect storage, `open_run(CreateNew)`, then `acquire_writer` with
   `TakeoverPolicy::Refuse`. There is no stale-writer takeover.
4. Open the journal against the run's `control/` binding with the same run,
   writer identity and epoch, rejecting a non-fresh recovery state for CreateNew.
5. Bind the namespace over those already-open sessions, handing it the lease so
   one owner renews, releases and mutates.
6. Render enforcement from the run's own root, re-verify the workspace inventory,
   then connect the platform, require exactly one ABI capability matching
   `<platform>-<arch>-abi-v<decimal version>` for the negotiated architecture,
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

## Event loop

`Supervisor::launch_prepared` takes ownership of the stopped tree.
`run`/`step`/`handle_event` maintain process, thread and exec-generation
inventory, service lease renewal between provider/namespace calls (including
ABI memory callbacks and before resuming), and process one syscall at a
time: decode through the negotiated ABI, resolve through the namespace, prepare
(which journals intent and performs copy-up or creation), ask the platform to
prepare the physical rewrite, apply it, then resume to the matching exit, observe
the outcome and commit or abort. A kernel errno is an observed outcome, not an
interception failure.

Any failure in that chain poisons the run: nothing resumes afterwards. Denial and
emulation fail closed, because skipping a trapped syscall is not in the platform
contract and rewriting return registers alone would execute the very call the
namespace refused. The loop ends when every process has exited, not when the root
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
the host filesystem, mapping logical paths back to host paths. Freezing is
enforced by the installed sandbox — a tracee may write only inside its own run
root — not by copying the host. One consequence is deliberate: when the namespace
resolves a non-mutating open to `NotFound`, the syscall resumes unmodified,
because with this base the kernel produces exactly the outcome the namespace
predicted. That equivalence holds only while the base is the host.

```sh
cargo check -p umbra-supervisor
cargo test -p umbra-supervisor
```
