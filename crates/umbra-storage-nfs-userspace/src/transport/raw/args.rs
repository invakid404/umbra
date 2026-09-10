//! Heap-owned argument arena: one [`Compound`] marshalled into libnfs's
//! `COMPOUND4args` with every buffer C can read owned by this allocation.
//!
//! # Why an arena rather than locals
//!
//! `rpc_nfs4_write_task` adds the WRITE payload to the PDU as an *iovector
//! referencing the caller's buffer* (`nfs4/nfs4.c`, `rpc_nfs4_writev_task`);
//! `rpc_nfs4_read_task` decodes reply bytes straight into the caller's
//! destination buffer. Neither is copied at call time, so every byte C can
//! reach must stay at a fixed address until the call completes or is proven
//! withdrawn. An arena makes that lifetime one owned value the pump's registry
//! holds, instead of a set of locals whose scope a reviewer has to check — the
//! defect the audited spike carried in `raw4.rs:646–687`.
//!
//! Nothing here dereferences a reply. The arena only produces arguments.

use core::ffi::c_char;

use crate::error::TransportError;
use crate::transport::{
    AttrMask, AttrValues, Compound, CreateType, Nfs4Op, OpCode, OpenClaim, OpenHow, Stability,
    TransportLimits, TransportResult,
};

use super::sys;

/// Which libnfs task primitive this COMPOUND must be dispatched through.
///
/// libnfs documents that `rpc_nfs4_compound_task` cannot carry `OP_READ` or
/// `OP_WRITE`, and that when present the operation must be last in the
/// COMPOUND. That is a property of the argument shape, so it is decided here,
/// once, rather than re-derived at the dispatch site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DispatchKind {
    /// Plain COMPOUND with no bulk-data operation.
    Compound,
    /// COMPOUND ending in READ; the arena carries the destination buffer.
    Read,
    /// COMPOUND ending in WRITE; the arena carries the source buffer.
    Write,
}

/// How many `CallArena`s are alive right now.
///
/// **R3-004.** The lifetime rule this module exists to keep — argument memory
/// stays owned for as long as libnfs can read it — is not observable from
/// outside, so it cannot be asserted. This counter makes it observable: a harness
/// can watch the ordering of "C is told to dispose" against "the arena is
/// released", which is exactly the ordering a use-after-free violates.
///
/// It is a plain relaxed counter, not a diagnostic surface: nothing branches on
/// it, and it costs one atomic per dispatch and one per drop.
static LIVE_ARENAS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Arenas currently alive. See [`LIVE_ARENAS`].
///
/// The accessor is test-only; the counter is not. Keeping the increment and
/// decrement in the ordinary path is what makes the harness measure the real
/// code rather than a test-only variant of it.
#[cfg(test)]
pub(super) fn live_arenas() -> usize {
    LIVE_ARENAS.load(std::sync::atomic::Ordering::Relaxed)
}

/// One dispatch's arguments, owned for the whole call.
pub(super) struct CallArena {
    /// Tag bytes referenced by `compound.tag`.
    _tag: Box<[u8]>,
    /// Operation array referenced by `compound.argarray.argarray_val`.
    ops: Vec<sys::nfs_argop4>,
    /// Every variable-length argument buffer C can read.
    _owned: Vec<Box<[u8]>>,
    /// Attribute bitmaps, kept separate because they are `u32` words.
    ///
    /// The `Box` is load-bearing and `clippy::vec_box` is wrong here: C holds
    /// pointers into these arrays for the whole dispatch. A `Vec<[u32; 2]>`
    /// would move every element when it reallocates, invalidating pointers
    /// libnfs is still going to read. The indirection is what keeps each
    /// bitmap at a fixed address.
    #[allow(clippy::vec_box)]
    _bitmaps: Vec<Box<[u32; 2]>>,
    /// READ destination or WRITE source, when the COMPOUND has one.
    ///
    /// The allocation is never zero-sized — see [`own`] — so its length is
    /// storage, not the length the operation declares. Those are tracked apart
    /// by [`Arena::io_len`] (**F22**).
    io: Option<Box<[u8]>>,
    /// The I/O length this COMPOUND actually declares, which for an empty
    /// buffer is *not* the allocation's length.
    ///
    /// **F22.** `own` pads an empty buffer to one byte so the pointer C receives
    /// is never dangling, and `io_buffer` returned that padded length as the
    /// operation's. A zero-length WRITE therefore encoded `data_len: 0` in the
    /// XDR arguments while handing libnfs a one-byte iovector, so the argument
    /// and the vector disagreed about the request; a zero-count READ likewise
    /// exposed a destination larger than the count it asked for.
    io_len: usize,
    /// The root argument struct handed to libnfs.
    compound: Box<sys::COMPOUND4args>,
    /// Task primitive this COMPOUND needs.
    kind: DispatchKind,
}

// SAFETY: every pointer inside `compound` and `ops` addresses an allocation
// this arena owns (`_tag`, `_owned`, `_bitmaps`, `io`, `ops`). Nothing else
// aliases them, and they are reachable only through `&mut CallArena`, so moving
// the arena between threads moves exclusive ownership of the whole graph.
unsafe impl Send for CallArena {}

// SAFETY-adjacent bookkeeping only: dropping an arena is what releases every
// buffer libnfs was reading from, so it is the moment the lifetime rule is
// either kept or broken. Counting it is what makes the ordering observable
// (**R3-004**).
impl Drop for CallArena {
    fn drop(&mut self) {
        LIVE_ARENAS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl CallArena {
    /// Marshal a COMPOUND, or refuse it before anything is registered.
    pub(super) fn build(call: &Compound, limits: &TransportLimits) -> TransportResult<Self> {
        let kind = classify(call)?;

        LIVE_ARENAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut arena = Self {
            _tag: own(call.tag.clone()),
            ops: Vec::with_capacity(call.ops.len()),
            _owned: Vec::new(),
            _bitmaps: Vec::new(),
            io: None,
            io_len: 0,
            compound: Box::new(unsafe { core::mem::zeroed() }),
            kind,
        };

        for op in &call.ops {
            let encoded = arena.encode(op, limits)?;
            arena.ops.push(encoded);
        }

        // Pointers are taken only after every push, because `Vec` reallocates.
        arena.compound.minorversion = crate::transport::MINOR_VERSION;
        arena.compound.tag.utf8string_len =
            u32::try_from(call.tag.len()).map_err(|_| malformed("tag longer than 4 GiB"))?;
        arena.compound.tag.utf8string_val = arena._tag.as_mut_ptr().cast::<c_char>();
        arena.compound.argarray.argarray_len = u32::try_from(arena.ops.len())
            .map_err(|_| malformed("COMPOUND longer than 4 GiB operations"))?;
        arena.compound.argarray.argarray_val = arena.ops.as_mut_ptr();

        Ok(arena)
    }

    /// Task primitive this COMPOUND must use.
    pub(super) fn kind(&self) -> DispatchKind {
        self.kind
    }

    /// Pointer to the argument root. Valid while `self` is alive and unmoved
    /// out of the registry slot that owns it.
    pub(super) fn compound_ptr(&mut self) -> *mut sys::COMPOUND4args {
        &mut *self.compound
    }

    /// Bytes libnfs decoded into the READ destination, bounded by `count`.
    ///
    /// Returns `None` when this arena has no destination buffer, and refuses a
    /// count larger than the READ actually asked for rather than reading past it.
    ///
    /// **F22.** Bounded by the *declared* length, not by the allocation. The two
    /// differ for a zero-count READ, where the allocation is padded to one byte:
    /// bounding by storage would have let a server that answered with more bytes
    /// than were requested have one of them read back.
    pub(super) fn read_bytes(&self, count: usize) -> Option<&[u8]> {
        if count > self.io_len {
            return None;
        }
        let buffer = self.io.as_deref()?;
        buffer.get(..count)
    }

    /// READ destination or WRITE source, if this COMPOUND has one.
    ///
    /// **F22.** The length is what the operation declares, so the iovector agrees
    /// with the XDR argument beside it. The pointer is still the padded
    /// allocation's, which is what keeps it non-dangling for a zero-length call.
    pub(super) fn io_buffer(&mut self) -> Option<(*mut u8, usize)> {
        let length = self.io_len;
        self.io.as_mut().map(|buffer| (buffer.as_mut_ptr(), length))
    }

    /// Take ownership of one variable-length argument buffer and return the
    /// `(pointer, length)` pair C will read it through.
    fn own_bytes(&mut self, bytes: Vec<u8>) -> (*mut c_char, u32) {
        let length = bytes.len();
        let mut boxed = own(bytes);
        let pointer = boxed.as_mut_ptr().cast::<c_char>();
        self._owned.push(boxed);
        (pointer, length as u32)
    }

    /// Own a NUL-terminated C string. Used only for the declined callback
    /// address, which must be present but empty.
    fn own_cstr(&mut self, text: &str) -> *mut c_char {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let mut boxed = own(bytes);
        let pointer = boxed.as_mut_ptr().cast::<c_char>();
        self._owned.push(boxed);
        pointer
    }

    /// Own an attribute bitmap and return the `bitmap4` C reads it through.
    fn own_bitmap(&mut self, mask: AttrMask) -> sys::bitmap4 {
        let mut words = Box::new([mask.word0, mask.word1]);
        let pointer = words.as_mut_ptr();
        self._bitmaps.push(words);
        sys::bitmap4 {
            bitmap4_len: 2,
            bitmap4_val: pointer,
        }
    }

    /// Encode one operation into its `nfs_argop4`.
    ///
    /// The match is total over [`Nfs4Op`], which is why no NFSv4.1 union arm of
    /// `nfs_argop4` is reachable: the frozen enum has no variant that selects
    /// one. libnfs's ABI carries `opexchangeid`, `opsequence` and friends; this
    /// transport has no expression that writes them.
    fn encode(
        &mut self,
        op: &Nfs4Op,
        limits: &TransportLimits,
    ) -> TransportResult<sys::nfs_argop4> {
        let mut out = sys::nfs_argop4 {
            argop: opnum(op.opcode()),
            nfs_argop4_u: unsafe { core::mem::zeroed() },
        };

        match op {
            Nfs4Op::PutRootFh | Nfs4Op::GetFh | Nfs4Op::LookupParent | Nfs4Op::SaveFh => {}

            Nfs4Op::PutFh(handle) => {
                let (pointer, length) = self.own_bytes(handle.as_bytes().to_vec());
                out.nfs_argop4_u.opputfh = sys::PUTFH4args {
                    object: sys::nfs_fh4 {
                        nfs_fh4_len: length,
                        nfs_fh4_val: pointer,
                    },
                };
            }

            Nfs4Op::GetAttr(mask) => {
                let bitmap = self.own_bitmap(*mask);
                out.nfs_argop4_u.opgetattr = sys::GETATTR4args {
                    attr_request: bitmap,
                };
            }

            Nfs4Op::Lookup(name) => {
                let (pointer, length) = self.own_bytes(name.as_bytes().to_vec());
                out.nfs_argop4_u.oplookup = sys::LOOKUP4args {
                    objname: sys::utf8string {
                        utf8string_len: length,
                        utf8string_val: pointer,
                    },
                };
            }

            Nfs4Op::ReadDir {
                cookie,
                verifier,
                dir_count,
                max_count,
                attrs,
            } => {
                let capped = (*max_count as usize).min(limits.max_reply_bytes);
                let bitmap = self.own_bitmap(*attrs);
                out.nfs_argop4_u.opreaddir = sys::READDIR4args {
                    cookie: cookie.0,
                    cookieverf: to_c_bytes(verifier.0),
                    dircount: *dir_count,
                    maxcount: capped as u32,
                    attr_request: bitmap,
                };
            }

            Nfs4Op::Read {
                stateid,
                offset,
                count,
            } => {
                let capped = (*count as usize).min(limits.max_reply_bytes);
                // Destination for the zero-copy decode libnfs performs. The
                // allocation is padded to a byte so its pointer is valid; the
                // length the request declares is `capped` (F22).
                self.io = Some(own(vec![0u8; capped]));
                self.io_len = capped;
                out.nfs_argop4_u.opread = sys::READ4args {
                    stateid: to_stateid(stateid),
                    offset: *offset,
                    count: capped as u32,
                };
            }

            Nfs4Op::Write {
                stateid,
                offset,
                stability,
                data,
            } => {
                // Owned here for the whole dispatch: libnfs references this
                // buffer from the PDU's iovector rather than copying it.
                let mut buffer = own(data.clone());
                let pointer = buffer.as_mut_ptr().cast::<c_char>();
                let length = data.len() as u32;
                self.io = Some(buffer);
                // F22: what the WRITE declares, not what was allocated for it.
                self.io_len = data.len();
                out.nfs_argop4_u.opwrite = sys::WRITE4args {
                    stateid: to_stateid(stateid),
                    offset: *offset,
                    stable: match stability {
                        Stability::Unstable => sys::stable_how4_UNSTABLE4,
                        Stability::DataSync => sys::stable_how4_DATA_SYNC4,
                        Stability::FileSync => sys::stable_how4_FILE_SYNC4,
                    },
                    data: sys::WRITE4args__bindgen_ty_1 {
                        data_len: length,
                        data_val: pointer,
                    },
                };
            }

            Nfs4Op::Commit { offset, count } => {
                out.nfs_argop4_u.opcommit = sys::COMMIT4args {
                    offset: *offset,
                    count: *count,
                };
            }

            Nfs4Op::Open(args) => {
                let (owner_ptr, owner_len) = self.own_bytes(args.owner.as_bytes().to_vec());
                let openhow = self.encode_openhow(&args.how)?;
                let claim = self.encode_claim(&args.claim);
                out.nfs_argop4_u.opopen = sys::OPEN4args {
                    seqid: args.seqid,
                    share_access: args.share_access.0,
                    share_deny: args.share_deny.0,
                    owner: sys::open_owner4 {
                        clientid: args.owner.client_id().0,
                        owner: sys::open_owner4__bindgen_ty_1 {
                            owner_len,
                            owner_val: owner_ptr,
                        },
                    },
                    openhow,
                    claim,
                };
            }

            Nfs4Op::OpenConfirm { stateid, seqid } => {
                out.nfs_argop4_u.opopen_confirm = sys::OPEN_CONFIRM4args {
                    open_stateid: to_stateid(stateid),
                    seqid: *seqid,
                };
            }

            Nfs4Op::Close { seqid, stateid } => {
                out.nfs_argop4_u.opclose = sys::CLOSE4args {
                    seqid: *seqid,
                    open_stateid: to_stateid(stateid),
                };
            }

            Nfs4Op::Lock(args) => {
                let (owner_ptr, owner_len) = self.own_bytes(args.lock_owner.clone());
                let locker = if args.new_lock_owner {
                    sys::locker4 {
                        new_lock_owner: 1,
                        locker4_u: sys::locker4__bindgen_ty_1 {
                            open_owner: sys::open_to_lock_owner4 {
                                open_seqid: args.open_seqid,
                                open_stateid: to_stateid(&args.open_stateid),
                                lock_seqid: args.lock_seqid,
                                lock_owner: sys::lock_owner4 {
                                    clientid: 0,
                                    owner: sys::lock_owner4__bindgen_ty_1 {
                                        owner_len,
                                        owner_val: owner_ptr,
                                    },
                                },
                            },
                        },
                    }
                } else {
                    sys::locker4 {
                        new_lock_owner: 0,
                        locker4_u: sys::locker4__bindgen_ty_1 {
                            lock_owner: sys::exist_lock_owner4 {
                                lock_stateid: to_stateid(&args.open_stateid),
                                lock_seqid: args.lock_seqid,
                            },
                        },
                    }
                };
                out.nfs_argop4_u.oplock = sys::LOCK4args {
                    locktype: lock_type(args.lock_type),
                    reclaim: 0,
                    offset: args.offset,
                    length: args.length,
                    locker,
                };
            }

            Nfs4Op::Locku(args) => {
                out.nfs_argop4_u.oplocku = sys::LOCKU4args {
                    locktype: lock_type(args.lock_type),
                    seqid: args.lock_seqid,
                    lock_stateid: to_stateid(&args.lock_stateid),
                    offset: args.offset,
                    length: args.length,
                };
            }

            Nfs4Op::Renew(client_id) => {
                out.nfs_argop4_u.oprenew = sys::RENEW4args {
                    clientid: client_id.0,
                };
            }

            Nfs4Op::SetClientId(args) => {
                let (id_ptr, id_len) = self.own_bytes(args.id.clone());
                // `CallbackPolicy::Declined` is the only variant: advertise no
                // usable callback path so the server has no reason to grant a
                // delegation M1 could not return.
                let netid = self.own_cstr("");
                let addr = self.own_cstr("");
                out.nfs_argop4_u.opsetclientid = sys::SETCLIENTID4args {
                    client: sys::nfs_client_id4 {
                        verifier: to_c_bytes(args.verifier.0),
                        id: sys::nfs_client_id4__bindgen_ty_1 {
                            id_len,
                            id_val: id_ptr,
                        },
                    },
                    callback: sys::cb_client4 {
                        cb_program: 0,
                        cb_location: sys::clientaddr4 {
                            r_netid: netid,
                            r_addr: addr,
                        },
                    },
                    callback_ident: 0,
                };
            }

            Nfs4Op::SetClientIdConfirm { client_id, confirm } => {
                out.nfs_argop4_u.opsetclientid_confirm = sys::SETCLIENTID_CONFIRM4args {
                    clientid: client_id.0,
                    setclientid_confirm: to_c_bytes(confirm.0),
                };
            }

            Nfs4Op::Remove { name } => {
                let (pointer, length) = self.own_bytes(name.as_bytes().to_vec());
                out.nfs_argop4_u.opremove = sys::REMOVE4args {
                    target: sys::utf8string {
                        utf8string_len: length,
                        utf8string_val: pointer,
                    },
                };
            }

            Nfs4Op::Rename { old_name, new_name } => {
                let (old_ptr, old_len) = self.own_bytes(old_name.as_bytes().to_vec());
                let (new_ptr, new_len) = self.own_bytes(new_name.as_bytes().to_vec());
                out.nfs_argop4_u.oprename = sys::RENAME4args {
                    oldname: sys::utf8string {
                        utf8string_len: old_len,
                        utf8string_val: old_ptr,
                    },
                    newname: sys::utf8string {
                        utf8string_len: new_len,
                        utf8string_val: new_ptr,
                    },
                };
            }

            Nfs4Op::Create {
                object_type,
                name,
                attributes,
            } => {
                let (name_ptr, name_len) = self.own_bytes(name.as_bytes().to_vec());
                let createattrs = self.encode_fattr(attributes);
                // NF4DIR selects no arm of `createtype4_u`; zeroing it is the
                // encoding, not an omission. NF4LNK and the device types would
                // need `linkdata`/`devdata`, and `CreateType` has no variant
                // that reaches them.
                let objtype = sys::createtype4 {
                    type_: match object_type {
                        CreateType::Directory => NF4DIR,
                    },
                    createtype4_u: unsafe { core::mem::zeroed() },
                };
                out.nfs_argop4_u.opcreate = sys::CREATE4args {
                    objtype,
                    objname: sys::utf8string {
                        utf8string_len: name_len,
                        utf8string_val: name_ptr,
                    },
                    createattrs,
                };
            }

            Nfs4Op::SetAttr {
                stateid,
                attributes,
            } => {
                if attributes.is_empty() {
                    return Err(malformed(
                        "SETATTR names no attribute; an empty set would report a change \
                         that never happened",
                    ));
                }
                let obj_attributes = self.encode_fattr(attributes);
                out.nfs_argop4_u.opsetattr = sys::SETATTR4args {
                    stateid: to_stateid(stateid),
                    obj_attributes,
                };
            }
        }

        Ok(out)
    }

    /// Encode [`AttrValues`] as one `fattr4`.
    ///
    /// The `attrlist4` is the XDR encoding of each present value **in ascending
    /// attribute number**, which is what the bitmap means; emitting them in any
    /// other order would hand the server a correctly-sized blob it parses into
    /// the wrong fields. [`AttrValues::mask`] walks the same order, so the two
    /// cannot disagree.
    fn encode_fattr(&mut self, values: &AttrValues) -> sys::fattr4 {
        let bitmap = self.own_bitmap(values.mask());
        let mut blob = Vec::new();
        if let Some(size) = values.size {
            blob.extend_from_slice(&size.to_be_bytes());
        }
        if let Some(mode) = values.mode {
            blob.extend_from_slice(&mode.to_be_bytes());
        }
        for name in [&values.owner, &values.owner_group].into_iter().flatten() {
            push_utf8(&mut blob, name);
        }
        for time in [values.time_access, values.time_modify]
            .into_iter()
            .flatten()
        {
            // settime4: SET_TO_CLIENT_TIME4 then the nfstime4 itself.
            blob.extend_from_slice(&SET_TO_CLIENT_TIME4.to_be_bytes());
            blob.extend_from_slice(&time.seconds.to_be_bytes());
            blob.extend_from_slice(&time.nanoseconds.to_be_bytes());
        }
        let (values_ptr, length) = self.own_bytes(blob);
        sys::fattr4 {
            attrmask: bitmap,
            attr_vals: sys::attrlist4 {
                attrlist4_len: length,
                attrlist4_val: values_ptr,
            },
        }
    }

    /// Encode `OPEN4args.openhow`.
    fn encode_openhow(&mut self, how: &OpenHow) -> TransportResult<sys::openflag4> {
        let mut flag = sys::openflag4 {
            opentype: 0,
            openflag4_u: unsafe { core::mem::zeroed() },
        };
        match how {
            OpenHow::NoCreate => {
                flag.opentype = OPEN4_NOCREATE;
            }
            OpenHow::Unchecked { mode } => {
                flag.opentype = OPEN4_CREATE;
                flag.openflag4_u.how = self.mode_createhow(UNCHECKED4, *mode);
            }
            OpenHow::Guarded { mode } => {
                flag.opentype = OPEN4_CREATE;
                flag.openflag4_u.how = self.mode_createhow(GUARDED4, *mode);
            }
            OpenHow::Exclusive { verifier } => {
                flag.opentype = OPEN4_CREATE;
                flag.openflag4_u.how = sys::createhow4 {
                    mode: EXCLUSIVE4,
                    createhow4_u: sys::createhow4__bindgen_ty_1 {
                        createverf: to_c_bytes(verifier.0),
                    },
                };
            }
        }
        Ok(flag)
    }

    /// A `createhow4` carrying just `FATTR4_MODE`.
    fn mode_createhow(&mut self, create_mode: sys::createmode4, mode: u32) -> sys::createhow4 {
        let bitmap = self.own_bitmap(AttrMask::MODE);
        let (values, length) = self.own_bytes(mode.to_be_bytes().to_vec());
        sys::createhow4 {
            mode: create_mode,
            createhow4_u: sys::createhow4__bindgen_ty_1 {
                createattrs: sys::fattr4 {
                    attrmask: bitmap,
                    attr_vals: sys::attrlist4 {
                        attrlist4_len: length,
                        attrlist4_val: values,
                    },
                },
            },
        }
    }

    /// Encode `OPEN4args.claim`. NFSv4.0 has exactly two claims in M1 scope.
    fn encode_claim(&mut self, claim: &OpenClaim) -> sys::open_claim4 {
        let mut out = sys::open_claim4 {
            claim: 0,
            open_claim4_u: unsafe { core::mem::zeroed() },
        };
        match claim {
            OpenClaim::Null { name } => {
                let (pointer, length) = self.own_bytes(name.as_bytes().to_vec());
                out.claim = CLAIM_NULL;
                out.open_claim4_u.file = sys::utf8string {
                    utf8string_len: length,
                    utf8string_val: pointer,
                };
            }
            OpenClaim::Previous { delegate_type } => {
                out.claim = CLAIM_PREVIOUS;
                out.open_claim4_u.delegate_type = delegation(*delegate_type);
            }
        }
        out
    }
}

/// Decide which task primitive a COMPOUND needs, and reject shapes libnfs's
/// raw surface cannot express.
fn classify(call: &Compound) -> TransportResult<DispatchKind> {
    if call.ops.is_empty() {
        return Err(malformed("COMPOUND carries no operations"));
    }

    let bulk: Vec<(usize, OpCode)> = call
        .ops
        .iter()
        .enumerate()
        .map(|(index, op)| (index, op.opcode()))
        .filter(|(_, code)| matches!(code, OpCode::Read | OpCode::Write))
        .collect();

    match bulk.as_slice() {
        [] => Ok(DispatchKind::Compound),
        [(index, code)] => {
            if *index + 1 != call.ops.len() {
                return Err(malformed(format!(
                    "{code:?} must be the last operation in a COMPOUND; it is at index {index} of {}",
                    call.ops.len()
                )));
            }
            Ok(match code {
                OpCode::Read => DispatchKind::Read,
                _ => DispatchKind::Write,
            })
        }
        _ => Err(malformed(
            "a COMPOUND carries at most one READ or WRITE operation",
        )),
    }
}

/// Append one XDR `utf8str_mixed`: a length then the bytes, padded to four.
fn push_utf8(blob: &mut Vec<u8>, text: &[u8]) {
    blob.extend_from_slice(&(text.len() as u32).to_be_bytes());
    blob.extend_from_slice(text);
    blob.resize(blob.len().next_multiple_of(4), 0);
}

/// Allocate an owned buffer, never zero-sized: a zero-length `Box<[u8]>` has a
/// dangling pointer, and libnfs hands argument pointers to `writev`.
fn own(mut bytes: Vec<u8>) -> Box<[u8]> {
    if bytes.is_empty() {
        bytes.push(0);
    }
    bytes.into_boxed_slice()
}

fn to_stateid(stateid: &crate::handle::Stateid) -> sys::stateid4 {
    sys::stateid4 {
        seqid: stateid.seqid,
        other: to_c_bytes(stateid.other),
    }
}

/// Reinterpret owned bytes as the `char` array libnfs's XDR structs use.
fn to_c_bytes<const N: usize>(bytes: [u8; N]) -> [c_char; N] {
    core::array::from_fn(|index| bytes[index] as c_char)
}

fn lock_type(kind: crate::transport::LockType) -> sys::nfs_lock_type4 {
    use crate::transport::LockType;
    match kind {
        LockType::Read => READ_LT,
        LockType::Write => WRITE_LT,
        LockType::ReadBlocking => READW_LT,
        LockType::WriteBlocking => WRITEW_LT,
    }
}

fn delegation(kind: crate::transport::DelegationType) -> sys::open_delegation_type4 {
    use crate::transport::DelegationType;
    match kind {
        DelegationType::None => OPEN_DELEGATE_NONE,
        DelegationType::Read => OPEN_DELEGATE_READ,
        DelegationType::Write => OPEN_DELEGATE_WRITE,
    }
}

/// Map the frozen [`OpCode`] onto libnfs's `nfs_opnum4`.
///
/// [`OpCode`] has no variant at or above 40, so this function cannot produce a
/// v4.1 operation number however it is called.
fn opnum(code: OpCode) -> sys::nfs_opnum4 {
    code as u32
}

fn malformed(detail: impl Into<String>) -> TransportError {
    TransportError::Malformed(detail.into())
}

// RFC 7530 constants libnfs spells with an enum-prefixed name.
const OPEN4_NOCREATE: sys::opentype4 = 0;
const OPEN4_CREATE: sys::opentype4 = 1;
const UNCHECKED4: sys::createmode4 = 0;
const GUARDED4: sys::createmode4 = 1;
const EXCLUSIVE4: sys::createmode4 = 2;
const CLAIM_NULL: sys::open_claim_type4 = 0;
const CLAIM_PREVIOUS: sys::open_claim_type4 = 1;
const OPEN_DELEGATE_NONE: sys::open_delegation_type4 = 0;
const OPEN_DELEGATE_READ: sys::open_delegation_type4 = 1;
const OPEN_DELEGATE_WRITE: sys::open_delegation_type4 = 2;
const NF4DIR: sys::nfs_ftype4 = 2;
const SET_TO_CLIENT_TIME4: u32 = 1;
const READ_LT: sys::nfs_lock_type4 = 1;
const WRITE_LT: sys::nfs_lock_type4 = 2;
const READW_LT: sys::nfs_lock_type4 = 3;
const WRITEW_LT: sys::nfs_lock_type4 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{FileHandle, Stateid};
    use crate::transport::{Compound, Nfs4Op, Stability};

    fn limits() -> TransportLimits {
        TransportLimits {
            max_inflight: 1,
            max_queue_depth: 8,
            max_reply_bytes: 1024 * 1024,
            default_deadline: crate::transport::Deadline { millis: 5_000 },
        }
    }

    fn handle() -> FileHandle {
        FileHandle::from_wire(vec![0u8; 8]).expect("eight bytes is a valid filehandle")
    }

    /// **F22.** A zero-length WRITE declares zero bytes and hands libnfs a
    /// zero-length iovector, not the one-byte allocation that keeps its pointer
    /// valid.
    ///
    /// `own` pads an empty buffer to a byte because a zero-length `Box<[u8]>`
    /// has a dangling pointer and libnfs passes argument pointers to `writev`.
    /// `io_buffer` returned that padded length as the operation's, so the XDR
    /// argument said `data_len: 0` while the vector said one byte: the request
    /// and the vector describing it disagreed.
    #[test]
    fn f22_a_zero_length_write_declares_zero_bytes() {
        let mut arena = CallArena::build(
            &Compound::new(
                *b"wr00",
                vec![
                    Nfs4Op::PutFh(handle()),
                    Nfs4Op::Write {
                        stateid: Stateid::ANONYMOUS,
                        offset: 0,
                        stability: Stability::FileSync,
                        data: Vec::new(),
                    },
                ],
            ),
            &limits(),
        )
        .expect("a zero-length write is a legal COMPOUND");

        let (pointer, length) = arena.io_buffer().expect("a WRITE arena has a buffer");
        assert!(
            !pointer.is_null(),
            "the pointer stays valid, which is why own pads"
        );
        assert_eq!(
            length, 0,
            "and the declared length is the one the WRITE carries"
        );
    }

    /// **F22.** A non-empty WRITE is unaffected: declared length is the data's.
    #[test]
    fn f22_a_nonempty_write_declares_its_own_length() {
        let mut arena = CallArena::build(
            &Compound::new(
                *b"wr04",
                vec![
                    Nfs4Op::PutFh(handle()),
                    Nfs4Op::Write {
                        stateid: Stateid::ANONYMOUS,
                        offset: 0,
                        stability: Stability::FileSync,
                        data: b"abcd".to_vec(),
                    },
                ],
            ),
            &limits(),
        )
        .expect("build");
        assert_eq!(arena.io_buffer().expect("buffer").1, 4);
    }

    /// **F22.** A zero-count READ exposes a zero-length destination, and cannot
    /// have a byte read back out of its padding.
    #[test]
    fn f22_a_zero_count_read_exposes_no_destination_bytes() {
        let mut arena = CallArena::build(
            &Compound::new(
                *b"rd00",
                vec![
                    Nfs4Op::PutFh(handle()),
                    Nfs4Op::Read {
                        stateid: Stateid::ANONYMOUS,
                        offset: 0,
                        count: 0,
                    },
                ],
            ),
            &limits(),
        )
        .expect("a zero-count read is a legal COMPOUND");

        let (pointer, length) = arena.io_buffer().expect("a READ arena has a buffer");
        assert!(!pointer.is_null());
        assert_eq!(
            length, 0,
            "the destination the server is told about is empty"
        );
        assert_eq!(
            arena.read_bytes(0).map(<[u8]>::len),
            Some(0),
            "zero bytes read back is the whole of a zero-count READ"
        );
        assert_eq!(
            arena.read_bytes(1),
            None,
            "a server answering more than it was asked for must not be read out of the padding"
        );
    }

    /// **F22.** A non-zero READ still bounds `read_bytes` by what it asked for.
    #[test]
    fn f22_a_read_bounds_its_destination_by_the_declared_count() {
        let mut arena = CallArena::build(
            &Compound::new(
                *b"rd08",
                vec![
                    Nfs4Op::PutFh(handle()),
                    Nfs4Op::Read {
                        stateid: Stateid::ANONYMOUS,
                        offset: 0,
                        count: 8,
                    },
                ],
            ),
            &limits(),
        )
        .expect("build");
        assert_eq!(arena.io_buffer().expect("buffer").1, 8);
        assert_eq!(arena.read_bytes(8).map(<[u8]>::len), Some(8));
        assert_eq!(arena.read_bytes(9), None);
    }
}
