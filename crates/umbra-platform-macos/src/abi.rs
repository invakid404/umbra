//! Pure ABI core: 34 little-endian u64 slots, x0..x30, sp, pc, cpsr.
use crate::{error, unsupported};
use umbra_core::*;
use umbra_platform::{SyscallAbi, TraceMemory};
pub const MAX_PATH: usize = 4096;
pub const PC: usize = 32;
pub const CPSR: usize = 33;
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
/// Register slot for each path operand a syscall takes, slot N being xN.
///
/// Non-path arguments — dirfds included — keep their original registers: a
/// qualified physical rewrite is absolute, and the kernel ignores the dirfd for
/// an absolute path.
pub fn path_operands(number: u64) -> Result<&'static [(PathOperand, usize)]> {
    Ok(match number {
        // open, open_nocancel, execve: path in x0. readlink: path in x0, with
        // the output buffer in x1 and its length in x2.
        5 | 398 | 59 | 58 => &[(PathOperand::Path, 0)],
        // symlink: x0 holds literal target bytes, x1 the link name.
        57 => &[(PathOperand::Path, 1)],
        // openat, openat_nocancel, posix_spawn: path in x1. unlinkat,
        // mkdirat, faccessat, fchmodat, fchownat, fstatat, fstatat64,
        // readlinkat: dirfd x0, path x1.
        463 | 464 | 244 | 466 | 467 | 468 | 469 | 470 | 472 | 473 | 475 => {
            &[(PathOperand::Path, 1)]
        }
        // renameat, renameatx_np, linkat: source dirfd x0, source x1,
        // destination dirfd x2, destination x3.
        465 | 471 | 488 => &[(PathOperand::Source, 1), (PathOperand::Destination, 3)],
        // symlinkat: x0 holds literal target bytes the tracee will read back,
        // never a pathname operand to physicalize. Link dirfd x1, name x2.
        474 => &[(PathOperand::Path, 2)],
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
/// keep it in x2.
pub fn stat_buffer(regs: &RegisterSet) -> Result<u64> {
    match get(regs, 16)? {
        469 | 470 => get(regs, 2),
        number => Err(unsupported(format!("stat buffer for syscall {number}"))),
    }
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
                    flags: OpenFlags {
                        read: native & 3 != 1,
                        write: native & 3 != 0,
                        append: native & 8 != 0,
                        create: native & 0x200 != 0,
                        exclusive: native & 0x800 != 0,
                        truncate: native & 0x400 != 0,
                        directory: native & 0x100000 != 0,
                        no_follow: native & 0x100 != 0,
                        close_on_exec: native & 0x1000000 != 0,
                    },
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
}
