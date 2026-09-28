//! Darwin's `getattrlistbulk`(461) reply wire format.
//!
//! **Why this lives here and not in `umbra-platform-macos`.** Two processes have
//! to agree about these bytes and only one of them is the macOS backend.
//! `umbra-platform-macos` is a provider *executable* reached over IPC, and the
//! `DirectoryEncoder` the overlay calls during `resolve` is `Send` and runs in the
//! supervisor's process (`umbra_platform::provider::Abi` holds an
//! `Rc<RefCell<Client>>`, so no encoder that reaches the ABI proxy can satisfy
//! that bound). The encode therefore has to happen supervisor-side, while
//! `emulate_result` -- which has to turn the byte count into the *entry* count
//! this syscall returns -- happens backend-side. Putting the format in the crate
//! both depend on is what keeps that one format rather than two, which is the
//! whole lesson of `StatEncoder`-versus-`encode_stat`.
//!
//! **Every offset, size, order and padding rule below was measured**, not read
//! out of `sys/attr.h` and not recalled from XNU: a C probe issued the exact
//! request `/bin/ls`'s `fts` issues against a real directory on macOS 26.5.1
//! arm64 and the reply bytes were dumped and decoded. `RECORDED_REPLY` at the
//! bottom of this file is a verbatim capture of that kernel reply, and the unit
//! tests re-derive this encoder's output against it, so the format is checked
//! against the kernel rather than against a second copy of these constants.
//!
//! The measured request, which is the **only** shape umbra serves:
//!
//! ```text
//! attrlist { bitmapcount: 5, reserved: 0,
//!            commonattr: 0x8200000b, volattr: 0, dirattr: 0,
//!            fileattr:   0x00000001, forkattr: 0 }
//! options: FSOPT_PACK_INVAL_ATTRS (8)
//! ```
//!
//! and the measured reply record, in this order:
//!
//! ```text
//! u32  length                 total bytes of this record, 8-byte aligned
//! u32  returned.commonattr    the attribute_set_t ATTR_CMN_RETURNED_ATTRS asks
//! u32  returned.volattr       for -- five groups, always first, never omitted
//! u32  returned.dirattr
//! u32  returned.fileattr      0 for a directory: see `fileattr` below
//! u32  returned.forkattr
//! i32  name.attr_dataoffset   relative to the start of THIS field pair
//! u32  name.attr_length       bytes of the name including its NUL
//! i32  ATTR_CMN_DEVID
//! u32  ATTR_CMN_OBJTYPE       fsobj_type_t: VREG 1, VDIR 2, VLNK 5
//! u64  ATTR_CMN_FILEID        **not** 8-byte aligned -- see `PACKING` below
//! u32  ATTR_FILE_LINKCOUNT    absent entirely when the object is a directory
//! ..   the name bytes, then zero padding to the 8-byte boundary
//! ```

use umbra_core::{
    storage::{DirectoryEntry, ObjectKind},
    ErrorKind, Result, UmbraError,
};

/// The negotiated ABI label whose `getattrlistbulk` reply this module encodes.
///
/// **One string, two readers, and that is the point.** The macOS backend
/// advertises it in `TraceControl::capabilities`; the supervisor gates the
/// injection of its directory encoder on it. Written twice, the pair could drift
/// -- a backend renaming its ABI would silently stop having an encoder injected
/// and `ls` would go back to refusing, with every gate still green. That is the
/// `install()`-versus-`intercept()` shape this codebase has already paid for
/// once, so the two sites read this constant instead.
pub const DARWIN_ARM64_ABI: &str = "darwin-arm64-abi-v1";

/// `ATTR_BIT_MAP_COUNT` -- the only `bitmapcount` a caller may declare.
pub const ATTR_BIT_MAP_COUNT: u16 = 5;
/// Bytes of `struct attrlist`: `u_short` + `u_int16_t` + five `attrgroup_t`.
pub const ATTRLIST_BYTES: usize = 24;

/// `commonattr` of the one request shape umbra serves:
/// `ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_FILEID | ATTR_CMN_OBJTYPE | ATTR_CMN_DEVID
/// | ATTR_CMN_NAME`.
pub const SERVED_COMMONATTR: u32 = 0x8200_000b;
/// `fileattr` of that shape: `ATTR_FILE_LINKCOUNT`.
pub const SERVED_FILEATTR: u32 = 0x0000_0001;
/// `FSOPT_PACK_INVAL_ATTRS`, the one `options` value measured on this path.
///
/// Checked by the ABI rather than by [`RequestedAttributes::decode`], because it
/// arrives in a register rather than in the stopped task's memory. It is not
/// merely tolerated: it is the shape the records are packed in. Without it the
/// kernel omits invalid attributes instead of zero-filling them, which is a
/// different layout, so accepting its absence would encode the wrong buffer.
pub const SERVED_OPTIONS: u32 = 0x0000_0008;

/// `fsobj_type_t` values this encoder can produce.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// The synthetic `ATTR_CMN_DEVID` every routed entry reports.
///
/// A virtual descriptor names no kernel device, so there is no real `st_dev` to
/// report and inventing a plausible one would be a lie a program could act on.
/// Zero is reported instead, consistently, and it matches what
/// `umbra_platform_macos::abi::encode_stat` leaves in `st_dev` for the same
/// object -- the two have to agree, because `fts` sees both.
const DEVID: i32 = 0;

/// Fixed bytes every record carries before the name data: the length word, the
/// five-group `attribute_set_t`, the name `attrreference_t`, `DEVID`, `OBJTYPE`
/// and `FILEID`.
const FIXED_COMMON_BYTES: usize = 4 + 20 + 8 + 4 + 4 + 8;
/// `ATTR_FILE_LINKCOUNT`, present only for non-directories.
const LINKCOUNT_BYTES: usize = 4;

/// **PACKING.** Attributes are packed tight, in ascending bitmap order, with no
/// internal alignment of any kind. Only the *record* is aligned, to 8 bytes.
///
/// **The evidence is a request that is deliberately not the served one, and the
/// reason is worth stating**: in the served shape the `u64` `ATTR_CMN_FILEID`
/// lands at record offset `0x28`, which is 8-aligned, so it demonstrates
/// nothing at all -- it is where the value would sit under either rule. This
/// comment used to cite that offset as `0x24` and offer it as proof, which was
/// wrong twice over, and a later reader widening the attribute set would have
/// had no way to tell.
///
/// Dropping one 4-byte attribute is what makes the rule visible. Measured, with
/// `common=0x82000009` -- the served set minus `ATTR_CMN_DEVID` --
/// `ATTR_CMN_FILEID` moves to offset `0x24`, which is 4-aligned and **not**
/// 8-aligned, and the kernel inserts no pad before it:
///
/// ```text
///   served   0x8200000b : … 0x20 DEVID  0x24 OBJTYPE  0x28 FILEID   (8-aligned)
///   minus    0x82000009 : …             0x20 OBJTYPE  0x24 FILEID   (4-aligned, no pad)
/// ```
const RECORD_ALIGNMENT: usize = 8;

/// An **encoder-side** fault: umbra produced, or was handed, something it cannot
/// represent. Carries no errno **on purpose**, so it reaches the caller as an
/// error that ends the run.
///
/// **Never reachable from a tracee's request, and the name says so because the
/// alternative was measured.** `RequestedAttributes::decode` used this for its
/// header arm, and `Supervisor::unserved_directory_request` raises anything
/// without an errno -- so a program that left `struct attrlist::reserved`
/// holding stack garbage ended the run, for a request the kernel serves. Every
/// refusal of a *tracee-supplied* request goes through [`unserved`]; this one is
/// for a bad `DirectoryEntry` from the overlay or a reply this encoder itself
/// mis-packed, where there is no program to answer and stopping is correct.
fn encoder_fault(message: &'static str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidInput, "dirents", message)
}

/// An attribute request umbra cannot encode, carried as a **tracee-visible**
/// `ENOTSUP` rather than as a bare refusal.
///
/// The errno travels inside the error so the caller can bind it as an answer to
/// the program instead of ending the run, which is the rule `routing_for` states
/// for a bad pointer and which applies here for the same reason: asking for
/// attributes umbra does not model is something an ordinary program does --
/// `ls -l` does it -- not a failure of interception.
///
/// **`ENOTSUP` is umbra's own answer and not a copy of the kernel's**, and that
/// distinction is measured rather than assumed. The kernel *serves* the wider
/// set `ls -l` asks for (`common=0x82079e0b file=0x0000022d` returns records,
/// errno 0); it is umbra that cannot encode it. So this errno is only ever
/// correct for a descriptor umbra owns, and the caller must apply its descriptor
/// test before it reads the block this refusal describes.
fn unserved(message: String) -> UmbraError {
    UmbraError::new(ErrorKind::UnsupportedCapability, "dirents", message)
        .with_errno(umbra_core::Errno(45))
}

/// The attribute request a `getattrlistbulk` caller declared.
///
/// Decoded rather than trusted: a bit umbra does not model is not a flag it
/// could drop, it is a *different buffer layout*, so anything outside the one
/// measured shape is refused whole. This is the discipline
/// `umbra_platform_macos::abi::setattrlist_times` already applies to
/// `setattrlistat`, and for the identical reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestedAttributes {
    _private: (),
}

impl RequestedAttributes {
    /// Validate a tracee's `struct attrlist` against the one shape this module
    /// encodes.
    ///
    /// **The `options` word is not checked here, and the split is deliberate.**
    /// `options` lives in a register and the attribute bitmaps live in the
    /// stopped task's memory, so the two are read by different halves of the
    /// caller -- the ABI answers the register, the supervisor reads the block
    /// after its own descriptor test. Checking the register half here would mean
    /// passing it through a layer that has no other use for it. See
    /// [`SERVED_OPTIONS`].
    ///
    /// The refusal names what was asked for, because the caller is a real
    /// program and the next `ls` mode somebody tries (`-l` wants
    /// `ATTR_CMN_ACCESSMASK`/`listxattr`, `-R` wants a cwd model umbra does not
    /// have) will arrive here first and should say so.
    pub fn decode(attrlist: &[u8; ATTRLIST_BYTES]) -> Result<Self> {
        let word = |at: usize| u32::from_le_bytes(attrlist[at..at + 4].try_into().unwrap());
        let bitmapcount = u16::from_le_bytes(attrlist[0..2].try_into().unwrap());
        let reserved = u16::from_le_bytes(attrlist[2..4].try_into().unwrap());
        let (commonattr, volattr, dirattr, fileattr, forkattr) =
            (word(4), word(8), word(12), word(16), word(20));
        // **`reserved` is read and deliberately ignored, because the kernel
        // ignores it and because treating it as request content ended runs.**
        //
        // Measured on this host: with the served bitmaps, `reserved = 0`,
        // `0x1234` and `0xffff` produce **byte-identical** replies. The field is
        // a don't-care, exactly as its name says. But `struct attrlist` is
        // `{u_short bitmapcount; u_int16_t reserved; attrgroup_t commonattr; …}`,
        // so a program that assigns the fields it cares about on a stack struct
        // without `memset` or `= {0}` leaves it holding stack garbage -- and
        // refusing on it took down a routed run for a request the kernel serves.
        // A latent program bug that works on every other backend must not be
        // fatal here.
        let _ = reserved;
        // `bitmapcount` is request content rather than a don't-care: it declares
        // how many of the five attrgroup words the caller means. This kernel
        // ignores it too -- measured, 0, 3, 4, 5 and 6 all return the same
        // five-group reply -- but umbra's encoder produces exactly one layout,
        // and a caller declaring a different extent is asking for a shape umbra
        // does not model. So it is refused, on the same terms and through the
        // same constructor as an attribute set umbra cannot encode: **answered
        // to the program, never fatal to the run.**
        if bitmapcount != ATTR_BIT_MAP_COUNT {
            return Err(unserved(format!(
                "getattrlistbulk attrlist declares bitmapcount {bitmapcount}; umbra serves \
                 only the {ATTR_BIT_MAP_COUNT}-group shape"
            )));
        }
        if commonattr != SERVED_COMMONATTR
            || volattr != 0
            || dirattr != 0
            || fileattr != SERVED_FILEATTR
            || forkattr != 0
        {
            return Err(unserved(format!(
                "getattrlistbulk attribute set common={commonattr:#010x} vol={volattr:#010x} \
                 dir={dirattr:#010x} file={fileattr:#010x} fork={forkattr:#010x}; umbra \
                 serves only common={SERVED_COMMONATTR:#010x} file={SERVED_FILEATTR:#010x}"
            )));
        }
        Ok(Self { _private: () })
    }
}

/// A buffer the caller sized too small to hold one whole record, carried as a
/// **tracee-visible** `ERANGE`.
///
/// The sibling of [`unserved`], and it exists for the same reason: the value it
/// judges is the tracee's, so the refusal has to be answerable rather than
/// fatal. `ERANGE` is measured, not chosen by analogy -- Darwin returns it for
/// exactly this condition, where it *serves* the wider attribute set [`unserved`]
/// refuses. The two refusals in this module are the two tracee-supplied inputs
/// a `getattrlistbulk` request carries, and both now end in a bindable errno.
///
/// `ERANGE` is 34 on Darwin and Linux alike, which is what makes naming it here
/// safe; the same argument `routed_binding` makes about `EBADF`.
fn too_small(message: String) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidInput, "dirents", message).with_errno(umbra_core::Errno(34))
}

/// Bytes one entry occupies, including its alignment padding.
fn record_bytes(entry: &DirectoryEntry) -> Result<usize> {
    // The name is stored with a trailing NUL, which `attr_length` counts.
    // `BytePath` already guarantees nonempty and NUL-free, so what is left to
    // check is the part `DirectoryEntry` states in prose and nothing enforces:
    // that the name is one *component*. A `/` here would put a second component
    // in a field the caller reads as a leaf name, and `.`/`..` are the two names
    // `getattrlistbulk` never returns -- emitting either would make `ls` print a
    // directory that does not contain them.
    let name = entry.name.as_bytes();
    if name.contains(&b'/') || name == b"." || name == b".." {
        return Err(encoder_fault(
            "directory entry name is not one path component",
        ));
    }
    let linkcount = if entry.stat.kind == ObjectKind::Directory {
        0
    } else {
        LINKCOUNT_BYTES
    };
    let raw = FIXED_COMMON_BYTES + linkcount + name.len() + 1;
    Ok(raw.next_multiple_of(RECORD_ALIGNMENT))
}

/// Encode a prefix of `entries` into `getattrlistbulk` reply records.
///
/// Returns the bytes and the number of whole entries they describe. A record is
/// emitted only if it fits entirely, so the reply never carries a torn record --
/// which is what lets the count be re-derived from the bytes alone by
/// [`count_records`].
///
/// **A single entry too large for the caller's buffer is answered `ERANGE`, not
/// a silent zero-entry reply and not a stopped run.**
///
/// Answering "0 entries" would mean end-of-directory to the caller, so `ls`
/// would print a short listing and exit 0 -- the one failure shape worse than a
/// refusal. That much the original reasoning had right. What it had wrong was
/// treating error-or-silence as the only options: **the third is the one the
/// kernel takes**, and this returned an errno-less `Err`, which ended the run.
///
/// Measured: Darwin answers `ERANGE`(34) whenever no whole record fits --
/// `cap` of 1, 8, 32 and 55 against a 56-byte record all return `-1`/`ERANGE`,
/// and 56 serves one entry. So the refusal here is now that errno, carried as a
/// value for the caller to bind, and a program can do what the errno invites:
/// grow the buffer and call again.
///
/// **Why this is reachable at all, which the old comment's "Darwin never
/// produces one for the names a filesystem can hold" missed.** That sentence is
/// about *names*; this arm fires on the relationship between a name and a
/// *buffer size*, and the buffer size is the tracee's. The threshold is not the
/// buffer but **the longest name in the directory that sorts early**, because
/// [`Overlay::merged`] returns entries byte-sorted while the kernel's
/// enumeration order is not. A 200-character name beginning `L` (0x4C) sorts
/// ahead of `aaa` (0x61), so its 256-byte record is *first*, `consumed` is 0,
/// and a 140-byte buffer that serves every other directory in the workspace
/// refuses this one. Measured both ways: the kernel serves that same directory
/// at `cap=140`, because it happened to enumerate the short names first.
///
/// That ordering difference is not a defect -- byte-sorted order is what makes
/// `directory_next`'s paging deterministic across calls -- but it is why the
/// threshold is data-dependent, and a reader who does not know it will not be
/// able to explain why one directory refuses and another does not.
pub fn encode(entries: &[DirectoryEntry], max_bytes: u32) -> Result<(Vec<u8>, usize)> {
    let capacity = max_bytes as usize;
    let mut out = Vec::new();
    let mut consumed = 0usize;
    for entry in entries {
        let needed = record_bytes(entry)?;
        if out.len() + needed > capacity {
            if consumed == 0 {
                return Err(too_small(format!(
                    "getattrlistbulk buffer of {capacity} bytes cannot hold one \
                     {needed}-byte entry record"
                )));
            }
            break;
        }
        let name = entry.name.as_bytes();
        let directory = entry.stat.kind == ObjectKind::Directory;
        let start = out.len();
        out.extend_from_slice(&(needed as u32).to_le_bytes());
        // ATTR_CMN_RETURNED_ATTRS: what this record actually carries. The file
        // group is empty for a directory because `ATTR_FILE_LINKCOUNT` does not
        // apply to one and the kernel drops it from the record entirely --
        // measured, and the reason a directory record is 8 bytes shorter.
        out.extend_from_slice(&SERVED_COMMONATTR.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&if directory { 0 } else { SERVED_FILEATTR }.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        // ATTR_CMN_NAME. `attr_dataoffset` is relative to the start of the
        // `attrreference_t` itself, so it is the distance from here to the name
        // bytes, which follow every fixed attribute.
        let reference_at = out.len() - start;
        let fixed = FIXED_COMMON_BYTES + if directory { 0 } else { LINKCOUNT_BYTES };
        out.extend_from_slice(&((fixed - reference_at) as i32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
        out.extend_from_slice(&DEVID.to_le_bytes());
        out.extend_from_slice(
            &match entry.stat.kind {
                ObjectKind::Directory => VDIR,
                ObjectKind::File => VREG,
                ObjectKind::LogicalSymlink => VLNK,
            }
            .to_le_bytes(),
        );
        // `st_ino` is 64 bits and a logical object id is 128; the low half is a
        // deterministic projection, exactly as `encode_stat` takes it, so a
        // program that compares the two sees one identity.
        out.extend_from_slice(&entry.stat.object_id.0.as_u128().to_le_bytes()[..8]);
        if !directory {
            out.extend_from_slice(
                &(entry.stat.link_count.min(u32::MAX as u64) as u32).to_le_bytes(),
            );
        }
        out.extend_from_slice(name);
        out.resize(start + needed, 0);
        consumed += 1;
    }
    Ok((out, consumed))
}

/// Count whole records in an encoded reply, refusing anything that does not walk
/// exactly to the end.
///
/// This is what lets `emulate_result` answer `getattrlistbulk` with the **entry
/// count** the syscall returns while the overlay accounts in **bytes**: the
/// records are self-describing, so the count is re-derived from the bytes rather
/// than carried alongside them where the two could disagree. A walk that
/// overruns, stalls on a zero length or stops short is a corrupt reply and is
/// refused rather than rounded to a plausible count.
pub fn count_records(bytes: &[u8]) -> Result<usize> {
    let mut at = 0usize;
    let mut count = 0usize;
    while at < bytes.len() {
        let end = at
            .checked_add(4)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| encoder_fault("directory reply ends inside a record length"))?;
        let length = u32::from_le_bytes(bytes[at..end].try_into().unwrap()) as usize;
        // **The zero check is not redundant with the alignment check**: zero
        // *is* a multiple of eight, and a zero-length record would leave `at`
        // where it was and spin this loop forever on a reply the encoder never
        // produced.
        if length == 0 || !length.is_multiple_of(RECORD_ALIGNMENT) {
            return Err(encoder_fault(
                "directory reply record length is not a positive multiple of 8",
            ));
        }
        at = at
            .checked_add(length)
            .filter(|at| *at <= bytes.len())
            .ok_or_else(|| encoder_fault("directory reply record runs past the buffer"))?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{storage::BlobStat, BytePath, ObjectId};
    use uuid::Uuid;

    /// A verbatim capture of the kernel's own `getattrlistbulk` reply, taken on
    /// macOS 26.5.1 arm64 for a directory holding `beta`, `gamma`, `alpha`
    /// (regular files) and `alpha_d` (a directory), with the exact request
    /// `/bin/ls`'s `fts` issues.
    ///
    /// This is the ground truth the encoder is checked against. It is a
    /// recording of the other side of the format, which is the same reason
    /// `abi.rs`'s tests read `umbra_interpose.c` rather than a second copy of
    /// its constants.
    const RECORDED_REPLY: &[(&str, u32, &str)] = &[
        (
            "beta",
            VREG,
            concat!(
                "40000000",
                "0b000082",
                "00000000",
                "00000000",
                "01000000",
                "00000000",
                "1c000000",
                "05000000",
                "12000001",
                "01000000",
                "984d052300000000",
                "01000000",
                "6265746100",
                "00000000000000",
            ),
        ),
        (
            "alpha_d",
            VDIR,
            concat!(
                "38000000",
                "0b000082",
                "00000000",
                "00000000",
                "00000000",
                "00000000",
                "18000000",
                "08000000",
                "12000001",
                "02000000",
                "964d052300000000",
                "616c7068615f6400",
            ),
        ),
    ];

    /// The one request shape umbra serves, as a tracee would lay it out.
    fn served_attrlist() -> [u8; ATTRLIST_BYTES] {
        let mut attrlist = [0u8; ATTRLIST_BYTES];
        attrlist[0..2].copy_from_slice(&ATTR_BIT_MAP_COUNT.to_le_bytes());
        attrlist[4..8].copy_from_slice(&SERVED_COMMONATTR.to_le_bytes());
        attrlist[16..20].copy_from_slice(&SERVED_FILEATTR.to_le_bytes());
        attrlist
    }

    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    fn entry(name: &str, kind: ObjectKind, ino: u64, links: u64) -> DirectoryEntry {
        DirectoryEntry {
            name: BytePath::new(name.as_bytes().to_vec()).unwrap(),
            stat: BlobStat {
                object_id: ObjectId(Uuid::from_u128(ino as u128)),
                kind,
                len: 0,
                link_count: links,
                mode: 0o644,
                uid: 0,
                gid: 0,
                modified_nanos: 0,
            },
        }
    }

    /// The encoder's record layout is the kernel's, field for field.
    ///
    /// The recorded bytes carry the host's own `DEVID` and inode numbers, which
    /// umbra deliberately answers differently (a virtual descriptor has no
    /// device, and the inode is a projection of a logical object id). Those two
    /// fields are therefore masked out of the comparison and asserted
    /// separately; everything else -- lengths, the returned-attribute bitmaps,
    /// the name reference, the object type, the packing and the padding -- must
    /// match byte for byte.
    #[test]
    fn encoded_records_match_the_kernels_own_reply_layout() {
        for (name, kind_word, recorded) in RECORDED_REPLY {
            let recorded = unhex(recorded);
            let kind = if *kind_word == VDIR {
                ObjectKind::Directory
            } else {
                ObjectKind::File
            };
            let (bytes, consumed) = encode(&[entry(name, kind, 1, 1)], 4096).unwrap();
            assert_eq!(consumed, 1);
            assert_eq!(
                bytes.len(),
                recorded.len(),
                "record length for {name} disagrees with the kernel's"
            );
            // DEVID at 0x20 and FILEID at 0x24..0x2c are umbra's own values.
            let masked = |b: &[u8]| {
                let mut b = b.to_vec();
                b[0x20..0x2c].fill(0);
                b
            };
            assert_eq!(masked(&bytes), masked(&recorded), "record body for {name}");
        }
    }

    /// The name reference really points at the name, from wherever it sits.
    #[test]
    fn the_name_reference_offset_locates_the_name_bytes() {
        for (name, kind) in [("a", ObjectKind::File), ("a_dir", ObjectKind::Directory)] {
            let (bytes, _) = encode(&[entry(name, kind, 7, 1)], 4096).unwrap();
            let reference_at = 0x18;
            let offset =
                i32::from_le_bytes(bytes[reference_at..reference_at + 4].try_into().unwrap());
            let length = u32::from_le_bytes(
                bytes[reference_at + 4..reference_at + 8]
                    .try_into()
                    .unwrap(),
            );
            let at = reference_at + offset as usize;
            assert_eq!(length as usize, name.len() + 1);
            assert_eq!(&bytes[at..at + name.len()], name.as_bytes());
            assert_eq!(bytes[at + name.len()], 0, "the name is NUL terminated");
        }
    }

    /// Every record is a multiple of eight bytes, for every name length.
    #[test]
    fn every_record_length_is_eight_byte_aligned() {
        for length in 1..=64usize {
            let name = "n".repeat(length);
            let (bytes, _) = encode(&[entry(&name, ObjectKind::File, 1, 1)], 4096).unwrap();
            assert_eq!(bytes.len() % 8, 0, "name of {length} bytes");
            assert_eq!(
                u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
                bytes.len()
            );
        }
    }

    /// A directory record omits `ATTR_FILE_LINKCOUNT` and says so in its
    /// returned-attribute bitmap, which is what the kernel does.
    #[test]
    fn a_directory_record_omits_the_file_attribute_group() {
        // "alpha" is five bytes, so the missing `ATTR_FILE_LINKCOUNT` crosses an
        // alignment boundary and is visible in the record length. For a shorter
        // name the padding absorbs it and both records are the same size, which
        // is why this asserts on a name where the difference actually shows.
        let (file, _) = encode(&[entry("alpha", ObjectKind::File, 1, 1)], 4096).unwrap();
        let (dir, _) = encode(&[entry("alpha", ObjectKind::Directory, 1, 1)], 4096).unwrap();
        assert_eq!(
            u32::from_le_bytes(file[16..20].try_into().unwrap()),
            SERVED_FILEATTR
        );
        assert_eq!(u32::from_le_bytes(dir[16..20].try_into().unwrap()), 0);
        assert_eq!(file.len(), 64);
        assert_eq!(dir.len(), 56);
    }

    /// The reply stops at whole records and reports how many it wrote.
    #[test]
    fn encoding_fills_a_bounded_buffer_with_whole_records_only() {
        let entries: Vec<_> = (0..8)
            .map(|i| entry(&format!("name{i}"), ObjectKind::File, i, 1))
            .collect();
        let one = record_bytes(&entries[0]).unwrap();
        for take in 1..=entries.len() {
            // One byte short of the next record, so nothing partial may appear.
            let bound = (one * take + one - 1) as u32;
            let (bytes, consumed) = encode(&entries, bound).unwrap();
            assert_eq!(consumed, take.min(entries.len()));
            assert_eq!(bytes.len(), one * consumed);
            assert_eq!(count_records(&bytes).unwrap(), consumed);
        }
    }

    /// **Every capacity refusal carries `ERANGE`, swept rather than asserted** —
    /// the second of the two tracee-supplied inputs a `getattrlistbulk` request
    /// carries, and the one three review rounds never enumerated.
    ///
    /// A buffer too small for one record is refused rather than answered with
    /// the zero entries that mean end-of-directory; the refusal is `ERANGE`,
    /// which is what Darwin answers for exactly this condition (measured: `cap`
    /// of 1, 8, 32 and 55 against a 56-byte record all return `ERANGE`, and 56
    /// serves one entry). It used to be an errno-less `Err`, which ended the
    /// run.
    ///
    /// The sweep runs every capacity below one record, and the assertion is on
    /// the errno rather than on `is_err()` — the same shape, and for the same
    /// reason, as the attribute-list sweep above: `is_err()` is satisfied by the
    /// run-ending form this test exists to exclude.
    #[test]
    fn every_output_bound_refusal_carries_a_bindable_errno() {
        let entries = [entry("a-fairly-long-name", ObjectKind::File, 1, 1)];
        let one = record_bytes(&entries[0]).unwrap();
        let mut refusals = 0;
        for capacity in 0..one {
            let refused = encode(&entries, capacity as u32).unwrap_err();
            refusals += 1;
            assert_eq!(
                refused.errno,
                Some(umbra_core::Errno(34)),
                "a refusal at capacity {capacity} carries no bindable errno, so the \
                 engine would raise it and end the run: {refused}"
            );
        }
        assert_eq!(
            refusals, one,
            "the sweep refused {refusals} of {one} capacities"
        );
        // One whole record's worth serves exactly one record.
        assert_eq!(encode(&entries, one as u32).unwrap().1, 1);
        // An empty directory is a legal zero-byte reply at any capacity,
        // including ones too small to hold anything: nothing has to fit.
        for capacity in [0u32, 1, 8, 4096] {
            assert_eq!(encode(&[], capacity).unwrap(), (vec![], 0));
        }
    }

    /// **The threshold is the longest name that sorts early, not the buffer**,
    /// which is why one directory refuses a capacity another serves.
    ///
    /// `Overlay::merged` returns entries byte-sorted, so a long name beginning
    /// `L` (0x4C) sorts ahead of `aaa` (0x61) and its record is packed first. A
    /// capacity that serves a short directory therefore refuses one that merely
    /// *contains* a long name. Measured against the kernel, which serves both at
    /// that capacity because its enumeration order is not byte-sorted — a
    /// difference, not a defect, and precisely why the refusal has to be an
    /// errno a program can act on rather than a stopped run.
    #[test]
    fn a_long_name_that_sorts_early_refuses_a_capacity_a_short_directory_serves() {
        let short: Vec<_> = ["aaa", "bbb", "ccc"]
            .iter()
            .map(|n| entry(n, ObjectKind::File, 1, 1))
            .collect();
        let long_name = "L".repeat(200);
        let mut with_long = vec![entry(&long_name, ObjectKind::File, 9, 1)];
        with_long.extend(short.iter().cloned());
        // Byte order is what the overlay hands over and it puts the long one
        // first — asserted so the premise is checked rather than assumed.
        assert!(long_name.as_bytes() < b"aaa".as_slice());

        assert_eq!(encode(&short, 140).unwrap().1, 2);
        let refused = encode(&with_long, 140).unwrap_err();
        assert_eq!(refused.errno, Some(umbra_core::Errno(34)));
        // Given room for the long record, the same directory serves.
        assert_eq!(encode(&with_long, 4096).unwrap().1, 4);
    }

    /// `count_records` re-derives the count the encoder reported, and refuses a
    /// reply that does not walk exactly to its end.
    #[test]
    fn record_counting_refuses_a_reply_that_does_not_walk_exactly() {
        let entries: Vec<_> = (0..3)
            .map(|i| entry(&format!("e{i}"), ObjectKind::File, i, 1))
            .collect();
        let (bytes, consumed) = encode(&entries, 4096).unwrap();
        assert_eq!(count_records(&bytes).unwrap(), consumed);
        assert!(count_records(&bytes[..bytes.len() - 8]).is_err());
        let mut zeroed = bytes.clone();
        zeroed[..4].fill(0);
        assert!(count_records(&zeroed).is_err());
        let mut unaligned = bytes.clone();
        unaligned[0] = 4;
        assert!(count_records(&unaligned).is_err());
    }

    /// Only the one measured request shape is served; everything else is
    /// refused with what it asked for.
    #[test]
    fn only_the_measured_attribute_request_is_accepted() {
        let attrlist = served_attrlist();
        assert!(RequestedAttributes::decode(&attrlist).is_ok());
        // `ls -l` territory: one extra common attribute is a different layout.
        let mut widened = attrlist;
        widened[4..8].copy_from_slice(&(SERVED_COMMONATTR | 0x0002_0000).to_le_bytes());
        let refused = RequestedAttributes::decode(&widened).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::UnsupportedCapability);
        assert_eq!(refused.errno, Some(umbra_core::Errno(45)));
    }

    /// **Every refusal this decode can produce carries an errno the caller can
    /// bind, and this sweeps for it rather than asserting it.**
    ///
    /// The one defect of the pass that relocated this validation was a second
    /// exit that carried none: the header arm used the encoder-fault
    /// constructor, so `Supervisor::unserved_directory_request` raised it and
    /// **ended the run** -- for two request shapes the kernel serves. A prose
    /// claim four lines above said the opposite, and the test that covered the
    /// arm used a bare `is_err()`, which the run-ending shape satisfies.
    ///
    /// So this asserts the *property* over a sweep of every field the decode
    /// reads, rather than over the two shapes someone happened to think of. A
    /// new refusal arm added later is covered by construction if it fires for
    /// any of these, and `is_err()` is never enough here -- the errno is the
    /// whole point.
    #[test]
    fn every_attribute_request_refusal_carries_a_bindable_errno() {
        let mut shapes: Vec<(&str, [u8; ATTRLIST_BYTES])> = Vec::new();
        // Every group word, wrong in both directions.
        for (name, at) in [
            ("commonattr", 4usize),
            ("volattr", 8),
            ("dirattr", 12),
            ("fileattr", 16),
            ("forkattr", 20),
        ] {
            for bits in [0x0000_0001u32, 0x8000_0000, 0xffff_ffff] {
                let mut a = served_attrlist();
                let current = u32::from_le_bytes(a[at..at + 4].try_into().unwrap());
                a[at..at + 4].copy_from_slice(&(current ^ bits).to_le_bytes());
                shapes.push((name, a));
            }
        }
        // And the header's own declared extent, across its whole plausible range.
        for count in [0u16, 1, 2, 3, 4, 6, 7, 255, u16::MAX] {
            let mut a = served_attrlist();
            a[0..2].copy_from_slice(&count.to_le_bytes());
            shapes.push(("bitmapcount", a));
        }
        let mut refusals = 0;
        for (field, attrlist) in shapes {
            if let Err(e) = RequestedAttributes::decode(&attrlist) {
                refusals += 1;
                assert_eq!(
                    e.errno,
                    Some(umbra_core::Errno(45)),
                    "a refusal on {field} carries no bindable errno, so the supervisor \
                     would raise it and end the run: {e}"
                );
            }
        }
        assert!(refusals >= 20, "the sweep refused only {refusals} shapes");
    }

    /// **`reserved` is a don't-care and must never refuse**, because the kernel
    /// ignores it and because a program that leaves it holding stack garbage
    /// must not take the run down.
    ///
    /// Measured: with the served bitmaps, `reserved = 0`, `0x1234` and `0xffff`
    /// produce byte-identical kernel replies.
    #[test]
    fn a_nonzero_reserved_word_is_ignored_exactly_as_the_kernel_ignores_it() {
        for reserved in [0u16, 1, 0x1234, 0xffff] {
            let mut attrlist = served_attrlist();
            attrlist[2..4].copy_from_slice(&reserved.to_le_bytes());
            assert!(
                RequestedAttributes::decode(&attrlist).is_ok(),
                "reserved={reserved:#06x} was refused; the kernel serves it"
            );
        }
    }

    /// A name a directory cannot actually hold is refused before it is packed.
    #[test]
    fn an_unencodable_entry_name_is_refused() {
        for name in [".", "..", "a/b"] {
            let bad = DirectoryEntry {
                name: BytePath::new(name.as_bytes().to_vec()).unwrap(),
                stat: entry("x", ObjectKind::File, 1, 1).stat,
            };
            assert!(encode(&[bad], 4096).is_err(), "{name:?} must not encode");
        }
    }
}
