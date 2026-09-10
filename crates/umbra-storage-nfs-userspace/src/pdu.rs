//! Who owns an outstanding PDU, and therefore who may free it.
//!
//! # Why this is its own module
//!
//! The raw transport lives behind the `transport-raw` feature, so its logic is
//! only compiled when a native libnfs toolchain is present. The rule this module
//! encodes is not about libnfs at all — it is bookkeeping about which side of the
//! FFI boundary currently owns a pointer — so it lives here, is always compiled,
//! and is tested in the default build. `transport::raw::pump` applies it to a
//! `*mut rpc_pdu`.
//!
//! # The rule (R1-009)
//!
//! `rpc_cancel_pdu` dereferences the PDU it is handed (`lib/pdu.c` reads
//! `pdu->xid` before looking it up), so cancelling a PDU libnfs has already freed
//! is undefined behaviour, not a no-op.
//!
//! libnfs frees outstanding PDUs in two situations the wrapper must account for:
//!
//! 1. **Per call.** A completion callback ran for this call, after which
//!    `rpc_free_pdu` is called on it.
//! 2. **Per connection.** A socket error makes `rpc_service` return a negative
//!    value. Before it does, `rpc_reconnect_requeue` errors *every* outstanding
//!    PDU — with `auto_reconnect` off it calls each completion and frees each PDU
//!    (`lib/init.c`). So one failed `rpc_service` invalidates every PDU pointer
//!    the wrapper is holding, not only the one it was waiting for.
//!
//! The second case is what the review found unhandled: the wrapper returned
//! `Disconnected` while leaving its slot in `Queued`, and retirement then handed
//! the freed pointer to `rpc_cancel_pdu`.
//!
//! A generation counter closes it. Every dispatch records the generation it was
//! queued in; a connection-wide disposal bumps the generation. A slot whose
//! generation is stale is one libnfs has already freed, and its pointer is never
//! handed back.

/// Monotonic count of connection-wide PDU disposals.
///
/// Starts at zero and only ever increases, so a wrapped comparison cannot make a
/// stale slot look current.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DisposalGeneration(u64);

impl DisposalGeneration {
    /// The generation a freshly connected context starts in.
    pub const START: Self = Self(0);

    /// Record that libnfs has disposed of every PDU outstanding on the context.
    ///
    /// Saturating rather than wrapping: at `u64::MAX` the counter stops moving,
    /// which leaves every existing slot looking *current*. That is the direction
    /// that fails safe here only if paired with `Ownership::Libnfs`, so the
    /// counter is documented as unreachable in practice — one increment per
    /// connection loss — rather than relied on.
    #[must_use]
    pub fn disposed(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The raw count, for diagnostics.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Who owns a dispatched call's PDU right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// libnfs still holds it and it may be cancelled exactly once.
    Cancellable,
    /// libnfs has already freed it. The pointer must never be handed back.
    Freed,
}

/// Decide whether a slot's PDU may still be cancelled.
///
/// `queued_in` is the generation the call was dispatched in and `current` is the
/// context's generation now. `completion_seen` is whether a completion for this
/// call has been observed, which means its callback already ran.
#[must_use]
pub fn ownership(
    queued_in: DisposalGeneration,
    current: DisposalGeneration,
    completion_seen: bool,
) -> Ownership {
    if completion_seen {
        // The callback ran, so libnfs is done with this PDU and will free it.
        return Ownership::Freed;
    }
    if queued_in != current {
        // A connection-wide disposal happened after this call was queued, so
        // libnfs errored and freed it along with everything else outstanding.
        return Ownership::Freed;
    }
    Ownership::Cancellable
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **R1-009.** The exact trace from the review: a call is queued, the socket
    /// errors, libnfs frees every outstanding PDU, and `rpc_service` returns -1.
    /// The wrapper must not then cancel that call's PDU.
    #[test]
    fn r1_009_a_connection_wide_disposal_makes_every_queued_pdu_unfreeable() {
        let queued_in = DisposalGeneration::START;
        // Still connected: the call is cancellable, which is what a deadline path
        // legitimately does.
        assert_eq!(
            ownership(queued_in, DisposalGeneration::START, false),
            Ownership::Cancellable
        );

        // The socket errors. libnfs called every completion and freed every PDU.
        let after = DisposalGeneration::START.disposed();
        assert_eq!(
            ownership(queued_in, after, false),
            Ownership::Freed,
            "a PDU outstanding across a connection loss has already been freed"
        );
    }

    /// **R1-009.** A completed call's PDU is never cancellable, connection loss or
    /// not. This is the per-call half of the rule.
    #[test]
    fn r1_009_an_observed_completion_retires_the_pdu_pointer() {
        assert_eq!(
            ownership(DisposalGeneration::START, DisposalGeneration::START, true),
            Ownership::Freed
        );
        assert_eq!(
            ownership(
                DisposalGeneration::START,
                DisposalGeneration::START.disposed(),
                true
            ),
            Ownership::Freed
        );
    }

    /// **R1-009.** Every call outstanding across one disposal is covered, not just
    /// the one the pump happened to be waiting on. The review's trace only needs
    /// one call; the defect affects all of them.
    #[test]
    fn r1_009_one_disposal_covers_every_call_outstanding_at_the_time() {
        let generation = DisposalGeneration::START;
        let queued: Vec<DisposalGeneration> = (0..4).map(|_| generation).collect();
        let after = generation.disposed();
        for (index, queued_in) in queued.iter().enumerate() {
            assert_eq!(
                ownership(*queued_in, after, false),
                Ownership::Freed,
                "call {index} was outstanding across the disposal"
            );
        }
        // A call dispatched after the disposal is on the new generation and is
        // cancellable again.
        assert_eq!(ownership(after, after, false), Ownership::Cancellable);
    }

    /// A second disposal does not resurrect anything.
    #[test]
    fn r1_009_generations_only_move_forward() {
        let first = DisposalGeneration::START.disposed();
        let second = first.disposed();
        assert!(second > first);
        assert_eq!(ownership(first, second, false), Ownership::Freed);
        assert_eq!(second.get(), 2);
    }
}
