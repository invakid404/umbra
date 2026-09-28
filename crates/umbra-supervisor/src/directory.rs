//! The production [`DirectoryEncoder`], and the per-syscall buffer binding it
//! answers from.
//!
//! **This exists because injecting nothing is the failure this slice was most
//! at risk of.** `umbra_overlay::Engine::resolve_directory` hard-requires an
//! injected `DirectoryEncoder`; before this module nothing in the tree
//! implemented the trait and `set_directory_encoder` was called nowhere, so the
//! overlay's whole merged-directory path -- paging, whiteout rules, bound
//! checks, all of it already written and already tested -- was unreachable in
//! production. An encoder implemented and injected nowhere would have left every
//! local gate green with `ls` still broken, which is `intercept()`-versus-
//! `TRACED_STUBS` ([#116]) one altitude up. The injection is therefore in
//! `Supervisor::launch_prepared`, on the single path every launched run takes.
//!
//! [#116]: https://github.com/invakid404/umbra/issues/116
//!
//! **Why the encoding happens here and not in the ABI backend.**
//! `DirectoryEncoder` is `Send` and is called from inside the overlay's
//! `resolve`, in the supervisor's process. The production `SyscallAbi` is an IPC
//! proxy holding an `Rc<RefCell<Client>>` to a provider *executable*, so it is
//! neither `Send` nor in this address space, and no encoder that reached for it
//! could satisfy the bound. The wire format therefore lives in
//! `umbra_platform::dirents`, which both this crate and the macOS backend
//! depend on -- one format, two readers, rather than the two-implementations
//! hazard that `StatEncoder` and `SyscallAbi::encode_stat` are still an example
//! of. That pair is **not** resolved here and is not this slice's to resolve:
//! it is tracked as [#125], with the `Send`/`Rc` constraint above recorded as
//! the thing that decides which way it should go.
//!
//! [#125]: https://github.com/invakid404/umbra/issues/125

use std::sync::{Arc, Mutex};

use umbra_core::{
    storage::DirectoryEntry, EmulatedResult, ErrorKind, FsOp, IoBuffer, MemoryWrite,
    OperationOutcome, ProcessContext, Result, UmbraError,
};
use umbra_overlay::{DirectoryEncoder, EncodedDirectory};
use umbra_platform::dirents;

fn error(message: impl Into<String>) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, "supervisor.directory", message)
}

/// The output buffer of the `getattrlistbulk` currently stopped at its entry.
///
/// Bound by the supervisor before `resolve` and consumed by the encoder during
/// it, the way `set_routed_request` binds a routed operation's runtime details
/// -- and consumed for the same reason that one is: a binding left in place is a
/// binding the *next*, unrelated directory read would answer from, writing one
/// call's entries into another call's buffer.
#[derive(Clone, Default)]
pub struct DirectoryBuffer(Arc<Mutex<Option<IoBuffer>>>);

impl DirectoryBuffer {
    /// Bind the buffer this `ReadDir` must be answered into.
    pub fn set(&self, buffer: IoBuffer) -> Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| error("the directory buffer binding is poisoned"))? = Some(buffer);
        Ok(())
    }

    /// Drop any binding an earlier operation left behind.
    pub fn clear(&self) -> Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| error("the directory buffer binding is poisoned"))? = None;
        Ok(())
    }

    fn take(&self) -> Result<IoBuffer> {
        self.0
            .lock()
            .map_err(|_| error("the directory buffer binding is poisoned"))?
            .take()
            .ok_or_else(|| {
                error(
                    "a directory read reached the encoder with no output buffer bound; \
                     the supervisor binds one at the syscall entry",
                )
            })
    }
}

/// Encodes merged directory entries into the stopped tracee's buffer.
pub struct AbiDirectoryEncoder {
    buffer: DirectoryBuffer,
}

impl AbiDirectoryEncoder {
    /// Build an encoder answering from `buffer`.
    pub fn new(buffer: DirectoryBuffer) -> Self {
        Self { buffer }
    }
}

impl DirectoryEncoder for AbiDirectoryEncoder {
    fn encode(
        &mut self,
        _context: &ProcessContext,
        operation: &FsOp,
        entries: &[DirectoryEntry],
    ) -> Result<EncodedDirectory> {
        let FsOp::ReadDir { max_bytes, .. } = operation else {
            return Err(error(
                "the directory encoder was given a non-ReadDir operation",
            ));
        };
        let buffer = self.buffer.take()?;
        // **The two bounds are cross-checked rather than one trusted.** The
        // operation's `max_bytes` and the binding's `length` are two readings of
        // the same tracee register, taken by `decode_entry` and `io_buffer`; the
        // shipped `fstat` join makes the same check for the same reason, and a
        // disagreement here would mean the encoder was sized against one call
        // and aimed at another's buffer.
        if buffer.length != *max_bytes {
            return Err(error(format!(
                "the bound directory buffer holds {} bytes but the operation names {max_bytes}",
                buffer.length
            )));
        }
        // MUTATION PROBE -- the directory reply's contents. Compiled out of
        // every build that does not ask for it, so no product binary contains
        // this branch.
        //
        // **It corrupts names and nothing else, and that is what makes it
        // discriminating.** Each name is reversed, so every record length, name
        // reference offset and alignment stays exactly as the encoder would have
        // produced it: the reply still walks, `count_records` still agrees,
        // `getattrlistbulk` still returns the right count and the tracee still
        // exits 0. What changes is the one thing the proof is about.
        //
        // That is deliberate and it is the reason this probe exists in this
        // shape. The design gate recorded the measurement that forced it: nine
        // errnos swept through `getattrlistbulk` all produce exit 1, and the
        // `read`/`write`/`fstat` probes land on exit 1 too, so **no probe here
        // can be discriminated by exit code**. A test that watched the status
        // would pass with this enabled. Only reading the names back through the
        // client can tell a correct listing from a corrupt one -- or, for that
        // matter, from an empty one, which is what a directory read that is not
        // routed at all produces.
        #[cfg(feature = "mutation-probe-readdir")]
        let entries: Vec<DirectoryEntry> = entries
            .iter()
            .map(|entry| {
                let mut name = entry.name.as_bytes().to_vec();
                name.reverse();
                DirectoryEntry {
                    name: umbra_core::BytePath::new(name).expect("a reversed name is still bytes"),
                    stat: entry.stat.clone(),
                }
            })
            .collect();
        #[cfg(feature = "mutation-probe-readdir")]
        let entries = entries.as_slice();
        let (bytes, consumed) = dirents::encode(entries, *max_bytes)?;
        // **The records are re-walked before they are handed over.**
        //
        // `consumed` is what the tracee will be told it received and what the
        // overlay will page past, and up to here it is a number this function
        // counted while packing. Walking the finished buffer back re-derives it
        // from the bytes themselves -- each record opens with its own length --
        // so a packing bug that wrote a short, torn or misaligned record is
        // caught here rather than becoming a `getattrlistbulk` reply the tracee
        // walks off the end of.
        let counted = dirents::count_records(&bytes)?;
        if counted != consumed {
            return Err(error(format!(
                "the directory reply holds {counted} records but {consumed} entries were packed"
            )));
        }
        // No bytes means end-of-directory, and it is answered with no write at
        // all rather than a zero-length one: zero records is what
        // `getattrlistbulk` returns there, and a zero-length write into a tracee
        // is a round trip that says nothing.
        let memory_writes = if bytes.is_empty() {
            vec![]
        } else {
            vec![MemoryWrite {
                address: buffer.address,
                bytes,
            }]
        };
        // `getattrlistbulk` answers with the number of entry records it packed,
        // never the number of bytes -- measured against the kernel. That is the
        // value `resolve_directory` validates this against, and the value
        // `emulate_result` puts in x0 with no ABI special case at all.
        Ok(EncodedDirectory {
            result: EmulatedResult {
                outcome: OperationOutcome::Success {
                    return_value: consumed as u64,
                },
                memory_writes,
            },
            consumed,
        })
    }
}
