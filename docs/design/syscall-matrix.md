# Userspace NFS: M2 syscall contract

Design, 2026-09-09; no implementation or qualification claim. Repository reference: `4e58ae4a`. M1 builds the storage provider; M2 supplies the filesystem execution path below; M3 qualifies remote durability and crash takeover. See [failure model](failure-model.md), [fencing](fencing-survey.md), and [M1 spike](managed-lifecycle-spike.md).

## Interpretation and visibility

Statuses are intended M2 behavior for run-backed files: **emulated** through the logical namespace/storage; **passthrough** only for explicitly classified kernel resources; **rejected** before effects; **TODO** is a design gate, never permission to pass an unknown operation to the host. Unsupported flags/suboperations return a platform-appropriate error. Linux-only names describe later Linux ABI coverage. libc wrappers are listed because applications use them; intercept their actual kernel calls, including Darwin nocancel/64-bit aliases and direct syscalls.

- **P (pathname):** fresh anchored lookup reflects server-visible edits, creation, removal and replacement. Never follow physical server symlinks outside the anchor. Logical symlinks remain overlay-owned.
- **H (handle):** reads/stat reference the opened object, including after rename/unlink/replacement. In-place edits are visible on subsequent reads; a replacement affects new opens, not old handles. Pin object identity, not only its pathname.
- **D (directory):** a fresh enumeration sees current server names plus overlay rules. Continuing cursors may be invalidated explicitly; never return an indefinitely cached snapshot on reopening. Already delivered entries/application buffers cannot be recalled.
- **V (virtual):** descriptor offsets, flags, cwd and aliasing are Umbra state, not remote metadata.
- These guarantees begin after the external writer's bytes/metadata reach the server and exclude concurrent conflicting mutations. Read through/revalidate all relevant positive, negative, data and overlay caches. Do not promise cross-client linearizability or automatic repair of externally modified `control/`/`.provider`. External mutation of an approved immutable base invalidates that base contract; run-shadow files remain coherent.
- No remote file operation is passed to a fabricated local path. Pipe/socket/terminal operations may use kernel descriptors under independent enforcement; provider sockets and credentials never reach tracees. Every supported mutation retains the run/operation/epoch and replay semantics from the failure model.

## File and namespace surface

| Calls | Status | Required semantics / external visibility |
| --- | --- | --- |
| `open`, `openat` | emulated | P→H; resolve cwd/dirfd, access and create/exclusive/truncate flags before effects. Truncation is a mutation. Allocate a virtual open-file description, not an NFS socket FD. |
| `close` | emulated | H/V reference release and explicit close-error reporting; last alias closes server state. Closing does not silently certify durability. |
| `read`, `write` | emulated | H; share offset across dup/fork aliases; correct short I/O, EINTR and partial effects. Append serialized among Umbra writers; no atomic-append promise against outside writers. |
| `pread`, `pwrite` | emulated | H; explicit offset, bounded data, no shared seek change; distinguish Linux/Darwin append interactions. |
| `lseek` | emulated | V; SEEK_SET/CUR/END, server-fresh size for END; reject unsupported SEEK_DATA/HOLE rather than invent extents. |
| `stat`, `fstatat`, `lstat` | emulated | P; translated logical identity/mode/owner/size/times. Respect nofollow flags; lstat reports logical symlink itself. |
| `fstat` | emulated | H; reflect in-place size/metadata changes without switching to a replacement. |
| `mkdir`, `mkdirat` | emulated | P; exclusive creation, logical umask/mode and parent checks. |
| `unlink`, `unlinkat`, `rmdir` | emulated | P/H; correct directory flag/type; preserve open objects and other links. Whiteouts stay overlay policy. |
| `rename`, `renameat` | emulated | P/H; atomic supported replacement within the qualified filesystem; journal whiteout/publication recovery. Cross-anchor moves rejected. |
| `renameat2` | emulated | Linux flags=0 as rename. NOREPLACE only if qualified atomically; EXCHANGE/WHITEOUT rejected initially. Never check-then-rename as atomic no-replace. |
| `link`, `linkat` | rejected | M2 initial capability false. No copy-as-link substitution; real-agent acceptance must verify fallback or trigger a scoped capability addition. |
| `symlink`, `symlinkat` | emulated | P; byte target and logical metadata through overlay; no physical escape-capable symlink. |
| `readlink`, `readlinkat` | emulated | P; exact logical target bytes, correct truncation/no terminating NUL semantics. |
| `chmod`, `fchmodat` | emulated | P; supported mode/flag semantics through SETATTR; reject unsupported nofollow combinations. |
| `fchmod` | emulated | H; permissions apply to the opened object. |
| `chown`, `fchownat` | emulated | P; server permission/squash checks; explicit numeric/domain identity mapping, unchanged-ID sentinel handling. **Implementation note (M1):** `fchownat` decodes the sentinel and honours it by refusing the one shape that cannot preserve it; identity mapping is not built, so raw uid/gid reach the kernel against the shadow object. |
| `fchown` | emulated | H; same identity mapping, no false ownership success. |
| `utimensat`, `futimens` | emulated | P/H respectively; NOW/OMIT flags, precision limits documented; never silently update a replaced pathname for futimens. |
| `access`, `faccessat` | emulated | P; correct real/effective identity and nofollow flags; ACCESS is a current probe, not authorization for a later mutation. |
| `opendir`, `readdir`, `closedir` | emulated | libc wrappers over emulated open/enumerate/close; D/H. Bounded buffers, byte names, stable object identity; no host DIR pointer on the wire. |
| `getdents64` | emulated | Linux D; ABI-encoded complete records and opaque cookies, correct EOF/buffer bounds. |
| `statfs`, `fstatfs` | emulated | P/H; translate qualified server capacity/limits/filesystem identity; no invented local free-space answer. |
| `getcwd`, `chdir`, `fchdir` | emulated | V/P/H; logical cwd and pinned directory identity. Revalidate renamed/unlinked cwd; report failure when no valid logical path exists. |
| `dup`, `dup2`, `dup3` | emulated | V/H; shared open-file description, atomic dup2 replacement, CLOEXEC, valid platform flags. Kernel-resource aliases remain kernel-backed and tracked. |
| `fcntl` | emulated | V/H; GET/SETFD, GET/SETFL, duplication and supported record locks. Unknown commands rejected. Lock design must match per-process/OFD distinctions and NFS lock-owner mapping; no local-only locks advertised as cross-client locks. |
| `flock` | TODO | M2 locking gate: qualify flock/fcntl interaction and NFS mapping with git/agent runtimes. Until settled, reject; never report success from a private mutex against external lock users. |
| `fsync`, `fdatasync` | emulated | H; explicit data/metadata barrier, original write errors and verifier recovery; receipts limited by qualification. No host fdatasync surrogate. |
| `pipe` (and `pipe2`) | passthrough | Kernel pipe only; track descriptors, inheritance, poll and read/write dispatch. No persistent file content. |
| `mmap`, `munmap`, `msync` | TODO | File-backed mappings require the design below. Anonymous mappings may pass through after flag/address validation and descriptor classification. |

## Darwin-facing surface

| Calls | Status | Required semantics / external visibility |
| --- | --- | --- |
| `getdirentriesattr` | rejected | Legacy bulk directory API; explicit unsupported response, no fabricated records. |
| `getattrlist`, `getattrlistbulk` | emulated | P/H and D respectively; bounded native attribute packing, returned-attribute masks, fresh size/identity/name. Reject unsupported requested attributes/options honestly. |
| `getxattr`, `setxattr`, `listxattr`, `removexattr` | rejected | Initial xattr capability false, including fd/nofollow forms. Report unsupported, not an empty successful list or discarded write. |
| `exchangedata` | rejected | No qualified atomic data exchange. Several writes/renames are not a substitute. |
| `fclonefileat` | rejected | No reflink/clone guarantee; applications may use ordinary copy fallback. |
| `fcopyfile` | emulated | Library API: qualify its constituent descriptor I/O for data-only copying (H). Metadata/xattr/ACL and clone flags unsupported unless separately qualified; do not promise whole-copy rollback when a library wrapper performs several calls. |
| `getdirentries`, `getdirentries64` | emulated | D; actual Darwin directory syscalls must be covered in addition to the user-facing wrappers above. |

## mmap design gate (required before M2 implementation)

File-backed mmap remains in scope; leaving it permanently rejected does not complete M2. The [Darwin mmap contract](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/mmap.2) motivates separate private/shared behavior. Syscall interception alone does not observe CPU loads/stores to resident pages.

| Mapping | Required design decision and acceptance |
| --- | --- |
| Anonymous | Native mappings allowed under normal platform memory policy; never attach a remote object accidentally. |
| File-backed MAP_PRIVATE | Pin H; initial/faulted bytes and truncation behavior specified. Private COW stores never write back. After COW, do not overwrite private bytes with external changes. Define freshness of clean resident pages; mmap does not inherit a blanket “next read RPC” rule. |
| File-backed MAP_SHARED | Shared aliases and inherited mappings reference H. Design a pager/write-fault mechanism, dirty-range tracking, bounded buffering and remote writeback. A one-time anonymous copy is insufficient. |
| External in-place edit | Define revalidation/invalidation of clean mapped pages and a conflict rule for overlapping dirty ranges. No silent lost dirty data. Require a bounded coherence point (fault, explicit refresh/msync, or documented polling interval); do not promise instantaneous visibility to arbitrary CPU loads. |
| Replacement/unlink | Existing mappings remain on H. New opens/maps use P. Specify unlinked-object retention, truncate-below-mapping/SIGBUS, holes and partial last-page handling. |
| `msync` | MS_SYNC waits for relevant dirty writeback and the advertised barrier; MS_ASYNC queues bounded work with retained errors. Specify INVALIDATE behavior and error reporting. |
| `munmap`, exit, fork, `mprotect` | Define dirty-data ownership after unmap/exit, inherited/shared versus COW behavior, MAP_FIXED replacement and permission transitions. No success path may silently discard promised dirty shared bytes. Quiescence includes dirty mappings. |

M2 design artifact must select a mechanism feasible on supported macOS with the existing security posture, prove controller-loss behavior, and state CPU/page-size/executable-map limits. If a new kernel extension, privileged service or FUSE dependency is required, return for scope approval; do not add it as an implementation detail. File-backed `execve`/`posix_spawn` and dynamic loading require a separate executable materialization/pager decision consistent with this table.

## Notifications decision and real-agent acceptance

**M2 remote file-change notifications are out of scope.** External changes are visible to filesystem operations; notification delivery is a separate facility. NFS reads do not yield a complete change-event stream. Do not watch the dangerous kernel mount, manufacture a local mirror, or claim FSEvents history for remote state.

- `kqueue`/`kevent` for permitted sockets/pipes/timers may pass through. `EVFILT_VNODE` registration on virtual remote descriptors is explicitly rejected. See [XNU kqueue](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/kqueue.2).
- FSEvents is a library/daemon service, not just a syscall. Do not grant tracees that service to observe a fabricated remote path. Agent adapters must disable native remote watchers/use polling; preflight rejects a required watcher mode with no fallback. Later Linux inotify requires the same explicit decision.
- M2 qualification pins exact Codex and Claude Code versions, launch flags, runtime, macOS/architecture, and base fingerprint before coding. At least one real coding-agent workflow must run→edit/build/git→checkpoint→resume on host B; qualify each advertised adapter separately. Polling-mode limitations are product-visible.
- M2 contract design must supply provider-owned open-object tokens and handle-based I/O/metadata/close, with bounded IPC lifetimes. Today's pathname-only Storage convenience methods cannot represent an unlinked open object. Prefer new typed operations under `execute` while preserving the synchronous object-safe trait shape; generic core/storage contracts and overlay consumers own this extension, never a backend-specific CLI branch. M1's internal pinned handles are preparation, not a claim that this public M2 contract already exists.
- Add missing operations found by a source/trace audit before implementing: `readv/writev/preadv/pwritev`, truncate/ftruncate, fcntl subcommands, Darwin `renameatx_np`/`renamex_np`, copy/sendfile, `mprotect`, fork/exec/spawn, FD passing and poll/select readiness. Unknown regular-filesystem access fails closed. Required TODOs (locking, mmap, executable files) must resolve before declaring M2 complete.
- External-client acceptance cases: modify/truncate an open file, atomic pathname replacement, add/remove/rename a directory entry, and change metadata. Verify fresh typed Storage results in M1, then actual tracee syscalls and overlay caches in M2. These are future tests; none runs during design.
