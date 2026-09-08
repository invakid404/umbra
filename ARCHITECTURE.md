# Umbra architecture

Umbra supervises a process tree and maps its logical filesystem into a selected
run shadow. Every regular-filesystem mutation must reach that shadow or be denied.
Translation supplies filesystem semantics; an independent kernel enforcement
boundary prevents host writes when translation is unsupported or fails. Strict
remote-backed runs require qualified remote persistence. Local storage is an
explicit development or local-persistence choice and cannot satisfy that claim.

This document describes the required buildout architecture. The Rust workspace is
under construction: contract declarations and backend scaffolds do not establish
working supervision or durability. Provider harnesses/proxies and namespace lifecycle
contracts are implemented; tracing, namespace transactions and backend qualification
remain incomplete. Per-crate READMEs describe local implementation status.

The [engineering handoff](docs/handoff.md) supplies the product and filesystem
semantics. Its illustrative `fsvirt-*` decomposition is superseded by the following
16-member `umbra-*` workspace. [M0 Gate 2](docs/m0/gate-2.md) records the prototype
evidence and outstanding qualification work.

## Crate topology

Each package lives under `crates/` and owns its README. A backend's
library and provider binary belong to the same package. Existing
[fixtures](experiments/fixtures/README.md) are not a seventeenth workspace member.

| Crate documentation | Responsibility | Allowed direct Umbra dependencies |
| --- | --- | --- |
| [umbra-core](crates/umbra-core/README.md) | Byte paths, filesystem operation IR, stable IDs, structured errors, process/thread/fd state, shared DTOs, policy and persistent records | None |
| [umbra-platform](crates/umbra-platform/README.md) | Tracing, syscall ABI, memory and lifecycle contracts; platform protocol | `umbra-core` |
| [umbra-platform-macos](crates/umbra-platform-macos/README.md) | macOS transport, Darwin arm64 ABI, descendant capture and sandbox integration | `umbra-platform`, `umbra-core` |
| [umbra-platform-linux](crates/umbra-platform-linux/README.md) | Linux ptrace, aarch64/x86-64 ABIs and enforcement | `umbra-platform`, `umbra-core` |
| [umbra-storage](crates/umbra-storage/README.md) | Run storage operations, capabilities, durability and writer authority; storage protocol | `umbra-core` |
| [umbra-storage-nfs](crates/umbra-storage-nfs/README.md) | Validated mounted NFS layout, shadow primitives, remote durability and fencing | `umbra-storage`, `umbra-core` |
| [umbra-storage-local](crates/umbra-storage-local/README.md) | Equivalent semantic API for development and local persistence | `umbra-storage`, `umbra-core` |
| [umbra-storage-tar](crates/umbra-storage-tar/README.md) | Indexed tar snapshots, persistent staging and local durability for small runs | `umbra-storage`, `umbra-core` |
| [umbra-overlay](crates/umbra-overlay/README.md) | Namespace contracts and shared copy-up, read-through and whiteout engine; namespace protocol | `umbra-core`, `umbra-storage`, `umbra-journal` |
| [umbra-journal](crates/umbra-journal/README.md) | Durable operation, replay and checkpoint contracts; journal protocol | `umbra-core` |
| [umbra-journal-file](crates/umbra-journal-file/README.md) | Framed checksummed append-only journal, snapshots and recovery | `umbra-journal`, `umbra-core` |
| [umbra-agent](crates/umbra-agent/README.md) | Declarative launch/resume/stop plans and session contracts; agent protocol | `umbra-core` |
| [umbra-agent-codex](crates/umbra-agent-codex/README.md) | Codex plans, logical state roots and session metadata | `umbra-agent`, `umbra-core` |
| [umbra-agent-claude](crates/umbra-agent-claude/README.md) | Claude plans, stable project identity and session metadata | `umbra-agent`, `umbra-core` |
| [umbra-supervisor](crates/umbra-supervisor/README.md) | Ordered event loop, process tree, transactions, recovery and checkpoint/handoff assembly | `umbra-core` and the five trait crates |
| [umbra-cli](crates/umbra-cli/README.md) | `run`, `stop`, `checkpoint`, `resume`, `inspect`; configuration and provider connections | `umbra-core`, `umbra-supervisor`, the five trait crates as needed |

The five trait crates are platform, storage, overlay, journal, and agent. Overlay
also owns the backend-independent namespace engine: it receives `dyn Storage` and
`dyn Journal` and does not select NFS, local storage, tar storage, or a file journal.

## Dependency direction

Dependencies point from consumers to contracts to core. Leaf implementations
depend on their own contract and core. The table specifies allowed edges, not a
requirement to import every allowed crate.

- Core and trait crates never depend on or re-export implementation crates. This
  includes optional, feature-selected, target-specific, build, and dev dependencies.
- Each leaf backend has only its own trait crate and core as direct Umbra
  dependencies. NFS cannot import local storage; the file journal cannot import
  storage; agent adapters cannot import platform implementations. Transitive trait
  dependencies do not authorize additional direct edges.
- Supervisor and CLI depend on contracts, never concrete backends, even in tests.
  Backend-selecting Cargo features and closed backend enums are forbidden. Open
  provider IDs are configuration data; operation/event/error enums remain useful.
- Native OS bindings and target-specific build settings stay in platform backends.
  Shared DTOs belong in core, and per-trait protocols/factories in the trait crate.
- Conformance helpers belong with contracts and use fakes or injected trait objects.
  Tests obey the same dependency boundaries as production code.

Contracts are synchronous and object-safe: constructors stay outside the runtime
trait surface, which has no generic methods, unconstrained associated types, or
opaque `impl Trait` returns. Platform objects may be thread-affine; construct them
on their dedicated control thread. Apply `Send` only where ownership crosses
threads; storage, journal, and agent contracts require it. Use bounded owned worker
messages with explicit ordering rather than making the debugger state machine async.

## Pluggability, per trait

Each trait README contains the backend-author walkthrough, signatures, registration
requirements, and conformance obligations. The lifecycle and IPC surfaces described
here are required design contracts; consult those READMEs for implementation gaps.

| Extension point | Contract and responsibilities | Local walkthrough |
| --- | --- | --- |
| Platform | `TraceBackend` launches stopped processes, reports events, reads/writes memory and registers, and resumes execution. `TraceControl` adds capabilities, whole-tree quiescence and safe termination. | [Adding a new platform backend](crates/umbra-platform/README.md#adding-a-new-platform-backend) |
| Syscall ABI and memory | `SyscallAbi` decodes entries, applies rewrites and emulates results. `TraceMemory` binds reads to the stopped task. A platform supplies a paired control/ABI session with negotiated architecture support. | [Platform contracts](crates/umbra-platform/README.md) |
| Storage | `Storage` owns one run binding, typed shadow operations, atomic writer acquisition/renewal/release, durability receipts and close errors. Namespace policy stays in overlay. | [Adding a new storage backend](crates/umbra-storage/README.md#adding-a-new-storage-backend) |
| Namespace | `NamespaceResolver::resolve` classifies an operation without unjournaled mutation. The required `NamespaceSession` extension prepares, observes results, commits, aborts/reconciles and checkpoints. | [Adding a new namespace backend](crates/umbra-overlay/README.md#adding-a-new-namespace-backend) |
| Journal | `Journal` opens recovery state, appends ordered records, flushes explicitly, replays validated records, publishes checkpoints and reports close failures. Append alone is not durability. | [Adding a new journal backend](crates/umbra-journal/README.md#adding-a-new-journal-backend) |
| Agent | `Agent` supplies capabilities and launch/resume/stop plans, and observes bounded session evidence. Plans carry argv, environment, logical cwd/state roots and stable session identity. | [Adding a new agent backend](crates/umbra-agent/README.md#adding-a-new-agent-backend) |

To add a backend:

1. Create `crates/umbra-<role>-<name>` with its README and inherited workspace
   metadata, and add the package to the root member list. Its only direct Umbra
   dependencies are its own trait crate and core. An alternative namespace backend
   uses `umbra-overlay` and its re-exported storage/journal contracts.
2. Implement the relevant object-safe traits and semantic obligations. Platforms
   qualify stopped launch, descendants, exec, sandboxing and controller loss.
   Storage qualifies containment, idempotency, durability and fencing of existing
   writable descriptors/mappings. Journals accept injected writer authority and
   runtime control bindings, never acquire a competing lease or construct storage.
   Agents return plans for a pinned vendor version; only the supervisor launches
   processes through the tracing backend.
3. Supply a provider executable in that package using the role's server harness.
   Install its descriptor and explicitly configure its executable and opaque
   options in the runtime registry. This
   requires no edits to established traits, factory match arms, supervisor imports,
   CLI features/subcommands, or peer backends merely to add an implementation.
4. Run contract and backend conformance cases, including unsupported operations,
   disconnects and recovery. Prove selection through the CLI connection path with
   a synthetic provider. Qualify platform/service/version claims on their actual
   targets; successful compilation or declared capabilities alone is insufficient.

## Runtime composition without backend linking

Trait objects alone do not explain how an executable constructs an unknown backend.
The specified CLI solves this with provider executables over private local IPC.
Each implementation constructs its own concrete type; its trait crate owns the
generic server harness, protocol schema and client proxy implementing the trait.
CLI connects proxies and passes trait objects into supervisor. Packaging installs
provider executables without adding backend dependencies to either assembly crate.

A provider descriptor contains an open string ID, role, protocol version,
executable path and capabilities. An explicit registry selects installed paths and
opaque provider options; it does not scan PATH. Common transport framing and runtime
registration DTOs live in `umbra-core::provider`; each trait crate owns its role schema,
server dispatch and proxy in `provider`. See [provider configuration](docs/providers.md). Validate role/version/capabilities
before launching the tracee tree. The standard overlay can be constructed through
an `umbra-overlay` factory taking boxed storage and journal contracts and returning
a namespace session. Alternative namespace providers receive role connection
information through the same provider mechanism.

IPC uses framed, versioned, bounded request/response messages with request IDs,
structured errors, owned DTOs and byte arrays for Unix paths. Opaque tokens name
provider-owned resources. Never serialize Rust trait objects, vtables, references
or OS pointers. Journal replay uses bounded pages/cursors on the wire. Platform ABI
decoding requires a separate or multiplexed `TraceMemory` callback lane; keep the
transport available and never hold a connection lock across a callback.

Providers are trusted supervisor components outside the tracee sandbox. Their
private sockets/pipes and descriptors must never be inherited by tracees. Timeouts,
backpressure and disconnect handling are mandatory. Loss of a required provider
blocks mutations while retaining kernel enforcement; the platform keeps tracees
stopped or terminates them safely on controller loss. There is no host-write fallback.

IPC overhead, especially at syscall stops and memory reads, needs measurement.
In-process injection remains useful for tests and library consumers. A downstream
composition binary may statically link implementations, but the specified CLI may
not. A future shared-library mechanism would need a stable ABI of its own.

## State, transactions and handoff

Core owns byte-preserving logical paths, distinct run/object/operation IDs, typed
errors and versioned records. Physical mount roots and control handles are runtime
bindings, never persistent identity. Records retain logical paths, stable IDs,
format and agent/supervisor versions, base/toolchain fingerprints, operation
sequences and fencing epochs. The tracee's `root/` cannot expose `control/`.

The supervisor owns process/thread/fd state and orders each transaction:

```text
ObservedEntry -> Decoded -> Resolved -> Prepared
    -> KernelExecuted or Emulated -> ResultObserved
    -> MetadataCommitted -> TraceeResumed
```

Resolution returns base-read, rewrite, emulation or denial actions. Preparation
journals intent and materializes copy-up/reservations before execution, flushing
intent before irreversible effects when recovery requires it. Commit updates
namespace and fd state only after observing the actual result. Abort must reconcile
already executed writes or leave the run stopped and recovery-required. Unknown
potential mutations are denied; ABI decoding returns `None` only for a positively
classified non-filesystem operation.

Storage owns fenced single-writer authority; the journal records its evidence.
Lease expiry alone cannot permit takeover while old descriptors or mappings remain
writable. Renewal must not depend on indefinitely blocked trace-event reads. Lease
loss prevents mutation and requests quiescence. The file journal stores framed,
checksummed records in the injected control directory on the selected backing
store. A torn final frame may be recovered under documented rules; interior
corruption is an error.

For checkpoint/handoff, reject new mutations and children, quiesce the whole tree,
request orderly agent exit and handle remaining descendants, reconcile transactions,
flush tracee files/backing storage/journal/manifest, publish the durable clean
checkpoint, then release the fenced lease. Journal success alone does not establish
a clean handoff. Resume validates compatibility, acquires safe exclusive ownership,
reconstructs logical state at the new mount root and launches a new agent process
using its stable recorded session. External credentials remain external.

## Evidence and CI scope

[Gate 2](docs/m0/gate-2.md) passed six disposable Python fixture cases on macOS
26.5.1 arm64 with SIP enabled and permits moving to M1. It used a local NFS stub.
Outstanding qualification includes multithreaded fork, raw vfork, non-null spawn
attributes/file actions, broader wait semantics, exec argv[0], copy-up/read-through,
relative paths/dirfds/symlinks/hard links, inherited descriptors, Rosetta/JIT, real
NFS durability, and composition with the independent fail-closed sandbox.

[CI](.github/workflows/ci.yml) runs the four workspace formatting/lint/check/test
commands plus doctests on `macos-14` and `ubuntu-latest`, using the pinned toolchain and cached Cargo registry/build
directories. This runner label maps to Apple Silicon in the
[GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners);
the workflow also checks the host architecture. Each matrix job builds on its native host. CI compilation does not reproduce
the separate Gate 2 environment. Actual Linux and
macOS behavior each require qualification on their supported targets.

The product excludes live process migration, external service side effects,
unrelated daemons, pseudo-filesystems, and external secrets, as detailed in the
[handoff's exclusions](docs/handoff.md#23-explicit-exclusions).
