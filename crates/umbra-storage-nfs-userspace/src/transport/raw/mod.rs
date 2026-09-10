//! `LibnfsRawTransport`: the raw-RPC implementation of [`RawTransport`].
//!
//! This module and its children are the only place in the crate where `unsafe`
//! is enabled, and the only place that knows libnfs exists. Protocol state —
//! client ids, seqids, open owners, renewal, grace, reclaim — is deliberately
//! absent: stateids and open-owner bytes travel through here as opaque owned
//! values inside [`Nfs4Op`], and nothing in this module reads, advances or
//! stores them.
//!
//! # The two spike hazards, and where each is closed
//!
//! **Borrow lifetimes (`pump.rs:13–14` in the spike).** Nothing here borrows
//! from libnfs. [`decode::compound_reply`] copies every reply byte into owned
//! Rust values *inside* the completion callback, while libnfs's buffer is still
//! valid, and the frozen [`FileHandle`](crate::handle::FileHandle) owns its
//! bytes. There is no pointer whose validity a reader has to establish from
//! scope order.
//!
//! **Stack `CallState` handed to C (`raw4.rs:646–687` in the spike).** libnfs
//! receives a `u64` call id, never the address of a Rust value
//! ([`pump::CallSlot`], [`pump`] module docs). Argument memory lives in a
//! heap [`args::CallArena`] owned by the slot for the whole dispatch, and the
//! PDU pointer is reachable exactly once through [`pump::Dispatch`], so a
//! deadline cancels and drains rather than leaking a pointer libnfs can still
//! write through.

// The single `unsafe` opt-in for the whole crate.
//
// `lib.rs` sets `#![deny(unsafe_code)]`; this is the one module the contracts
// permit to lift it, and the lint level covers the child modules below. Every
// `unsafe` block under it carries a `SAFETY:` comment naming the invariant it
// relies on, and the module-level docs above state where each of the two spike
// hazards is closed.
#![allow(unsafe_code)]

mod args;
mod decode;
mod pump;
mod sys;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use crate::error::TransportError;
use crate::transport::{
    CallToken, Compound, CompoundReply, ConnectionEpoch, ConnectionState, Deadline, FaultAction,
    FaultContext, FaultPlan, FaultPoint, NoFaults, OpCode, OpReply, ProtocolError, RawTransport,
    Retirement, TransportLimits, TransportResult, WireProfile,
};

use pump::{Completion, EventPump, ServiceOutcome};

/// Where a [`LibnfsRawTransport`] connects and the bounds it will enforce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTransportConfig {
    /// Server address. Resolved by libnfs; M1 fixtures use `127.0.0.1`.
    pub host: String,
    /// TCP port. There is no portmapper query in M1.
    pub port: u16,
    /// Wire profile. Anything but [`WireProfile::V40_TCP_SYS`] is refused
    /// before a socket is opened.
    pub profile: WireProfile,
    /// Bounds this context advertises and enforces.
    pub limits: TransportLimits,
}

impl RawTransportConfig {
    /// A configuration for a loopback fixture with conservative bounds.
    pub fn loopback(port: u16) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port,
            profile: WireProfile::V40_TCP_SYS,
            limits: TransportLimits {
                max_inflight: 16,
                max_queue_depth: 64,
                max_reply_bytes: 1 << 20,
                default_deadline: Deadline { millis: 10_000 },
            },
        }
    }
}

/// A raw-RPC NFSv4.0 transport over one libnfs context.
///
/// One instance owns exactly one event pump. `submit` is the only thing that
/// drives it, and it takes `&mut self`, so there is no second pump and no
/// concurrent service of the same context.
pub struct LibnfsRawTransport {
    pump: EventPump,
    profile: WireProfile,
    limits: TransportLimits,
    faults: Box<dyn FaultPlan>,
    /// Calls registered with the pump but not yet retired.
    ///
    /// Keyed by the same integer the public [`CallToken`] carries. A slot lives
    /// here only between dispatch and retirement, and dropping it is what
    /// releases the argument arena.
    inflight: BTreeMap<u64, pump::CallSlot>,
}

impl std::fmt::Debug for LibnfsRawTransport {
    /// Reports the observable state only. No pointer is printed: a raw address
    /// in a log is noise at best and a hint about process layout at worst.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibnfsRawTransport")
            .field("profile", &self.profile)
            .field("limits", &self.limits)
            .field("connection", &self.pump.state())
            .field("inflight", &self.inflight.len())
            .finish()
    }
}

impl LibnfsRawTransport {
    /// Validate the profile, then connect.
    ///
    /// The profile is checked **before** any socket work, so a v4.1 or non-TCP
    /// request is refused without touching the network.
    pub fn connect(config: RawTransportConfig) -> TransportResult<Self> {
        config.profile.check()?;

        let mut pump = EventPump::new(&config.host, config.port)?;
        pump.connect(config.limits.default_deadline.millis)?;

        Ok(Self {
            pump,
            profile: config.profile,
            limits: config.limits,
            faults: Box::new(NoFaults),
            inflight: BTreeMap::new(),
        })
    }

    /// The operation a fault plan is deciding about.
    ///
    /// Filehandle plumbing (`PUTFH`, `PUTROOTFH`, `GETFH`) is not what a
    /// failure-model scenario targets, so the last operation that is not
    /// plumbing is reported. That makes `PUTFH; OPEN; GETFH` decide about
    /// `Open` at index 1.
    fn subject(call: &Compound) -> (OpCode, u32) {
        let plumbing = |code: OpCode| {
            matches!(
                code,
                OpCode::PutFh | OpCode::PutRootFh | OpCode::GetFh | OpCode::SaveFh
            )
        };
        call.ops
            .iter()
            .enumerate()
            .rev()
            .find(|(_, op)| !plumbing(op.opcode()))
            .or_else(|| call.ops.iter().enumerate().next_back())
            .map(|(index, op)| (op.opcode(), index as u32))
            .unwrap_or((OpCode::PutRootFh, 0))
    }

    /// Ask the installed plan what should happen at `point`.
    fn consult(
        &mut self,
        point: FaultPoint,
        op: OpCode,
        index: u32,
        token: Option<CallToken>,
    ) -> FaultAction {
        self.faults.decide(point, FaultContext { op, index, token })
    }

    /// A reply that carries only a recorded failure.
    ///
    /// `CompoundReply::expect` returns the failure in preference to any
    /// positional result, so a substituted status reaches every caller as a
    /// protocol error rather than as a missing result.
    fn substituted(call: &Compound, status: crate::error::Nfs4Status) -> CompoundReply {
        let (op, index) = Self::subject(call);
        CompoundReply {
            tag: call.tag.clone(),
            results: Vec::new(),
            failure: Some(ProtocolError { status, op, index }),
        }
    }

    /// Apply the reply-shaping fault actions.
    fn reshape(action: &FaultAction, call: &Compound, reply: &mut CompoundReply) {
        match action {
            FaultAction::Substitute(status) => {
                let (op, index) = Self::subject(call);
                reply.failure = Some(ProtocolError {
                    status: *status,
                    op,
                    index,
                });
            }
            FaultAction::ShortWrite(count) => {
                for result in &mut reply.results {
                    if let OpReply::Write(write) = result {
                        // A short count is a real answer, so it is recorded as
                        // the server's, not turned into an error.
                        write.count = write.count.min(*count);
                    }
                }
            }
            FaultAction::RotateVerifier(verifier) => {
                for result in &mut reply.results {
                    match result {
                        OpReply::Write(write) => write.verifier = *verifier,
                        OpReply::Commit(commit) => commit.verifier = *verifier,
                        _ => {}
                    }
                }
            }
            FaultAction::Proceed | FaultAction::Fail(_) | FaultAction::DropReply => {}
        }
    }

    /// Fill in a zero-copy READ's bytes from the arena that received them.
    ///
    /// `rpc_nfs4_read_task` writes reply data straight into the destination
    /// buffer the arena owns and leaves the XDR pointer null, so the bytes are
    /// only reachable here, while the slot still holds the arena.
    fn complete_zero_copy_read(
        &mut self,
        id: u64,
        decoded: decode::DecodedReply,
    ) -> TransportResult<CompoundReply> {
        let decode::DecodedReply {
            mut reply,
            zero_copy_read,
        } = decoded;

        let Some((index, count)) = zero_copy_read else {
            return Ok(reply);
        };

        let slot = self.inflight.get(&id).ok_or_else(|| {
            TransportError::Malformed(
                "a zero-copy READ completed for a call that is no longer registered".into(),
            )
        })?;
        let bytes = slot.arena.read_bytes(count).ok_or_else(|| {
            TransportError::Malformed(format!(
                "READ declared {count} bytes, more than the destination buffer holds"
            ))
        })?;

        match reply.results.get_mut(index) {
            Some(OpReply::Read(read)) => read.data = bytes.to_vec(),
            _ => {
                return Err(TransportError::Malformed(
                    "a zero-copy READ completed against a result that is not a READ".into(),
                ))
            }
        }
        Ok(reply)
    }

    /// Retire a slot and return the proof, never leaving a registration behind.
    fn retire(&mut self, token: CallToken) -> Retirement {
        match self.cancel(token) {
            Ok(retirement) => retirement,
            // `cancel` is infallible in this implementation; if that ever
            // changes, a call must still not be reported without a retirement.
            Err(_) => Retirement::new(token, false),
        }
    }
}

impl RawTransport for LibnfsRawTransport {
    fn wire_profile(&self) -> WireProfile {
        self.profile
    }

    fn limits(&self) -> TransportLimits {
        self.limits
    }

    fn connection(&self) -> ConnectionState {
        self.pump.state()
    }

    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply> {
        // Invariant 6: the profile is checked and never renegotiated.
        self.profile.check()?;

        let (op, index) = Self::subject(&call);
        let deadline_millis = if deadline.millis == 0 {
            self.limits.default_deadline.millis
        } else {
            deadline.millis
        };

        // --- BeforeDispatch: nothing is registered yet ----------------------
        let mut discard_reply = false;
        match self.consult(FaultPoint::BeforeDispatch, op, index, None) {
            FaultAction::Proceed => {}
            FaultAction::Fail(error) => return Err(error),
            FaultAction::Substitute(status) => return Ok(Self::substituted(&call, status)),
            FaultAction::DropReply => discard_reply = true,
            FaultAction::ShortWrite(_) | FaultAction::RotateVerifier(_) => {}
        }

        // --- Invariant 3: backpressure before dispatch ----------------------
        let inflight = self.inflight.len() as u32;
        if inflight >= self.limits.max_inflight {
            return Err(TransportError::QueueFull {
                depth: inflight,
                capacity: self.limits.max_inflight,
            });
        }
        let queued = self.pump.queue_length();
        if queued >= self.limits.max_queue_depth {
            return Err(TransportError::QueueFull {
                depth: queued,
                capacity: self.limits.max_queue_depth,
            });
        }

        // Marshalling can refuse a COMPOUND shape; refusing here means no call
        // was ever registered, so there is nothing to retire.
        let arena = args::CallArena::build(&call, &self.limits)?;
        let slot = self.pump.dispatch(arena, self.limits.max_reply_bytes)?;
        let id = slot.id;
        let token = CallToken::new(NonZeroU64::new(id).expect("call ids start at one"));
        self.inflight.insert(id, slot);

        // --- AfterDispatch: the request is on the wire ----------------------
        match self.consult(FaultPoint::AfterDispatch, op, index, Some(token)) {
            FaultAction::Proceed => {}
            FaultAction::Fail(error) => {
                self.retire(token);
                return Err(error);
            }
            FaultAction::Substitute(status) => {
                self.retire(token);
                return Ok(Self::substituted(&call, status));
            }
            FaultAction::DropReply => discard_reply = true,
            FaultAction::ShortWrite(_) | FaultAction::RotateVerifier(_) => {}
        }

        if discard_reply {
            if let Some(slot) = self.inflight.get_mut(&id) {
                slot.drop_reply = true;
            }
        }

        let outcome = self.pump.service_until(id, deadline_millis, discard_reply);

        // Whatever happened, the PDU is no longer owed a cancellation once a
        // completion has been seen. R1-009: `service_until` now reconciles the
        // registry on every return, including the connection-failure paths, so a
        // completion delivered by `rpc_reconnect_requeue` on its way out reaches
        // this arm as `Completed` rather than being lost behind `Disconnected`.
        if matches!(
            outcome,
            ServiceOutcome::Completed | ServiceOutcome::Discarded
        ) {
            if let Some(slot) = self.inflight.get_mut(&id) {
                slot.dispatch.mark_completed();
            }
        }

        match outcome {
            ServiceOutcome::Completed => {
                let completion = pump::take_completion(id);
                let mut reply = match completion {
                    Some(Completion::Reply(decoded)) => {
                        // R1-010: `?` here used to return before `retire`, so a
                        // malformed or over-budget reply left its registration,
                        // slot and arena in `inflight` forever. With the loopback
                        // default of 16 concurrent calls, repeated decode failures
                        // accumulated retained slots until every later submission
                        // was refused `QueueFull`. The transport facade's
                        // all-path retirement invariant admits no exception for a
                        // reply that failed to decode.
                        //
                        // The zero-copy READ is completed from the arena first,
                        // because retiring drops the arena it reads from.
                        let outcome = (*decoded)
                            .and_then(|decoded| self.complete_zero_copy_read(id, decoded));
                        match outcome {
                            Ok(reply) => {
                                self.retire(token);
                                reply
                            }
                            Err(error) => {
                                // Retire first, then report the original error
                                // unchanged.
                                self.retire(token);
                                return Err(error);
                            }
                        }
                    }
                    Some(Completion::Failed(detail)) => {
                        self.retire(token);
                        return Err(TransportError::Disconnected {
                            epoch: self.pump.epoch(),
                            detail,
                        });
                    }
                    Some(Completion::Cancelled) => {
                        self.retire(token);
                        return Err(TransportError::Cancelled(token));
                    }
                    Some(Completion::Connected) | None => {
                        self.retire(token);
                        return Err(TransportError::Malformed(
                            "the pump reported a completion the call never made".into(),
                        ));
                    }
                };

                // --- BeforeReturn: the reply is decoded and owned -----------
                let action = self.consult(FaultPoint::BeforeReturn, op, index, Some(token));
                match action {
                    FaultAction::Fail(error) => Err(error),
                    FaultAction::DropReply => {
                        // The reply is thrown away, so the caller must see the
                        // same shape as a lost reply: a deadline, with proof
                        // the registration was withdrawn first.
                        let retirement = self.retire(token);
                        Err(TransportError::DeadlineExpired { retirement })
                    }
                    other => {
                        Self::reshape(&other, &call, &mut reply);
                        Ok(reply)
                    }
                }
            }

            ServiceOutcome::Discarded | ServiceOutcome::TimedOut => {
                // --- OnDeadline --------------------------------------------
                let action = self.consult(FaultPoint::OnDeadline, op, index, Some(token));
                // Invariant 4: retire before reporting, always.
                let retirement = self.retire(token);
                match action {
                    FaultAction::Fail(error) => Err(error),
                    FaultAction::Substitute(status) => Ok(Self::substituted(&call, status)),
                    _ => Err(TransportError::DeadlineExpired { retirement }),
                }
            }

            ServiceOutcome::Disconnected => {
                // --- OnConnection ------------------------------------------
                let action = self.consult(FaultPoint::OnConnection, op, index, Some(token));
                self.retire(token);
                let epoch = self.pump.epoch();
                match action {
                    FaultAction::Fail(error) => Err(error),
                    FaultAction::Substitute(status) => Ok(Self::substituted(&call, status)),
                    _ => Err(TransportError::Disconnected {
                        epoch,
                        detail: self.pump.error(),
                    }),
                }
            }
        }
    }

    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement> {
        let Some(mut slot) = self.inflight.remove(&token.get()) else {
            // Unknown or already retired: nothing is registered, so the pump is
            // idle with respect to this call by definition.
            return Ok(Retirement::new(token, true));
        };
        let drained = self.pump.retire(&mut slot);
        // Dropping the slot here is what releases the argument arena, and it
        // happens only after libnfs has been proven to have let the PDU go.
        drop(slot);
        Ok(Retirement::new(token, drained))
    }

    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        match self.consult(FaultPoint::OnConnection, OpCode::Renew, 0, None) {
            FaultAction::Fail(error) => return Err(error),
            FaultAction::Proceed
            | FaultAction::Substitute(_)
            | FaultAction::ShortWrite(_)
            | FaultAction::RotateVerifier(_)
            | FaultAction::DropReply => {}
        }

        // Every outstanding call is withdrawn before the socket goes, so no
        // arena is released while libnfs could still reach it.
        let outstanding: Vec<u64> = self.inflight.keys().copied().collect();
        for id in outstanding {
            if let Some(token) = NonZeroU64::new(id).map(CallToken::new) {
                let _ = self.cancel(token);
            }
        }

        self.pump.disconnect();
        self.pump.connect(self.limits.default_deadline.millis)
    }

    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.faults = plan;
    }
}

impl Drop for LibnfsRawTransport {
    fn drop(&mut self) {
        // Retire every registration before the pump is destroyed, so a
        // teardown callback cannot find a live id.
        let outstanding: Vec<u64> = self.inflight.keys().copied().collect();
        for id in outstanding {
            if let Some(token) = NonZeroU64::new(id).map(CallToken::new) {
                let _ = self.cancel(token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{AttrMask, ComponentName, Nfs4Op};

    fn compound(ops: Vec<Nfs4Op>) -> Compound {
        Compound::new(*b"test", ops)
    }

    #[test]
    fn a_non_v4_0_profile_is_refused_before_any_socket_work() {
        let mut config = RawTransportConfig::loopback(1);
        config.profile = WireProfile {
            minor_version: 1,
            ..WireProfile::V40_TCP_SYS
        };
        // Port 1 is never listening; reaching a connection error would prove
        // the profile was not checked first.
        let error = LibnfsRawTransport::connect(config).unwrap_err();
        assert!(matches!(error, TransportError::UnsupportedProfile(_)));
    }

    #[test]
    fn the_fault_subject_is_the_operation_not_the_filehandle_plumbing() {
        let call = compound(vec![
            Nfs4Op::PutFh(crate::handle::FileHandle::from_wire(vec![1, 2, 3]).unwrap()),
            Nfs4Op::Lookup(ComponentName::new(b"name".to_vec()).unwrap()),
            Nfs4Op::GetFh,
            Nfs4Op::GetAttr(AttrMask::STAT),
        ]);
        // GETATTR is last and is not plumbing.
        assert_eq!(LibnfsRawTransport::subject(&call), (OpCode::GetAttr, 3),);

        let open = compound(vec![
            Nfs4Op::PutFh(crate::handle::FileHandle::from_wire(vec![1, 2, 3]).unwrap()),
            Nfs4Op::GetFh,
        ]);
        // Only plumbing: the last operation is reported rather than nothing.
        assert_eq!(LibnfsRawTransport::subject(&open), (OpCode::GetFh, 1));
    }

    #[test]
    fn a_substituted_status_is_returned_by_expect_over_any_result() {
        let call = compound(vec![Nfs4Op::Renew(crate::handle::ClientId(7))]);
        let reply = LibnfsRawTransport::substituted(&call, crate::error::Nfs4Status::GRACE);
        assert_eq!(
            reply.expect(0).unwrap_err().status(),
            Some(crate::error::Nfs4Status::GRACE)
        );
    }

    #[test]
    fn a_short_write_action_lowers_the_count_and_never_raises_it() {
        let call = compound(vec![Nfs4Op::Commit {
            offset: 0,
            count: 0,
        }]);
        let mut reply = CompoundReply {
            tag: b"test".to_vec(),
            results: vec![OpReply::Write(crate::transport::WriteReply {
                count: 10,
                committed: crate::transport::Stability::FileSync,
                verifier: crate::transport::WriteVerifier([0; 8]),
            })],
            failure: None,
        };
        LibnfsRawTransport::reshape(&FaultAction::ShortWrite(4), &call, &mut reply);
        match &reply.results[0] {
            OpReply::Write(write) => assert_eq!(write.count, 4),
            other => panic!("unexpected {other:?}"),
        }
        // A cap above the real count cannot invent bytes the server never took.
        LibnfsRawTransport::reshape(&FaultAction::ShortWrite(99), &call, &mut reply);
        match &reply.results[0] {
            OpReply::Write(write) => assert_eq!(write.count, 4),
            other => panic!("unexpected {other:?}"),
        }
    }
}
