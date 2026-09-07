# Engineering handoff: macOS-first remotely backed filesystem harness for coding-agent sessions

Status: proposed architecture and feasibility plan  
Primary implementation language: Rust  
Initial platform: Apple Silicon macOS  
Second platform: Linux (aarch64 and x86-64)  
Storage target: NFSv4-backed, per-session directory

## 1. Executive decision

This project is viable as an engineering prototype on macOS. It is not yet responsible to promise perfect, production-grade interception of every filesystem mutation made by an arbitrary process tree.

The proposed system launches Codex, Claude Code, or another command beneath a supervisor. The supervisor intercepts filesystem-related system calls made by the command and all of its descendants, translates their *logical* paths into a per-session NFS namespace, and edits the stopped thread's registers or memory so the kernel performs the operation against NFS. A restrictive OS sandbox is the fail-closed layer: if translation misses an operation, the kernel should deny the local write rather than permit leakage.

The macOS implementation is debugger-like, as originally proposed. It does not depend on `LD_PRELOAD`, and a program issuing a syscall directly is not inherently invisible. On Apple Silicon, the tracer can stop before an `svc` instruction, inspect and change syscall arguments, single-step the syscall, and then continue. The difficult parts are not whether registers can theoretically be changed; they are reliably obtaining control of production-signed binaries, attaching every descendant before its first mutation, covering the complete filesystem ABI, and preserving path semantics under concurrency.

Accordingly, the project should begin with a deliberately adversarial feasibility spike. If the macOS spike passes its gates, proceed to the common Rust implementation. If it fails the task-access or descendant-capture gates, the honest choices are to weaken the guarantee or make Linux the strict backend. Do not quietly substitute “usually redirects writes” for the intended invariant.

## 2. Product contract

### 2.1 Intended guarantee

For a supervised process tree:

> Every mutation of a regular filesystem object is either applied inside that run's NFS-backed namespace or rejected. No mutation is silently applied to the host's ordinary local filesystem.

This includes:

- files and directories opened or created beneath arbitrary logical paths, including `/tmp`, a user's home directory, and repository paths;
- Git worktrees and their administrative metadata;
- writes through file descriptors opened by a supervised process;
- rename, unlink, link, symlink, metadata, extended-attribute, and memory-mapped-file mutations;
- filesystem actions by children, grandchildren, and later `exec` images.

The target behavior for the motivating example is:

```text
logical path seen by tracee:       /tmp/some-folder/some-file.txt
physical path used by the kernel:  /mnt/nfs/fsvirt/runs/01J.../root/tmp/some-folder/some-file.txt
```

The physical representation should preserve the hierarchy. A flattened name such as `tmp__some-folder__some-file.txt` creates avoidable collisions and cannot faithfully represent directory traversal, directory file descriptors, subtree renames, hard links, or normal tooling.

### 2.2 What “resume on another machine” means

This is filesystem and agent-session resumption, not live process migration. The handoff sequence is:

1. Quiesce or terminate the process tree on machine A.
2. Commit a clean filesystem checkpoint and release the run lease.
3. Mount the same NFS export on machine B.
4. Start a new supervisor using the same run manifest and logical namespace.
5. Start the agent using its persisted session ID and state directory.

The new process does not retain machine A's registers, RAM, open network sockets, Mach ports, pipes, terminal state, or external daemon state. Worktrees, generated files, agent transcripts, local databases, and other filesystem state can persist if they were inside the supervised namespace and were cleanly flushed.

Codex supports relocating its state root with `CODEX_HOME`; transcripts are stored below its sessions directory, and the CLI can resume a session by ID. `CODEX_SQLITE_HOME` separately controls the SQLite state directory. Claude Code supports relocating its configuration and session history with `CLAUDE_CONFIG_DIR`, assigning a stable project-storage name with `CLAUDE_CODE_PROJECT_DIR_NAME`, and redirecting its internal temporary directory with `CLAUDE_CODE_TMPDIR`. Credentials are a separate concern: Claude Code can use the macOS Keychain, which is not contained in NFS. See the official [Codex environment-variable documentation](https://learn.chatgpt.com/docs/config-file/environment-variables), [Codex resume documentation](https://learn.chatgpt.com/docs/developer-commands?surface=cli), [Claude Code environment-variable documentation](https://code.claude.com/docs/en/env-vars), and [Claude Code session documentation](https://code.claude.com/docs/en/sessions).

### 2.3 Explicit exclusions

The first version does not virtualize or migrate:

- anonymous memory, process state, open sockets, pipes, devices, Mach ports, or Linux IPC;
- remote side effects performed through HTTP, database clients, cloud CLIs, or already-running host daemons;
- kernel pseudo-filesystems such as `/dev`, `/proc`, and `/sys` as ordinary persistent files;
- writes performed on behalf of the tracee by an unrelated, unsupervised process unless its protocol is also mediated;
- secrets held in the Keychain, Secure Enclave, hardware tokens, environment injection service, or external credential helper.

These boundaries belong in the public contract, not only in implementation notes.

## 3. Threat and correctness model

The primary target is ordinary but complicated developer software, including tools that use raw syscalls or unusual paths. The first release need not claim resistance to a deliberately hostile program that detects and attacks its debugger, races the tracer, generates syscall instructions dynamically, or exploits a kernel flaw.

Nevertheless, design for failure containment:

- The translator is the semantics layer.
- The OS sandbox is the enforcement layer.
- Optional audit facilities detect unexplained denials or mutations.
- Anything not confidently translated is denied in strict mode.

On macOS, Endpoint Security can authorize or deny monitored operations but is not a path-rewriting API. It also requires special deployment privileges, so treat it as optional auditing, not the foundation. Apple's [Endpoint Security message API](https://developer.apple.com/documentation/endpointsecurity/message) describes authorization and notification events. Codex itself uses the macOS Seatbelt facility to constrain spawned commands, demonstrating the general availability of a kernel-enforced sandbox on macOS, although a generic product must validate its own supported deployment mechanism; see [Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing).

## 4. Namespace and storage model

### 4.1 On-NFS layout

Use a versioned per-run directory:

```text
/mnt/nfs/fsvirt/runs/<run-id>/
├── root/                         # physical shadow of the logical filesystem
│   ├── tmp/
│   ├── Users/alice/project/
│   ├── fsvirt/agent/codex/
│   ├── fsvirt/agent/claude/
│   └── ...
├── control/                      # never exposed as part of the logical root
│   ├── manifest.json
│   ├── lease.json
│   ├── journal/
│   ├── checkpoints/
│   ├── whiteouts/
│   └── diagnostics/
```

`root` is a sparse copy-on-write shadow, not initially a copy of the whole host. Reads may fall through to an immutable local base. Mutations are copied up or created in `root`. Deletions are represented by whiteouts so that deleting a base file does not make it reappear.

The immutable base is part of the run contract. Record the OS build, architecture, supervisor and agent versions, and fingerprints for relied-upon toolchain/base roots. Machine B must provide a compatible base or materialize the required base objects into NFS before handoff. For a stronger but more expensive mode, copy every object into a versioned NFS base and allow no host fallthrough at all. The sparse default guarantees that session-created and session-mutated filesystem data is remote; it does not make two different host installations magically identical.

Never persist the current physical NFS mount point as the identity of a logical object. Machine B may mount the export at a different physical path. Persistent records use normalized logical byte paths and stable object IDs; the current backend derives physical paths at runtime.

### 4.2 Overlay behavior

For a logical object `L`:

| Operation | Required behavior |
|---|---|
| Read/stat | Use the NFS shadow if present; return not-found if whiteouted; otherwise read the local immutable base. |
| Directory enumeration | Merge shadow and base entries, hide whiteouts and shadowed base names, and provide stable continuation semantics. |
| Create | Create parents as needed and create in the NFS shadow. |
| Open with write intent | Copy the base object and required metadata to NFS, then open the NFS copy. |
| Truncate/chmod/chown/xattr | Copy up first, then mutate only the NFS copy. |
| Unlink/rmdir | Remove the shadow object if present and record a whiteout for the logical base object. |
| Rename | Materialize the source if needed, perform the rename in the shadow, and update source/destination whiteouts atomically in the control journal. |
| Hard link | Materialize the source and create a link within the shadow; preserve identity in metadata. |
| Symlink | Preserve the tracee-visible target while preventing later resolution from escaping the logical namespace. |

The implementation must not use string concatenation plus `canonicalize()` as a security boundary. Resolution must be component-by-component, byte-preserving, symlink-aware, and anchored to already-open directory handles where the platform permits it. Unix paths are byte strings, not necessarily UTF-8 strings.

### 4.3 Symlinks are a first-class design problem

Absolute symlinks cannot simply contain the NFS mount path: doing so exposes physical implementation details through `readlink` and breaks when the NFS mount point changes. Relative symlinks containing `..` can also escape a naïve physical shadow root.

Recommended first implementation:

1. Store the tracee-visible symlink value in control metadata keyed by logical object identity.
2. Put a safe backend representation in the shadow tree.
3. Intercept `readlink`/`readlinkat` and return the logical value.
4. Resolve every later traversal against the logical namespace rather than trusting the host kernel to follow an unchecked physical symlink.

An alternative is to rewrite absolute symlink targets to physical NFS paths and reverse the translation on `readlink`; this is faster but is more fragile around mount-point changes and un-intercepted kernel traversal. Use it only after tests establish that it cannot escape.

### 4.4 File descriptors and pathless mutations

Path translation at `open` time is insufficient by itself. The process model must track:

- current working directory and logical root;
- directory file descriptors used by `*at` operations;
- every open descriptor's logical object, physical disposition, access mode, and inheritance flags;
- descriptor duplication, closing, `fcntl`, fork inheritance, and descriptor receipt over Unix sockets;
- executable memory mappings backed by writable files;
- `mmap` with `MAP_SHARED`, `msync`, and truncation interactions.

If all writable descriptors refer to NFS shadow objects, ordinary `write`, `pwrite`, and shared mappings naturally mutate NFS and usually need no path rewrite. Conversely, a writable local descriptor inherited at launch, received through `SCM_RIGHTS`, or opened during a coverage gap defeats the invariant. Strict launch therefore closes or denies inherited writable descriptors except an explicit allowlist, and strict mode must address descriptor passing before claiming complete coverage.

## 5. Shared Rust architecture

Use a Cargo workspace with a small, platform-independent semantics core and thin platform-specific tracing layers:

```text
Cargo.toml
crates/
├── fsvirt-core/          # logical paths, operation IR, policy, process model
├── fsvirt-overlay/       # NFS shadow, copy-up, whiteouts, safe resolver
├── fsvirt-journal/       # leases, framed log, recovery, checkpoints
├── fsvirt-supervisor/    # event loop, process tree, lifecycle
├── fsvirt-macos/         # Mach/debugger transport and Darwin ABI
├── fsvirt-linux/         # ptrace/seccomp transport and Linux ABI
├── fsvirt-agents/        # Codex and Claude launch/resume adapters
├── fsvirt-cli/           # run, stop, checkpoint, resume, inspect
└── fsvirt-test-child/    # adversarial syscall fixtures
```

The common boundary should describe *filesystem intent*, not platform syscall numbers:

```rust
pub enum FsOp {
    Open { dir: DirRef, path: BytePath, flags: OpenFlags, mode: u32 },
    Stat { dir: DirRef, path: BytePath, follow: bool },
    Rename { from_dir: DirRef, from: BytePath, to_dir: DirRef, to: BytePath },
    Link { /* logical source and destination */ },
    Symlink { target: BytePath, link_dir: DirRef, link_name: BytePath },
    Unlink { dir: DirRef, path: BytePath, directory: bool },
    Chdir { dir: DirRef, path: BytePath },
    GetCwd,
    ReadLink { dir: DirRef, path: BytePath },
    MmapFile { fd: TracedFd, protection: Prot, flags: MapFlags },
    // Additional metadata and filesystem operations.
}

pub enum ResolvedAction {
    AllowBaseRead,
    Rewrite(PhysicalOperation),
    Emulate(EmulatedResult),
    Deny(Errno),
}

pub trait TraceBackend {
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle>;
    fn next_event(&mut self) -> Result<TraceEvent>;
    fn read_memory(&mut self, task: TaskId, address: u64, out: &mut [u8]) -> Result<()>;
    fn write_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()>;
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet>;
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()>;
    fn resume(&mut self, command: ResumeCommand) -> Result<()>;
}

pub trait SyscallAbi {
    fn decode_entry(&self, regs: &RegisterSet, memory: &mut dyn TraceMemory)
        -> Result<Option<FsOp>>;
    fn apply_rewrite(&self, regs: &mut RegisterSet, rewrite: &PreparedRewrite)
        -> Result<()>;
    fn emulate_result(&self, regs: &mut RegisterSet, result: &EmulatedResult)
        -> Result<()>;
}

pub trait NamespaceResolver {
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp)
        -> Result<ResolvedAction>;
}
```

Keep the debugger loop synchronous on a dedicated thread. Debug events are inherently ordered and stop-the-world transitions are easier to reason about synchronously. Send structured operations to storage/journal workers through bounded channels. Do not make the core state machine generally async unless profiling proves a need.

Suggested dependencies include `serde`, `thiserror`, `tracing`, `clap`, `uuid`, `crc32fast`, bounded channels, and `object` for Mach-O/ELF parsing. `mach2` exposes many raw Mach APIs from Rust, including thread state and Mach VM facilities, but should be treated as a low-level binding, not a complete debugger abstraction; see the [`mach2` crate](https://docs.rs/mach2/latest/mach2/). Avoid making an immature GDB-remote crate foundational. A small, tested subset of the remote protocol can be implemented in-tree if that route is selected.

### 5.1 Process model

Maintain a record per process and thread:

```text
ProcessState
  pid / task identity
  parent identity
  architecture and ABI
  exec generation
  logical cwd and logical root
  fd table and directory-fd bindings
  loaded executable mappings
  breakpoint inventory
  child-capture state

ThreadState
  thread identity
  stop reason
  current intercepted syscall
  scratch-memory allocation
  saved instruction/register state
```

Fork clones the logical filesystem context and descriptor table. Exec retains the applicable descriptors but replaces the executable mappings and breakpoint set. PID alone is not a durable identity because PIDs can be reused.

### 5.2 Entry/exit transaction

Each intercepted mutation follows a journaled state machine:

```text
ObservedEntry
  -> Decoded
  -> Resolved
  -> Prepared       (copy-up/whiteout reservation/scratch path)
  -> KernelExecuted or Emulated
  -> ResultObserved
  -> MetadataCommitted
  -> TraceeResumed
```

Every journal frame has a format version, monotonically increasing sequence, operation ID, payload length, and checksum. Periodic compact checkpoints permit replay from the last valid frame after a crash. The recovery algorithm must tolerate a torn final frame and an operation prepared but not committed.

## 6. macOS backend

### 6.1 Recommended implementation strategy

Use three deliberate stages:

1. **LLDB-based proof:** a small LLDB Python prototype validates task access, pre-syscall register rewriting, memory injection, single stepping, `exec`, and descendant capture on the exact supported macOS versions. This is disposable feasibility code, not the product architecture.
2. **Rust supervisor with a debugger transport:** either control Apple's signed `debugserver` using the GDB Remote Serial Protocol, or call Mach/ptrace APIs directly through Rust bindings. Reuse the common Rust IR and overlay engine either way.
3. **Direct Mach backend if justified:** once entitlement, signing, exception forwarding, and descendant behavior are understood, remove intermediary dependencies where doing so improves reliability.

`gdbstub` is primarily for implementing a debug server, whereas this product needs to control a tracee or a debug server. Do not choose it merely because “GDB” appears in the name. LLDB's Debug Adapter Protocol can initiate child debug sessions, but that is not evidence that its behavior is race-free enough for this invariant; child capture remains a test gate. See the [LLDB DAP documentation](https://github.com/llvm/llvm-project/blob/main/lldb/tools/lldb-dap/README.md).

### 6.2 Launch and task control

The Apple Silicon MVP should:

1. Build a sanitized environment and descriptor table.
2. Launch the initial process stopped. The current macOS SDK exposes `POSIX_SPAWN_START_SUSPENDED`; a traced-child launch is another option.
3. Obtain the required task/thread control through the chosen debugger transport.
4. Install exception handling or debugger breakpoints before the tracee runs application code.
5. Discover dyld-loaded images and executable mappings.
6. Enable the fail-closed sandbox before unrestricted execution.

macOS `ptrace` supports tracing, attaching, continuing, and single-stepping, but its public interface does not offer Linux's convenient syscall-entry stop request. That is why the design uses instruction breakpoints or Mach exceptions rather than assuming a `PTRACE_SYSCALL` equivalent. See Apple's archived but still relevant [`ptrace(2)` documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/ptrace.2.html).

### 6.3 Syscall interception on Apple Silicon

For normal dynamically linked code, identify syscall stubs in `libsystem_kernel` and place software breakpoints on their `svc` trap instructions. At that point the syscall number and ABI arguments should already be prepared. On a hit:

1. Stop the other threads in the process for the first correct implementation.
2. Decode the Darwin syscall number and registers into `FsOp`.
3. Read path bytes and structures from tracee memory with strict size limits.
4. Resolve the logical operation against cwd, root, directory FDs, shadow state, and whiteouts.
5. Allocate or reuse per-thread scratch memory in the tracee.
6. Write translated physical paths or structures into scratch memory.
7. Change argument registers to refer to scratch memory.
8. Restore the original trap instruction at the breakpoint site.
9. Single-step the actual syscall.
10. Reinsert the breakpoint and observe the kernel result.
11. Update the logical descriptor/process state and journal.
12. Resume the stopped threads.

Some operations must be emulated. For example, `getcwd` should return the logical cwd, not the NFS mount path, and `readlink` may need to return a logical symlink target. Emulation means skipping the kernel trap, copying the result into tracee memory, setting the ABI's success/error registers, and advancing the program counter correctly.

Direct syscalls are addressed by scanning executable pages for syscall trap instructions (`svc` on arm64, later `syscall` on x86-64/Rosetta) and setting equivalent breakpoints. Scan newly executable ranges after `mmap` and `mprotect`. Self-modifying/JIT-generated trap instructions, Rosetta transitions, and hostile anti-debugging are later hardening work, not MVP promises.

### 6.4 Descendant capture: the principal macOS risk

The supervisor must gain control before a new child can make its first filesystem mutation. Merely attaching shortly after observing a child is a race.

The feasibility spike must separately test:

- `fork`, `vfork`, `posix_spawn`, and `exec`;
- children launched concurrently by multiple threads;
- grandchildren created immediately after child startup;
- helper processes launched by shell scripts, language runtimes, Git, and the two target agents;
- detached/background services;
- failure behavior when a child cannot be traced.

For `posix_spawn`, the translator may be able to force a start-suspended flag while rewriting the spawn attributes. Fork-family operations require a debugger or exception mechanism that reports and stops the new task before user code runs. If this cannot be demonstrated without disabling core platform security, strict native macOS support is a no-go.

Claude Code exposes `CLAUDE_CODE_PROCESS_WRAPPER` for processes it starts, including background services. Use it as a cooperative additional capture route, not as proof of complete mediation, because not every descendant necessarily originates through an agent-specific hook. The setting's current behavior and placement requirements are documented in [Claude Code environment variables](https://code.claude.com/docs/en/env-vars).

### 6.5 macOS fail-closed enforcement

The tracee should see the host for permitted reads but have write permission only to its physical NFS run root and tightly scoped runtime endpoints. A missed local write must fail with `EACCES`/`EPERM`.

This needs a supported deployment story. Seatbelt is the relevant built-in mechanism, but direct use through private or deprecated interfaces can be fragile across OS releases. Validate whether the intended distribution model can create the required policy, whether Codex/Claude's own sandbox can be composed with the tracer, and whether required Unix sockets, toolchains, and read-only SDK paths remain usable. Endpoint Security may provide a second audit signal if the product can obtain its restricted entitlement.

Debugger ownership can also conflict with crash reporters, another debugger, profiling tools, or anti-debugging flags. State this limitation explicitly.

### 6.6 macOS architecture order

Support in this order:

1. Native arm64 processes on a pinned macOS release.
2. Native arm64 over a defined supported-version matrix.
3. x86-64 syscall ABI and Intel Macs only if still commercially necessary.
4. Rosetta-translated process trees.
5. JIT/self-modifying executable pages and hostile direct-syscall generation.

Do not allow an x86-64 or unknown-architecture child to continue in strict mode before its decoder and breakpoints are active.

## 7. Linux backend

Linux uses the same `FsOp`, namespace resolver, overlay, journal, agent adapters, and acceptance suite. Only the process transport and syscall ABI decoder differ.

The baseline implementation should use `ptrace` syscall-entry/syscall-exit stops and enable descendant events for clone, fork, vfork, and exec. Decode both x86-64 and aarch64 registers; verify the reported syscall architecture on every stop. Read and write tracee memory with `process_vm_readv`/`process_vm_writev` where available, falling back to ptrace access. Maintain the same per-thread scratch strategy used by macOS.

PRoot is prior art demonstrating that ptrace can implement user-space path translation and a chroot-like view without privileges; see the [PRoot manual](https://github.com/termux/proot/blob/master/doc/proot/manual.txt). Linux seccomp user notification or `SECCOMP_RET_TRACE` can later reduce interception overhead or provide a more selective trap path, but user notification exposes register arguments, not automatically safe copies of pointed-to path data. The implementation must defend against time-of-check/time-of-use races and validate the syscall architecture. See the kernel's [seccomp userspace API documentation](https://github.com/torvalds/linux/blob/master/Documentation/userspace-api/seccomp_filter.rst).

Linux also permits a second, stronger backend based on a mount namespace plus `pivot_root`/`chroot` and overlay mounts. That may ultimately be the preferred strict Linux implementation because the kernel performs path resolution. It does not invalidate the shared Rust design: policy, manifest, agent adapters, journaling, NFS lifecycle, test fixtures, and much of overlay behavior remain common. The syscall tracer can remain useful for audit, special emulation, and a rootless mode.

## 8. Agent integration

### 8.1 Common launch environment

Give every session a stable logical home and temporary directory, for example:

```text
HOME=/fsvirt/home
TMPDIR=/fsvirt/tmp
XDG_CONFIG_HOME=/fsvirt/home/.config
XDG_CACHE_HOME=/fsvirt/home/.cache
XDG_STATE_HOME=/fsvirt/home/.local/state
```

These are cooperative fast paths: well-behaved tools naturally choose remote-backed locations, reducing interception load. They are not the enforcement mechanism.

Pin agent and supervisor versions in the manifest. Disable automatic agent updates during a resumable run; executable drift can change formats, syscall behavior, and session schemas between hosts.

### 8.2 Codex adapter

The launcher should:

- set `CODEX_HOME` to a logical directory such as `/fsvirt/agent/codex`;
- set `CODEX_SQLITE_HOME` to an explicitly persisted logical directory;
- start the workspace at its logical path inside the virtual namespace;
- record the Codex session ID and exact CLI version in the run manifest;
- use the supported resume command on machine B;
- preserve history settings required for transcript-based resumption.

Codex's sandbox applies to commands it spawns, so test the interaction of its configured sandbox mode with the outer supervisor and NFS paths. Never assume the inner sandbox covers the Codex parent process or replaces process-tree tracing.

### 8.3 Claude Code adapter

The launcher should:

- set `CLAUDE_CONFIG_DIR` to a logical directory such as `/fsvirt/agent/claude`;
- set a stable `CLAUDE_CODE_PROJECT_DIR_NAME` so a different physical checkout path does not create a different project-history bucket;
- set `CLAUDE_CODE_TMPDIR` to the logical remote temporary directory;
- configure `CLAUDE_CODE_PROCESS_WRAPPER` to re-enter or notify the supervisor for agent-launched background processes;
- disable updates for the run;
- configure session retention so automatic cleanup does not delete desired history;
- record the session ID and exact Claude Code version.

Do not automatically copy Keychain credentials into NFS. Machine B should authenticate independently or obtain credentials from an approved secret provider. NFS-resident transcripts and generated files may contain secrets, so require encryption in transit, storage access controls, and documented retention.

## 9. NFS coordination and durability

Use NFSv4 with consistent identity mapping and a single-writer lease per run. Multiple readers may inspect a completed checkpoint, but two active supervisors must not mutate the same run unless a later distributed-concurrency design explicitly supports it.

Recommended lease fields:

```text
run_id
writer_instance_id
host_fingerprint
supervisor_version
acquired_at
renewed_at
lease_epoch
```

Use an atomic create or compare-and-swap mechanism whose behavior has been tested against the actual NFS server. Do not infer safety from local-filesystem tests.

Avoid putting the supervisor's own authoritative control state in SQLite on NFS unless locking, recovery, and cache semantics are tested against every supported client/server combination. Prefer a checksummed append-only journal plus immutable snapshots. Agent-owned SQLite files still need workload testing because their location is controlled by the agent.

A clean handoff should:

1. Reject new child creation and new mutating operations.
2. Stop all tracee threads at known boundaries.
3. Ask the agent to exit cleanly when practical, then terminate remaining background children according to policy.
4. Complete or roll back prepared overlay transactions.
5. Flush tracee-owned files, the journal, and the manifest.
6. Write a checkpoint containing the last committed sequence number.
7. Mark the checkpoint clean and release the writer lease.

After a crash, machine B replays from the last valid checkpoint and journal frame. It must never treat an expired lease alone as proof that the old writer is dead; use fencing epochs or an external lease service if split-brain is possible.

NFS clients will use RAM caches. The meaningful guarantee is that persistent session data does not intentionally land on the machine's local disk, not that bytes never occupy client memory. Disable local offline caching features that could persist NFS contents outside the export.

## 10. Syscall coverage plan

Generate the platform syscall tables from pinned SDK/kernel descriptions where possible, then review them manually. At minimum classify:

- path opens and creation: `open`, `openat`, platform variants, `creat`;
- directory and status operations: `mkdir`, `rmdir`, `stat` families, access checks, directory enumeration;
- namespace changes: `rename` families, unlink, link, symlink, readlink;
- working-directory and root operations: `chdir`, `fchdir`, `getcwd`, `chroot`-like operations;
- descriptor operations: close, dup, `fcntl`, inherited descriptors, descriptor passing;
- metadata: chmod, chown, timestamps, flags, ACLs, xattrs;
- file size and allocation: truncate, fallocate equivalents, cloning/copyfile facilities;
- mappings: mmap, mprotect, msync, munmap;
- execution and children: exec families, fork/vfork/clone equivalents, posix spawn;
- filesystem control calls: ioctl and fcntl commands that mutate files;
- platform-specific bulk-copy, clone, exchange, snapshot, or named-stream APIs.

Unknown filesystem-relevant syscalls are denied in strict mode and logged with enough register/mapping information to add support. An allowlist model is safer than assuming an unrecognized operation is read-only.

## 11. Verification strategy

### 11.1 Synthetic fixture

`fsvirt-test-child` should expose one deterministic subcommand per behavior and support both libc calls and inline raw syscalls. Include:

- absolute, relative, empty, non-UTF-8, and maximum-length paths;
- cwd changes and nested `openat` directory FDs;
- create, overwrite, append, truncate, rename, exchange, unlink, and recreate;
- absolute and relative symlinks, cycles, dangling links, and `..` escapes;
- hard links and inode comparisons;
- writable inherited FDs and descriptors passed with `SCM_RIGHTS`;
- shared writable mappings and writes after unlink;
- fork, immediate child write, exec, posix spawn, grandchildren, and background children;
- multi-threaded simultaneous path and cwd operations;
- raw syscall instructions in the main binary, a loaded library, and newly executable memory;
- crash at every journal transaction boundary.

### 11.2 Leakage oracle

Run tests inside a disposable host tree with filesystem event observation and before/after snapshots. Attribute mutations to the target process tree where possible. The test fails if any regular filesystem object outside the NFS run directory changes, except a small declared allowlist of runtime endpoints. Sandbox denial logs should be part of the test artifact.

This oracle must be independent of the translator's own journal; otherwise the same missed interception can be missed by both the implementation and its test.

### 11.3 Real workloads

Exercise:

- Codex and Claude Code creating and switching Git worktrees at unusual absolute paths;
- Git clones, submodules, worktree administrative files, locks, and atomic renames;
- Node package installation, Cargo builds, Python virtual environments, and compiler temporary files;
- agent session resume on a second Mac with a different NFS mount point;
- NFS disconnects, slow responses, server restart, and stale handles;
- supervisor kill -9 followed by recovery;
- attempted simultaneous writers;
- large repositories and high-small-file-count workloads.

Measure syscall-stop overhead, wall-clock slowdown, NFS operations, copy-up amplification, journal latency, and maximum stop-the-world pause. Package managers and builds may expose a performance ceiling earlier than simple file tests.

## 12. Milestones and gates

### M0 — macOS feasibility spike

Deliver a throwaway but reproducible Apple Silicon demonstrator that:

- launches a child stopped under debugger control;
- breaks before a known filesystem syscall;
- changes `/tmp/fsvirt-proof` to a path on NFS by editing tracee memory/registers;
- lets the syscall complete and returns the expected result;
- repeats the result for a hand-written direct syscall;
- catches an immediate-writing child and grandchild before their writes;
- runs representative signed Codex and Claude Code binaries without disabling SIP;
- denies an intentionally unhandled local mutation with the sandbox;
- records exact OS, hardware, signing, entitlement, and debugger configuration.

Go only if all of these hold:

1. The supported agent binaries can be launched and controlled without an unacceptable system-security configuration.
2. All supported child creation paths can be stopped before first user-code mutation.
3. A fail-closed policy can make only the NFS shadow writable while leaving the tools usable.
4. Direct arm64 syscall sites can be found and trapped with stable stepping behavior.

If gates 1–3 fail, native macOS cannot honestly provide the strict product contract. Stop, revise the contract, or move the strict backend to Linux.

### M1 — common semantics and single-process arm64

- Implement byte paths, the `FsOp` IR, resolver, shadow tree, copy-up, and whiteouts.
- Implement single-process Darwin syscall decoding and rewriting.
- Support open/create/stat/unlink/rename/cwd/readlink basics.
- Add the framed journal and crash replay.
- Pass synthetic single-process tests.

### M2 — process tree and agent adapters

- Track fork/spawn/exec and descriptor inheritance.
- Sanitize launch descriptors and environment.
- Add Codex and Claude adapters and manifests.
- Resume one real session on a second Mac.
- Add process-tree leakage tests.

### M3 — semantic completeness and fail-closed mode

- Complete dirfd, symlink, link, metadata, mmap, xattr, and descriptor-passing policies.
- Integrate the production sandbox story.
- Deny unknown filesystem mutations.
- Pass NFS failure and crash-recovery suites.

### M4 — Linux backend

- Implement ptrace event transport and x86-64/aarch64 syscall ABIs.
- Reuse the same conformance suite and agent adapters.
- Evaluate a mount-namespace backend and seccomp acceleration.
- Demonstrate cross-platform reuse of the same NFS run format.

### M5 — performance and hardening

- Reduce global process stops where correctness permits.
- Cache safe resolution and materialization decisions.
- Add Rosetta/x86 support if required.
- Address newly executable/JIT code and anti-debugging expectations.
- Define a supported macOS/kernel/NFS matrix and update qualification process.

## 13. Acceptance criteria for a strict release

A release is strict only if all of the following are continuously tested:

1. Every attempted regular-filesystem mutation by the supervised process tree lands below the run's NFS root or receives a deterministic denial.
2. The `/tmp/some-folder/some-file.txt` example is transparent: create, reopen, stat, rename, and delete behave as though the logical path were real.
3. Git can create and use worktrees at arbitrary logical paths without exposing the physical NFS mount path in repository metadata where that would prevent migration.
4. An independent leakage oracle observes no undeclared host-local mutation.
5. A clean checkpoint resumes both a Codex and a Claude Code session on a second supported Mac.
6. A crash leaves a recoverable journal and never produces two unfenced writers.
7. Unknown architectures, syscalls, child processes, or descriptor transfers fail closed.
8. Secrets and transcript retention follow the documented storage policy.

## 14. Principal risks

| Risk | Consequence | Mitigation or decision |
|---|---|---|
| macOS task-port/signing restrictions | Cannot control target registers or memory | Prove against real signed agents in M0; do not require disabling SIP for a supported product. |
| Child-capture race | A child writes locally before attachment | Gate the product on pre-execution capture; combine debugger events, suspended spawn, wrapper hooks, and sandbox denial. |
| Unsupported/private sandbox interface | OS update breaks fail-closed policy | Validate distribution mechanism, pin support matrix, add release qualification; otherwise narrow the guarantee. |
| Incomplete path semantics | Escapes or incorrect tool behavior | Central component-wise resolver, explicit symlink/dirfd model, adversarial conformance suite. |
| Writable inherited/received FD | Path interception is bypassed | Descriptor sanitization/tracking, SCM_RIGHTS policy, sandbox backstop. |
| Physical paths leak into state | Resume fails at a different mount point | Persist logical paths only; emulate path-returning calls; test a changed physical mount. |
| NFS cache, locking, or latency | Corruption or unusable builds | NFSv4 qualification, single writer/fencing, append journal, performance tests, server snapshots. |
| Agent or OS update | Decoder/session format changes | Version pinning and explicit qualification matrix. |
| External daemon side effects | Files appear outside the process tree | Exclude or proxy the service; do not imply coverage. |
| Debugger/profiler conflict | Tools cannot attach concurrently | Document exclusive ownership; provide diagnostics. |
| Rosetta/JIT/direct syscall variants | Missed trap sites | Native arm64 first; deny unknown execution modes; add executable-page monitoring later. |

## 15. Recommended first repository deliverables

The first implementation pull request should contain only:

1. A short architecture decision record defining the strict invariant and exclusions.
2. An arm64 Darwin syscall fixture with a libc open and a raw `svc` open.
3. The LLDB proof script and a reproducible NFS test setup.
4. Immediate child/grandchild write fixtures for every creation API under test.
5. A sandbox profile or supported enforcement experiment that permits writes only to the chosen NFS run root.
6. An automated M0 report that says pass or fail for each gate and records the environment.

Do not begin the full overlay engine until this pull request establishes that the three non-negotiable platform properties—task control, race-free descendant capture, and fail-closed enforcement—can coexist.

## 16. Final recommendation

Proceed with M0. The debugger-style approach is technically real on macOS and can include direct syscalls; it is not merely a thought experiment. The strongest design is a Rust-owned logical filesystem and overlay core with a macOS debugger/Mach backend and a Linux ptrace or namespace backend. Most of the difficult semantic work—path resolution, copy-up, whiteouts, descriptor state, journaling, NFS handoff, agent adapters, and testing—can and should be shared.

The project should, however, describe itself as **prototype-worthy, with production feasibility gated by macOS process-tree control**. That wording is not hedging around syscall interception. It identifies the actual uncertainty: whether current macOS security and process-creation behavior allow a shippable supervisor to gain control early enough, for every descendant, while retaining a kernel-enforced no-local-write backstop.

## 17. Source notes

- Apple's public `ptrace` interface: [ptrace(2)](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/ptrace.2.html)
- Apple Endpoint Security event model: [Endpoint Security messages](https://developer.apple.com/documentation/endpointsecurity/message)
- Darwin syscall definitions: [Apple open-source XNU `syscalls.master`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/syscalls.master)
- Linux userspace path-translation precedent: [PRoot manual](https://github.com/termux/proot/blob/master/doc/proot/manual.txt)
- Linux syscall interception facility: [seccomp userspace API](https://github.com/torvalds/linux/blob/master/Documentation/userspace-api/seccomp_filter.rst)
- GDB's syscall catchpoints are target-dependent: [GDB catchpoints](https://sourceware.org/gdb/current/onlinedocs/gdb.html/Set-Catchpoints.html)
- Remote GDB syscall catching depends on stub support: [GDB remote `QCatchSyscalls`](https://sourceware.org/gdb/current/onlinedocs/gdb.html/General-Query-Packets.html)
- Rust Mach bindings: [`mach2`](https://docs.rs/mach2/latest/mach2/)
- Codex state variables and resumption: [environment variables](https://learn.chatgpt.com/docs/config-file/environment-variables), [CLI resume](https://learn.chatgpt.com/docs/developer-commands?surface=cli), and [sandboxing](https://learn.chatgpt.com/docs/sandboxing)
- Claude Code state and process hooks: [environment variables](https://code.claude.com/docs/en/env-vars), [sessions](https://code.claude.com/docs/en/sessions), and [session storage](https://code.claude.com/docs/en/agent-sdk/session-storage)