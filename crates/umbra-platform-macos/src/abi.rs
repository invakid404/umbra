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
pub fn path_slot(number: u64) -> Result<usize> {
    match number {
        5 | 398 | 59 => Ok(0),
        463 | 464 | 244 => Ok(1),
        _ => Err(unsupported(format!("path syscall {number}"))),
    }
}
/// Build a scratch write and register update independently of transport.
pub fn prepare_path(
    regs: &RegisterSet,
    address: u64,
    path: &BytePath,
    operation: FsOp,
) -> Result<PreparedRewrite> {
    if path.as_bytes().len() >= MAX_PATH
        || address == 0
        || address
            .checked_add(path.as_bytes().len() as u64 + 1)
            .is_none()
    {
        return Err(error("rewrite", "invalid scratch path bounds"));
    }
    let mut bytes = path.as_bytes().to_vec();
    bytes.push(0);
    Ok(PreparedRewrite {
        operation: PhysicalOperation {
            operation,
            paths: vec![PathRewrite {
                operand: PathOperand::Path,
                path: PhysicalPath(path.clone()),
            }],
        },
        arguments: vec![ArgumentRewrite {
            index: path_slot(get(regs, 16)?)? as u8,
            value: address,
        }],
        memory_writes: vec![MemoryWrite { address, bytes }],
    })
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
