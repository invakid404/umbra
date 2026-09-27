//! Pure ABI core: 34 little-endian u64 slots, x0..x30, sp, pc, cpsr.
use crate::{error, unsupported};
use umbra_core::*;
use umbra_platform::{SyscallAbi, TraceMemory};
pub const MAX_PATH: usize = 4096;
pub const PC: usize = 32;
pub const CPSR: usize = 33;

/// The reserved x16 value umbra's userspace-routing interposer traps with.
///
/// **This is a wire format, shared with `interpose/umbra_interpose.c`**, which
/// carries the same literal; `interpose_trap_number_matches_the_interposer`
/// asserts the two agree by reading the C source. Far above every Darwin BSD
/// syscall number and not a Mach trap (those are negative), so nothing the
/// kernel implements can collide with it in either direction.
///
/// It is reachable only from an instruction inside the interposer image: the
/// backend scans that one extra image for `svc` sites and no other, so a trap
/// number arriving from anywhere else was never breakpointed and never reaches
/// this decoder. That containment is a property of where the breakpoints are
/// planted, not of this constant, and `install_image` is where it lives.
pub const INTERPOSE_TRAP: u64 = 0x554d_4252;

/// Operation code in x0 of an [`INTERPOSE_TRAP`]. Mirrors the C header.
const INTERPOSE_OPEN: u64 = 1;
/// Operation code in x0 of an [`INTERPOSE_TRAP`]. Mirrors the C header.
const INTERPOSE_READ: u64 = 2;
/// Operation code in x0 of an [`INTERPOSE_TRAP`]. Mirrors the C header.
const INTERPOSE_WRITE: u64 = 3;
/// Operation code in x0 of an [`INTERPOSE_TRAP`]. Mirrors the C header.
const INTERPOSE_CLOSE: u64 = 4;

/// Decode Darwin `O_*` bits into the contract's flag set.
///
/// Shared by the `open`/`openat` syscall arms and the interposer's OPEN trap,
/// which carries the tracee's flags register verbatim, so the two cannot drift
/// into disagreeing about what `O_CREAT|O_WRONLY|O_TRUNC` means.
fn open_flags(native: u64) -> OpenFlags {
    OpenFlags {
        read: native & 3 != 1,
        write: native & 3 != 0,
        append: native & 8 != 0,
        create: native & 0x200 != 0,
        exclusive: native & 0x800 != 0,
        truncate: native & 0x400 != 0,
        directory: native & 0x100000 != 0,
        no_follow: native & 0x100 != 0,
        close_on_exec: native & 0x1000000 != 0,
    }
}

/// The byte count a routed transfer is serviced for.
///
/// Clamped to [`MAX_IO_BYTES`] rather than refused, and the two callers
/// ([`DarwinArm64Abi::decode_entry`] and [`DarwinArm64Abi::io_buffer`]) must
/// clamp identically or the operation would name more bytes than the buffer
/// binding covers. A short `read` or `write` is a POSIX-legal answer, so
/// clamping is honest; refusing an oversized request would fail a program that
/// merely offered a large buffer.
fn transfer_length(requested: u64) -> u64 {
    requested.min(MAX_IO_BYTES as u64)
}

/// Decode one interposer trap into the contract operation it names.
///
/// The operands are in registers, exactly as a syscall's would be, so this needs
/// no request block in tracee memory and no second read of it: x1..x3 carry
/// everything but the `open` path, which is read through the same
/// [`read_path`] the `open` syscall arm uses.
///
/// `offset: None` on both transfers means "at the descriptor's current
/// position", which is the whole of what the interposer promises: it interposes
/// `read` and `write` and not `pread`/`pwrite`/`lseek`, so a routed transfer is
/// always positional-by-descriptor. The position itself lives in
/// [`FdState::offset`] and is umbra's, never the interposer's.
fn decode_interpose(regs: &RegisterSet, memory: &mut dyn TraceMemory) -> Result<FsOp> {
    let operation = get(regs, 0)?;
    Ok(match operation {
        INTERPOSE_OPEN => FsOp::Open {
            // Always `Cwd`: the interposer replaces `open`, which has no dirfd
            // operand, and does not replace `openat`. A relative path is
            // resolved against the process context's logical cwd by the overlay,
            // which is the same resolver an unrouted `open` would have reached.
            dir: DirRef::Cwd,
            path: read_path(memory, get(regs, 1)?)?,
            flags: open_flags(get(regs, 2)?),
            mode: get(regs, 3)? as u32,
        },
        INTERPOSE_READ => FsOp::Read {
            fd: TracedFd(get(regs, 1)? as i32),
            length: transfer_length(get(regs, 3)?),
            offset: None,
        },
        INTERPOSE_WRITE => FsOp::Write {
            fd: TracedFd(get(regs, 1)? as i32),
            length: transfer_length(get(regs, 3)?),
            offset: None,
        },
        INTERPOSE_CLOSE => FsOp::Close {
            fd: TracedFd(get(regs, 1)? as i32),
        },
        // Not a wildcard for convenience: an unknown code means the loaded
        // library and this decoder disagree about the wire format, which is a
        // mismatch to refuse rather than guess at. The build compiles the C from
        // source in the same `cargo build`, so reaching here means something
        // other than that library issued the trap.
        _ => {
            return Err(unsupported(format!(
                "unknown umbra interposer operation {operation}"
            )))
        }
    })
}
pub fn validate(regs: &RegisterSet) -> Result<()> {
    if regs.architecture() != &Architecture::Aarch64 || regs.as_bytes().len() != 272 {
        return Err(unsupported(
            "expected darwin-arm64-v1 registers (34 LE u64 slots)",
        ));
    }
    Ok(())
}
pub fn get(regs: &RegisterSet, slot: usize) -> Result<u64> {
    validate(regs)?;
    let b = regs
        .as_bytes()
        .get(slot * 8..slot * 8 + 8)
        .ok_or_else(|| error("register", "slot out of range"))?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}
pub fn set(regs: &mut RegisterSet, slot: usize, value: u64) -> Result<()> {
    validate(regs)?;
    let b = regs
        .as_bytes_mut()
        .get_mut(slot * 8..slot * 8 + 8)
        .ok_or_else(|| error("register", "slot out of range"))?;
    b.copy_from_slice(&value.to_le_bytes());
    Ok(())
}
/// Reads at most 4 KiB without crossing a 4 KiB page in one callback.
/// 64-byte chunks also respect the provider's 256-callback budget.
pub fn read_path(memory: &mut dyn TraceMemory, pointer: u64) -> Result<BytePath> {
    if pointer == 0 || pointer.checked_add(MAX_PATH as u64).is_none() {
        return Err(error("path", "null or overflowing pointer").with_errno(Errno(14)));
    }
    let mut bytes = Vec::new();
    while bytes.len() < MAX_PATH {
        let address = pointer + bytes.len() as u64;
        let len = (MAX_PATH - bytes.len())
            .min(64)
            .min(4096 - (address as usize & 4095));
        let mut chunk = vec![0; len];
        memory.read(address, &mut chunk)?;
        if let Some(end) = chunk.iter().position(|b| *b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return BytePath::new(bytes);
        }
        bytes.extend(chunk);
    }
    Err(error("path", "unterminated 4 KiB path").with_errno(Errno(63)))
}
/// SDK `sys/fcntl.h:172-184`. `AT_FDCWD` is a signed sentinel, not a descriptor.
pub const AT_FDCWD: i32 = -2;
/// Check against the effective user and group IDs rather than the real ones.
pub const AT_EACCESS: u32 = 0x0010;
/// Act on the link itself rather than its target.
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x0020;
/// Act on the target of a link.
pub const AT_SYMLINK_FOLLOW: u32 = 0x0040;
/// The path names a directory to remove.
pub const AT_REMOVEDIR: u32 = 0x0080;

/// Decode a dirfd register. Dirfds are signed 32-bit: arm64 leaves the upper
/// half of a register holding an `int` unspecified, so only the low word is the
/// callee's value. `AT_FDCWD` anchors at the process cwd; every other value is
/// a tracked descriptor whose logical anchor the namespace owns. An absolute
/// path ignores the anchor entirely, including an invalid descriptor, so no
/// validation happens here.
pub fn dir_ref(raw: u64) -> DirRef {
    match raw as i32 {
        AT_FDCWD => DirRef::Cwd,
        fd => DirRef::Fd(TracedFd(fd)),
    }
}
/// Truncate an `int` flag register and refuse any bit outside `allowed`.
///
/// Unmodelled `*at` bits are rejected rather than dropped: silently forwarding
/// a flag the decode did not represent would execute different semantics from
/// the ones the namespace resolved.
fn at_flags(raw: u64, allowed: u32) -> Result<u32> {
    let flags = raw as u32;
    if flags & !allowed != 0 {
        return Err(unsupported(format!("*at flags {flags:#x}")));
    }
    Ok(flags)
}
/// What the tracer does with a breakpointed stub's syscall when it fires.
///
/// The variants are the dispositions `NativeTracer::intercept` implements, and
/// the match over them there is **exhaustive by design**: adding a variant is a
/// compile error at the one site obliged to say what to do with it.
///
/// **Exhaustiveness proves that nobody forgot to choose, not that anybody chose
/// correctly**, and the distinction is the whole remaining risk.
/// `("mkdir", 136, Delivery::Fork)` type-checks, compiles and runs; at runtime
/// the tracee's `mkdir` would take the fork arm, snapshot every breakpoint and
/// have its return value read as a child pid. That is corruption of the tracing
/// model rather than a refusal, and it is the same *shape* as the defect this
/// table exists to end.
///
/// The compiler closes one half. The other half is
/// `every_traced_stub_carries_the_delivery_its_number_implies`, which derives
/// the expected disposition from `decode_entry` for **every** row rather than
/// from a list written beside the table -- a list beside a table being exactly
/// what went wrong before.
///
/// **That mechanism also rests on this enum not being `#[non_exhaustive]`**, and
/// it is worth saying so in the same terms `umbra-overlay`'s
/// `replay_must_poison` says it of `JournalIntent`: this type is `pub`, so
/// `#[non_exhaustive]` is a natural future addition for a public enum, and
/// adding it would force a wildcard arm in `intercept` and destroy the
/// compile-time half *silently* -- the code would keep building, and every
/// future variant would inherit whatever that wildcard said. Adding the
/// attribute is therefore a decision about `intercept`, not only about this
/// enum, and the two must be revisited together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Pushed as a `TraceEvent::SyscallEntry` for the caller to decode, resolve,
    /// answer and observe. Every filesystem call is this.
    Namespace,
    /// `fork`. The backend single-threads the session and takes a return stop.
    Fork,
    /// `wait4` and its `_nocancel` twin. The backend plans the wait itself.
    Wait,
    /// `execve` and `posix_spawn`. The backend rewrites the image path to a
    /// resigned twin before letting the call through.
    Exec,
}

/// **Every libc stub umbra breakpoints, and the syscall its `svc` issues.**
///
/// This table exists because there are **two** gates on one decision and they
/// used to be written twice. `install()` decided which instructions get a
/// breakpoint from a list of symbol names; `intercept()` decided which
/// breakpointed numbers become a `SyscallEntry` from a separate list of
/// numbers. Four stub rows were added to the first and not the second, so every
/// breakpoint they planted fell through to `intercept()`'s refusal and stopped
/// the run -- including `fstat`(339), which `_os_feature_table_once` issues on a
/// kernel descriptor before `main` in every process. Both lists were correct in
/// isolation; the pair was not.
///
/// That is the shape [#116](https://github.com/invakid404/umbra/issues/116)
/// named for `admit_run`: **a re-derived admission set drifts clause by
/// clause.** The remedy is not a third list to check the other two against. It
/// is that there is now one list, both gates read it, and `intercept()` matches
/// exhaustively on [`Delivery`] -- so a stub row whose disposition nobody chose
/// cannot compile, and a row added here is routed without touching `intercept`
/// at all.
///
/// The number is **verified against the host on every launch**, not trusted:
/// `install()` scans each resolved stub for the `movz x16, #imm` that precedes
/// its `svc` and refuses to plant a breakpoint if it disagrees with this table.
/// A stale row fails the launch with a diagnosis instead of failing a run later.
///
/// Notes on the symbols, all measured on this host with `dlsym` + an `svc`
/// scan rather than read out of a header:
///
/// * Several have no `__`-prefixed form, so the public symbol is the stub
///   carrying the `svc`: `symlink`, `readlink`, `mkdir`, `setattrlistat` and
///   every `*at` form but `__unlinkat`, `__fstatat`, `__renameat` and
///   `__renameatx_np`.
/// * `unlinkat`'s public wrapper carries its own `svc` (at +48) rather than
///   tail-calling `__unlinkat`, so **both** are installed and both reach 472.
/// * `fstatat`/`fstatat64` is one symbol reaching 470; `__fstatat` is a separate
///   stub reaching 469. `fstat`/`fstat64` is likewise one symbol reaching 339 --
///   they resolve to the *same address*, so listing `fstat64` would plant a
///   second breakpoint on one instruction -- and `__fstat` is separate at 189.
/// * There is no `__mkdir`. `/bin/mkdir` issues the bare form and nothing else,
///   which is why it was refused by enforcement rather than routed
///   ([#114](https://github.com/invakid404/umbra/issues/114)).
/// * `utimensat` and `futimens` carry **no `svc` at all** -- both scan clean --
///   because they are wrappers that build an `attrlist` and tail-call
///   `setattrlistat`(524). Neither is a stub candidate; that one is.
///
/// [`INTERPOSE_TRAP`] is deliberately **not** here. It is not a libc stub and
/// has no symbol to resolve: its sites are breakpointed by `install_image` on
/// the interposer's own text, which is a different mechanism with a different
/// containment argument, and `intercept()` gives it its own arm for that reason.
pub const TRACED_STUBS: &[(&str, u64, Delivery)] = &[
    ("__open", 5, Delivery::Namespace),
    ("__open_nocancel", 398, Delivery::Namespace),
    ("__openat", 463, Delivery::Namespace),
    ("__openat_nocancel", 464, Delivery::Namespace),
    ("__execve", 59, Delivery::Exec),
    ("__posix_spawn", 244, Delivery::Exec),
    ("__fork", 2, Delivery::Fork),
    ("__wait4", 7, Delivery::Wait),
    ("__wait4_nocancel", 400, Delivery::Wait),
    ("symlink", 57, Delivery::Namespace),
    ("readlink", 58, Delivery::Namespace),
    ("__renameat", 465, Delivery::Namespace),
    ("__renameatx_np", 488, Delivery::Namespace),
    ("__unlinkat", 472, Delivery::Namespace),
    ("unlinkat", 472, Delivery::Namespace),
    ("linkat", 471, Delivery::Namespace),
    ("symlinkat", 474, Delivery::Namespace),
    ("mkdirat", 475, Delivery::Namespace),
    ("fchmodat", 467, Delivery::Namespace),
    ("fchownat", 468, Delivery::Namespace),
    ("__fstatat", 469, Delivery::Namespace),
    ("fstatat", 470, Delivery::Namespace),
    ("readlinkat", 473, Delivery::Namespace),
    ("faccessat", 466, Delivery::Namespace),
    ("mkdir", 136, Delivery::Namespace),
    ("fstat", 339, Delivery::Namespace),
    ("__fstat", 189, Delivery::Namespace),
    ("setattrlistat", 524, Delivery::Namespace),
];

/// What `intercept()` must do with a breakpointed syscall number, or `None` for
/// a number no stub in [`TRACED_STUBS`] issues.
///
/// `None` is the honest answer rather than a default disposition: a breakpoint
/// fired for a number this table does not know is umbra's own wiring fault, and
/// guessing a disposition for it would resume or refuse a call nobody chose to
/// intercept.
pub fn delivery(number: u64) -> Option<Delivery> {
    TRACED_STUBS
        .iter()
        .find(|(_, traced, _)| *traced == number)
        .map(|(_, _, delivery)| *delivery)
}

/// Register slot for each path operand a syscall takes, slot N being xN.
///
/// Non-path arguments — dirfds included — keep their original registers: a
/// qualified physical rewrite is absolute, and the kernel ignores the dirfd for
/// an absolute path.
pub fn path_operands(number: u64) -> Result<&'static [(PathOperand, usize)]> {
    Ok(match number {
        // open, open_nocancel, execve: path in x0. readlink: path in x0, with
        // the output buffer in x1 and its length in x2. mkdir(136): path in x0,
        // mode in x1 -- the bare form, whose `mkdirat` sibling is in the at-form
        // row below.
        5 | 398 | 59 | 58 | 136 => &[(PathOperand::Path, 0)],
        // symlink: x0 holds literal target bytes, x1 the link name.
        57 => &[(PathOperand::Path, 1)],
        // openat, openat_nocancel, posix_spawn: path in x1. unlinkat,
        // mkdirat, faccessat, fchmodat, fchownat, fstatat, fstatat64,
        // readlinkat, setattrlistat: dirfd x0, path x1.
        463 | 464 | 244 | 466 | 467 | 468 | 469 | 470 | 472 | 473 | 475 | 524 => {
            &[(PathOperand::Path, 1)]
        }
        // renameat, renameatx_np, linkat: source dirfd x0, source x1,
        // destination dirfd x2, destination x3.
        465 | 471 | 488 => &[(PathOperand::Source, 1), (PathOperand::Destination, 3)],
        // symlinkat: x0 holds literal target bytes the tracee will read back,
        // never a pathname operand to physicalize. Link dirfd x1, name x2.
        474 => &[(PathOperand::Path, 2)],
        // fstat(339) and __fstat(189) are intercepted and decoded, and are
        // deliberately absent: they name a descriptor and no path, so there is
        // no operand to physicalize. A caller that reached here for one is
        // asking the wrong question, and the refusal says so rather than
        // inventing slot zero.
        _ => return Err(unsupported(format!("path syscall {number}"))),
    })
}
/// Slot of a single-path syscall's only operand. Compatibility wrapper.
pub fn path_slot(number: u64) -> Result<usize> {
    match path_operands(number)? {
        [(PathOperand::Path, slot)] => Ok(*slot),
        _ => Err(unsupported(format!("syscall {number} takes two paths"))),
    }
}
/// Build the scratch writes and register updates for a resolved physical
/// operation, independently of transport.
///
/// `addresses` is parallel to `operation.paths`: one bounded, separately
/// NUL-terminated scratch buffer per operand. Every operand the syscall
/// declares must appear exactly once, so a missing, duplicated or unexpected
/// operand is an error rather than a half-applied rewrite.
pub fn prepare_paths(
    regs: &RegisterSet,
    operation: PhysicalOperation,
    addresses: &[u64],
) -> Result<PreparedRewrite> {
    let expected = path_operands(get(regs, 16)?)?;
    if operation.paths.len() != expected.len() || addresses.len() != expected.len() {
        return Err(error("rewrite", "operand count does not match the syscall"));
    }
    let mut arguments = Vec::new();
    let mut memory_writes = Vec::new();
    for (operand, slot) in expected {
        let mut found = operation
            .paths
            .iter()
            .enumerate()
            .filter(|(_, p)| p.operand == *operand);
        let (index, rewrite) = found
            .next()
            .ok_or_else(|| error("rewrite", "missing path operand"))?;
        if found.next().is_some() {
            return Err(error("rewrite", "duplicate path operand"));
        }
        let bytes = rewrite.path.0.as_bytes();
        let address = addresses[index];
        if bytes.len() >= MAX_PATH
            || address == 0
            || address.checked_add(bytes.len() as u64 + 1).is_none()
        {
            return Err(error("rewrite", "invalid scratch path bounds"));
        }
        arguments.push(ArgumentRewrite {
            index: *slot as u8,
            value: address,
        });
        let mut bytes = bytes.to_vec();
        bytes.push(0);
        memory_writes.push(MemoryWrite { address, bytes });
    }
    Ok(PreparedRewrite {
        operation,
        arguments,
        memory_writes,
    })
}
/// Single-path compatibility wrapper over [`prepare_paths`].
pub fn prepare_path(
    regs: &RegisterSet,
    address: u64,
    path: &BytePath,
    operation: FsOp,
) -> Result<PreparedRewrite> {
    prepare_paths(
        regs,
        PhysicalOperation {
            operation,
            paths: vec![PathRewrite {
                operand: PathOperand::Path,
                path: PhysicalPath(path.clone()),
            }],
        },
        &[address],
    )
}
/// Output buffer address and length for a readlink-family syscall: `readlink`
/// keeps them in x1/x2, `readlinkat` in x2/x3. The length is a 64-bit
/// `size_t`, bounded before narrowing to the output buffer's `u32` contract.
pub fn readlink_buffer(regs: &RegisterSet) -> Result<(u64, u32)> {
    let slot = match get(regs, 16)? {
        58 => 1,
        473 => 2,
        number => return Err(unsupported(format!("readlink buffer for syscall {number}"))),
    };
    let len = get(regs, slot + 1)?;
    if len > MAX_IO_BYTES as u64 {
        return Err(unsupported("readlink buffer exceeds MAX_IO_BYTES"));
    }
    Ok((get(regs, slot)?, len as u32))
}
/// Output buffer address for a stat-family syscall: `fstatat` and `fstatat64`
/// keep it in x2, `fstat`(339) and `__fstat`(189) in x1.
///
/// The two families differ only by the leading dirfd/path pair the at-form
/// carries, so the slot moves by one and nothing else does -- both write the
/// same [`STAT_BYTES`] image.
pub fn stat_buffer(regs: &RegisterSet) -> Result<u64> {
    match get(regs, 16)? {
        469 | 470 => get(regs, 2),
        189 | 339 => get(regs, 1),
        number => Err(unsupported(format!("stat buffer for syscall {number}"))),
    }
}

/// `ATTR_CMN_MODTIME` / `ATTR_CMN_ACCTIME` from `sys/attr.h`.
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_ACCTIME: u32 = 0x0000_1000;
/// `ATTR_BIT_MAP_COUNT` -- the only `bitmapcount` a caller may declare.
const ATTR_BIT_MAP_COUNT: u16 = 5;
/// `FSOPT_NOFOLLOW` / `FSOPT_UTIMES_NULL` from `sys/attr.h`.
const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
const FSOPT_UTIMES_NULL: u32 = 0x0000_0400;
/// Bytes of `struct attrlist`: `u_short` + `u_int16_t` + five `attrgroup_t`.
const ATTRLIST_BYTES: usize = 24;
/// Bytes of one arm64 `struct timespec` in an attribute buffer.
const TIMESPEC_BYTES: usize = 16;

/// Decode the one `setattrlistat`(524) shape libc's `utimensat` emits.
///
/// **This is a shape refusal, not a flag refusal, and the distinction is the
/// point.** `setattrlist`-family calls carry a caller-declared attribute
/// bitmap, and the *layout of the buffer that follows is dictated by that
/// bitmap*: a bit umbra does not model is not a flag it could drop, it is a
/// different buffer. So anything outside the modtime/acctime pair is refused
/// whole, the discipline [`at_flags`] already applies to unmodelled `*at` bits.
///
/// Measured on macOS 26.5.1 arm64, by breaking on the stub and reading the
/// tracee's memory back -- every claim below is an observation, not a reading
/// of the header:
///
/// * `bitmapcount` is always `ATTR_BIT_MAP_COUNT` (5) and `reserved` zero.
/// * `commonattr` is `ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME` (`0x1400`) for a
///   two-time request, and `ATTR_CMN_MODTIME` alone (`0x400`) when the caller
///   passed `UTIME_OMIT` for the access time. `volattr`/`dirattr`/`fileattr`/
///   `forkattr` are zero.
/// * The buffer holds one `timespec` per set bit, **in ascending bit order** --
///   so modtime (`0x400`) precedes acctime (`0x1000`), which is the reverse of
///   the `times[2]` array the caller wrote. Confirmed by passing two
///   distinguishable times and reading which came back first.
/// * `options` is `0` for explicit times, `FSOPT_UTIMES_NULL` when the caller
///   passed `NULL` or `UTIME_NOW`, and `FSOPT_NOFOLLOW` for
///   `AT_SYMLINK_NOFOLLOW`. **`FSOPT_UTIMES_NULL` needs no special handling
///   here**: libc has already resolved "now" into the buffer, so the decoded
///   times are the ones the caller means either way. It is accepted rather
///   than refused, and it is accepted rather than *ignored* -- the bit changes
///   the kernel's permission rule, not the value, and umbra's own resolution
///   answers permission from the namespace.
///
/// `/usr/bin/touch <existing>` emits exactly the first form with
/// `FSOPT_UTIMES_NULL` set.
fn setattrlist_times(
    regs: &RegisterSet,
    memory: &mut dyn TraceMemory,
) -> Result<(Option<i128>, Option<i128>, bool)> {
    // MUTATION PROBE -- `setattrlistat` routing. Compiled out of every build
    // that does not ask for it, so no product binary contains this branch.
    //
    // It refuses the call at the decode, and **what that produces is a stopped
    // run**, not the pre-slice behaviour. This comment used to say the refusal
    // is "where the number landed before this arm existed", which was false for
    // both of probe D's reasons: before the arm existed, `setattrlistat` was
    // not in `TRACED_STUBS`, so 524 was never breakpointed and never reached
    // this decoder; and a decode error is propagated by `syscall_entry` with
    // `?`, which has no resume fallback. Measured with the shipped probe
    // binary on `--local-dev`:
    //
    //   unmutated:  `touch: <path>: Operation not supported`, child exit 1,
    //               `ProcessFailed during run.child` -- the tracee was refused
    //               and the RUN SURVIVED.
    //   mutated:    `UnsupportedCapability during macos: mutation probe:
    //               setattrlistat is refused` -- no tracee message, no child
    //               status, the RUN ABORTED.
    //
    // Before the arm existed it was a third thing again: 524 reached the kernel
    // and Seatbelt refused it, with the run surviving. So the probe creates a
    // state that neither ships nor shipped. It is still a valid mutation -- the
    // abort is caused by this arm and nothing else -- but the justification has
    // to describe what it does rather than what it is imagined to restore, or a
    // reader cannot check it. (Probe D's comment was corrected for exactly this
    // in round 1 and this one was not swept with it, which is the same
    // lexical-not-mechanism miss the correction was about.)
    //
    // The discriminator is a *pair*: `touch <existing>` fails while
    // `touch <absent>` still succeeds and still leaves its empty file in the
    // shadow, because the create path never reaches this syscall at all. No
    // other probe here produces that pair.
    //
    // The refusal is placed inside this function rather than around the decode
    // arm so that nothing else in this file goes dead when the probe is on: a
    // probe that changed which warnings the crate emits would be a probe that
    // changed more than the one thing it claims to.
    #[cfg(feature = "mutation-probe-setattrlistat")]
    {
        // Decoded first and *then* refused, deliberately: the probe removes the
        // answer and not the reading of it, so a case that passes under it
        // cannot have passed because the attrlist stopped being parsed. It also
        // keeps every helper and constant below live, so enabling the probe
        // changes no warning this crate emits.
        setattrlist_times_inner(regs, memory)?;
        Err(unsupported("mutation probe: setattrlistat is refused"))
    }
    #[cfg(not(feature = "mutation-probe-setattrlistat"))]
    {
        setattrlist_times_inner(regs, memory)
    }
}

fn setattrlist_times_inner(
    regs: &RegisterSet,
    memory: &mut dyn TraceMemory,
) -> Result<(Option<i128>, Option<i128>, bool)> {
    let options = get(regs, 5)? as u32;
    if options & !(FSOPT_NOFOLLOW | FSOPT_UTIMES_NULL) != 0 {
        return Err(unsupported(format!("setattrlistat options {options:#x}")));
    }
    let mut list = [0u8; ATTRLIST_BYTES];
    memory.read(get(regs, 2)?, &mut list)?;
    let word = |offset: usize| u32::from_le_bytes(list[offset..offset + 4].try_into().unwrap());
    let half = |offset: usize| u16::from_le_bytes(list[offset..offset + 2].try_into().unwrap());
    let common = word(4);
    // Every field of the declared shape is checked, not only the one that is
    // read: a nonzero `volattr` or `forkattr` means the buffer this decode is
    // about to walk has entries in it that this decode does not know are there.
    // `common == 0` is refused with the rest, and it is the one member of this
    // list that is not obviously malformed. libc never emits it: two
    // `UTIME_OMIT`s make `utimensat` a no-op that issues no syscall at all. If
    // it arrived anyway it would decode to a `SetTimes` naming neither time,
    // which the overlay would carry all the way to a `SetMetadata` that every
    // storage backend refuses as "update names nothing" -- from inside
    // `prepare`, with the intent already flushed. Refusing it here costs a shape
    // no caller produces and removes a way for one to stop a run.
    if half(0) != ATTR_BIT_MAP_COUNT
        || half(2) != 0
        || common == 0
        || common & !(ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME) != 0
        || word(8) | word(12) | word(16) | word(20) != 0
    {
        return Err(unsupported(format!(
            "setattrlistat attrlist shape bitmapcount={} common={common:#x}              vol={:#x} dir={:#x} file={:#x} fork={:#x}",
            half(0),
            word(8),
            word(12),
            word(16),
            word(20),
        )));
    }
    let wanted = [ATTR_CMN_MODTIME, ATTR_CMN_ACCTIME]
        .iter()
        .filter(|bit| common & **bit != 0)
        .count();
    let size = get(regs, 4)?;
    // The caller's own byte count has to agree with the bitmap before a single
    // entry is read. A buffer shorter than the bitmap declares would be read
    // past its end; a longer one means the two halves of the request disagree,
    // and guessing which is authoritative is exactly the silent wrong answer
    // this file refuses elsewhere.
    if size != (wanted * TIMESPEC_BYTES) as u64 {
        return Err(unsupported(format!(
            "setattrlistat buffer is {size} bytes for {wanted} attribute(s)"
        )));
    }
    let mut buffer = vec![0u8; wanted * TIMESPEC_BYTES];
    if !buffer.is_empty() {
        memory.read(get(regs, 3)?, &mut buffer)?;
    }
    let mut entries = buffer.as_chunks::<TIMESPEC_BYTES>().0.iter();
    let mut take = |bit: u32| -> Result<Option<i128>> {
        if common & bit == 0 {
            return Ok(None);
        }
        let entry = entries.next().expect("one entry per set bit, just sized");
        let seconds = i64::from_le_bytes(entry[..8].try_into().unwrap());
        let nanos = i64::from_le_bytes(entry[8..].try_into().unwrap());
        // `tv_nsec` outside one second is not a time this decode can normalise
        // without inventing a second's worth of meaning. Darwin answers
        // `EINVAL`; refusing is umbra's equivalent and keeps the refusal
        // visible.
        if !(0..1_000_000_000).contains(&nanos) {
            return Err(unsupported(format!("setattrlistat tv_nsec {nanos}")));
        }
        Ok(Some(
            i128::from(seconds) * 1_000_000_000 + i128::from(nanos),
        ))
    };
    // Ascending bit order, which is the order the buffer is packed in.
    let modified = take(ATTR_CMN_MODTIME)?;
    let accessed = take(ATTR_CMN_ACCTIME)?;
    Ok((accessed, modified, options & FSOPT_NOFOLLOW == 0))
}
/// Size of Darwin's `struct stat`, the `__DARWIN_INODE64` layout an arm64
/// `fstatat` writes. Offsets below were read from the host's `sys/stat.h`
/// through `offsetof`, not remembered.
pub const STAT_BYTES: usize = 144;
/// `S_IFMT` / `S_IFLNK` / `S_IFREG` / `S_IFDIR` from `sys/stat.h`.
const S_IFLNK: u32 = 0o120000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// Encode logical metadata as a Darwin `struct stat` image.
///
/// The placeholder's native file kind and size are never consulted: a logical
/// symlink is reported as `S_IFLNK` with its target length, so a no-follow
/// stat can never expose the empty regular file the overlay stores for it.
/// Only fields this layout defines are written; the rest stay zero. A kind the
/// layout cannot represent fails explicitly rather than guessing a mode.
pub fn encode_stat(stat: &BlobStat) -> Result<Vec<u8>> {
    let format = match stat.kind {
        ObjectKind::LogicalSymlink => S_IFLNK,
        ObjectKind::File => S_IFREG,
        ObjectKind::Directory => S_IFDIR,
    };
    if stat.mode & !0o7777 != 0 {
        return Err(unsupported(format!("stat mode {:#o}", stat.mode)));
    }
    let seconds = stat.modified_nanos.div_euclid(1_000_000_000);
    let nanos = stat.modified_nanos.rem_euclid(1_000_000_000);
    let (seconds, nanos) = (
        i64::try_from(seconds).map_err(|_| unsupported("stat timestamp out of range"))?,
        nanos as i64,
    );
    let mut bytes = vec![0u8; STAT_BYTES];
    let mut put =
        |offset: usize, value: &[u8]| bytes[offset..offset + value.len()].copy_from_slice(value);
    put(4, &((format | stat.mode) as u16).to_le_bytes());
    put(
        6,
        &(stat.link_count.min(u16::MAX as u64) as u16).to_le_bytes(),
    );
    // `st_ino` is 64 bits and a logical object id is 128; the low half is a
    // deterministic projection, not an identity the namespace relies on.
    put(8, &stat.object_id.0.as_u128().to_le_bytes()[..8]);
    put(16, &stat.uid.to_le_bytes());
    put(20, &stat.gid.to_le_bytes());
    for offset in [32, 48, 64, 80] {
        put(offset, &seconds.to_le_bytes());
        put(offset + 8, &nanos.to_le_bytes());
    }
    put(96, &stat.len.to_le_bytes());
    put(104, &stat.len.div_ceil(512).to_le_bytes());
    put(112, &4096u32.to_le_bytes());
    Ok(bytes)
}

#[derive(Debug, Default)]
pub struct DarwinArm64Abi;
impl SyscallAbi for DarwinArm64Abi {
    fn decode_entry(
        &self,
        regs: &RegisterSet,
        memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>> {
        let number = get(regs, 16)?;
        let op = match number {
            INTERPOSE_TRAP => decode_interpose(regs, memory)?,
            5 | 398 | 463 | 464 => {
                let slot = path_slot(number)?;
                let native = get(regs, slot + 1)?;
                let fd = get(regs, 0)? as i32;
                FsOp::Open {
                    dir: if slot == 0 || fd == -2 {
                        DirRef::Cwd
                    } else {
                        DirRef::Fd(TracedFd(fd))
                    },
                    path: read_path(memory, get(regs, slot)?)?,
                    flags: open_flags(native),
                    mode: get(regs, slot + 2)? as u32,
                }
            }
            // renameat, renameatx_np: independent source and destination
            // anchors. renameatx_np's swap/exclusive/nofollow flags change the
            // operation itself, so a non-zero mask is refused rather than
            // downgraded to an ordinary rename.
            465 | 488 => {
                if number == 488 && get(regs, 4)? as u32 != 0 {
                    return Err(unsupported(format!(
                        "renameatx_np flags {:#x}",
                        get(regs, 4)? as u32
                    )));
                }
                FsOp::Rename {
                    from_dir: dir_ref(get(regs, 0)?),
                    from: read_path(memory, get(regs, 1)?)?,
                    to_dir: dir_ref(get(regs, 2)?),
                    to: read_path(memory, get(regs, 3)?)?,
                }
            }
            471 => {
                let flags = at_flags(get(regs, 4)?, AT_SYMLINK_FOLLOW)?;
                FsOp::Link {
                    from_dir: dir_ref(get(regs, 0)?),
                    from: read_path(memory, get(regs, 1)?)?,
                    to_dir: dir_ref(get(regs, 2)?),
                    to: read_path(memory, get(regs, 3)?)?,
                    follow: flags & AT_SYMLINK_FOLLOW != 0,
                }
            }
            472 => {
                let flags = at_flags(get(regs, 2)?, AT_REMOVEDIR)?;
                FsOp::Unlink {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    directory: flags & AT_REMOVEDIR != 0,
                }
            }
            // symlink and symlinkat: x0 is the literal target the tracee reads
            // back through readlink, not a pathname to resolve, prefix or
            // physicalize. symlink has no dirfd, so its link name is anchored
            // at the process cwd.
            57 => FsOp::Symlink {
                target: read_path(memory, get(regs, 0)?)?,
                link_dir: DirRef::Cwd,
                link_name: read_path(memory, get(regs, 1)?)?,
            },
            58 => FsOp::ReadLink {
                dir: DirRef::Cwd,
                path: read_path(memory, get(regs, 0)?)?,
            },
            474 => FsOp::Symlink {
                target: read_path(memory, get(regs, 0)?)?,
                link_dir: dir_ref(get(regs, 1)?),
                link_name: read_path(memory, get(regs, 2)?)?,
            },
            475 => FsOp::Mkdir {
                dir: dir_ref(get(regs, 0)?),
                path: read_path(memory, get(regs, 1)?)?,
                mode: get(regs, 2)? as u32,
            },
            // Bare `mkdir`(136): no dirfd, so the name is anchored at the
            // process cwd exactly as `symlink`'s link name is. `/bin/mkdir`
            // issues this and no other filesystem call -- measured; the `stat`,
            // `umask` and `chmod` its `nm -u` lists belong to `-m` and `-p`.
            //
            // MUTATION PROBE -- the `mkdir` decode arm. Compiled out of every
            // build that does not ask for it, so no product binary contains this
            // branch.
            //
            // With it enabled the number falls through to the "unclassified
            // Darwin syscall" refusal below, and **what that produces is a
            // stopped run**, not the pre-#114 behaviour. This comment used to
            // claim the opposite -- that the operand "reaches the kernel
            // unrewritten and Seatbelt refuses the call" -- and that was false
            // twice over: before this arm existed, `mkdir` was not in
            // `TRACED_STUBS`, so 136 was never breakpointed and never reached
            // this decoder at all; and a decode error is propagated by
            // `syscall_entry` with `?`, which has no resume fallback. The probe
            // therefore creates a third state that neither ships nor shipped,
            // and saying so is the point: a probe whose justification describes
            // a mechanism the code does not have cannot be checked by a reader.
            //
            // The probe still discriminates, and the test says how: `mkdir`
            // exits nonzero with the directory nowhere -- not on the host, not
            // in the store -- **and** `touch <absent>` still exits 0 with its
            // file in the store, in the same probe binary. That positive half
            // is what stops the case passing for an unrelated common cause; it
            // was added after the shipped tree's own `intercept()` defect was
            // found to satisfy every assertion the probe originally made.
            #[cfg(not(feature = "mutation-probe-mkdir"))]
            136 => FsOp::Mkdir {
                dir: DirRef::Cwd,
                path: read_path(memory, get(regs, 0)?)?,
                mode: get(regs, 1)? as u32,
            },
            // `fstat`(339) and `__fstat`(189): a descriptor and an output
            // buffer, and no path operand at all. The descriptor's logical
            // anchor belongs to the namespace, so nothing is resolved here; the
            // buffer is reported through `io_buffer` rather than carried on the
            // operation, for the reason that method states.
            //
            // Both numbers decode identically because both stubs are the same
            // call: `fstat64` resolves to the `fstat` symbol, and `__fstat` is a
            // separate stub reaching 189. The `fstatat`/`__fstatat` pair above
            // has the same shape and the same reason.
            189 | 339 => FsOp::Fstat {
                fd: TracedFd(get(regs, 0)? as i32),
            },
            // `setattrlistat`(524). libc's `utimensat` carries no `svc`: it
            // builds an `attrlist` and tail-calls this, so this is the only
            // place a time change can be intercepted. `setattrlist_times`
            // refuses any attrlist shape outside the modtime/acctime pair that
            // wrapper emits.
            524 => {
                let (accessed_nanos, modified_nanos, follow) = setattrlist_times(regs, memory)?;
                FsOp::SetTimes {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    accessed_nanos,
                    modified_nanos,
                    follow,
                }
            }
            467 => {
                let flags = at_flags(get(regs, 3)?, AT_SYMLINK_NOFOLLOW)?;
                FsOp::Chmod {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    mode: get(regs, 2)? as u32,
                    follow: flags & AT_SYMLINK_NOFOLLOW == 0,
                }
            }
            // fstatat (469) and fstatat64 (470) share this argument layout.
            // libc's `fstatat`/`fstatat64` is one symbol reaching 470; 469 is
            // the separate `__fstatat` stub. Both are decoded; neither buffer
            // layout is interpreted here, because the kernel writes it.
            469 | 470 => {
                let flags = at_flags(get(regs, 3)?, AT_SYMLINK_NOFOLLOW)?;
                FsOp::Stat {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    follow: flags & AT_SYMLINK_NOFOLLOW == 0,
                }
            }
            473 => FsOp::ReadLink {
                dir: dir_ref(get(regs, 0)?),
                path: read_path(memory, get(regs, 1)?)?,
            },
            // faccessat: x2 carries the R_OK|W_OK|X_OK mask, or F_OK (0) for a
            // bare existence probe. A bit outside that mask is a check this
            // decode does not represent, so it is refused rather than dropped.
            466 => {
                let flags = at_flags(get(regs, 3)?, AT_EACCESS | AT_SYMLINK_NOFOLLOW)?;
                let mode = get(regs, 2)? as u32;
                if mode & !7 != 0 {
                    return Err(unsupported(format!("faccessat mode {mode:#x}")));
                }
                FsOp::Access {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    mode: AccessMode {
                        read: mode & 4 != 0,
                        write: mode & 2 != 0,
                        execute: mode & 1 != 0,
                    },
                    flags: AccessFlags {
                        effective_ids: flags & AT_EACCESS != 0,
                        follow: flags & AT_SYMLINK_NOFOLLOW == 0,
                    },
                }
            }
            // fchownat: uid/gid are unsigned, and the caller spells "leave this
            // one alone" as -1, which arrives as 0xffffffff. Decoding that as a
            // literal ID 4294967295 would be an ownership change, not a no-op.
            468 => {
                let flags = at_flags(get(regs, 4)?, AT_SYMLINK_NOFOLLOW)?;
                let id = |raw: u64| match raw as u32 {
                    u32::MAX => None,
                    value => Some(value),
                };
                FsOp::Fchownat {
                    dir: dir_ref(get(regs, 0)?),
                    path: read_path(memory, get(regs, 1)?)?,
                    uid: id(get(regs, 2)?),
                    gid: id(get(regs, 3)?),
                    flags: ChownFlags {
                        follow: flags & AT_SYMLINK_NOFOLLOW == 0,
                    },
                }
            }
            // Positively classified process-control calls handled by the backend.
            1 | 2 | 7 | 20 | 59 | 244 | 400 => return Ok(None),
            _ => return Err(unsupported(format!("unclassified Darwin syscall {number}"))),
        };
        Ok(Some(op))
    }
    fn encode_stat(&self, stat: &BlobStat) -> Result<Vec<u8>> {
        // The free function is the implementation and stays public: the dirfd
        // integration fixture injects its own `StatEncoder` over it, and the
        // unit tests below check the layout offset by offset.
        encode_stat(stat)
    }
    fn io_buffer(&self, regs: &RegisterSet) -> Result<Option<IoBuffer>> {
        // `fstat` is the one breakpointed syscall on this ABI that names an
        // output buffer of its own. Its length is not a caller-supplied count
        // but the fixed width of the layout the kernel would have written, so it
        // is reported as that: a caller writing fewer bytes would leave the
        // tail of the tracee's `struct stat` holding whatever was there before.
        //
        // **The same null/overflow guard the interposer branch below applies,
        // and for the same measured reason.** `fstat(fd, NULL)` is an ordinary
        // program bug, and Darwin answers it `EFAULT` -- so the errno travels
        // *inside* the error, for the caller to bind as a tracee-visible
        // refusal, exactly as `read(fd, NULL, 4)` does. Without it the bad
        // pointer reached `write_memory`, which failed, which stopped the whole
        // run: the defect `routing_for`'s doc comment records as measured and
        // fixed, one syscall over.
        if matches!(get(regs, 16)?, 189 | 339) {
            let address = stat_buffer(regs)?;
            if address == 0 || address.checked_add(STAT_BYTES as u64).is_none() {
                return Err(
                    error("io_buffer", "null or overflowing stat buffer").with_errno(Errno(14))
                );
            }
            return Ok(Some(IoBuffer {
                address,
                length: STAT_BYTES as u32,
            }));
        }
        if get(regs, 16)? != INTERPOSE_TRAP {
            // Every other intercepted call on this ABI names paths, not data
            // buffers. `read`/`write` are not breakpointed syscalls here: the
            // only data transfer umbra services besides `fstat`'s reply is the
            // interposer's.
            return Ok(None);
        }
        if !matches!(get(regs, 0)?, INTERPOSE_READ | INTERPOSE_WRITE) {
            return Ok(None);
        }
        let address = get(regs, 2)?;
        let length = transfer_length(get(regs, 3)?);
        // A zero-length transfer has no buffer to bind and is answered without
        // one; anything that would run off the end of the address space is a
        // malformed request rather than a short read.
        if length == 0 {
            return Ok(None);
        }
        if address == 0 || address.checked_add(length).is_none() {
            return Err(
                error("io_buffer", "null or overflowing transfer buffer").with_errno(Errno(14))
            );
        }
        Ok(Some(IoBuffer {
            address,
            length: length as u32,
        }))
    }
    fn apply_rewrite(&self, regs: &mut RegisterSet, rewrite: &PreparedRewrite) -> Result<()> {
        validate(regs)?;
        if rewrite.arguments.iter().any(|a| a.index > 7)
            || rewrite.memory_writes.iter().any(|w| {
                w.address == 0
                    || w.bytes.len() > MAX_IO_BYTES
                    || w.address.checked_add(w.bytes.len() as u64).is_none()
            })
        {
            return Err(error("rewrite", "invalid argument or memory bounds"));
        }
        for arg in &rewrite.arguments {
            set(regs, arg.index as usize, arg.value)?;
        }
        Ok(())
    }
    fn emulate_result(&self, regs: &mut RegisterSet, result: &EmulatedResult) -> Result<()> {
        let pc = get(regs, PC)?
            .checked_add(4)
            .ok_or_else(|| error("emulate", "PC overflow"))?;
        let mut flags = get(regs, CPSR)? & !(1 << 29);
        let value = match result.outcome {
            OperationOutcome::Success { return_value } => return_value,
            OperationOutcome::Failure(Errno(errno)) if errno > 0 => {
                flags |= 1 << 29;
                errno as u64
            }
            _ => return Err(error("emulate", "errno must be positive")),
        };
        set(regs, 0, value)?;
        set(regs, CPSR, flags)?;
        set(regs, PC, pc)
    }
}
pub fn outcome(regs: &RegisterSet) -> Result<OperationOutcome> {
    Ok(if get(regs, CPSR)? & (1 << 29) != 0 {
        OperationOutcome::Failure(Errno(get(regs, 0)? as i32))
    } else {
        OperationOutcome::Success {
            return_value: get(regs, 0)?,
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The C source of the interposer, so the wire format below is checked
    /// against the other side of it rather than against a second copy of this
    /// side. `build.rs` compiles this same file into the shipped dylib.
    const INTERPOSER_C: &str = include_str!("../interpose/umbra_interpose.c");

    /// One `#define NAME <literal>` from the interposer's header block.
    fn defined(name: &str) -> u64 {
        let line = INTERPOSER_C
            .lines()
            .find_map(|line| line.strip_prefix(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("{name} is not defined in umbra_interpose.c"));
        let literal = line
            .split_whitespace()
            .next()
            .expect("a #define has a value")
            .trim_end_matches("ULL");
        match literal.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => literal.parse(),
        }
        .unwrap_or_else(|e| panic!("{name} = {literal:?} is not a number: {e}"))
    }

    /// The trap number and the four operation codes are a **wire format** shared
    /// with `interpose/umbra_interpose.c`, and nothing but this test makes the two
    /// copies agree.
    ///
    /// It is the test `INTERPOSE_TRAP`'s doc comment has always named. It did not
    /// exist -- CodeRabbit found the dangling reference on PR #116 (item 1) -- so
    /// the claim was the only thing holding the two sides together. A drift here
    /// is silent and total: the interposer traps with an x16 the decoder does not
    /// recognise, so every routed call falls through to a syscall number the
    /// kernel answers `ENOSYS` while posting `SIGSYS`.
    #[test]
    fn interpose_trap_number_matches_the_interposer() {
        assert_eq!(
            INTERPOSE_TRAP,
            defined("UMBRA_TRAP_NUMBER"),
            "the reserved x16 trap value disagrees with the interposer's"
        );
        for (name, ours) in [
            ("UMBRA_OP_OPEN", INTERPOSE_OPEN),
            ("UMBRA_OP_READ", INTERPOSE_READ),
            ("UMBRA_OP_WRITE", INTERPOSE_WRITE),
            ("UMBRA_OP_CLOSE", INTERPOSE_CLOSE),
        ] {
            assert_eq!(
                ours,
                defined(name),
                "{name} disagrees with the interposer's"
            );
        }
    }

    struct Memory {
        base: u64,
        data: Vec<u8>,
        reads: usize,
    }
    impl TraceMemory for Memory {
        fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
            self.reads += 1;
            let start = (address - self.base) as usize;
            let data = self
                .data
                .get(start..start + out.len())
                .ok_or_else(|| error("test", "unmapped page"))?;
            out.copy_from_slice(data);
            Ok(())
        }
    }
    #[test]
    fn bounded_path_preserves_bytes_and_page_boundary() {
        let mut m = Memory {
            base: 4093,
            data: b"/\xff\0".to_vec(),
            reads: 0,
        };
        assert_eq!(read_path(&mut m, 4093).unwrap().as_bytes(), b"/\xff");
        m = Memory {
            base: 4096,
            data: vec![1; 4096],
            reads: 0,
        };
        assert!(read_path(&mut m, 4096).is_err());
        assert!(m.reads <= 65);
    }
    /// Registers for a syscall entry: slot N is xN, the number in x16.
    fn entry(number: u64, args: [u64; 5]) -> RegisterSet {
        let mut regs = RegisterSet::new(Architecture::Aarch64, vec![0; 272]).unwrap();
        set(&mut regs, 16, number).unwrap();
        for (slot, value) in args.iter().enumerate() {
            set(&mut regs, slot, *value).unwrap();
        }
        regs
    }
    /// Four NUL-terminated byte strings at fixed offsets from 4096. Non-UTF-8
    /// bytes are deliberate: paths are bytes, never lossy strings.
    fn strings() -> Memory {
        let mut data = vec![0; 256];
        for (offset, bytes) in [
            (0usize, b"/src/\xff".as_slice()),
            (32, b"dst-\xfe".as_slice()),
            (64, b"/abs/one".as_slice()),
            (96, b"two".as_slice()),
        ] {
            data[offset..offset + bytes.len()].copy_from_slice(bytes);
        }
        Memory {
            base: 4096,
            data,
            reads: 0,
        }
    }
    const SRC: u64 = 4096;
    const DST: u64 = 4096 + 32;
    const ABS: u64 = 4096 + 64;
    const REL: u64 = 4096 + 96;

    /// The literal AT_FDCWD register value, spelled out rather than derived
    /// from the constant under test.
    const CWD: u64 = -2i32 as u32 as u64;

    #[test]
    fn dirfd_relative_syscalls_decode_their_declared_operands() {
        // Pin the SDK values themselves: sys/fcntl.h:172-180. Deriving the
        // test inputs from these constants would make any edit self-consistent.
        assert_eq!(AT_FDCWD, -2);
        assert_eq!(AT_EACCESS, 0x0010);
        assert_eq!(AT_SYMLINK_NOFOLLOW, 0x0020);
        assert_eq!(AT_SYMLINK_FOLLOW, 0x0040);
        assert_eq!(AT_REMOVEDIR, 0x0080);
        let mut memory = strings();
        let src = BytePath::new(b"/src/\xff".to_vec()).unwrap();
        let dst = BytePath::new(b"dst-\xfe".to_vec()).unwrap();
        let abs = BytePath::new(b"/abs/one".to_vec()).unwrap();
        let rel = BytePath::new(b"two".to_vec()).unwrap();
        // renameat: source dirfd x0/source x1, destination dirfd x2/destination
        // x3. A copied source dirfd or an x0/x1 swap changes this result.
        let op = DarwinArm64Abi
            .decode_entry(&entry(465, [7, ABS, 9, REL, 0]), &mut memory)
            .unwrap()
            .unwrap();
        assert_eq!(
            op,
            FsOp::Rename {
                from_dir: DirRef::Fd(TracedFd(7)),
                from: abs.clone(),
                to_dir: DirRef::Fd(TracedFd(9)),
                to: rel.clone(),
            }
        );
        // AT_FDCWD is the signed sentinel -2, in either operand slot, and an
        // absolute path keeps its bytes whatever the anchor is.
        let op = DarwinArm64Abi
            .decode_entry(&entry(465, [CWD, SRC, u64::MAX, DST, 0]), &mut memory)
            .unwrap()
            .unwrap();
        assert_eq!(
            op,
            FsOp::Rename {
                from_dir: DirRef::Cwd,
                from: src.clone(),
                // An invalid descriptor is decoded, not rejected: the namespace
                // ignores the anchor for an absolute path and validates it only
                // when it actually anchors a relative one.
                to_dir: DirRef::Fd(TracedFd(-1)),
                to: dst.clone(),
            }
        );
        // renameatx_np shares the layout; flags live in x4.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(488, [CWD, ABS, 3, REL, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Rename {
                from_dir: DirRef::Cwd,
                from: abs.clone(),
                to_dir: DirRef::Fd(TracedFd(3)),
                to: rel.clone(),
            }
        );
        // linkat: same two-operand layout plus AT_SYMLINK_FOLLOW in x4.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(471, [4, SRC, 5, DST, 0x0040]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Link {
                from_dir: DirRef::Fd(TracedFd(4)),
                from: src.clone(),
                to_dir: DirRef::Fd(TracedFd(5)),
                to: dst.clone(),
                follow: true,
            }
        );
        // unlinkat: AT_REMOVEDIR in x2 selects the directory form.
        for (flags, directory) in [(0, false), (0x0080, true)] {
            assert_eq!(
                DarwinArm64Abi
                    .decode_entry(&entry(472, [6, REL, flags, 0, 0]), &mut memory)
                    .unwrap()
                    .unwrap(),
                FsOp::Unlink {
                    dir: DirRef::Fd(TracedFd(6)),
                    path: rel.clone(),
                    directory,
                }
            );
        }
        // symlinkat: x0 is the literal target, x1 the link dirfd, x2 the name.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(474, [SRC, 8, REL, 0, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Symlink {
                target: src.clone(),
                link_dir: DirRef::Fd(TracedFd(8)),
                link_name: rel.clone(),
            }
        );
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(475, [CWD, ABS, 0o755, 0, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Mkdir {
                dir: DirRef::Cwd,
                path: abs.clone(),
                mode: 0o755,
            }
        );
        // fchmodat, fstatat and fstatat64 all read AT_SYMLINK_NOFOLLOW.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(467, [2, REL, 0o640, 0x0020, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Chmod {
                dir: DirRef::Fd(TracedFd(2)),
                path: rel.clone(),
                mode: 0o640,
                follow: false,
            }
        );
        for number in [469, 470] {
            assert_eq!(
                DarwinArm64Abi
                    .decode_entry(&entry(number, [3, ABS, 0, 0, 0]), &mut memory)
                    .unwrap()
                    .unwrap(),
                FsOp::Stat {
                    dir: DirRef::Fd(TracedFd(3)),
                    path: abs.clone(),
                    follow: true,
                }
            );
        }
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(473, [CWD, REL, 0, 0, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::ReadLink {
                dir: DirRef::Cwd,
                path: rel.clone(),
            }
        );
        // faccessat: x2 is the R_OK|W_OK|X_OK mask, x3 the flags. R_OK is 4,
        // W_OK 2 and X_OK 1, so each mode bit is pinned separately against a
        // transposed read/execute pair.
        for (mode, expected) in [
            (
                0u64,
                AccessMode {
                    read: false,
                    write: false,
                    execute: false,
                },
            ),
            (
                1,
                AccessMode {
                    read: false,
                    write: false,
                    execute: true,
                },
            ),
            (
                2,
                AccessMode {
                    read: false,
                    write: true,
                    execute: false,
                },
            ),
            (
                4,
                AccessMode {
                    read: true,
                    write: false,
                    execute: false,
                },
            ),
            (
                7,
                AccessMode {
                    read: true,
                    write: true,
                    execute: true,
                },
            ),
        ] {
            assert_eq!(
                DarwinArm64Abi
                    .decode_entry(&entry(466, [5, ABS, mode, 0, 0]), &mut memory)
                    .unwrap()
                    .unwrap(),
                FsOp::Access {
                    dir: DirRef::Fd(TracedFd(5)),
                    path: abs.clone(),
                    mode: expected,
                    flags: AccessFlags {
                        effective_ids: false,
                        follow: true,
                    },
                },
                "faccessat mode {mode:#x}"
            );
        }
        // AT_EACCESS and AT_SYMLINK_NOFOLLOW are independent, and `follow` is
        // the inverse of the nofollow bit rather than a flag of its own.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(466, [CWD, REL, 4, 0x0010 | 0x0020, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Access {
                dir: DirRef::Cwd,
                path: rel.clone(),
                mode: AccessMode {
                    read: true,
                    write: false,
                    execute: false,
                },
                flags: AccessFlags {
                    effective_ids: true,
                    follow: false,
                },
            }
        );
        // fchownat: x2 uid, x3 gid, x4 flags. Both IDs are ordinary values here.
        assert_eq!(
            DarwinArm64Abi
                .decode_entry(&entry(468, [6, ABS, 501, 20, 0]), &mut memory)
                .unwrap()
                .unwrap(),
            FsOp::Fchownat {
                dir: DirRef::Fd(TracedFd(6)),
                path: abs.clone(),
                uid: Some(501),
                gid: Some(20),
                flags: ChownFlags { follow: true },
            }
        );
        // The unchanged-ID sentinel is -1 in an unsigned field. Each operand
        // carries it independently, and it is a no-op rather than ID 4294967295.
        // The upper half of the register is not the callee's value, so a
        // sign-extended -1 must decode the same way as a bare 0xffffffff.
        for (uid_raw, gid_raw, uid, gid) in [
            (0xffff_ffffu64, 0xffff_ffffu64, None, None),
            (u64::MAX, 0, None, Some(0)),
            (0, u64::MAX, Some(0), None),
            (0xffff_ffff_ffff_fffe, 7, Some(u32::MAX - 1), Some(7)),
        ] {
            assert_eq!(
                DarwinArm64Abi
                    .decode_entry(
                        &entry(468, [CWD, REL, uid_raw, gid_raw, 0x0020]),
                        &mut memory
                    )
                    .unwrap()
                    .unwrap(),
                FsOp::Fchownat {
                    dir: DirRef::Cwd,
                    path: rel.clone(),
                    uid,
                    gid,
                    flags: ChownFlags { follow: false },
                },
                "fchownat uid {uid_raw:#x} gid {gid_raw:#x}"
            );
        }
    }

    #[test]
    fn faccessat_and_fchownat_physicalize_their_dirfd_relative_path() {
        // prepare_paths refuses a syscall path_operands does not list, so an
        // omission here makes every resolved rewrite fail at rewrite time
        // rather than at decode.
        for number in [466u64, 468] {
            assert_eq!(
                path_operands(number).unwrap(),
                &[(PathOperand::Path, 1)],
                "syscall {number}"
            );
            assert_eq!(path_slot(number).unwrap(), 1, "syscall {number}");
        }
    }

    #[test]
    fn unmodelled_flags_and_untyped_operations_are_refused() {
        let mut memory = strings();
        // renameatx_np swap/exclusive/nofollow change the operation itself.
        // Downgrading any of them to an ordinary rename would erase semantics.
        for flags in [0x2u64, 0x4, 0x10, 0x6] {
            let err = DarwinArm64Abi
                .decode_entry(&entry(488, [1, SRC, 2, DST, flags]), &mut memory)
                .unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::UnsupportedCapability,
                "flags {flags:#x}"
            );
        }
        // Zero flags is an ordinary rename, and only the low word is the
        // callee's: arm64 leaves the upper half of an `int` unspecified.
        assert!(DarwinArm64Abi
            .decode_entry(
                &entry(488, [1, SRC, 2, DST, 0xffff_ffff_0000_0000]),
                &mut memory
            )
            .is_ok());
        // *at bits the decode does not model are refused, never dropped.
        for (number, args) in [
            (472u64, [1, REL, 0x0020, 0, 0]),
            (471, [1, SRC, 2, DST, 0x0080]),
            (467, [1, REL, 0o600, 0x0040, 0]),
            (469, [1, ABS, 0, 0x0800, 0]),
        ] {
            let err = DarwinArm64Abi
                .decode_entry(&entry(number, args), &mut memory)
                .unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::UnsupportedCapability,
                "syscall {number}"
            );
        }
        // faccessat models AT_EACCESS and AT_SYMLINK_NOFOLLOW only, and
        // fchownat only the latter: AT_EACCESS is not a fchownat flag.
        for (number, args) in [
            (466u64, [1, REL, 0, 0x0040, 0]),
            (466, [1, REL, 0, 0x0080, 0]),
            (468, [1, REL, 0, 0, 0x0010]),
            (468, [1, REL, 0, 0, 0x0040]),
        ] {
            let err = DarwinArm64Abi
                .decode_entry(&entry(number, args), &mut memory)
                .unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::UnsupportedCapability,
                "syscall {number} x3 {:#x} x4 {:#x}",
                args[3],
                args[4]
            );
            // decode_entry's `_` fallback returns this same kind, so the kind
            // alone cannot tell "we modelled this flag and rejected it" from
            // "we never modelled this syscall". Without this the test stays
            // green even if the whole 466 or 468 arm is deleted.
            assert!(
                !err.context.contains("unclassified"),
                "syscall {number} reached the unclassified fallback"
            );
        }
        // An access mode outside R_OK|W_OK|X_OK is a check this decode does not
        // represent. Refusing beats silently narrowing it to the low three bits.
        for mode in [8u64, 0x10, 0xf] {
            let err = DarwinArm64Abi
                .decode_entry(&entry(466, [1, REL, mode, 0, 0]), &mut memory)
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::UnsupportedCapability, "mode {mode:#x}");
            assert!(
                !err.context.contains("unclassified"),
                "mode {mode:#x} reached the unclassified fallback"
            );
        }
        // Only the low word is the callee's, so upper-half garbage in either
        // the mode or the flag register is not a refusal.
        assert!(DarwinArm64Abi
            .decode_entry(
                &entry(
                    466,
                    [1, REL, 0xffff_ffff_0000_0007, 0xffff_ffff_0000_0000, 0]
                ),
                &mut memory
            )
            .is_ok());
    }

    #[test]
    fn two_path_rewrites_stay_isolated() {
        let regs = entry(465, [7, SRC, 9, DST, 0]);
        let physical = |paths: Vec<(PathOperand, &[u8])>| PhysicalOperation {
            operation: FsOp::Rename {
                from_dir: DirRef::Fd(TracedFd(7)),
                from: BytePath::new(b"a".to_vec()).unwrap(),
                to_dir: DirRef::Fd(TracedFd(9)),
                to: BytePath::new(b"b".to_vec()).unwrap(),
            },
            paths: paths
                .into_iter()
                .map(|(operand, path)| PathRewrite {
                    operand,
                    path: PhysicalPath(BytePath::new(path.to_vec()).unwrap()),
                })
                .collect(),
        };
        let plan = prepare_paths(
            &regs,
            physical(vec![
                (PathOperand::Source, b"/shadow/\xff"),
                (PathOperand::Destination, b"/shadow/to"),
            ]),
            &[8192, 16384],
        )
        .unwrap();
        // Source lands in x1 and destination in x3, in separate buffers; the
        // dirfd registers are untouched because a qualified physical path is
        // absolute and the kernel ignores the anchor for one.
        assert_eq!(
            plan.arguments,
            vec![
                ArgumentRewrite {
                    index: 1,
                    value: 8192
                },
                ArgumentRewrite {
                    index: 3,
                    value: 16384
                }
            ]
        );
        assert_eq!(plan.memory_writes[0].bytes, b"/shadow/\xff\0");
        assert_eq!(plan.memory_writes[1].bytes, b"/shadow/to\0");
        let mut applied = regs.clone();
        DarwinArm64Abi.apply_rewrite(&mut applied, &plan).unwrap();
        assert_eq!(get(&applied, 0).unwrap(), 7);
        assert_eq!(get(&applied, 1).unwrap(), 8192);
        assert_eq!(get(&applied, 2).unwrap(), 9);
        assert_eq!(get(&applied, 3).unwrap(), 16384);
        // A missing, duplicated or single-operand plan is refused rather than
        // half applied: one rewritten endpoint would mutate outside the shadow.
        for paths in [
            vec![(PathOperand::Source, b"/shadow/a".as_slice())],
            vec![
                (PathOperand::Source, b"/shadow/a".as_slice()),
                (PathOperand::Source, b"/shadow/b".as_slice()),
            ],
            vec![
                (PathOperand::Path, b"/shadow/a".as_slice()),
                (PathOperand::Destination, b"/shadow/b".as_slice()),
            ],
        ] {
            let count = paths.len();
            assert!(
                prepare_paths(&regs, physical(paths), &[8192, 16384][..count]).is_err(),
                "operand set must be exact"
            );
        }
        // The one-path wrapper still targets a single-operand syscall's slot.
        let open = entry(463, [3, 0, 0, 0, 0]);
        let plan = prepare_path(
            &open,
            8192,
            &BytePath::new(b"/shadow/x".to_vec()).unwrap(),
            FsOp::Stat {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/x".to_vec()).unwrap(),
                follow: true,
            },
        )
        .unwrap();
        assert_eq!(
            plan.arguments,
            vec![ArgumentRewrite {
                index: 1,
                value: 8192
            }]
        );
        assert!(
            path_slot(465).is_err(),
            "a two-path syscall has no single slot"
        );
    }

    fn blob(kind: ObjectKind, len: u64, mode: u32) -> BlobStat {
        BlobStat {
            object_id: ObjectId(uuid::Uuid::from_u128(
                0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            )),
            kind,
            len,
            link_count: 1,
            mode,
            uid: 501,
            gid: 20,
            modified_nanos: 1_500_000_000,
        }
    }

    #[test]
    fn darwin_stat_encoding_never_exposes_a_symlink_placeholder() {
        // Field offsets and the 144-byte size are sys/stat.h's
        // __DARWIN_INODE64 layout, read from the host through `offsetof`.
        let bytes = encode_stat(&blob(ObjectKind::LogicalSymlink, 7, 0o777)).unwrap();
        assert_eq!(bytes.len(), STAT_BYTES);
        let mode = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as u32;
        // S_IFLNK, never the placeholder object's S_IFREG.
        assert_eq!(mode & 0o170000, 0o120000);
        assert_eq!(mode & 0o7777, 0o777);
        // st_size is the target length, not the placeholder's zero length.
        assert_eq!(u64::from_le_bytes(bytes[96..104].try_into().unwrap()), 7);
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 1);
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            0x090a_0b0c_0d0e_0f10
        );
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 501);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 20);
        // st_mtimespec splits into whole seconds and a non-negative remainder.
        assert_eq!(i64::from_le_bytes(bytes[48..56].try_into().unwrap()), 1);
        assert_eq!(
            i64::from_le_bytes(bytes[56..64].try_into().unwrap()),
            500_000_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[112..116].try_into().unwrap()),
            4096
        );
        for (kind, format) in [
            (ObjectKind::File, 0o100000u32),
            (ObjectKind::Directory, 0o040000),
        ] {
            let bytes = encode_stat(&blob(kind, 3, 0o644)).unwrap();
            let mode = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as u32;
            assert_eq!(mode & 0o170000, format);
        }
        // A mode carrying format bits is not a permission set: refuse rather
        // than fold it into the kind this layout already decided.
        assert!(encode_stat(&blob(ObjectKind::File, 0, 0o100644)).is_err());
    }

    #[test]
    fn readlink_and_stat_buffers_come_from_their_own_registers() {
        // readlink keeps buffer/length in x1/x2, readlinkat in x2/x3, and the
        // length is the callee's 64-bit `size_t`.
        let mut regs = entry(58, [SRC, 4096, 0x0000_0000_0000_0040, 0, 0]);
        assert_eq!(readlink_buffer(&regs).unwrap(), (4096, 0x40));
        regs = entry(473, [3, SRC, 8192, 16, 0]);
        assert_eq!(readlink_buffer(&regs).unwrap(), (8192, 16));
        for (number, slot) in [(58, 2), (473, 3)] {
            let mut regs = entry(number, [SRC, 4096, 8192, 16, 0]);
            set(&mut regs, slot, MAX_IO_BYTES as u64).unwrap();
            assert_eq!(readlink_buffer(&regs).unwrap().1, MAX_IO_BYTES as u32);
            for len in [
                MAX_IO_BYTES as u64 + 1,
                0x1_0000_0040,
                0xffff_ffff_0000_0040,
            ] {
                set(&mut regs, slot, len).unwrap();
                assert!(readlink_buffer(&regs).is_err());
            }
        }
        assert!(readlink_buffer(&entry(5, [0; 5])).is_err());
        for number in [469, 470] {
            assert_eq!(
                stat_buffer(&entry(number, [1, SRC, 2048, 0, 0])).unwrap(),
                2048
            );
        }
        assert!(stat_buffer(&entry(58, [0; 5])).is_err());
    }

    #[test]
    fn openat_rewrite_and_errno() {
        let mut r = RegisterSet::new(Architecture::Aarch64, vec![0; 272]).unwrap();
        set(&mut r, 16, 463).unwrap();
        set(&mut r, 0, 9).unwrap();
        set(&mut r, 1, 4096).unwrap();
        set(&mut r, 2, 0x601).unwrap();
        let mut m = Memory {
            base: 4096,
            data: vec![0; 64],
            reads: 0,
        };
        m.data[..3].copy_from_slice(b"/a\0");
        let op = DarwinArm64Abi.decode_entry(&r, &mut m).unwrap().unwrap();
        assert!(matches!(
            op,
            FsOp::Open {
                dir: DirRef::Fd(TracedFd(9)),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    truncate: true,
                    ..
                },
                ..
            }
        ));
        let plan = prepare_path(
            &r,
            8192,
            &BytePath::new(b"/shadow/\xff".to_vec()).unwrap(),
            op,
        )
        .unwrap();
        DarwinArm64Abi.apply_rewrite(&mut r, &plan).unwrap();
        assert_eq!(get(&r, 1).unwrap(), 8192);
        DarwinArm64Abi
            .emulate_result(
                &mut r,
                &EmulatedResult {
                    outcome: OperationOutcome::Failure(Errno(13)),
                    memory_writes: vec![],
                },
            )
            .unwrap();
        assert_eq!(outcome(&r).unwrap(), OperationOutcome::Failure(Errno(13)));
        assert_eq!(get(&r, PC).unwrap(), 4);
        set(&mut r, 16, 999).unwrap();
        assert!(DarwinArm64Abi.decode_entry(&r, &mut m).is_err());
    }

    /// A `setattrlistat` entry over a memory image, with the attrlist and the
    /// attribute buffer laid out the way libc's `utimensat` lays them out.
    ///
    /// Offsets are fixed and distinct so a decode that read the wrong operand
    /// slot reads the wrong bytes rather than plausible ones.
    fn setattrlistat_entry(
        common: u32,
        times: &[(i64, i64)],
        options: u64,
        bitmapcount: u16,
        volattr: u32,
        size_override: Option<u64>,
    ) -> (RegisterSet, Memory) {
        const PATH: u64 = 4096;
        const LIST: u64 = 4096 + 64;
        const BUFFER: u64 = 4096 + 128;
        let mut data = vec![0u8; 256];
        data[..12].copy_from_slice(b"/w/seed.txt\0");
        let list = 64;
        data[list..list + 2].copy_from_slice(&bitmapcount.to_le_bytes());
        data[list + 4..list + 8].copy_from_slice(&common.to_le_bytes());
        data[list + 8..list + 12].copy_from_slice(&volattr.to_le_bytes());
        for (index, (seconds, nanos)) in times.iter().enumerate() {
            let at = 128 + index * TIMESPEC_BYTES;
            data[at..at + 8].copy_from_slice(&seconds.to_le_bytes());
            data[at + 8..at + 16].copy_from_slice(&nanos.to_le_bytes());
        }
        let mut regs = entry(
            524,
            [
                CWD,
                PATH,
                LIST,
                BUFFER,
                size_override.unwrap_or((times.len() * TIMESPEC_BYTES) as u64),
            ],
        );
        set(&mut regs, 5, options).unwrap();
        (
            regs,
            Memory {
                base: 4096,
                data,
                reads: 0,
            },
        )
    }

    /// Bare `mkdir`(136) is the whole of `/bin/mkdir`'s filesystem footprint, so
    /// it has to decode to the same operation `mkdirat` does -- anchored at the
    /// process cwd, because it carries no dirfd -- and its operand has to be
    /// rewritable. Neither was true before
    /// [#114](https://github.com/invakid404/umbra/issues/114): the number
    /// reached the "unclassified Darwin syscall" refusal.
    #[test]
    fn bare_mkdir_decodes_to_a_cwd_anchored_mkdir_with_a_rewritable_operand() {
        let mut memory = strings();
        let decoded = DarwinArm64Abi
            .decode_entry(&entry(136, [ABS, 0o755, 0, 0, 0]), &mut memory)
            .unwrap();
        assert_eq!(
            decoded,
            Some(FsOp::Mkdir {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/abs/one".to_vec()).unwrap(),
                mode: 0o755,
            })
        );
        // The operand row. It is a fact about the syscall's shape, and on the
        // `Mkdir` path it is **inert**: `path_operands` is consulted only when
        // an action is a `Rewrite`, and `Overlay::resolve` answers `Mkdir` with
        // `Emulate` on every backend -- see
        // `mkdir_resolves_to_an_emulated_answer_with_no_rewrite_to_depend_on`.
        // Asserted anyway, because a wrong slot here would be a latent trap for
        // the first caller that does take a rewrite, and because #114 named
        // this table as half the fix.
        assert_eq!(path_operands(136).unwrap(), &[(PathOperand::Path, 0)]);
        // The at-form sibling keeps its own slot; one is not the other's alias.
        assert_eq!(path_operands(475).unwrap(), &[(PathOperand::Path, 1)]);
    }

    /// `fstat` is the first descriptor-relative call the tracer carries, and the
    /// three things that makes true are asserted together because a decode that
    /// got any one of them wrong would write a `struct stat` somewhere the tracee
    /// did not ask for: it names a descriptor and no path, its output buffer is
    /// in x1 rather than the at-form's x2, and that buffer is exactly one
    /// `struct stat` wide.
    #[test]
    fn fstat_decodes_to_a_descriptor_operation_and_names_its_own_output_buffer() {
        for number in [189u64, 339] {
            let mut memory = strings();
            let regs = entry(number, [7, 8192, 0, 0, 0]);
            assert_eq!(
                DarwinArm64Abi.decode_entry(&regs, &mut memory).unwrap(),
                Some(FsOp::Fstat { fd: TracedFd(7) }),
                "syscall {number}"
            );
            assert_eq!(memory.reads, 0, "syscall {number} read tracee memory");
            assert_eq!(stat_buffer(&regs).unwrap(), 8192, "syscall {number}");
            assert_eq!(
                DarwinArm64Abi.io_buffer(&regs).unwrap(),
                Some(IoBuffer {
                    address: 8192,
                    length: STAT_BYTES as u32,
                }),
                "syscall {number}"
            );
            // No path operand exists, so asking for one is refused rather than
            // answered with slot zero.
            assert!(path_operands(number).is_err(), "syscall {number}");
        }
        // The at-form's buffer slot is unchanged by the addition.
        assert_eq!(stat_buffer(&entry(470, [0, 0, 4242, 0, 0])).unwrap(), 4242);
    }

    /// The exact shape libc's `utimensat` emits, measured on macOS 26.5.1 arm64
    /// by breaking on the stub and reading the tracee's memory back.
    ///
    /// The ordering claim is the one worth a test of its own: the attribute
    /// buffer is packed in ascending *bitmap* order, so the modification time
    /// comes first, which is the reverse of the `times[2]` array the caller
    /// wrote. Reading them the other way round would silently swap a file's
    /// access and modification times, which no later layer could detect.
    #[test]
    fn setattrlistat_decodes_the_attrlist_shape_utimensat_emits() {
        // Two times, distinguishable, in bitmap order: modtime then acctime.
        let (regs, mut memory) = setattrlistat_entry(
            ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME,
            &[(1_555_555_555, 666_777_888), (1_111_111_111, 222_333_444)],
            u64::from(FSOPT_UTIMES_NULL),
            ATTR_BIT_MAP_COUNT,
            0,
            None,
        );
        assert_eq!(
            DarwinArm64Abi.decode_entry(&regs, &mut memory).unwrap(),
            Some(FsOp::SetTimes {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/w/seed.txt".to_vec()).unwrap(),
                accessed_nanos: Some(1_111_111_111_222_333_444),
                modified_nanos: Some(1_555_555_555_666_777_888),
                follow: true,
            })
        );
        // `UTIME_OMIT` for the access time drops the bit and shortens the
        // buffer; the one entry left is the modification time.
        let (regs, mut memory) = setattrlistat_entry(
            ATTR_CMN_MODTIME,
            &[(1_555_555_555, 666_777_888)],
            0,
            ATTR_BIT_MAP_COUNT,
            0,
            None,
        );
        assert_eq!(
            DarwinArm64Abi.decode_entry(&regs, &mut memory).unwrap(),
            Some(FsOp::SetTimes {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/w/seed.txt".to_vec()).unwrap(),
                accessed_nanos: None,
                modified_nanos: Some(1_555_555_555_666_777_888),
                follow: true,
            })
        );
        // `FSOPT_NOFOLLOW` is `AT_SYMLINK_NOFOLLOW` by the time it arrives here.
        let (regs, mut memory) = setattrlistat_entry(
            ATTR_CMN_MODTIME,
            &[(1, 2)],
            u64::from(FSOPT_NOFOLLOW),
            ATTR_BIT_MAP_COUNT,
            0,
            None,
        );
        assert_eq!(
            DarwinArm64Abi.decode_entry(&regs, &mut memory).unwrap(),
            Some(FsOp::SetTimes {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/w/seed.txt".to_vec()).unwrap(),
                accessed_nanos: None,
                modified_nanos: Some(1_000_000_002),
                follow: false,
            })
        );
        // The operand is rewritable at the at-form's slot.
        assert_eq!(path_operands(524).unwrap(), &[(PathOperand::Path, 1)]);
    }

    /// Every attrlist shape outside that pair is refused **whole**.
    ///
    /// This is a shape refusal rather than a flag refusal, and that is why each
    /// of these must fail rather than be narrowed to what is understood: the
    /// bitmap dictates the layout of the buffer that follows, so an unmodelled
    /// bit is not a flag that could be dropped -- it is a different buffer, and
    /// walking it as if it were this one reads the wrong sixteen bytes as a
    /// time. `at_flags` applies the same rule to unmodelled `*at` bits.
    #[test]
    fn setattrlistat_refuses_every_attrlist_shape_outside_the_modtime_acctime_pair() {
        let refused: &[(&str, (RegisterSet, Memory))] = &[
            (
                "an unmodelled common attribute",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME | 0x0000_0200,
                    &[(1, 2), (3, 4)],
                    0,
                    ATTR_BIT_MAP_COUNT,
                    0,
                    None,
                ),
            ),
            (
                "a volume attribute the buffer walk knows nothing about",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME,
                    &[(1, 2)],
                    0,
                    ATTR_BIT_MAP_COUNT,
                    0x0000_0001,
                    None,
                ),
            ),
            (
                "a bitmapcount that is not ATTR_BIT_MAP_COUNT",
                setattrlistat_entry(ATTR_CMN_MODTIME, &[(1, 2)], 0, 3, 0, None),
            ),
            (
                "a buffer shorter than the bitmap declares",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME,
                    &[(1, 2), (3, 4)],
                    0,
                    ATTR_BIT_MAP_COUNT,
                    0,
                    Some(TIMESPEC_BYTES as u64),
                ),
            ),
            (
                "a buffer longer than the bitmap declares",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME,
                    &[(1, 2)],
                    0,
                    ATTR_BIT_MAP_COUNT,
                    0,
                    Some(2 * TIMESPEC_BYTES as u64),
                ),
            ),
            (
                "an option bit outside NOFOLLOW|UTIMES_NULL",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME,
                    &[(1, 2)],
                    0x0000_0002,
                    ATTR_BIT_MAP_COUNT,
                    0,
                    None,
                ),
            ),
            (
                "an attrlist naming no attribute at all",
                setattrlistat_entry(0, &[], 0, ATTR_BIT_MAP_COUNT, 0, None),
            ),
            (
                "a tv_nsec outside one second",
                setattrlistat_entry(
                    ATTR_CMN_MODTIME,
                    &[(1, 1_000_000_000)],
                    0,
                    ATTR_BIT_MAP_COUNT,
                    0,
                    None,
                ),
            ),
        ];
        for (why, (regs, memory)) in refused {
            let mut memory = Memory {
                base: memory.base,
                data: memory.data.clone(),
                reads: 0,
            };
            assert!(
                DarwinArm64Abi.decode_entry(regs, &mut memory).is_err(),
                "{why} was accepted"
            );
        }
    }

    /// The interposer's descriptor test and the supervisor's are twins, and
    /// this is the only thing that links them.
    ///
    /// `fstat` is routed through the tracer, not the interposer, so every
    /// `fstat` in the process reaches a breakpoint -- libsystem's own on kernel
    /// descriptors included. `Supervisor::syscall_entry` therefore applies the
    /// interposer's own floor test before resolving one, and the two
    /// expressions live in two languages with nothing between them. The *value*
    /// cannot drift: there is one `DESCRIPTOR_FENCE` and one assignment in the
    /// tree, and both read it. The *comparison* can.
    ///
    /// So this pins the C's text. It is deliberately a text assertion rather
    /// than a behavioural one: the C cannot be called from here, and what is
    /// being guarded is precisely that someone edits one copy of a predicate
    /// without the other. A change to `umbra_owns` fails here, with a message
    /// naming the Rust site to change with it.
    ///
    /// **What the pair is, stated exactly, because "pinned" invites a stronger
    /// reading than holds.** It is three things, not one proof:
    ///
    /// 1. *A change-detector on the C side* -- this test. A whitespace-
    ///    normalised exact-text assertion on the whole body of `umbra_owns`, in
    ///    the file that is actually compiled into the shipped dylib. Any token
    ///    change fails it.
    /// 2. *A behaviour test on the Rust side* --
    ///    `the_descriptor_fence_owns_exactly_the_fenced_range` in
    ///    `umbra-supervisor`, covering the unfenced branch, both fenced
    ///    branches and negative descriptors.
    /// 3. *A human-judged equivalence between them*, recorded as the term-for-
    ///    term table in `umbra_owns_descriptor`'s doc comment.
    ///
    /// **It is not a proof that the two implementations agree.** Edit both
    /// consistently wrong -- change the C's `>=` to `>` and update this literal
    /// to match, and change the Rust the same way -- and both tests pass. Only
    /// (3) stands between that and a shipped fence off by one, and (3) is prose.
    ///
    /// The coupling is also **one-directional**: a C edit fails a test whose
    /// message names the Rust function, while a Rust edit fails a test none of
    /// whose messages mention the C. And this assertion fires on cosmetic edits
    /// -- reformatting, renaming the parameter, adding a cast -- which trains a
    /// reader to update the literal rather than re-check the Rust. The message
    /// below is written against that habit and cannot fully defeat it.
    ///
    /// Given the C cannot be called from Rust without a harness this is close
    /// to the best available, and it is recorded here at what it is worth
    /// rather than at what it looks like.
    ///
    /// The Rust twin is `umbra_owns_descriptor` in
    /// `umbra-supervisor/src/events.rs`, named after this function so the
    /// correspondence is nominal rather than only semantic.
    #[test]
    fn the_interposers_descriptor_test_is_the_one_the_supervisor_applies() {
        let body = INTERPOSER_C
            .split_once("static int umbra_owns(int fd) {")
            .expect("umbra_owns is defined in umbra_interpose.c")
            .1
            .split_once('}')
            .expect("umbra_owns has a body")
            .0;
        let normalised = body.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            normalised, "return umbra_active() && fd >= 0 && (uint64_t)fd >= umbra_control.floor;",
            "`umbra_owns` changed. Its twin is `umbra_owns_descriptor` in \
             umbra-supervisor/src/events.rs, which gates the `FsOp::Fstat` \
             descriptor fence and must express the same test against the same \
             `descriptor_floor`. Change one, change both -- a virtual descriptor \
             the supervisor resumes instead of resolving is answered `EBADF` by a \
             kernel that does not own the number, and a kernel descriptor it \
             resolves instead of resuming is answered `EBADF` by umbra, which does \
             not."
        );
    }

    /// **The test that would have caught the `intercept()`/`install()` drift.**
    ///
    /// Four stub rows were added to the breakpointed list and the second
    /// admission gate was not extended, so every breakpoint they planted fell
    /// through to `intercept()`'s refusal and stopped the run -- including
    /// `fstat`(339), which libSystem issues before `main` in every process.
    /// Both lists were individually correct; nothing checked the pair.
    ///
    /// The structural half of the fix is that there is now one table and
    /// `intercept()` matches exhaustively on [`Delivery`], so that particular
    /// drift cannot recompile. This is the half the compiler cannot do: a stub
    /// row can still be marked `Namespace` while no decoder arm classifies its
    /// number, which would deliver a `SyscallEntry` the caller then refuses.
    /// `decode_entry`'s own `unclassified Darwin syscall` arm is the thing that
    /// would fire, so this asserts it does not, for every row.
    ///
    /// Operands are deliberately garbage: a decode may well fail on them, and
    /// that is fine. What may not happen is falling off the end of the
    /// classification, which is a different error and the only one checked.
    #[test]
    fn every_traced_stub_is_classified_by_the_decoder() {
        for (symbol, number, delivery) in TRACED_STUBS {
            if *delivery != Delivery::Namespace {
                // Handled by the backend itself; `decode_entry` never sees it,
                // except for the two it positively classifies as non-filesystem.
                continue;
            }
            let mut memory = strings();
            let outcome =
                DarwinArm64Abi.decode_entry(&entry(*number, [ABS, ABS, ABS, 0, 0]), &mut memory);
            if let Err(e) = outcome {
                assert!(
                    !format!("{e}").contains("unclassified"),
                    "{symbol}({number}) is breakpointed and delivered to the namespace, \
                     but the decoder does not classify it: {e}"
                );
            }
        }
    }

    /// **Every** row's `Delivery` against the one the decoder implies, derived
    /// from the table rather than from a list written beside it.
    ///
    /// The compiler proves nobody forgot to classify a variant. Nothing proves
    /// anybody classified a *row* correctly -- `("fchownat", 468,
    /// Delivery::Fork)` type-checks, and at runtime a tracee's `fchownat` would
    /// take the fork arm and have its return value read as a child pid.
    ///
    /// The property that decides it needs no second list. `decode_entry`
    /// answers `Ok(None)` for exactly the syscalls the backend handles itself
    /// -- its `1 | 2 | 7 | 20 | 59 | 244 | 400` arm -- and answers something
    /// else for every filesystem call. So over `TRACED_STUBS`:
    ///
    /// > a row is `Delivery::Namespace` **if and only if** `decode_entry` does
    /// > not answer `Ok(None)` for its number.
    ///
    /// Both directions matter and both are checked. A `Namespace` row whose
    /// number the backend handles would deliver a fork to the caller to decode;
    /// a process-control row whose number is a filesystem call would take a
    /// path operation into `intercept`'s fork, wait or exec machinery.
    ///
    /// Operands are deliberately garbage: a `Namespace` decode may well fail on
    /// them, and an `Err` is "not `Ok(None)`" just as much as an `Ok(Some(..))`
    /// is. What is being asked is which side of the classification the number
    /// falls on, not whether these particular registers decode.
    #[test]
    fn every_traced_stub_carries_the_delivery_its_number_implies() {
        for (symbol, number, delivery) in TRACED_STUBS {
            let mut memory = strings();
            let classified =
                DarwinArm64Abi.decode_entry(&entry(*number, [ABS, ABS, ABS, 0, 0]), &mut memory);
            let backend_handles = matches!(classified, Ok(None));
            let expected = if backend_handles {
                // The backend handles it, so `Namespace` would be wrong. Which
                // of Fork/Wait/Exec is right is what the spot-check below pins.
                assert_ne!(
                    *delivery,
                    Delivery::Namespace,
                    "{symbol}({number}) is delivered to the namespace, but the decoder \
                     positively classifies it as a process-control call the backend \
                     handles itself"
                );
                continue;
            } else {
                Delivery::Namespace
            };
            assert_eq!(
                *delivery, expected,
                "{symbol}({number}) is not delivered to the namespace, but the decoder \
                 classifies it as a filesystem call -- `intercept` would take it into \
                 fork, wait or exec machinery"
            );
        }
    }

    /// A spot-check of the five process-control numbers and the filesystem ones
    /// this slice cares about, against values measured on this host.
    ///
    /// Independent of the biconditional above rather than redundant with it:
    /// that one derives the expected disposition from `decode_entry`, so it
    /// cannot catch the two moving together. This one pins literal numbers to
    /// literal dispositions, which is the thing a reader can check against
    /// `nm`/`dlsym` output by hand.
    ///
    /// Pinned against the syscall numbers rather than the symbols, because the
    /// numbers are what `intercept()` dispatches on.
    #[test]
    fn process_control_stubs_are_not_delivered_to_the_namespace() {
        for (number, expected) in [
            (2u64, Delivery::Fork),
            (7, Delivery::Wait),
            (400, Delivery::Wait),
            (59, Delivery::Exec),
            (244, Delivery::Exec),
        ] {
            assert_eq!(delivery(number), Some(expected), "syscall {number}");
        }
        // And every filesystem call this slice routes is on the other side of
        // that line, including the three it added.
        for number in [5u64, 57, 58, 136, 189, 339, 398, 463, 470, 475, 488, 524] {
            assert_eq!(
                delivery(number),
                Some(Delivery::Namespace),
                "syscall {number}"
            );
        }
        // A number no stub issues has no disposition at all, which is what makes
        // `intercept()`'s refusal reachable rather than dead.
        assert_eq!(delivery(999), None);
    }

    /// A bad `struct stat` pointer is an errno the tracee can be answered with,
    /// not a fault that stops the run.
    ///
    /// The regression this pins is specific and was measured once already, for
    /// `read`: `routing_for`'s doc comment records that returning `Err` for a
    /// bad pointer *"stopped the whole run over an ordinary program bug"*. The
    /// `fstat` branch of `io_buffer` initially skipped the guard its immediate
    /// neighbour applies, so `fstat(vfd, NULL)` reached `write_memory` at
    /// address zero and killed the run where Darwin answers `EFAULT`.
    ///
    /// `Errno(14)` travelling *inside* the error is the mechanism: the caller
    /// binds it through `io_binding` and answers the tracee with it. An error
    /// carrying no errno would stop the run, which is why the assertion is on
    /// the errno and not merely on `is_err`.
    #[test]
    fn a_null_or_overflowing_fstat_buffer_is_an_errno_rather_than_a_dead_run() {
        for number in [189u64, 339] {
            let null = DarwinArm64Abi
                .io_buffer(&entry(number, [7, 0, 0, 0, 0]))
                .expect_err("a null stat buffer must be refused");
            assert_eq!(null.errno, Some(Errno(14)), "syscall {number}");

            let overflow = DarwinArm64Abi
                .io_buffer(&entry(number, [7, u64::MAX, 0, 0, 0]))
                .expect_err("an overflowing stat buffer must be refused");
            assert_eq!(overflow.errno, Some(Errno(14)), "syscall {number}");

            // The ordinary case still binds, so the guard refuses only what it
            // is for.
            assert_eq!(
                DarwinArm64Abi
                    .io_buffer(&entry(number, [7, 8192, 0, 0, 0]))
                    .unwrap(),
                Some(IoBuffer {
                    address: 8192,
                    length: STAT_BYTES as u32,
                }),
                "syscall {number}"
            );
        }
    }

    /// The ABI's own stat encoding is what a routed `fstat` answers with, so the
    /// trait method and the free function have to be the same encoding rather
    /// than two that happen to agree today.
    #[test]
    fn the_abi_encodes_a_stat_through_the_same_layout_the_free_function_does() {
        let stat = BlobStat {
            object_id: ObjectId(uuid::Uuid::from_u128(9)),
            kind: ObjectKind::File,
            len: 4096,
            link_count: 1,
            mode: 0o644,
            uid: 501,
            gid: 20,
            modified_nanos: 1_555_555_555_666_777_888,
        };
        let through_trait = DarwinArm64Abi.encode_stat(&stat).unwrap();
        assert_eq!(through_trait, encode_stat(&stat).unwrap());
        assert_eq!(through_trait.len(), STAT_BYTES);
    }
}
