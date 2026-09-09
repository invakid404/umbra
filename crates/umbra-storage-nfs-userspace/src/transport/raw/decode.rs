//! Reply decoding: libnfs's `COMPOUND4res` into an owned [`CompoundReply`].
//!
//! # Where the unsafe boundary sits
//!
//! [`compound_reply`] runs *inside* the libnfs completion callback, while the
//! `COMPOUND4res` and every buffer it points at are still owned by libnfs. It
//! copies each field into a Rust-owned value and returns. Nothing it produces
//! borrows C memory, so no reply outlives the buffer it was decoded from —
//! invariant 5 of the frozen [`RawTransport`](crate::transport::RawTransport)
//! contract.
//!
//! Attribute blobs are the opposite arrangement on purpose: `fattr4` carries an
//! opaque `attrlist4`, so the bytes are copied out first and then parsed by
//! [`Attributes`] decoding that is ordinary safe Rust over a slice, and is unit
//! tested without a server or an FFI call.
//!
//! # Alignment
//!
//! libnfs decodes a reply into a ZDR scratch buffer with a bump allocator that
//! only guarantees XDR's four-byte granularity, while `nfs_resop4`, `entry4`
//! and `COMPOUND4res` all contain `uint64_t` fields and therefore want eight.
//! A misaligned `&nfs_resop4` is undefined behaviour in Rust even though the C
//! library dereferences the same address happily, so every struct is lifted out
//! of the reply with [`core::ptr::read_unaligned`] and read from the aligned
//! local copy. Rust's debug alignment assertions caught this against a live
//! Ganesha reply; it is not a theoretical concern.

use crate::error::{Nfs4Status, ProtocolError, TransportError};
use crate::handle::{ClientId, FileHandle, Stateid};
use crate::transport::{
    AttrMask, Attributes, CommitReply, CompoundReply, DelegationType, DirCookie, DirEntry, DirPage,
    DirVerifier, Fsid, Nfs4Time, Nfs4Type, OpCode, OpReply, OpenReply, ReadReply, SetClientIdReply,
    Stability, TransportResult, Verifier, WriteReply, WriteVerifier,
};

use super::sys;

/// Budget for one reply, enforced before each allocation.
pub(super) struct ReplyBudget {
    remaining: usize,
}

impl ReplyBudget {
    pub(super) fn new(max_reply_bytes: usize) -> Self {
        Self {
            remaining: max_reply_bytes,
        }
    }

    /// Charge `bytes` before allocating them, or refuse the reply.
    fn charge(&mut self, bytes: usize) -> TransportResult<()> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
            TransportError::Malformed(format!(
                "reply exceeds the configured max_reply_bytes bound by at least {} bytes",
                bytes.saturating_sub(self.remaining)
            ))
        })?;
        Ok(())
    }
}

/// A decoded reply, plus where its READ data still has to come from.
pub(super) struct DecodedReply {
    /// The reply, owned.
    pub(super) reply: CompoundReply,
    /// `(result index, byte count)` when libnfs decoded READ data straight
    /// into the caller's destination buffer instead of its own.
    ///
    /// `rpc_nfs4_read_task` is a zero-copy path: it leaves `READ4resok.data`
    /// with a length but a null pointer, because the bytes were written into
    /// the arena's buffer. The reply is completed from that buffer by the
    /// dispatcher, which owns the arena; the callback cannot reach it.
    pub(super) zero_copy_read: Option<(usize, usize)>,
}

/// Copy a libnfs `COMPOUND4res` into an owned reply.
///
/// # Safety
///
/// `res` must be the live `COMPOUND4res` libnfs passed to a completion
/// callback, and this must be called before that callback returns. The pointer
/// is only read; nothing is retained.
pub(super) unsafe fn compound_reply(
    res: *const sys::COMPOUND4res,
    budget: &mut ReplyBudget,
) -> TransportResult<DecodedReply> {
    // Copied out rather than referenced: see the alignment note above.
    let res = core::ptr::read_unaligned(res);
    let tag = copy_bytes(
        res.tag.utf8string_val,
        res.tag.utf8string_len as usize,
        budget,
    )?;

    let count = res.resarray.resarray_len as usize;
    if count > 0 && res.resarray.resarray_val.is_null() {
        return Err(TransportError::Malformed(
            "COMPOUND reply declares results but carries none".into(),
        ));
    }

    let mut results = Vec::with_capacity(count);
    let mut failure = None;
    let mut zero_copy_read = None;

    for index in 0..count {
        let entry = core::ptr::read_unaligned(res.resarray.resarray_val.add(index));
        match op_reply(&entry, budget)? {
            Decoded::Ok(reply) => {
                if let OpReply::Read(read) = &reply {
                    if read.data.is_empty() {
                        if let Some(declared) = pending_read_length(&entry) {
                            zero_copy_read = Some((results.len(), declared));
                        }
                    }
                }
                results.push(reply)
            }
            Decoded::Failed(status, op) => {
                failure = Some(ProtocolError {
                    status,
                    op,
                    index: index as u32,
                });
                break;
            }
        }
    }

    // A COMPOUND whose overall status is an error but whose result array
    // recorded none is still a failure. Attribute it to the operation after the
    // last recorded result rather than inventing a success.
    if failure.is_none() && res.status != sys::NFS4_OK {
        failure = Some(ProtocolError {
            status: Nfs4Status(res.status),
            op: results
                .last()
                .map(OpReply::opcode)
                .unwrap_or(OpCode::PutRootFh),
            index: results.len() as u32,
        });
    }

    Ok(DecodedReply {
        reply: CompoundReply {
            tag,
            results,
            failure,
        },
        zero_copy_read,
    })
}

/// The byte count a zero-copy READ declared, if this result is one.
///
/// # Safety
///
/// `entry` is an aligned local copy of a result from the current reply.
unsafe fn pending_read_length(entry: &sys::nfs_resop4) -> Option<usize> {
    if entry.resop != 25 {
        return None;
    }
    let read = entry.nfs_resop4_u.opread;
    if read.status != sys::NFS4_OK {
        return None;
    }
    let data = read.READ4res_u.resok4.data;
    data.data_val.is_null().then_some(data.data_len as usize)
}

enum Decoded {
    Ok(OpReply),
    Failed(Nfs4Status, OpCode),
}

/// Decode one `nfs_resop4`.
///
/// # Safety
///
/// `entry` must be a live result from the reply currently being decoded.
unsafe fn op_reply(entry: &sys::nfs_resop4, budget: &mut ReplyBudget) -> TransportResult<Decoded> {
    let code = opcode(entry.resop)?;
    let union = &entry.nfs_resop4_u;

    // Every `*4res` begins with `nfsstat4 status`, but each is read through its
    // own arm so the union access matches the operation the server named.
    macro_rules! guard {
        ($arm:ident) => {{
            let status = union.$arm.status;
            if status != sys::NFS4_OK {
                return Ok(Decoded::Failed(Nfs4Status(status), code));
            }
        }};
    }

    let reply = match code {
        OpCode::PutRootFh => {
            guard!(opputrootfh);
            OpReply::PutRootFh
        }
        OpCode::PutFh => {
            guard!(opputfh);
            OpReply::PutFh
        }
        OpCode::Lookup => {
            guard!(oplookup);
            OpReply::Lookup
        }
        OpCode::LookupParent => {
            guard!(oplookupp);
            OpReply::LookupParent
        }
        OpCode::Renew => {
            guard!(oprenew);
            OpReply::Renew
        }
        OpCode::SetClientIdConfirm => {
            guard!(opsetclientid_confirm);
            OpReply::SetClientIdConfirm
        }
        OpCode::GetFh => {
            guard!(opgetfh);
            let handle = union.opgetfh.GETFH4res_u.resok4.object;
            let bytes = copy_bytes(handle.nfs_fh4_val, handle.nfs_fh4_len as usize, budget)?;
            OpReply::GetFh(FileHandle::from_wire(bytes).map_err(|error| {
                TransportError::Malformed(format!("GETFH returned an unusable filehandle: {error}"))
            })?)
        }
        OpCode::GetAttr => {
            guard!(opgetattr);
            let attrs = union.opgetattr.GETATTR4res_u.resok4.obj_attributes;
            OpReply::GetAttr(attributes(&attrs, budget)?)
        }
        OpCode::Read => {
            guard!(opread);
            let ok = union.opread.READ4res_u.resok4;
            // A null pointer with a non-zero length is the zero-copy path: the
            // bytes are already in the arena's destination buffer, and the
            // dispatcher completes the reply from there. Charging the budget
            // here still bounds it, because the destination was allocated at
            // `min(requested count, max_reply_bytes)`.
            let declared = ok.data.data_len as usize;
            let data = if ok.data.data_val.is_null() {
                budget.charge(declared)?;
                Vec::new()
            } else {
                copy_bytes(ok.data.data_val, declared, budget)?
            };
            OpReply::Read(ReadReply {
                data,
                eof: ok.eof != 0,
            })
        }
        OpCode::Write => {
            guard!(opwrite);
            let ok = union.opwrite.WRITE4res_u.resok4;
            OpReply::Write(WriteReply {
                count: ok.count,
                committed: stability(ok.committed)?,
                verifier: WriteVerifier(from_c_bytes(ok.writeverf)),
            })
        }
        OpCode::Commit => {
            guard!(opcommit);
            let ok = union.opcommit.COMMIT4res_u.resok4;
            OpReply::Commit(CommitReply {
                verifier: WriteVerifier(from_c_bytes(ok.writeverf)),
            })
        }
        OpCode::Open => {
            guard!(opopen);
            let ok = union.opopen.OPEN4res_u.resok4;
            OpReply::Open(OpenReply {
                stateid: stateid(&ok.stateid),
                // `OPEN4_RESULT_CONFIRM` is bit 1 of rflags (RFC 7530 §16.16).
                confirm_required: ok.rflags & OPEN4_RESULT_CONFIRM != 0,
                change_atomic: ok.cinfo.atomic != 0,
                change_before: ok.cinfo.before,
                change_after: ok.cinfo.after,
                delegation: delegation(ok.delegation.delegation_type),
            })
        }
        OpCode::OpenConfirm => {
            guard!(opopen_confirm);
            let ok = union.opopen_confirm.OPEN_CONFIRM4res_u.resok4;
            OpReply::OpenConfirm(stateid(&ok.open_stateid))
        }
        OpCode::Close => {
            guard!(opclose);
            let closed = union.opclose.CLOSE4res_u.open_stateid;
            OpReply::Close(crate::transport::CloseReply {
                stateid: stateid(&closed),
            })
        }
        OpCode::Lock => {
            guard!(oplock);
            let ok = union.oplock.LOCK4res_u.resok4;
            OpReply::Lock(crate::transport::LockReply {
                stateid: stateid(&ok.lock_stateid),
            })
        }
        OpCode::Locku => {
            guard!(oplocku);
            let released = union.oplocku.LOCKU4res_u.lock_stateid;
            OpReply::Locku(stateid(&released))
        }
        OpCode::SetClientId => {
            guard!(opsetclientid);
            let ok = union.opsetclientid.SETCLIENTID4res_u.resok4;
            OpReply::SetClientId(SetClientIdReply {
                client_id: ClientId(ok.clientid),
                confirm: Verifier(from_c_bytes(ok.setclientid_confirm)),
            })
        }
        OpCode::ReadDir => {
            guard!(opreaddir);
            let ok = union.opreaddir.READDIR4res_u.resok4;
            OpReply::ReadDir(dir_page(&ok, budget)?)
        }
        other => {
            return Err(TransportError::Malformed(format!(
                "server returned a {other:?} result this transport never requests"
            )))
        }
    };

    Ok(Decoded::Ok(reply))
}

/// Walk the READDIR entry list into an owned page.
///
/// # Safety
///
/// `ok` must belong to the reply currently being decoded.
unsafe fn dir_page(ok: &sys::READDIR4resok, budget: &mut ReplyBudget) -> TransportResult<DirPage> {
    let mut entries = Vec::new();
    let mut cursor = ok.reply.entries;

    while !cursor.is_null() {
        let entry = core::ptr::read_unaligned(cursor);
        let name = copy_bytes(
            entry.name.utf8string_val,
            entry.name.utf8string_len as usize,
            budget,
        )?;
        entries.push(DirEntry {
            cookie: DirCookie(entry.cookie),
            name: crate::transport::ComponentName::new(name)?,
            attributes: attributes(&entry.attrs, budget)?,
        });
        cursor = entry.nextentry;
    }

    Ok(DirPage {
        verifier: DirVerifier(from_c_bytes(ok.cookieverf)),
        entries,
        eof: ok.reply.eof != 0,
    })
}

/// Copy an attribute blob out of C memory, then parse it in safe Rust.
///
/// # Safety
///
/// `attrs` must belong to the reply currently being decoded.
unsafe fn attributes(attrs: &sys::fattr4, budget: &mut ReplyBudget) -> TransportResult<Attributes> {
    let mask = AttrMask {
        word0: attr_word(&attrs.attrmask, 0),
        word1: attr_word(&attrs.attrmask, 1),
    };
    let values = copy_bytes(
        attrs.attr_vals.attrlist4_val,
        attrs.attr_vals.attrlist4_len as usize,
        budget,
    )?;
    decode_attributes(mask, &values)
}

/// Read one word of a `bitmap4`, treating an absent word as zero.
///
/// # Safety
///
/// `bitmap` must belong to the reply currently being decoded.
unsafe fn attr_word(bitmap: &sys::bitmap4, index: u32) -> u32 {
    if index >= bitmap.bitmap4_len || bitmap.bitmap4_val.is_null() {
        return 0;
    }
    core::ptr::read_unaligned(bitmap.bitmap4_val.add(index as usize))
}

/// Copy `len` bytes out of C memory into an owned `Vec`.
///
/// # Safety
///
/// `pointer` must be valid for `len` bytes for the duration of this call.
unsafe fn copy_bytes(
    pointer: *const core::ffi::c_char,
    len: usize,
    budget: &mut ReplyBudget,
) -> TransportResult<Vec<u8>> {
    budget.charge(len)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err(TransportError::Malformed(
            "reply declares bytes but carries a null pointer".into(),
        ));
    }
    Ok(core::slice::from_raw_parts(pointer.cast::<u8>(), len).to_vec())
}

fn stateid(value: &sys::stateid4) -> Stateid {
    Stateid {
        seqid: value.seqid,
        other: from_c_bytes(value.other),
    }
}

fn from_c_bytes<const N: usize>(bytes: [core::ffi::c_char; N]) -> [u8; N] {
    core::array::from_fn(|index| bytes[index] as u8)
}

fn stability(value: sys::stable_how4) -> TransportResult<Stability> {
    match value {
        sys::stable_how4_UNSTABLE4 => Ok(Stability::Unstable),
        sys::stable_how4_DATA_SYNC4 => Ok(Stability::DataSync),
        sys::stable_how4_FILE_SYNC4 => Ok(Stability::FileSync),
        other => Err(TransportError::Malformed(format!(
            "WRITE reported stability {other}, which is not a stable_how4 value"
        ))),
    }
}

fn delegation(value: sys::open_delegation_type4) -> DelegationType {
    match value {
        1 => DelegationType::Read,
        2 => DelegationType::Write,
        _ => DelegationType::None,
    }
}

/// Map libnfs's `nfs_opnum4` onto the frozen [`OpCode`].
///
/// A v4.1 operation number has no [`OpCode`] variant and is rejected here, so a
/// server that answered one could not be silently accepted.
fn opcode(value: sys::nfs_opnum4) -> TransportResult<OpCode> {
    Ok(match value {
        3 => OpCode::Access,
        4 => OpCode::Close,
        5 => OpCode::Commit,
        6 => OpCode::Create,
        9 => OpCode::GetAttr,
        10 => OpCode::GetFh,
        11 => OpCode::Link,
        12 => OpCode::Lock,
        13 => OpCode::Lockt,
        14 => OpCode::Locku,
        15 => OpCode::Lookup,
        16 => OpCode::LookupParent,
        18 => OpCode::Open,
        20 => OpCode::OpenConfirm,
        21 => OpCode::OpenDowngrade,
        22 => OpCode::PutFh,
        24 => OpCode::PutRootFh,
        25 => OpCode::Read,
        26 => OpCode::ReadDir,
        27 => OpCode::ReadLink,
        28 => OpCode::Remove,
        29 => OpCode::Rename,
        30 => OpCode::Renew,
        31 => OpCode::RestoreFh,
        32 => OpCode::SaveFh,
        34 => OpCode::SetAttr,
        35 => OpCode::SetClientId,
        36 => OpCode::SetClientIdConfirm,
        38 => OpCode::Write,
        39 => OpCode::ReleaseLockOwner,
        other => {
            return Err(TransportError::Malformed(format!(
                "server returned operation {other}, which is outside NFSv4.0"
            )))
        }
    })
}

/// `OPEN4_RESULT_CONFIRM`.
const OPEN4_RESULT_CONFIRM: u32 = 0x0000_0002;

// --- Attribute decoding: safe, and tested without a server -------------------

/// A cursor over an XDR-encoded attribute value blob.
struct XdrReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> XdrReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> TransportResult<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| {
                TransportError::Malformed(format!(
                    "attribute blob ended after {} bytes; {count} more were needed",
                    self.bytes.len()
                ))
            })?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u32(&mut self) -> TransportResult<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> TransportResult<u64> {
        let high = u64::from(self.u32()?);
        let low = u64::from(self.u32()?);
        Ok(high << 32 | low)
    }

    fn i64(&mut self) -> TransportResult<i64> {
        Ok(self.u64()? as i64)
    }

    /// An XDR opaque, padded to a four-byte boundary.
    fn opaque(&mut self) -> TransportResult<Vec<u8>> {
        let len = self.u32()? as usize;
        let value = self.take(len)?.to_vec();
        let padding = (4 - (len % 4)) % 4;
        self.take(padding)?;
        Ok(value)
    }
}

/// Decode an NFSv4 attribute blob against the bitmap the server returned.
///
/// Attributes appear in ascending attribute number. A bit this transport does
/// not know how to decode is an error rather than a skip: a variable-length
/// attribute cannot be stepped over without decoding it, so guessing would
/// misalign every later value.
pub(super) fn decode_attributes(mask: AttrMask, values: &[u8]) -> TransportResult<Attributes> {
    let mut reader = XdrReader::new(values);
    let mut attributes = Attributes {
        returned: mask,
        ..Attributes::default()
    };

    for number in 0..64u32 {
        let present = if number < 32 {
            mask.word0 & (1 << number) != 0
        } else {
            mask.word1 & (1 << (number - 32)) != 0
        };
        if !present {
            continue;
        }

        match number {
            1 => attributes.file_type = Some(file_type(reader.u32()?)),
            3 => attributes.change = Some(reader.u64()?),
            4 => attributes.size = Some(reader.u64()?),
            8 => {
                attributes.fsid = Some(Fsid {
                    major: reader.u64()?,
                    minor: reader.u64()?,
                })
            }
            10 => attributes.lease_time = Some(reader.u32()?),
            11 => attributes.rdattr_error = Some(Nfs4Status(reader.u32()?)),
            20 => attributes.fileid = Some(reader.u64()?),
            33 => attributes.mode = Some(reader.u32()?),
            35 => attributes.numlinks = Some(reader.u32()?),
            36 => attributes.owner = Some(reader.opaque()?),
            37 => attributes.owner_group = Some(reader.opaque()?),
            53 => {
                attributes.time_modify = Some(Nfs4Time {
                    seconds: reader.i64()?,
                    nanoseconds: reader.u32()?,
                })
            }
            other => {
                return Err(TransportError::Malformed(format!(
                    "server returned attribute {other}, which this transport did not request \
                     and cannot skip without misaligning the rest of the blob"
                )))
            }
        }
    }

    Ok(attributes)
}

fn file_type(value: u32) -> Nfs4Type {
    match value {
        1 => Nfs4Type::Regular,
        2 => Nfs4Type::Directory,
        5 => Nfs4Type::Symlink,
        other => Nfs4Type::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_be_bytes()).collect()
    }

    #[test]
    fn identity_attributes_decode_in_bitmap_order() {
        // FSID (8) then FILEID (20), both word 0, ascending.
        let mask = AttrMask::IDENTITY;
        let values = blob(&[0, 7, 0, 9, 0, 4242]);
        let decoded = decode_attributes(mask, &values).unwrap();
        assert_eq!(decoded.fsid, Some(Fsid { major: 7, minor: 9 }));
        assert_eq!(decoded.fileid, Some(4242));
        assert_eq!(decoded.returned, mask);
    }

    #[test]
    fn an_absent_bit_leaves_the_field_none_rather_than_defaulted() {
        let decoded = decode_attributes(AttrMask::SIZE, &blob(&[0, 11])).unwrap();
        assert_eq!(decoded.size, Some(11));
        assert_eq!(decoded.mode, None);
        assert_eq!(decoded.file_type, None);
    }

    #[test]
    fn a_word_one_attribute_decodes_after_word_zero_attributes() {
        let mask = AttrMask::SIZE.union(AttrMask::MODE);
        let decoded = decode_attributes(mask, &blob(&[0, 64, 0o644])).unwrap();
        assert_eq!(decoded.size, Some(64));
        assert_eq!(decoded.mode, Some(0o644));
    }

    #[test]
    fn owner_bytes_are_preserved_with_their_xdr_padding_consumed() {
        // OWNER (36) is a 5-byte string padded to 8, then OWNER_GROUP (37).
        let mask = AttrMask::OWNER.union(AttrMask::OWNER_GROUP);
        let mut values = Vec::new();
        values.extend(5u32.to_be_bytes());
        values.extend(b"alice");
        values.extend([0, 0, 0]);
        values.extend(4u32.to_be_bytes());
        values.extend(b"staf");
        let decoded = decode_attributes(mask, &values).unwrap();
        assert_eq!(decoded.owner.as_deref(), Some(b"alice".as_slice()));
        assert_eq!(decoded.owner_group.as_deref(), Some(b"staf".as_slice()));
    }

    #[test]
    fn a_truncated_blob_is_malformed_rather_than_a_partial_answer() {
        let error = decode_attributes(AttrMask::SIZE, &blob(&[0])).unwrap_err();
        assert!(matches!(error, TransportError::Malformed(_)));
    }

    #[test]
    fn an_unrequested_attribute_bit_is_refused_not_skipped() {
        // Attribute 2 (FH_EXPIRE_TYPE) is never requested and cannot be skipped.
        let mask = AttrMask {
            word0: 1 << 2,
            word1: 0,
        };
        let error = decode_attributes(mask, &blob(&[0])).unwrap_err();
        assert!(matches!(error, TransportError::Malformed(_)));
    }

    #[test]
    fn a_rdattr_error_is_preserved_rather_than_discarded() {
        let mask = AttrMask::RDATTR_ERROR;
        let decoded = decode_attributes(mask, &blob(&[10008])).unwrap();
        assert_eq!(decoded.rdattr_error, Some(Nfs4Status::DELAY));
    }

    #[test]
    fn the_reply_budget_refuses_before_it_allocates() {
        let mut budget = ReplyBudget::new(8);
        assert!(budget.charge(4).is_ok());
        assert!(budget.charge(4).is_ok());
        assert!(budget.charge(1).is_err());
    }
}
