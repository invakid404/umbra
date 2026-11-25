//! Per-run managed cleanup protocol. Types alone do not acquire resources.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum encoded frame; reject its length before allocating payload storage.
pub const MAX_FRAME: usize = 1_048_576;
/// Combined parallel Start publication/ACK allowance in nanoseconds.
pub const ACK_NANOS: u64 = 250_000_000;

/// Length-prefixed JSON frame. Decoding never reads beyond the supplied bytes;
/// transport owners enforce the original ticket while assembling a frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, &'static str> {
    let payload = serde_json::to_vec(value).map_err(|_| "invalid frame payload")?;
    if payload.is_empty() || payload.len() > MAX_FRAME {
        return Err("invalid frame length");
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Reject oversized lengths, truncated payloads and trailing bytes before parsing.
pub fn decode_frame<T: serde::de::DeserializeOwned>(frame: &[u8]) -> Result<T, &'static str> {
    let header = frame.get(..4).ok_or("incomplete frame header")?;
    let length = u32::from_be_bytes(header.try_into().unwrap()) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("invalid frame length");
    }
    if frame.len() != 4 + length {
        return Err("incomplete or trailing frame bytes");
    }
    serde_json::from_slice(&frame[4..]).map_err(|_| "invalid frame payload")
}

/// A role with its own parent collection and notification responsibilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// External executable-library caller.
    Caller,
    /// Collects the guardian.
    Relay,
    /// Collects worker and cleanup owner.
    Guardian,
    /// Runs the supervised session, without owning the platform provider.
    Worker,
    /// Exclusively owns and collects the platform provider.
    CleanupOwner,
    /// Owns native program/debugger resources.
    Provider,
}

/// Birth-validated role identity, established before GO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    /// Runtime role.
    pub role: Role,
    /// Positive kernel process identifier.
    pub pid: u32,
    /// Kernel process start seconds and fractional microseconds.
    pub birth: (u64, u64),
}
impl Participant {
    /// Validate shape; the runtime must additionally validate against the kernel.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.pid == 0 || self.birth.0 == 0 || self.birth.1 >= 1_000_000 {
            return Err("invalid participant identity");
        }
        Ok(())
    }
}

/// Original cleanup path, never reset by progress or a later notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanupPath {
    /// Cooperative path: 26.35 + 4C seconds.
    Trusted,
    /// Notified failure: 30.6 + 8C seconds.
    Failed,
}
impl CleanupPath {
    /// Checked D4 allocation, with C counting simultaneous obligations only.
    pub fn allocation_nanos(self, capacity: u64) -> Result<u64, &'static str> {
        if capacity == 0 {
            return Err("zero cleanup capacity");
        }
        let (fixed, per_unit): (u64, u64) = match self {
            Self::Trusted => (26_350_000_000, 4_000_000_000),
            Self::Failed => (30_600_000_000, 8_000_000_000),
        };
        capacity
            .checked_mul(per_unit)
            .and_then(|n| n.checked_add(fixed))
            .ok_or("cleanup budget overflow")
    }
}

/// Boot-clock timestamps shared across processes, never serialized Rust Instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// Original Start boot-time timestamp in nanoseconds.
    pub started: u64,
    /// Absolute nonrenewable completion deadline.
    pub deadline: u64,
    /// Path whose allocation produced this ticket.
    pub path: CleanupPath,
}
impl Ticket {
    /// Calculate the absolute ticket before any stop operation or ACK.
    pub fn new(started: u64, path: CleanupPath, capacity: u64) -> Result<Self, &'static str> {
        let deadline = started
            .checked_add(path.allocation_nanos(capacity)?)
            .ok_or("absolute deadline overflow")?;
        Ok(Self {
            started,
            deadline,
            path,
        })
    }
    /// Validate an original ticket against the run's negotiated capacity.
    pub fn validate(&self, capacity: u64) -> Result<(), &'static str> {
        if *self != Self::new(self.started, self.path, capacity)? {
            return Err("invalid cleanup ticket");
        }
        Ok(())
    }
    /// Remaining original allocation; an expired ticket cannot renew itself.
    pub fn remaining(&self, now: u64) -> u64 {
        self.deadline.saturating_sub(now)
    }
}

/// Authenticated run identity for every managed protocol message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// Fresh nonce, never shared across runs.
    pub nonce: Uuid,
    /// Current nonzero authority generation.
    pub generation: u64,
}
impl RunIdentity {
    /// Reject uninitialized identities before any side effect.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.nonce.is_nil() || self.generation == 0 {
            return Err("invalid run identity");
        }
        Ok(())
    }
}

/// Complete first-trigger notification, forwarded without changing its origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// Run identity.
    pub run: RunIdentity,
    /// Original participant, not the forwarding peer.
    pub origin: Participant,
    /// Original absolute cleanup allocation.
    pub ticket: Ticket,
}

/// ACK proves the recipient committed a ticket no later than the original one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// Original complete Start, preventing cross-origin acknowledgement.
    pub start: Start,
    /// Participant that committed before replying.
    pub recipient: Participant,
    /// Committed minimum deadline.
    pub deadline: u64,
}

impl Ack {
    /// Validate against the exact publication and pre-GO recipient inventory.
    pub fn validate(&self, start: &Start, recipient: Participant) -> Result<(), &'static str> {
        recipient.validate()?;
        if &self.start != start
            || self.recipient != recipient
            || self.deadline > start.ticket.deadline
        {
            return Err("ACK identity or deadline mismatch");
        }
        Ok(())
    }
}

/// Caller state has no silence timeout before receiving a valid Start (D1).
#[derive(Debug)]
pub struct TicketState {
    run: RunIdentity,
    capacity: u64,
    ticket: Option<Ticket>,
    failed: bool,
}
impl TicketState {
    /// Construct per-run state without starting a session-age timer.
    pub fn new(run: RunIdentity, capacity: u64) -> Result<Self, &'static str> {
        run.validate()?;
        CleanupPath::Failed.allocation_nanos(capacity)?;
        Ok(Self {
            run,
            capacity,
            ticket: None,
            failed: false,
        })
    }
    /// Earliest committed ticket, or None if every caller route is isolated.
    pub fn ticket(&self) -> Option<Ticket> {
        self.ticket
    }
    /// Sticky protocol/publication failure; later collection cannot erase it.
    pub fn failed(&self) -> bool {
        self.failed
    }
    /// Retain a failed outcome independently of eventual collection.
    pub fn fail(&mut self) {
        self.failed = true;
    }
    /// Commit before returning the ACK. The caller validates the origin against
    /// its pre-GO inventory; forwarding cannot substitute a different identity.
    pub fn accept(
        &mut self,
        start: Start,
        expected_origin: Participant,
        recipient: Participant,
    ) -> Result<Ack, &'static str> {
        let validation = (|| {
            if start.run != self.run || start.origin != expected_origin {
                return Err("Start identity mismatch");
            }
            start.origin.validate()?;
            recipient.validate()?;
            start.ticket.validate(self.capacity)
        })();
        if let Err(error) = validation {
            self.fail();
            return Err(error);
        }
        if start.ticket.path == CleanupPath::Failed {
            self.fail();
        }
        if self
            .ticket
            .is_none_or(|current| start.ticket.deadline < current.deadline)
        {
            self.ticket = Some(start.ticket);
        }
        Ok(Ack {
            deadline: self.ticket.unwrap().deadline,
            start,
            recipient,
        })
    }
}

/// Registered executable worker dispatch; custom worker executables are not an adapter here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConfiguration {
    /// Nonce and generation allocated by the caller.
    pub run: RunIdentity,
    /// Simultaneous obligation limit, defaulted by the public entrypoint to 64.
    pub capacity: u64,
    /// Name registered by this executable before role dispatch.
    pub dispatch: String,
    /// Serializable staged launch inputs interpreted by that registered dispatch.
    pub payload: Vec<u8>,
}
impl LaunchConfiguration {
    /// Refuse unsupported/malformed configuration before acquiring resources.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.run.validate()?;
        CleanupPath::Failed.allocation_nanos(self.capacity)?;
        if self.dispatch.is_empty()
            || self.dispatch.len() > 128
            || self
                .dispatch
                .bytes()
                .any(|b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'-')
            || self.payload.len() > 65_536
        {
            return Err("invalid registered launch configuration");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run() -> RunIdentity {
        RunIdentity {
            nonce: Uuid::new_v4(),
            generation: 1,
        }
    }
    fn participant(role: Role) -> Participant {
        Participant {
            role,
            pid: 42,
            birth: (1, 0),
        }
    }

    #[test]
    fn cleanup_deadline_no_renewal() {
        let run = run();
        let origin = participant(Role::CleanupOwner);
        let recipient = participant(Role::Caller);
        let mut state = TicketState::new(run, 4).unwrap();
        assert_eq!(
            state.ticket(),
            None,
            "D1 cannot invent an unnotified deadline"
        );
        let first = Start {
            run,
            origin,
            ticket: Ticket::new(100, CleanupPath::Failed, 4).unwrap(),
        };
        let ack = state.accept(first.clone(), origin, recipient).unwrap();
        assert_eq!(state.ticket().unwrap().deadline, ack.deadline);
        let later = Start {
            ticket: Ticket::new(1_000, CleanupPath::Failed, 4).unwrap(),
            ..first.clone()
        };
        assert_eq!(
            state.accept(later, origin, recipient).unwrap().deadline,
            ack.deadline
        );
        let earlier = Start {
            ticket: Ticket::new(1, CleanupPath::Trusted, 4).unwrap(),
            ..first
        };
        let reduced = state.accept(earlier, origin, recipient).unwrap().deadline;
        assert!(reduced < ack.deadline);
        assert!(
            state.failed(),
            "eventual cooperative cleanup cannot clear failure"
        );
        assert_eq!(state.ticket().unwrap().remaining(u64::MAX), 0);
    }

    #[test]
    fn invalid_start_never_acks_or_publishes_a_ticket() {
        let run = run();
        let owner = participant(Role::CleanupOwner);
        let recipient = participant(Role::Caller);
        let mut state = TicketState::new(run, 64).unwrap();
        let mut start = Start {
            run,
            origin: owner,
            ticket: Ticket::new(20, CleanupPath::Failed, 64).unwrap(),
        };
        start.run.generation += 1;
        assert!(state.accept(start.clone(), owner, recipient).is_err());
        assert_eq!(state.ticket(), None);
        start.run = run;
        start.ticket.deadline += 1;
        assert!(state.accept(start, owner, recipient).is_err());
        assert_eq!(state.ticket(), None);
        assert!(state.failed());
    }

    #[test]
    fn r2_budget_overflow_is_refused_before_start() {
        assert!(TicketState::new(run(), u64::MAX).is_err());
        assert!(Ticket::new(u64::MAX, CleanupPath::Trusted, 1).is_err());
        assert_eq!(
            CleanupPath::Trusted.allocation_nanos(64).unwrap(),
            282_350_000_000
        );
        assert_eq!(
            CleanupPath::Failed.allocation_nanos(64).unwrap(),
            542_600_000_000
        );
    }

    #[test]
    fn bounded_frames_reject_truncation_overflow_and_trailing_data() {
        let config = LaunchConfiguration {
            run: run(),
            capacity: 64,
            dispatch: "session".into(),
            payload: vec![1, 2, 3],
        };
        let frame = encode_frame(&config).unwrap();
        assert_eq!(decode_frame::<LaunchConfiguration>(&frame).unwrap(), config);
        for end in 0..frame.len() {
            assert!(decode_frame::<LaunchConfiguration>(&frame[..end]).is_err());
        }
        let mut trailing = frame;
        trailing.push(0);
        assert!(decode_frame::<LaunchConfiguration>(&trailing).is_err());
        assert!(decode_frame::<LaunchConfiguration>(&u32::MAX.to_be_bytes()).is_err());
        assert!(decode_frame::<LaunchConfiguration>(&0u32.to_be_bytes()).is_err());
    }

    #[test]
    fn ack_cannot_substitute_recipient_origin_or_extend_deadline() {
        let run = run();
        let origin = participant(Role::CleanupOwner);
        let recipient = participant(Role::Caller);
        let start = Start {
            run,
            origin,
            ticket: Ticket::new(10, CleanupPath::Trusted, 64).unwrap(),
        };
        let mut state = TicketState::new(run, 64).unwrap();
        let ack = state.accept(start.clone(), origin, recipient).unwrap();
        ack.validate(&start, recipient).unwrap();
        assert!(ack.validate(&start, participant(Role::Relay)).is_err());
        let mut changed = ack.clone();
        changed.start.origin.role = Role::Worker;
        assert!(changed.validate(&start, recipient).is_err());
        changed = ack;
        changed.deadline += 1;
        assert!(changed.validate(&start, recipient).is_err());
    }

    #[test]
    fn admission_wire_replay_preserves_charges_and_rejects_conflicts() {
        use crate::admission::{AdmissionLedger, AdmissionOperation, AdmissionRequest};
        let run = run();
        let request = AdmissionRequest {
            nonce: run.nonce,
            generation: run.generation,
            sequence: 1,
            operation: AdmissionOperation::Reserve {
                obligation: Uuid::new_v4(),
            },
        };
        let wire = encode_frame(&request).unwrap();
        let mut ledger = AdmissionLedger::new(run.nonce, run.generation, 1).unwrap();
        assert_eq!(ledger.apply(decode_frame(&wire).unwrap()), Ok(1));
        assert_eq!(ledger.apply(decode_frame(&wire).unwrap()), Ok(1));
        let conflict = AdmissionRequest {
            operation: AdmissionOperation::Freeze,
            ..request
        };
        assert!(ledger.apply(conflict).is_err());
        assert!(ledger.failed());
        assert_eq!(ledger.current(), 1);
    }
}
