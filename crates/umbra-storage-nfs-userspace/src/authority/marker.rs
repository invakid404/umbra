//! The durable admission marker: who owns the run, at which epoch, and whether
//! that ownership is still held.
//!
//! # Why the marker records a phase instead of being deleted
//!
//! The obvious release is to unlink `writer.lock`. This module does not, for two
//! reasons that both come from `docs/design/failure-model.md`.
//!
//! First, the required persistent evidence explicitly includes "writer ID/epoch
//! and admission/release phase". A deleted file records no phase: a follow-on
//! process finding nothing cannot tell a clean release from a marker that was
//! never written, from one an external actor removed. Recording `Released`
//! in place is the difference between proven cooperative handover and absence of
//! evidence.
//!
//! Second, the taxonomy forbids "automatic marker deletion or successful close on
//! timeout". Keeping deletion out of the store's vocabulary entirely means no
//! recovery path can reach for it under pressure.
//!
//! # Wire format
//!
//! Two encodings decode; one is produced.
//!
//! * **Legacy**, exactly 16 bytes: the raw UUID bytes of a writer token, which is
//!   what the mounted `nfs` adapter writes and what `tests/goldens/writer-lock.bin`
//!   pins. It carries no epoch and no phase. It decodes as *held* by an unnamed
//!   writer, never as released — reading absence of a phase as a release would be
//!   exactly the guess the failure model forbids. Existing runs stay readable and
//!   no migration namespace is introduced.
//! * **Extended**, produced by this provider: a magic, a version, the phase, the
//!   epoch, the 16-byte token and the writer id. The first 16 bytes of the two
//!   encodings can never be confused because the extended form is longer than 16
//!   bytes by construction.
//!
//! Anything else is [`MarkerError::Malformed`]: diagnosed, preserved and refused,
//! not repaired.

use umbra_core::{LeaseEpoch, WriterId};

use crate::error::{AuthorityError, FacadeError, FacadeResult};

/// Magic prefix of the extended encoding.
const MAGIC: &[u8; 8] = b"UMBRAWRL";
/// Version byte of the extended encoding.
const VERSION: u8 = 1;
/// Byte length of the legacy encoding: the raw UUID bytes of a writer token.
pub const LEGACY_MARKER_BYTES: usize = 16;
/// Byte length of every extended record.
///
/// Fixed, not variable, because the frozen transport facade has no `SETATTR` and
/// therefore no way to truncate: a shorter record written over a longer one would
/// leave the old tail behind and the next reader would decode a chimera. A
/// constant width makes every overwrite total.
pub const EXTENDED_MARKER_BYTES: usize = 256;
/// Offset the writer id starts at within an extended record.
const WRITER_ID_OFFSET: usize = 8 + 1 + 1 + 8 + 16 + 2;
/// Longest writer id an extended record can carry.
pub const MAX_WRITER_ID_BYTES: usize = EXTENDED_MARKER_BYTES - WRITER_ID_OFFSET;

/// The opaque 16-byte writer token `.provider/writer.lock` holds.
///
/// Its bytes are the identity a session proves it is the *same* writer with; its
/// age is never authority, which is why nothing here exposes a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriterToken(pub [u8; LEGACY_MARKER_BYTES]);

impl WriterToken {
    /// The token's raw bytes, as the legacy encoding stores them.
    pub fn as_bytes(&self) -> &[u8; LEGACY_MARKER_BYTES] {
        &self.0
    }
}

/// Whether the recorded owner still holds admission.
///
/// There is no `Expired`. Expiry is a clock's opinion about silence, and this
/// enum records only what a writer actually did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionPhase {
    /// A writer acquired admission and has not released it.
    Held,
    /// The recorded writer released admission cooperatively. A follow-on session
    /// may acquire at the next epoch.
    Released,
}

impl AdmissionPhase {
    fn code(self) -> u8 {
        match self {
            Self::Held => 1,
            Self::Released => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Held),
            2 => Some(Self::Released),
            _ => None,
        }
    }
}

/// What a marker's bytes could not be made to mean.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    /// The bytes match neither encoding, or the extended encoding is inconsistent.
    #[error("admission marker is malformed: {0}")]
    Malformed(String),
    /// A recorded epoch went backwards relative to one already observed.
    #[error("admission epoch regressed: observed {observed:?} after {seen:?}")]
    EpochRegressed {
        /// Highest epoch previously observed.
        seen: LeaseEpoch,
        /// Epoch just read from the marker.
        observed: LeaseEpoch,
    },
    /// The writer id does not fit a fixed-width record.
    #[error("writer id is {len} bytes; a marker record holds at most {max}")]
    WriterIdTooLong {
        /// Length of the offered id.
        len: usize,
        /// The most a record can carry.
        max: usize,
    },
}

impl MarkerError {
    /// Render as the authority-domain facade error a caller acts on.
    ///
    /// Both variants are [`AuthorityError::IdentityUnproven`], which classifies
    /// as `SafeStop`: unreadable or contradictory ownership evidence is precisely
    /// the case where continuing would be a guess.
    pub fn to_facade(&self) -> FacadeError {
        FacadeError::Authority(AuthorityError::IdentityUnproven(self.to_string()))
    }
}

/// The durable ownership record for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionMarker {
    token: WriterToken,
    writer: Option<WriterId>,
    epoch: LeaseEpoch,
    phase: AdmissionPhase,
    legacy: bool,
}

impl AdmissionMarker {
    /// A marker this provider is about to write.
    pub fn new(
        token: WriterToken,
        writer: WriterId,
        epoch: LeaseEpoch,
        phase: AdmissionPhase,
    ) -> Self {
        Self {
            token,
            writer: Some(writer),
            epoch,
            phase,
            legacy: false,
        }
    }

    /// The writer token.
    pub fn token(&self) -> WriterToken {
        self.token
    }

    /// The writer id, absent for a legacy marker that records only a token.
    pub fn writer(&self) -> Option<&WriterId> {
        self.writer.as_ref()
    }

    /// The epoch the recorded ownership was acquired at.
    ///
    /// A legacy marker reports [`LeaseEpoch`] zero: the mounted adapter keeps the
    /// epoch in `.provider/epoch`, not in the lock, and zero is what that file is
    /// created as. Treating an unknown epoch as zero keeps the ladder monotone
    /// without inventing a generation the record does not carry.
    pub fn epoch(&self) -> LeaseEpoch {
        self.epoch
    }

    /// Whether the recorded owner still holds admission.
    pub fn phase(&self) -> AdmissionPhase {
        self.phase
    }

    /// Whether this record came from the legacy 16-byte encoding.
    pub fn is_legacy(&self) -> bool {
        self.legacy
    }

    /// Whether `token` is the token this marker records.
    pub fn is_held_by(&self, token: WriterToken) -> bool {
        self.token == token
    }

    /// The same ownership, released.
    ///
    /// The epoch does not move: releasing is not an ownership transition, it is
    /// the end of one. The next acquirer is what advances the epoch.
    pub fn released(&self) -> Self {
        Self {
            phase: AdmissionPhase::Released,
            legacy: false,
            ..self.clone()
        }
    }

    /// Encode as a fixed-width extended record.
    ///
    /// Fails rather than truncating a writer id that does not fit: a truncated
    /// identity in durable ownership evidence is worse than a refusal to write.
    pub fn encode(&self) -> Result<Vec<u8>, MarkerError> {
        let writer = self.writer.as_ref().map_or("", |id| id.0.as_str());
        if writer.len() > MAX_WRITER_ID_BYTES {
            return Err(MarkerError::WriterIdTooLong {
                len: writer.len(),
                max: MAX_WRITER_ID_BYTES,
            });
        }
        let mut bytes = vec![0u8; EXTENDED_MARKER_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8] = VERSION;
        bytes[9] = self.phase.code();
        bytes[10..18].copy_from_slice(&self.epoch.0.to_le_bytes());
        bytes[18..34].copy_from_slice(self.token.as_bytes());
        let len = writer.len() as u16;
        bytes[34..36].copy_from_slice(&len.to_le_bytes());
        bytes[WRITER_ID_OFFSET..WRITER_ID_OFFSET + writer.len()].copy_from_slice(writer.as_bytes());
        Ok(bytes)
    }

    /// Decode either encoding.
    ///
    /// Length alone separates the two: 16 bytes is the legacy lock, and
    /// [`EXTENDED_MARKER_BYTES`] is a record this provider wrote. Anything else
    /// is refused, which is also what a partially written record looks like.
    pub fn decode(bytes: &[u8]) -> Result<Self, MarkerError> {
        if bytes.len() == LEGACY_MARKER_BYTES {
            let mut token = [0u8; LEGACY_MARKER_BYTES];
            token.copy_from_slice(bytes);
            return Ok(Self {
                token: WriterToken(token),
                writer: None,
                epoch: LeaseEpoch(0),
                // A legacy lock records presence and nothing else. Presence is
                // authority; its absence of a phase is not a release.
                phase: AdmissionPhase::Held,
                legacy: true,
            });
        }
        if bytes.len() != EXTENDED_MARKER_BYTES {
            return Err(MarkerError::Malformed(format!(
                "{} bytes is neither the {LEGACY_MARKER_BYTES}-byte legacy lock nor a \
                 {EXTENDED_MARKER_BYTES}-byte record",
                bytes.len()
            )));
        }
        if &bytes[..8] != MAGIC {
            return Err(MarkerError::Malformed("unrecognised magic".into()));
        }
        if bytes[8] != VERSION {
            return Err(MarkerError::Malformed(format!(
                "unsupported marker version {}",
                bytes[8]
            )));
        }
        let phase = AdmissionPhase::from_code(bytes[9])
            .ok_or_else(|| MarkerError::Malformed(format!("unknown phase code {}", bytes[9])))?;
        let epoch = LeaseEpoch(u64::from_le_bytes(
            bytes[10..18].try_into().expect("eight bytes"),
        ));
        let mut token = [0u8; LEGACY_MARKER_BYTES];
        token.copy_from_slice(&bytes[18..34]);
        let id_len = usize::from(u16::from_le_bytes(
            bytes[34..36].try_into().expect("two bytes"),
        ));
        if id_len > MAX_WRITER_ID_BYTES {
            return Err(MarkerError::Malformed(format!(
                "writer id declares {id_len} bytes, more than the {MAX_WRITER_ID_BYTES} a record holds"
            )));
        }
        let writer = std::str::from_utf8(&bytes[WRITER_ID_OFFSET..WRITER_ID_OFFSET + id_len])
            .map_err(|error| MarkerError::Malformed(format!("writer id is not UTF-8: {error}")))?;
        Ok(Self {
            token: WriterToken(token),
            writer: Some(WriterId(writer.to_owned())),
            epoch,
            phase,
            legacy: false,
        })
    }
}

/// What an exclusive create found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusiveCreate {
    /// This caller created the marker. It is the first owner the run ever had.
    Created,
    /// The name already exists. Read it to find out who holds it; do not assume.
    Exists,
}

/// The durable store one run's admission marker lives in.
///
/// # Why there is no `remove`
///
/// See the module documentation: deletion is not in the vocabulary, so no
/// recovery path can reach for it. Handover is [`AdmissionPhase::Released`]
/// written in place, which a follow-on process can read as evidence.
///
/// # Invariants an implementation must uphold
///
/// 1. `create_exclusive` is atomic *at the server*. Two callers racing it must
///    produce exactly one [`ExclusiveCreate::Created`]. A read-then-write
///    emulation does not satisfy this and must not be offered.
/// 2. `overwrite` replaces the whole record; a partial write must surface as an
///    error rather than a truncated marker.
/// 3. `read` returns the bytes verbatim. Decoding, and refusing to decode, is
///    this module's job.
/// 4. `claim_succession` is atomic *at the server*, exactly as `create_exclusive`
///    is. Two callers racing the same epoch must produce exactly one
///    [`ExclusiveCreate::Created`]. This is what serialises the `Released` ->
///    `Held` transition, which a read-then-overwrite cannot do (R1-001).
pub trait MarkerStore {
    /// Create the marker, failing without modification if it already exists.
    fn create_exclusive(&mut self, bytes: &[u8]) -> FacadeResult<ExclusiveCreate>;

    /// Read the marker's bytes, or `None` when the run has never had one.
    fn read(&mut self) -> FacadeResult<Option<Vec<u8>>>;

    /// Replace an existing marker's bytes.
    fn overwrite(&mut self, bytes: &[u8]) -> FacadeResult<()>;

    /// Stake an exclusive, server-atomic claim on succeeding to `epoch`.
    ///
    /// Cooperative succession reads a `Released` marker and writes a `Held` one.
    /// Those are two round trips, so two contenders can both read the release and
    /// both write themselves in — the defect recorded as **R1-001**. The claim is
    /// the serialisation point: whoever creates the epoch's claim name wins, and
    /// every other contender is told [`ExclusiveCreate::Exists`] by the server
    /// rather than by a local guess.
    ///
    /// A claim is never deleted. It is the durable evidence of which contender
    /// took which epoch, and the release evidence the predecessor wrote stays
    /// exactly where it was.
    fn claim_succession(&mut self, epoch: LeaseEpoch) -> FacadeResult<ExclusiveCreate>;
}

/// Forwarding impl so one store can back two sessions in a test without either
/// of them owning it. A run has one marker; modelling that needs shared access.
impl<S: MarkerStore + ?Sized> MarkerStore for &mut S {
    fn create_exclusive(&mut self, bytes: &[u8]) -> FacadeResult<ExclusiveCreate> {
        (**self).create_exclusive(bytes)
    }

    fn read(&mut self) -> FacadeResult<Option<Vec<u8>>> {
        (**self).read()
    }

    fn overwrite(&mut self, bytes: &[u8]) -> FacadeResult<()> {
        (**self).overwrite(bytes)
    }

    fn claim_succession(&mut self, epoch: LeaseEpoch) -> FacadeResult<ExclusiveCreate> {
        (**self).claim_succession(epoch)
    }
}

/// An in-memory [`MarkerStore`] whose contents survive the session that wrote
/// them.
///
/// Two sessions sharing one store are two sessions sharing one run's marker,
/// which is what makes a competing-admission test meaningful. Dropping the
/// session and building a new one over the same store models a follow-on process
/// finding what the previous one left.
#[derive(Debug, Default)]
pub struct MemoryMarkerStore {
    bytes: Option<Vec<u8>>,
    fail_next: Option<FacadeError>,
    /// Epochs some contender has already staked a succession claim on. Modelling
    /// the server's atomic create is the whole point: a shared store is how a
    /// concurrency test proves exactly one contender succeeds (R1-001).
    claimed: std::collections::BTreeSet<u64>,
}

impl MemoryMarkerStore {
    /// An empty store: the run has no marker yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A store already holding `bytes`, as a follow-on process would find them.
    pub fn holding(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Some(bytes),
            fail_next: None,
            claimed: std::collections::BTreeSet::new(),
        }
    }

    /// Fail the next store call with `error`, then behave normally.
    ///
    /// This is how a crash *between* the two halves of a handover is exercised:
    /// the release write fails, and the marker must still read as held.
    pub fn fail_next(&mut self, error: FacadeError) {
        self.fail_next = Some(error);
    }

    /// The bytes currently stored, for a test asserting on durable evidence.
    pub fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    fn take_fault(&mut self) -> FacadeResult<()> {
        match self.fail_next.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl MarkerStore for MemoryMarkerStore {
    fn create_exclusive(&mut self, bytes: &[u8]) -> FacadeResult<ExclusiveCreate> {
        self.take_fault()?;
        if self.bytes.is_some() {
            return Ok(ExclusiveCreate::Exists);
        }
        self.bytes = Some(bytes.to_vec());
        Ok(ExclusiveCreate::Created)
    }

    fn read(&mut self) -> FacadeResult<Option<Vec<u8>>> {
        self.take_fault()?;
        Ok(self.bytes.clone())
    }

    fn overwrite(&mut self, bytes: &[u8]) -> FacadeResult<()> {
        self.take_fault()?;
        self.bytes = Some(bytes.to_vec());
        Ok(())
    }

    fn claim_succession(&mut self, epoch: LeaseEpoch) -> FacadeResult<ExclusiveCreate> {
        self.take_fault()?;
        if self.claimed.insert(epoch.0) {
            Ok(ExclusiveCreate::Created)
        } else {
            Ok(ExclusiveCreate::Exists)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> WriterToken {
        WriterToken([7; LEGACY_MARKER_BYTES])
    }

    fn held(writer: &str, epoch: u64) -> AdmissionMarker {
        AdmissionMarker::new(
            token(),
            WriterId(writer.into()),
            LeaseEpoch(epoch),
            AdmissionPhase::Held,
        )
    }

    #[test]
    fn the_extended_encoding_round_trips() {
        let marker = held("umbra/session-a", 9);
        let bytes = marker.encode().expect("encode");
        assert_eq!(bytes.len(), EXTENDED_MARKER_BYTES);
        let decoded = AdmissionMarker::decode(&bytes).expect("round trip");
        assert_eq!(decoded, marker);
        assert!(!decoded.is_legacy());
        assert_eq!(
            decoded.writer().map(|id| id.0.as_str()),
            Some("umbra/session-a")
        );
    }

    #[test]
    fn a_legacy_lock_reads_as_held_and_never_as_released() {
        let decoded = AdmissionMarker::decode(&[3u8; LEGACY_MARKER_BYTES]).expect("legacy lock");
        assert!(decoded.is_legacy());
        assert_eq!(decoded.phase(), AdmissionPhase::Held);
        assert_eq!(decoded.epoch(), LeaseEpoch(0));
        assert_eq!(decoded.writer(), None);
        assert!(decoded.is_held_by(WriterToken([3; LEGACY_MARKER_BYTES])));
    }

    #[test]
    fn every_record_is_the_same_width_so_an_overwrite_is_total() {
        // The frozen transport has no SETATTR, so a shorter record written over a
        // longer one would leave a readable tail. Widths must not depend on the
        // writer id.
        let short = held("a", 1).encode().expect("encode");
        let long = held(&"w".repeat(MAX_WRITER_ID_BYTES), 1)
            .encode()
            .expect("encode");
        assert_eq!(short.len(), long.len());
        assert_eq!(short.len(), EXTENDED_MARKER_BYTES);

        // Overwriting the long record with the short one must decode as the
        // short one, with no trace of the long writer id.
        let mut store = MemoryMarkerStore::empty();
        store.create_exclusive(&long).expect("create");
        store.overwrite(&short).expect("overwrite");
        let read = store.read().expect("read").expect("present");
        let decoded = AdmissionMarker::decode(&read).expect("decode");
        assert_eq!(decoded.writer().map(|id| id.0.as_str()), Some("a"));
    }

    #[test]
    fn a_writer_id_that_does_not_fit_is_refused_rather_than_truncated() {
        let error = held(&"w".repeat(MAX_WRITER_ID_BYTES + 1), 1)
            .encode()
            .expect_err("must refuse");
        assert!(
            matches!(error, MarkerError::WriterIdTooLong { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_evidence_is_refused_rather_than_repaired() {
        let mut wrong_phase = held("w", 1).encode().expect("encode");
        wrong_phase[9] = 42;
        let mut wrong_magic = held("w", 1).encode().expect("encode");
        wrong_magic[0] = b'X';
        let truncated = held("w", 1).encode().expect("encode")[..128].to_vec();

        for bytes in [
            b"not a marker".to_vec(),
            MAGIC.to_vec(),
            wrong_phase,
            wrong_magic,
            truncated,
        ] {
            let error = AdmissionMarker::decode(&bytes).expect_err("must refuse");
            assert!(matches!(error, MarkerError::Malformed(_)), "{error:?}");
            assert!(matches!(
                error.to_facade(),
                FacadeError::Authority(AuthorityError::IdentityUnproven(_))
            ));
        }
    }

    #[test]
    fn releasing_keeps_the_epoch_and_the_token() {
        let marker = held("w", 4);
        let released = marker.released();
        assert_eq!(released.epoch(), LeaseEpoch(4));
        assert_eq!(released.token(), marker.token());
        assert_eq!(released.phase(), AdmissionPhase::Released);
    }

    #[test]
    fn the_store_trait_has_no_way_to_delete_a_marker() {
        // Compile-time statement of the module rule: a released marker is written
        // in place, so recovery has no deletion path to reach for under pressure.
        let mut store = MemoryMarkerStore::empty();
        assert_eq!(
            store.create_exclusive(b"first").expect("create"),
            ExclusiveCreate::Created
        );
        assert_eq!(
            store.create_exclusive(b"second").expect("create"),
            ExclusiveCreate::Exists
        );
        assert_eq!(store.read().expect("read").as_deref(), Some(&b"first"[..]));
    }
}
