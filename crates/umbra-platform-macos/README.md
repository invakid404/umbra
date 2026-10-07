# umbra-platform-macos

Darwin arm64 tracing backend. Direct Umbra dependencies are `umbra-core` and
`umbra-platform`; `mach2` and `libc` are gated to `cfg(target_os = "macos")`.
Non-arm64-macOS builds still compile (through `src/unsupported.rs`) — this
covers non-macOS targets and macOS on x86_64. Fallible tracing/control calls
report `UnsupportedCapability`, and capabilities are empty. The pure arm64 ABI
module remains available on those hosts.

The crate implements the v2 experimental tracer described in
`experiments/tracer/umbra_tracer.py` and `experiments/tracer/REPORT-v2.md`
using a hybrid debugger interface: **Apple's `debugserver` over GDB Remote
Serial Protocol** for stop-and-control, and **`mach2` (`task_for_pid` +
`mach_vm_read_overwrite`) for victim-side memory reads**. Rationale: the
signed Apple debugserver provides the attach model used by Python v2;
runtime behavior still requires qualification on each supported OS release.
Direct Mach for memory keeps the decode callback out of the RSP round-trip.
The RSP client (`src/rsp.rs`) is a
small, bounded reverse-connect implementation with framed `$…#CC` packets,
RLE decode, and a per-request deadline.

## Public API

- `MacosTraceBackend` implements `umbra_platform::TraceBackend` and
  `umbra_platform::TraceControl`.
- `DarwinArm64Abi` implements `umbra_platform::SyscallAbi`; ABI decode and
  path rewrite live in `src/abi.rs` as a pure module (no debugger I/O),
  matching the spec's testable-core requirement.
- `Options { debugserver, twin_cache, timeout_ms }` — runtime configuration.
  `debugserver` overrides the `xcode-select -p` discovery; `twin_cache`
  overrides the `$HOME/Library/Caches/umbra/twins` default. No user-specific
  paths are baked into the code. When `debugserver` is unset, the
  `UMBRA_DEBUGSERVER` environment variable overrides discovery before
  `xcode-select -p` is consulted; it exists so the test suites can run on
  hosts whose selected Xcode keeps debugserver under `SharedFrameworks`
  rather than the `Library/PrivateFrameworks` path probed below.

## Shipped mechanisms

The M1 mechanisms named in the tracker spec are all present:

1. **Resign twin cache** at `$HOME/Library/Caches/umbra/twins/<sha256>/<basename>`
   via `codesign -f -s - --entitlements … --preserve-metadata=identifier,flags,runtime`.
   Entitlements match `experiments/gate-1/ent.plist`; a copy is shipped as
   `ent.plist` alongside `src/`.
   Both the cache root and each digest folder are created with mode `0700`,
   and that mode is enforced on existing directories. A cache directory owned
   by another user causes a hard error when permissions cannot be enforced;
   the cache is not silently trusted.
2. **Arm64 `svc #0x80` breakpoints** verified byte-for-byte in
   `libsystem_kernel` for `__open`, `__open_nocancel`, `__openat`,
   `__openat_nocancel`, `__execve`, `__posix_spawn`, `__fork`,
   `__wait4`, `__wait4_nocancel`, and the dirfd-relative family below.
3. **Bounded 4 KiB path decode** from `x0`/`x1` with page-boundary-aware
   chunked reads (`abi::read_path`).
4. **Rewrite** via scratch allocation on the tracee task plus register
   update (`Session::allocate` + `abi::prepare_path`).
5. **`svc+4 → b .` fork gate**: the parent's return site is patched to an
   infinite branch-to-self before executing fork; the child's COW-inherited
   copy spins there until we `task_for_pid` + attach a fresh debug session,
   restore bytes at the gate and at each inherited breakpoint site in the
   child, then reinstall the child's own breakpoints. See
   `native.rs::ReturnKind::Fork`.
6. **`__wait4` / `__wait4_nocancel` deferred wait**: debugger attach
   transiently reparents children, so parents get `ECHILD`; the tracer
   intercepts and defers, and return breakpoints track reaped children.
   See `native.rs::ReturnKind::Wait` and **Wait decisions** below.
7. **SIGHUP suppression** on newly attached children.
8. **Embedded watchdog**: an `mpsc`-cancellable worker thread terminates
   all held Mach tasks on deadline (`native.rs::Watchdog`).
9. **Userspace-routing interposer**: `interpose/umbra_interpose.c`, compiled
   universal (arm64 + arm64e) by `build.rs`, embedded in this binary and
   published into the twin cache at launch. `LaunchPolicy::interpose` loads it
   into the target image through `DYLD_INSERT_LIBRARIES`;
   `LaunchPolicy::descriptor_limit` fences `RLIMIT_NOFILE` soft *and* hard before
   the exec. The library ships **inert** and is switched on by
   `Session::arm_interposer`, which writes its `__DATA,__umbra_arm` control block
   after `install_image` has breakpointed its traps — it has no constructor and
   reads no environment. See **What interception does not cover** below.
10. **Inherited arming across `fork`**: a forked child of a routed tracee starts
    with the interposer already mapped *and* already armed, because `fork` copies
    `__DATA,__umbra_arm` with the rest of the address space. So `install` finds
    the load address in the child's own image list instead of running it to
    `main` — a forked child is past `main` and never reaches it again — and
    skips `arm_interposer`, whose zero-block precondition stays strict rather
    than being relaxed for this path. Both halves are required and the first is
    the safety-critical one: running the child to an entry point it cannot reach
    resumed it with nothing breakpointed, so it executed **unmediated** until its
    first routed call became an `svc` the kernel does not know and took `SIGSYS`.
    A child forked *before* its parent armed carries zeroes and is armed here
    like any other image. See `Session::interposer_armed` and
    `userspace_run.rs::a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes`.
    A routed descriptor survives the fork as well, offsets included, because the
    inherited-descriptor rule below is not limited to kernel descriptors —
    measured, a child's write through its parent's routed descriptor continues
    the parent's write in the store
    (`a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store`).
    **"Offsets included" means copied, not shared, and this item used to leave
    that open.** `track_process` clones the parent's `ProcessContext` by value
    and `FdState::offset` is a plain `u64` with no Open File Description
    indirection anywhere, so the fork hands out two offsets where POSIX gives
    one. Measured: with `seed` written before the fork, `child` written by the
    child through the inherited descriptor and `parent` written by the parent
    afterwards through the same descriptor, the object holds `seedparent` — the
    parent's third write lands at the offset its own first write left. POSIX
    requires `seedchildparent`. The claim above is unaffected; its converse is
    what fails, and `a_forked_child_s_write_and_its_parent_s_next_write_do_not_share_one_offset`
    pins the copy. That test is GREEN at this pin, because it asserts the
    divergent bytes and excludes the POSIX ones; it turns RED when shared-OFD
    routing lands, and is then the case to delete rather than to repair.
11. **Transient signals at a return gate**: a stop whose PC equals a pending
    return gate is a syscall return only when a breakpoint trap produced it, and
    a non-`SIGTRAP` stop there is either noise or the `SIGSYS` of an
    unintercepted routing trap. The two are separated by one list,
    `TRANSIENT_SIGNALS`, shared with the general stop path: `SIGCHLD` and the
    other attach/notification signals are continued over without being
    forwarded, and everything else — `SIGSYS` first — fails the run loudly.
    Absorbing `SIGSYS` here is the exact defect the gate exists to refuse;
    failing on `SIGCHLD` took `fork`/`wait4` runs out over an ordinary event.

12. **Interposer retargeting across `exec`**: the routed image is the one this
    session is *currently running*, not the one named at launch, and
    `Session::retarget_interposer` is what moves it — called at the exec stop and
    from `attach_child`, which covers the `posix_spawn` half. `exec` swaps the
    image and zeroes `__DATA,__umbra_arm`, so unlike the forked child of item 10
    the new image is mapped **inert** and is armed from scratch; `install` takes
    its fresh-image branch and `arm_interposer`'s zero-block precondition holds
    by construction. Before this, the requirement was pinned to the launch
    target, so a child that exec'd any other binary was **half**-mediated: its
    `open` was still routed through the breakpointed stubs and returned a virtual
    descriptor, while `write` — interposed only, never breakpointed — reached
    libc with a number the kernel does not own and answered `EBADF`. It is
    deliberately **not** called for a freshly attached root, which is what keeps
    the sandbox installer's inserted copy dormant. See
    `userspace_run.rs::a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image`
    and, for the failed-`execve` case that must not leave a later `fork` pointed
    at an image the tracee never ran,
    `a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran`.

## What interception does not cover

Stated here rather than implied, because two of these are architectural and a
future reader should not have to rediscover them.

**A syscall the tracee issues from memory it wrote itself is not intercepted.**
`install_image` scans the main image's executable sections and, for a routed run,
the interposer's; an `svc #0x80` at neither site has no address to breakpoint.
Measured on this host: a process allocates memory, writes `movz x16,#20; svc
#0x80; ret` into it, flips the page to `r-x` with no entitlement, and executes
it — the syscall runs unmediated. This is inherent to intercepting syscalls at
*known* code sites and is identical for library interposition and for
stub-breakpoint tracing; injection does not help, because injected code has the
same property.

What this bounds is the *claim*: "filesystem operations issued through libc, or
through the umbra interposer, are mediated" — not "all syscalls are mediated".
What fail-closes host writes is the **kernel-enforced Seatbelt profile** this
backend installs before the target's first instruction, never the interception.
Closing the gap needs kernel-assisted whole-process syscall filtering, which is
outside the accepted setup (Command Line Tools plus developer debugging
authorization, and nothing else).

**Writable shared file mappings are unreachable in principle.** A store to a
resident page is not a call of any kind, so neither a breakpoint nor an
interposed function observes it. This needs a pager, and is tracked separately
in `docs/design/syscall-matrix.md`.

**Library initializers run before the syscall sites are planted, on a routed
launch, and the interposer is inert until they have.** Finding the interposer's
load address requires letting dyld map it, which means running the tracee to its
`LC_MAIN` entry point — see `Session::wait_for_image` for the two ways of
watching dyld that were tried and do not work. So the window exists, and what
makes it survivable is the arming order: `Session::arm_interposer` writes the
library's `__DATA,__umbra_arm` control block *after* `install_image` has
breakpointed its traps, and before that write every interposed call passes
straight through to libc.

**That window is strictly larger than a rewrite-backed run's, and saying they
are the same was wrong.** A rewrite-backed run plants its breakpoints at the
dyld image-notifier stop, which *precedes* every initializer — so an
initializer's `open` is intercepted and rewritten into the store. A routed run
cannot: `wait_for_image` has to run the tracee to `LC_MAIN` first, so
initializers execute with no breakpoints planted and the interposer inert.

What happens in that window, measured with the same program on both backends —
a constructor doing `open(<workspace>/from-ctor.txt, O_CREAT|O_WRONLY)`:

| | routed | `local` |
| --- | --- | --- |
| result | exit **41** (`EPERM`) | exit **0** |
| file | nowhere — the Seatbelt profile refused the write | in the store at `<run>/root/<abs path>` |

So a program that writes from an initializer works on every other backend and
fails on a routed run. It fails *closed* — reads in the window reach the host
unrouted, writes are refused by the kernel-enforced profile — but it is a real
behavioural difference, not an equivalence.

This is not a preference. An earlier version let the library arm itself from its
own constructor, which put it live and un-breakpointed for that whole window:
measured, a program whose `__attribute__((constructor))` opened a file turned
that `open` into an `svc` the kernel does not know, took `SIGSYS`, and killed the
launch before `main` with an undecoded debugger stop-reply for a diagnosis and a
recovery-required run behind it. Any dynamically linked program with a
file-touching initializer, an ObjC `+load`, or its own inserted dylib was
affected. Do not move the arming earlier, and do not give the library a way to
arm itself: umbra owns that instant because only umbra knows when the traps
exist.

Unarmed is therefore a *passthrough*, not a refusal, which is why `install`
fails the launch outright when it cannot find or arm the library rather than
continuing with an inert one.

### Dirfd-relative filesystem calls

`src/abi.rs` classifies these numbers, resolved from the selected SDK's
`sys/syscall.h`, and decodes each operand layout. Slot N is register xN.

| Syscall | Number | Installed stub | Operands | Decoded as |
|---|---:|---|---|---|
| `renameat` | 465 | `__renameat` | x0/x1 source, x2/x3 destination | `FsOp::Rename` |
| `renameatx_np` | 488 | `__renameatx_np` | renameat plus flags in x4 | `FsOp::Rename` when flags are zero |
| `linkat` | 471 | `linkat` | renameat plus flags in x4 | `FsOp::Link` |
| `unlinkat` | 472 | `__unlinkat`, `unlinkat` | x0 dirfd, x1 path, x2 flags | `FsOp::Unlink` |
| `symlinkat` | 474 | `symlinkat` | x0 target, x1 dirfd, x2 name | `FsOp::Symlink` |
| `mkdirat` | 475 | `mkdirat` | x0 dirfd, x1 path, x2 mode | `FsOp::Mkdir` |
| `faccessat` | 466 | `faccessat` | x0 dirfd, x1 path, x2 mode, x3 flags | `FsOp::Access` |
| `fchmodat` | 467 | `fchmodat` | x0 dirfd, x1 path, x2 mode, x3 flags | `FsOp::Chmod` |
| `fchownat` | 468 | `fchownat` | x0 dirfd, x1 path, x2 uid, x3 gid, x4 flags | `FsOp::Fchownat` |
| `fstatat` | 469 | `__fstatat` | x0 dirfd, x1 path, x2 buffer, x3 flags | `FsOp::Stat` |
| `fstatat64` | 470 | `fstatat` | same layout | `FsOp::Stat` |
| `readlinkat` | 473 | `readlinkat` | x0 dirfd, x1 path, x2 buffer, x3 length | `FsOp::ReadLink` |
| `symlink` | 57 | `symlink` | x0 target, x1 link name | `FsOp::Symlink` |
| `readlink` | 58 | `readlink` | x0 path, x1 buffer, x2 length | `FsOp::ReadLink` |
| `mkdir` | 136 | `mkdir` | x0 path, x1 mode | `FsOp::Mkdir`, anchored at the cwd |
| `chdir` | 12 | `chdir` | x0 path | `FsOp::Chdir`, anchored at the cwd |
| `fstat` | 339 | `fstat` | x0 descriptor, x1 buffer | `FsOp::Fstat` |
| `fstat` | 189 | `__fstat` | same layout | `FsOp::Fstat` |
| `setattrlistat` | 524 | `setattrlistat` | x0 dirfd, x1 path, x2 attrlist, x3 buffer, x4 size, x5 options | `FsOp::SetTimes` |
| `getattrlistbulk` | 461 | `getattrlistbulk` | x0 descriptor, x1 attrlist, x2 buffer, x3 size, x4 options | `FsOp::ReadDir` |
| `fchdir` | 13 | `fchdir` | x0 descriptor | `FsOp::Fchdir` |
| `close` | 6 | `close` | x0 descriptor | `FsOp::Close` |
| `close_nocancel` | 399 | `__close_nocancel` | same layout | `FsOp::Close` |

The bare forms are there for the reason `symlink` and `readlink` are: they are
what the utility actually issues. `/bin/mkdir` issues `mkdir`(136) and nothing
else, and `utimensat` carries no `svc` at all — it builds an `attrlist` and
tail-calls `setattrlistat`, which is why that row exists and no `utimensat` row
does. `fstat` is the first **descriptor-relative** call here: it names no path,
so it is absent from `abi::path_operands` by design, and the supervisor applies
the descriptor fence to it before resolving so libSystem's own `fstat` on a
kernel descriptor passes through.

The last four rows are the directory read, and they are descriptor-relative for
the same reason — all four are absent from `path_operands` and all four take the
fence. They are a set rather than a list: `/bin/ls` imports **no** directory
symbol at all, so every one of them comes from `fts` inside `libsystem_c`, where
`DYLD_INTERPOSE` cannot reach and the interposer's `close` never sees them. `fts`
saves its working directory with a bare `open(".")`, `fchdir`s to it, reads with
`getattrlistbulk`, and releases the descriptor through `__close_nocancel` — so
routing three of the four leaves `ls` failing on the fourth. `close`(6) is both
interposed *and* breakpointed, which is not a duplication: the interposer catches
the calls an image makes through its own stubs, and the stub row catches
`libsystem_c`'s internal ones.

`setattrlistat` accepts exactly the attrlist shape libc's `utimensat` emits —
`bitmapcount` 5, `commonattr` limited to `ATTR_CMN_MODTIME|ATTR_CMN_ACCTIME`,
one `timespec` per set bit in **ascending bitmap order** so modtime precedes
acctime, `options` limited to `FSOPT_NOFOLLOW|FSOPT_UTIMES_NULL` — and refuses
every other shape whole, because the bitmap dictates the buffer's layout and an
unmodelled bit is a different buffer rather than a flag that could be dropped.

**This table and the tracer read one source.** `abi::TRACED_STUBS` carries every
stub name with the syscall its `svc` issues and what `intercept()` must do with
it; `install()` plants the breakpoints from it and `intercept()` matches
exhaustively on its `Delivery`. There were two lists once, and they drifted:
four stub rows were added to one and not the other, so every breakpoint they
planted fell through to the interceptor's refusal and stopped the run. Stub
names are the ones that actually carry the `svc`, and the number beside each is
**verified against the resolved symbol on every launch** rather than trusted —
`install()` reads the `movz x16, #imm` preceding the `svc` and refuses to plant
a breakpoint if it disagrees. Several have no `__`-prefixed form; `unlinkat`'s public wrapper
carries its own `svc` instead of tail-calling `__unlinkat`, so both are
installed. `fstatat` and `fstatat64` are **one symbol reaching 470**, while
`__fstatat` is a separate stub reaching 469 — libc's `fstatat()` does not reach
469, so both numbers are decoded.

- **Dirfds are signed 32-bit.** `abi::dir_ref` reads the low word, maps
  `AT_FDCWD` (-2) to `DirRef::Cwd` and every other value to `DirRef::Fd`, and
  keeps relative path bytes as they are. Anchors are resolved by the namespace
  against the tracee's own root, cwd and tracked descriptor identity — never
  the tracer's cwd, `/dev/fd`, or an `F_GETPATH` host path, which would make a
  physical path stand in for a logical one. An invalid descriptor is decoded
  rather than rejected: an absolute path ignores its anchor.
- **Unmodelled flags are refused, not dropped.** `abi::at_flags` truncates the
  flag register to its `int` and rejects any bit outside the ones the decode
  represents. `renameatx_np` with `RENAME_SWAP`, `RENAME_EXCL` or
  `RENAME_NOFOLLOW_ANY` is refused rather than downgraded to an ordinary
  rename; flags zero is an ordinary rename.
- **`symlink`/`symlinkat` x0 is not a pathname operand.** It holds the literal
  target bytes the tracee reads back, so it is never physicalized. `symlink`
  has no dirfd, so its link name anchors at the process cwd.
- **`faccessat` and `fchownat` decode into typed operations.** `faccessat`
  becomes `FsOp::Access`: `AccessMode` carries the `R_OK`/`W_OK`/`X_OK` intent,
  all-false being the bare `F_OK` existence probe, and `AccessFlags` carries
  `AT_EACCESS` alongside the usual `follow`. A mode bit outside
  `R_OK|W_OK|X_OK` is refused rather than narrowed to the ones it models.
  `fchownat` becomes `FsOp::Fchownat`. Its `uid`/`gid` operands are unsigned,
  so the caller spells "leave this one alone" as -1 and it arrives as
  `0xffffffff`; each decodes independently to `None`, never to a literal ID
  4294967295. Both reach the kernel against a path the namespace rewrote, so
  the probe and the ownership change act on the overlay's own object rather
  than on the tracee's host path.
- **Only the `*at` forms are mediated.** `access` (33), `chown` (16), `lchown`
  (364) and `fchown` (123) are absent from the breakpoint list above and from
  `decode_entry`, so a tracee calling them runs natively against its own host
  path, outside the namespace. On Darwin `access()` is its own syscall rather
  than a `faccessat` wrapper, and it is the more common libc entry point, so
  decoding 466 and 468 narrows the unmediated surface without closing it.
- **`linkat` and `fchmodat` decode but do not execute.** The overlay MVP
  answers `FsOp::Link` and `FsOp::Chmod` with
  `operation requires descriptor or metadata support beyond MVP`; hard links
  and mode changes need copy-up, identity and mode semantics that are not
  built. `fchownat` is the neighbouring row that does execute: ownership is
  the one metadata mutation the overlay carries through today, so the two
  rows do not behave alike.

`abi::path_operands` gives each syscall's operand-to-slot map, and
`abi::prepare_paths` (with `MacosTraceBackend::prepare_physical`) allocates one
bounded, separately NUL-terminated scratch buffer per operand. Every declared
operand must appear exactly once, so a missing, duplicated or unexpected
operand is an error rather than a half-applied rewrite. Non-path arguments,
dirfds included, keep their original registers: a qualified physical path is
absolute and the kernel ignores the anchor for an absolute path.
`abi::prepare_path` and `prepare_rewrite` remain the single-path wrappers.

### Logical symlinks

The overlay stores a symlink as a placeholder object plus its target bytes in
control metadata, never as a filesystem symlink, so creation resolves to an
emulated result: the tracer steps the PC past the `svc` and the syscall never
runs. Reads are answered from that metadata. Three ABI pieces make that work:

- **Readlink output buffer.** `abi::readlink_buffer` reads the tracee's buffer
  and length — x1/x2 for `readlink`, x2/x3 for `readlinkat` — and the caller
  binds them with `NamespaceSession::set_readlink_buffer` before resolution.
  The ABI reads the full 64-bit `size_t` length and rejects values above
  `MAX_IO_BYTES` before narrowing to the buffer contract.
  The overlay then truncates the target to that length, appends no NUL, and
  returns the number of bytes copied.
- **No-follow stat.** `abi::encode_stat` writes Darwin's `struct stat` (the
  144-byte `__DARWIN_INODE64` layout; offsets read from the host's `sys/stat.h`
  through `offsetof`) at the buffer `abi::stat_buffer` reads — from x2 for the
  `fstatat` family, x1 for `fstat`/`__fstat`, which differ only by the leading
  dirfd/path pair the at-form carries. A logical
  symlink is reported as `S_IFLNK` with its target length, so a no-follow stat
  never exposes the empty placeholder as a regular file. Object kinds and modes
  the layout cannot represent fail explicitly rather than guessing.
- **Link loops.** `umbra_core::ErrorKind::SymlinkLoop` is a distinct kind, so a
  caller can tell loop exhaustion from a containment failure without reading an
  error message, and containment failures stay `InvalidPath`. The overlay's
  overflow (`engine.rs:1022`) attaches `Errno(62)` to that kind and
  `Overlay::hidden_or` translates the pair into
  `Ok(ResolvedAction::Deny(Errno(62)))`. **Which paths that reaches is the
  whole of it.** A tracee whose operand resolves through `hidden_or` —
  `chdir`'s own resolver at `engine.rs:2222`, and `resolve`'s main operand
  resolution at `:2594`, which is the ordinary `open`/`stat` route — is
  answered `ELOOP` and continues:
  [#177](https://github.com/invakid404/umbra/issues/177) item (a). A `rename`
  whose *destination* operand exhausts the bound still ends the run, because
  that operand reaches `resolve_path_follow` through a bare `?` at `:2865` and
  so reaches no translator at all; that gap is filed as
  [#180](https://github.com/invakid404/umbra/issues/180) rather than fixed
  here. The kind is still not itself a wired errno path: nothing in
  production maps it to a number, and the translation is gated on the kind while
  the errno supplies the value. Tracee errnos are produced by
  `Ok(ResolvedAction::Deny(Errno(N)))` at the refusal site, per the idiom at
  `engine.rs:2615`, `:2853` and `:3046`. The overlay's 40-expansion bound is
  unchanged. There is no fallback to native symlink traversal when logical
  resolution fails.

  **This crate's own harness hid that defect for the life of it.** The tree's
  only `ErrorKind::SymlinkLoop => Errno(62)` mapping is in
  `tests/fixtures.rs`'s `denial`, and because that helper reads
  `error.errno.unwrap_or_else(..)` with a kind map behind it, `symlink_cycle`
  emulated the answered-`ELOOP` outcome while production still raised and ended
  the run. The case was re-run with both halves of the fix reverted and still
  passed. A harness oracle that can supply an errno production does not have is
  not an oracle for that errno, however loud its remaining `_ => panic!` arms
  are; the kind map now duplicates a translation the engine performs, rather
  than standing in for a missing one.

### Wait decisions

`native.rs::wait_plan` is the whole decision for an intercepted `wait4` (7) or
`wait4_nocancel` (400), separated from session I/O
so it has a truth table for a test. That test —
`native.rs::tests::wait_plan_decides_from_the_callee_visible_arguments` —
needs no debugger, fixture or environment variable.

- `Poll` — `WNOHANG` over children that are live but unfinished. The tracer
  writes `x0 = 0`, clears the CPSR carry bit, steps past the `svc` and resumes.
  The syscall never runs, `status`/`rusage` are not touched, and no child is
  marked reaped.
- `Park` — a blocking wait in the same state: the thread parks until a child
  finishes, then takes the return breakpoint.
- `Native` — nothing tracked matches, or a match has already finished. The
  kernel answers, so `ECHILD` and the real `status`/`rusage` writes survive.
- `Unsupported` — process-group selectors (`pid` 0 or negative other than -1)
  and any option bit outside `WNOHANG`, refused rather than mis-emulated.

`WNOHANG` is mask value 1 — the least significant bit, not `1 << 1`. The
option register `x2` is truncated to 32 bits before it is read: arm64 leaves
the upper half of a register holding an `int` argument unspecified and the
kernel's argument munger drops it, so reading all 64 bits turned legal
`WNOHANG` callers into refused option masks. The `wnohang-wait` fixture case
exposed that; the raw-`svc` leg with high bits set in `x2` guards it.

## Provider binary

`src/main.rs` is the shipped provider binary. It calls
`umbra_platform::provider::serve_provider("macos", …)` returning a
`PlatformSession { control: MacosTraceBackend, abi: DarwinArm64Abi }`, and
decodes JSON `Options` from opaque provider options (empty falls back to
`Options::default()`). Register it via `umbra providers --registry <path>
--role platform`.

**Launch and enforcement.** `TraceBackend::launch` and `launch_experimental`
share `native.rs::MacosTraceBackend::launch_traced`: both launch suspended,
sanitize inherited descriptors, and install the same syscall interception loop
before returning. Launch requires explicit `LocalDevelopment` or
`NfsClientFsync` persistence, stdio descriptors only, absolute executable/cwd,
and non-empty argv. A backend accepts one run, including after termination.
The caller must rewrite write-intent opens before resuming.

The `SandboxRequirement` on the spec decides enforcement, and there is no
default. `Required(profile)` launches an explicitly addressed
`/usr/bin/sandbox-exec` — resigned as a twin, never reached through a shell —
with the rendered profile as bounded argv and the target's own twin as the
command. The backend then drives that trusted bootstrap internally, consuming
its startup events rather than exposing them as workspace syscalls, and returns
only at the target's exec stop, verified with `proc_pidpath` to be the intended
image. That exec stop is the handoff boundary: the policy is in force and the
target has not run an instruction. An installer that exits, forks, execs
something else, or does not reach the exec within its event budget fails the
launch, and the tree is killed rather than returned without a handle.
`UnsandboxedExperiment` is the only way to run unenforced, it is a named
selection rather than an omission, and `launch_experimental` accepts nothing
else. `TraceBackend::launch` and the provider IPC dispatcher reject that selection.

For `UnsandboxedExperiment`, initial launch normalizes `argv[0]` to the
absolute vendor executable path, after validating the caller's value for NULs.
The executed image remains the resigned twin. Nested `execve` and `posix_spawn`
retain their own argument vectors and executable-path rewriting.

Under a required profile the target observes `argv[0]` as its resigned twin cache
path: `sandbox-exec` provides no way to preserve the requested value. `argv[1..]`
and `_NSGetExecutablePath` are unaffected by enforcement; the latter already
returns the twin path on both launch paths.

The backend advertises `sandboxed-stopped-launch-v1` and
`experimental-syscall-rewrite-v1`. `tests/sandbox_launch.rs` qualifies the first
in both directions on this host: with interception rewriting the open, the
payload lands in the shadow and the host destination stays absent; with the
identical fixture and destination but no rewrite, the open fails `EPERM` and
neither file appears, which is the only way to show the policy is real rather
than that a launch merely failed.

Invoke the shipped binary through a platform `ProviderDescriptor` and
`umbra_core::provider::Client::connect`, then send `Request::Capabilities`,
`Request::Launch(spec)`, and the `NextEvent` / memory / register / `Resume`
requests. The binary's `serve_provider("macos", …)` handles that handshake
and dispatch. For an in-process socketpair, use `serve_provider_on(connection,
"macos", …)`, which shares the same handshake and dispatcher.
`Request::Quiesce(process)` interrupts live sessions and waits for stopped
acknowledgements, returning the root and live task IDs. Stop replies remain
queued for `NextEvent`; consume them before resuming that thread. If a syscall
return is in flight, quiesce leaves the tree stopped and reports an error:
drain events to a transaction boundary before retrying. Interrupted transport
failures terminate held tasks. Finish with `Request::Terminate` using
`TerminationPolicy::Immediate`.

`tests/provider_ipc.rs` exercises the socketpair handshake and real `open-libc`
capture through these requests, including stopped and running quiescence.
It uses the fixture env vars and prints `SKIP` if either is missing:

```
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos --test provider_ipc -- --nocapture
```

## Test coverage

`tests/fixtures.rs` drives Track D's C fixtures through the Rust tracer.
It reads `UMBRA_TEST_FIXTURE_PATH` (path to `umbra-test-child`) and
`UMBRA_TEST_REDIRECT_ROOT` (shadow root); when either env var is unset,
each test body skips via `eprintln` — the binary still reports the cases as
passed, so qualification requires the `CAPTURED <case>` stderr verdict.
These are direct tracer tests: they exercise interception without enforcement,
and are lower-level than the integrated `umbra run` matrix, which drives the
original seven cases through storage, journal, namespace and an installed sandbox.
All thirteen direct tracer cases are enabled. The two multithreaded cases were
`#[ignore]`d measurements that failed on purpose until the window they measured
was closed (below). Fixture cases:

| Case | State |
|---|---|
| `open-libc`         | **CAPTURED** — the M1 minimum acceptance bar |
| `open-svc`          | **CAPTURED** |
| `fork-write`        | **CAPTURED** |
| `posix-spawn-write` | **CAPTURED** |
| `exec-write`        | **CAPTURED** — see the closed M2 gap below |
| `grandchild-write`  | **CAPTURED** |
| `dup-inherit-write` | **CAPTURED** |
| `argv0-check`       | **CAPTURED** — the unsandboxed `argv[0]` contract above |
| `wnohang-wait`      | **CAPTURED** — the wait decisions above |
| `dirfd-rename`      | **CAPTURED** — the dirfd family above |
| `symlink-cycle`     | **CAPTURED** — the logical symlinks above |
| `mt-write`          | **CAPTURED** — the closed multithreaded window below |
| `mt-spawn`          | **CAPTURED** — the closed multithreaded window below |

### The two multithreaded cases: what they measured, and how it was closed

`mt-write` and `mt-spawn` are the measurement that sized the multithreaded-tracee
arc. They run on the two dispatch paths `single_thread()` does **not** gate:
`Delivery::Namespace` and `Delivery::Exec`. They were `#[ignore]`d while they
failed on purpose; **both now pass**, and the rest of this section is kept because
the defect they measured is the reason the current design looks the way it does.

**What closed them: `return_stop` resumes the trapping thread alone.** The window
below is not narrowed, it is emptied of anyone who could walk into it —
`vCont;c:<tid>` resumes exactly one thread and leaves every sibling stopped, so
for the whole of the interval in which the entry stub is un-armed there is no
other thread running to reach it. **The `z0`/`Z0` sequence is untouched by that**:
not one line of the release, the gate plant or the re-arm changed, which matters
because debugserver's `Z0` registrations are reference counted and this crate has
already been bitten once by that (the closed M2 gap below). Only *which threads
run* changed.

Alongside it, `Session::pending` and `Session::entry` are keyed by `ThreadId`,
and the return window carries the thread that opened it so `finish_return`
attributes its `SyscallExit` from the window rather than from `Session::thread` —
a slot every stop overwrites. That last part is not cosmetic: the caller removes
its in-flight operation by thread and treats a miss as a call it never
intercepted, letting the kernel result stand, so attributing a return to the
wrong thread would have abandoned the rewrite **silently**. Closing a loud escape
is not worth opening a quiet one.

**Which mechanism closes which case, measured rather than assumed.** Reverting
only the per-thread resume to a bare `c`, with the per-thread slots kept, fails
`mt-write` again and leaves `mt-spawn` passing. Keying every window back onto one
shared slot, with the per-thread resume kept, leaves **both** passing. So the
resume is what closes `mt-write`, and it closes `mt-spawn` too — the per-thread
`pending` is not what keeps that case green, because a thread inside a window is
the only thread running and no sibling is left to collide with it. The slots
remain what makes the invariant expressible and what `thread_index` resolves
from. What was measured is the `pending` half; `entry` was not separately
reverted, so no claim is made about it either way.

**One behaviour change comes with the freeze, and it is bounded.** A routed
`read`/`write`/`close` that blocks now blocks with the siblings held, where the
bare `c` would have let them run. The session watchdog kills the tree at its
deadline, so this degrades to a **watchdog kill naming a timeout, not a hang**.

**Still deferred, deliberately:** `single_thread()` stays on `Delivery::Fork` and
`WaitPlan::Park`, so a multithreaded parent that forks is refused rather than
mediated — an explicit non-goal here, and pinned by
`native.rs::tests::the_fork_and_park_paths_still_refuse_a_multithreaded_tracee`
so it stays deferred by test rather than by intention.

`single_thread()` (`native.rs`) is called from two places only, `Delivery::Fork`
and `WaitPlan::Park`. Four dispatch arms are ungated, and they do **not** all
touch the two single-element slots — stated per arm, because the generalisation
that they do was wrong and stood in this README for one revision:

| ungated arm | `Session::entry` | `Session::pending` |
|---|---|---|
| `Delivery::Namespace` | set at the entry | one hop later, when the caller resumes the thread |
| `Delivery::Exec` | — | `return_stop` |
| `WaitPlan::Native` | — | `return_stop` |
| `WaitPlan::Poll` | — | — (**neither**: it rewrites `x0`/`CPSR`/`PC` and continues) |

`Delivery::Namespace` does not call `return_stop` itself — it records the entry
PC and emits the event; `resume()` takes that PC and plants the return gate
(`return_stop(ReturnKind::Syscall)`). A multithreaded tracee doing ordinary file
I/O or a `posix_spawn` was therefore **not refused; it was unmediated** — it ran
through an ungated arm into a window no sibling was held out of. **That is what
the per-thread resume closed**, and it is why these two arms are ungated *and*
mediated today rather than ungated and unmediated.

**`WaitPlan::Native` is a third ungated slot writer and is still not measured.**
It writes `pending` exactly as `Exec` does, so it was recorded as *expected to
collide* the way `mt-spawn` then did. That prediction has been overtaken rather
than confirmed: `mt-spawn` no longer collides, because a thread inside a return
window is the only thread running, and the same reason covers this arm. No fixture
drives it and no claim here rests on it.

`mt-spawn` fired the `debug_assert!` in `return_stop` that was planted as this
arc's tripwire: a second intercepted syscall enters while one is still in flight.
That assertion was **converted, not deleted** — it is now per thread, and a second
assertion beside it covers the different property the absorbed-stop resume leans
on: that at most one window is open *process-wide*. Neither assertion establishes
that property. **The freeze does** — while a window is open only its owner runs, so
no sibling is left to reach `return_stop` and open a second — and the assertions
are tripwires over that reasoning. An earlier revision of this section credited the
per-thread assertion with pinning the process-wide property; it does not, because
two windows on two different threads pass it untouched.
A release build compiles that assertion out, so on the tree that measurement was
taken against the *backend* took the overwrite silently — but the run did not end
silently. For **this** case what ended it was the path decode refusing the
overwritten operand: `Io during path: null or overflowing pointer` (`EFAULT`),
measured in 5 of 5 enforced release runs, each exiting non-zero. The overwrite is
not reachable now, in either profile.
Per-thread slots do close this case, and so, independently, does the per-thread
resume; both were measured by reverting one and keeping the other, above.

(The supervisor also holds an independent per-`ThreadId` guard in
`umbra-supervisor/src/events.rs` that returns a real `Err(InvalidState)` in release
when a second entry arrives for a thread whose operation is still awaiting its exit.
It is worth knowing about — it is why that layer is not single-slot — but it is a
contention-dependent race, and on `mt-spawn` it fired in **0 of 10** runs. It is not
what makes this case non-silent, and the paragraph above used to say it was.)

`mt-write` was a second, distinct defect, and per-thread slots do **not** close it
— measured, by reverting the resume and keeping the slots. To let the stopped
thread execute the `svc` it is sitting on, `return_stop`
releases the entry breakpoint (`z0`), plants the return gate at `pc + 4`, and
resumes; `finish_return` puts the entry breakpoint back. A debugserver `Z0` is
per-process, so for the whole of that window the stub carries no breakpoint for
anyone — and the resume used to be a bare `c`, which put every sibling back on the
road while it was open. Measured on that shape, three runs of three: the sibling
thread's `open`, `write` and `close` produced no `SyscallEntry` at all, the rewrite
never happened, and the tracee's own path reached the kernel. The resume is now
per thread, so the siblings are not running for any of it.

The two cases failed differently for one reason: `mt-write`'s threads share **one
stub at one address**, so the `z0` window un-armed it for the sibling; `mt-spawn`'s
threads use **different** stubs, so the collision landed on the shared slot
instead. Both capture now, and measured, it is the per-thread resume that closes
both — the slots close only the second.

**What that costs depends on the harness, and the difference is measured.** In
this crate's direct-tracer harness the escaped write lands **on the host**,
because `fixture_argv` launches `UnsandboxedExperiment` with `interpose: false`
and no descriptor fence — it measures interception, not the boundary. Under a
real enforced `umbra run` it does not: 20 enforced runs, 10 of them release,
exited 1 every time with no host file ever created, because the rendered Seatbelt
profile grants `file-write*` to exactly one subpath (the run's own root inside the
store) and the escaped write is denied `EPERM`. So on the shipped path this is an
**availability and correctness** defect, not a namespace escape.

Closing it needs no sibling to be able to reach the entry site while it is
un-armed. Three shapes were costed: single-step the trapping thread over the
`svc`; hold the siblings stopped over the Mach task port this backend already
holds (`task_threads` + `thread_suspend`); or per-thread RSP resume (`vCont`).
**The third was taken.** The first is not available in the form it was recorded —
a planted `Z0` occupies the `svc`'s bytes, so a step re-executes the `BRK` and the
syscall never runs. The second works but needs a Mach-port-to-thread-id bridge
`mach2` does not expose, so it is not the dependency-free option it was recorded
as, and it manipulates the `Z0` refcount. The third needs one packet this crate
already speaks per-thread three times over (`g`, `p`, `P` under
`QThreadSuffixSupported`) and touches the refcount not at all.

These cases assert tracee-visible entry names and bytes, in two destinations
that differ in both. The journal cannot serve as the oracle: `JournalRecord`
carries no task or thread field, so cross-thread corruption still journals
`Prepare`/`ObservedResult`/`Commit` triples that pair correctly by
`OperationId`.

`wnohang-wait` polls through the public `wait4`, both stubs resolved with
`dlsym(RTLD_DEFAULT, …)`, and raw `svc #0x80` naming numbers 7 and 400, because
one public call does not reach both stubs: on this host `wait4` routes through
`__wait4_nocancel`, and removing `__wait4` from the installed list above leaves
that case failing only on its explicit `__wait4` leg. Its child is held on a
pipe, so a poll that wrongly blocks fails against the session deadline instead
of passing on timing.

`dirfd-rename` and `symlink-cycle` do not use the redirect harness above. It binds a real
`umbra-storage-local` shadow run, an approved empty immutable base and a
recording journal, and drives each operation the case performs through the
overlay transaction flow: `resolve`, then, for an action that plans one,
`prepare` → apply the rewrite or the emulated result → `observe_result` →
`commit` or `abort`. Directory opens that succeed are tracked into
`ProcessContext.fds` with the logical path and the object identity the overlay
reports, which is what lets a later `DirRef::Fd` resolve. The case deliberately
looks up a name the rename removed: that path is whiteouted, so the overlay
resolves it to `Deny(ENOENT)`, which mints no operation. Like the supervisor,
the harness emulates that errno and resumes without preparing. A resolution that
instead fails with an error is handled by shape, mirroring the supervisor: a
non-mutating `NotFound` is resumed unrewritten (with the host as base the kernel
yields the same ENOENT), while a mutating `NotFound` or any other refusal is
translated to its errno and emulated. Gating the passthrough on `!mutation`
matters — resuming a mutating call unrewritten would touch the host.
dyld and library
startup reads are left pointing at the host, unrewritten — the bound base is
empty, so the tracee could not otherwise start; that carve-out is by logical
root and tracked descriptor, and is a property of the test harness, not of the
tracer. The overlay dev-dependencies are test-only: the crate itself still
depends on no namespace or storage backend.

`argv0-check`, `wnohang-wait`, `dirfd-rename` and `symlink-cycle` are the cases
whose `CAPTURED` line the C fixture prints rather than the harness: only the
tracee can observe its own `argv[0]`, a poll that did not block, or its own
view of a renamed name or a followed link. The harness hands
the launcher a decoy `argv[0]`, and the fixture captures only when `argv[0]`
equals the vendor path passed as its operand and differs from the running image
reported by `_NSGetExecutablePath`. The shadow bytes the harness then asserts
are written only after that check passes, so a redirect failure still fails the
test.

`mt-write` and `mt-spawn` also opt out of the harness verdict, for a different
reason: their second destination is checked after `fixture_argv` returns, so the
`CAPTURED` line is printed by the test function once *both* destinations agree.
**Both reach it on every run.** While the multithreaded window was open neither
did, and this sentence said so; it is the verdict, not the harness, that changed.

Software breakpoint ownership is per debugserver connection: successful
`Z0` installs populate that session's registry, and successful `z0`
removals remove entries. Fork children receive only inherited instruction
repairs before installing their own breakpoints. Temporary return and dyld
breakpoints use the same registry; the parent's fork gate uses `Z1`/`z1`.

Debugserver's own registrations are reference counted and **survive
`execve` on that connection**, so this crate's registry must be released
rather than discarded across an exec — see the closed M2 gap below. A
duplicate `Z0` at one address makes the matching `z0` decrement without
restoring the instruction, and debugserver answers `OK` either way.

`posix_spawn` needs no per-release layout knowledge. `struct
_posix_spawnattr` is private and moves between releases — `sizeof` 248 with
`psa_ports` at 192 on macOS 26, 256 and 200 on 27.0 — so this crate never
reconstructs it. `spawn_attributes` has the host's own libc build the block
(`posix_spawnattr_init` plus `posix_spawnattr_setflags`), takes its length
from `malloc_size` rather than a constant, and copies the bytes without
interpreting them.

The kernel copies `offsetof(_posix_spawnattr, psa_ports)` from the attribute
block and `sizeof(struct _posix_spawn_args_desc)` from the descriptor, both
its own constants, and tests `attr_size` only against zero
(`bsd/kern/kern_exec.c`). Neither length is reported to userspace, so both
buffers are padded to `SPAWN_BUFFER_BYTES` and zero-filled beyond the bytes
written: every trailing field is a pointer or a size/pointer pair, so
anything the kernel reads past the snapshot reads as absent rather than as
neighbouring scratch. The one retained assumption is `psa_flags` leading the
struct, checked before use so a future move refuses instead of misbehaving.

`native.rs::tests::live_kernel_accepts_the_spawn_buffers` is the drift
detector for that arrangement. The attribute side is sound by construction —
libc ships with the kernel, so a `malloc_size` snapshot always covers the
prefix the kernel reads — but the descriptor's length is not measurable from
userspace and is only assumed to fit `SPAWN_BUFFER_BYTES`. Rather than assert
remembered offsets, the test builds both buffers through the helpers the
tracer uses and invokes `posix_spawn` directly, so the live kernel judges
them. It needs no debugger, fixture or environment variable and runs anywhere
the crate compiles. Mutation-checked in both directions: shrinking the
descriptor below the kernel's struct fails with `EINVAL`, and clearing the
suspend flag fails on the marker the child then leaves behind. The marker
command is exec'd directly rather than through a shell, so a temporary
directory containing whitespace cannot split the path and turn a regression
into a silent pass; both mutations were re-confirmed under such a `TMPDIR`.


## Closed M2 gap: `exec-write` post-exec breakpoint loop

`exec-write` used to live-lock after `execve`: the first post-exec
`SyscallEntry` landed on an `__openat`-family stub, the tracer sent
`z0,<entry>,4` (`OK`) + `Z0,<entry+4>,4` (`OK`) + `c`, and the very next
stop reported the ENTRY address again rather than the gate. It repeated
thousands of times until the 25 s session watchdog fired. Both packets
were ACK'd, so the tracee behaved as if `z0` had not cleared the trap.

**Cause.** Debugserver's breakpoint registrations survive `execve` on the
same connection, and its `Z0` handler is reference counted. The exec reset
dropped this crate's own `breaks` registry without releasing those
registrations, then called `install()`, which re-registered the same
shared-cache addresses. Debugserver's count for each reached two, so the
single `z0` that retires an entry site decremented to one and left the
`BRK` in place — while still answering `OK`, because the request itself
succeeded.

The mismatch was invisible to the obvious probe: debugserver masks its own
breakpoints in `m` reads, so `m<entry>,4` returned the original
`svc #0x80` bytes both before and after the `z0`. A `mach_vm_read` of the
same address returned `BRK` in both cases, and `vCont;s` did not advance
the PC. That pairing is what identified the leak.

**Fix.** The `reason:exec` handler now sends `z0` for every address the
session still owns before clearing `breaks` and re-resolving the new
image. A refused release is not fatal — the new image need not map an
address the old one did, and a refusal means the registration is already
gone. See the `reason:exec` branch in `src/native.rs`.

`src/rsp.rs::tests::debugserver_reverse_connect_no_ack_handshake` spawns a
real debugserver and asserts the negotiated `qHostInfo.ostype` is
`macosx`, guarding the reverse-connect nonblocking-inheritance fix from
regressing. It resolves the binary through the same `discover_debugserver`
helper `connect` uses — `UMBRA_DEBUGSERVER` first, then the path below
`xcode-select -p` — and checks that the resolved path is a file before
connecting; missing tools print `SKIP` and return. Sharing that helper is
what stops the test from skipping on a host where only the environment
override names a usable binary.
That guard is implemented. A present debugger still requires socket and
debugging permissions.

## Verification

```
cargo check -p umbra-platform-macos
UMBRA_TEST_FIXTURE_PATH=<abs> UMBRA_TEST_REDIRECT_ROOT=<abs> \
  cargo test -p umbra-platform-macos
```

Successful compilation does not qualify tracing on any target — the M1
minimum bar is what `open-libc` CAPTURED demonstrates.

The fixture, IPC, and sandbox-launch harnesses resolve `UMBRA_TEST_REDIRECT_ROOT`
through `tests/support/mod.rs` and require an existing directory. They reuse that
resolved root for rewrite destinations and, where enforcement is installed, both
the profile source and its declared write root. This matches production preparation:
Seatbelt matches resolved paths, so a raw `/var/...` rule does not grant the
corresponding `/private/var/...` write. The sandbox-launch suite also qualifies a
root reached through an explicit symlink, asserting the rewritten write succeeds
and the host destination remains absent.

Set `UMBRA_INTEGRATION_REQUIRED=1` to make missing fixture inputs fail the sandbox,
IPC and direct tracer suites. CI's `native-qualification` job runs these and the
CLI's fourteen-case matrix on a runner labelled `umbra-integration`; that runner
requires debugger permission and an existing NFSv4 mount configured through the
`UMBRA_TEST_NFS_ROOT` repository variable. No test provisions a mount or prompts.

Sandbox handoff compares canonical target paths with `proc_pidpath`. The twin
cache root is also canonicalized so a symlinked cache recognizes already-resigned
executables instead of creating a second twin during bootstrap. Installed
sandbox qualification tests exercise a symlinked cache root.

Failed attach and sandbox-handoff paths set `launch_tree_terminated` on the returned
error only when `waitpid` confirms reaping every created tracee. Missing evidence
remains false so the supervisor retains writer authority after uncertain launch failure.
