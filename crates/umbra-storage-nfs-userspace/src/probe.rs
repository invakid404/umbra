//! The per-run boundary probe: the *proved* half of the durability claim.
//!
//! [`crate::storage::QUALIFIED_DURABILITY`] is a claim about a persistence
//! boundary, and until this module existed nothing tied it to evidence: it was a
//! bare constant read straight into every run binding and every flush receipt,
//! so a build with no live transport linked at all still advertised
//! [`umbra_core::Durability::Remote`]. The doc comment already stated the
//! degradation rule — *"were the qualifying live evidence ever removed from the
//! verification, this must degrade to `Durability::Local`"* — as a human
//! instruction. This is that instruction as a mechanism.
//!
//! # What the probe proves, and what it cannot
//!
//! [`qualify`] drives one synthetic `OPEN → WRITE(UNSTABLE) → COMMIT →
//! verifier-compare → CLOSE → REMOVE` cycle against the run's own `.provider`
//! directory, once per `open_run`. A matched verifier is, per RFC 7530 §16.4,
//! the server's acknowledgement that those bytes are on stable storage — so the
//! cycle proves the acknowledgement is really available on *this* mount, in
//! *this* session, rather than assumed from a constant.
//!
//! It proves nothing about *which* server answered. [`crate::fake::FakeTransport`]
//! returns the same verifier on WRITE and on COMMIT and would pass this cycle
//! exactly as a real Ganesha does. That is why the probe is only ever ANDed with
//! [`crate::transport::RawTransport::persistence_boundary`], which is a property
//! of the implementation that was compiled in rather than of a round trip.
//!
//! # Containment is the safety property
//!
//! A probe failure means "this run may not claim `Remote`". It must never mean
//! anything more than that, and in particular it must never become run
//! bookkeeping:
//!
//! * It does **not** enter `unsettled`. `flush`'s case-3 refusal
//!   (`storage.rs`) turns a non-empty `unsettled` into a refusal for the whole
//!   life of the run, so a leaky probe would convert "cannot claim `Remote`"
//!   into "this run can never flush again".
//! * It does **not** enter `stable_write_failure` or `recovery_blocked`, both of
//!   which are likewise terminal.
//! * It does **not** fold into the [`RunLedger`](crate::storage) — a synthetic
//!   write no caller made would otherwise be counted in `committed_writes`,
//!   `objects` and `last_verifier` in every receipt's evidence.
//! * It does **not** go through the durable retry journal or the replay facade,
//!   so `.provider/retries` stays empty. It drives the transport's own
//!   WRITE/COMMIT shape helpers directly, exactly as the anchor writes in
//!   [`crate::anchor`] do.
//!
//! Every one of those is enforced by construction here: the only thing this
//! module hands back is a `bool`, and every error it meets is consumed on the
//! way to producing it.
//!
//! # Fault ordering: arm after `open_run`, never before it
//!
//! On a transport that declares [`PersistenceBoundary::RemoteServer`], the
//! probe's WRITE and COMMIT happen *inside* `open_run`. A one-shot fault plan
//! armed before the run is opened is therefore spent on the probe rather than on
//! the operation under test: the probe fails, the run honestly degrades to
//! [`umbra_core::Durability::Local`], and the call the test meant to fault runs
//! *unfaulted* and succeeds — a symptom that names neither the probe nor the
//! ordering that caused it.
//!
//! **So a fault test over a declaring transport must arm its plan after
//! `open_run` has returned**, through
//! [`NfsUserspaceStorage::transport`](crate::storage::NfsUserspaceStorage::transport),
//! or else target a point the probe does not reach. This is not hypothetical and
//! not merely advice for the future: `tests/fault_matrix.rs`'s live
//! COMMIT-failure case opens a run and arms afterwards for exactly this reason,
//! having first failed in CI by arming before it.
//!
//! The in-memory doubles are all `Unqualified`, so the probe never runs under
//! them and the ordering cannot bite there. A transport that declares is the
//! case this rule exists for.
//!
//! # The artifact is always removed
//!
//! On *every* exit: REMOVE is attempted whatever the WRITE and COMMIT did, CLOSE
//! before it, and — because `UNCHECKED4` creates the name at OPEN time — even on
//! the path where the OPEN itself returned `Err`, because such an OPEN may have
//! created the name before failing. A cycle that could not clean up after itself
//! does not qualify.
//!
//! `tests/fake_fault_matrix.rs` pins this on the success path and on the failure
//! path alike, by rendering the run's layout after a *probing* `open_run` +
//! `close_run`. `tests/golden_compat.rs`'s byte-for-byte eight-row golden is a
//! second net, but only on the live leg: it builds over `FakeTransport`, which
//! declares `Unqualified`, so no probe runs there and no artifact can leak.

use crate::anchor::Anchor;
use crate::crud::{CreateDisposition, OpenObject};
use crate::state::open_owner::{close, CloseOutcome, OpenOwnerRegistry};
use crate::transport::{
    ComponentName, Compound, Deadline, Nfs4Op, OpReply, PersistenceBoundary, RawTransport,
    ShareAccess, Stability,
};

/// Name of the probe artifact inside the run's `.provider` directory.
///
/// Deterministic rather than random, and opened `UNCHECKED4` rather than
/// `GUARDED4`, so a process that died between this file's creation and its
/// removal leaves one name that the next `open_run` reuses and then removes —
/// instead of one orphan per crashed run accumulating in the run's private
/// state forever.
const PROBE_FILE: &[u8] = b"boundary-probe";

/// Bytes the probe writes. Content is irrelevant; the verifier is the evidence.
const PROBE_PAYLOAD: &[u8] = b"umbra-boundary-probe";

/// Whether this run may advertise [`crate::storage::QUALIFIED_DURABILITY`].
///
/// Both gates, ANDed, evaluated once per `open_run`:
///
/// 1. `transport` declares [`PersistenceBoundary::RemoteServer`]; and
/// 2. the synthetic matched-verifier COMMIT cycle below completed against the
///    run's own `.provider` directory, leaving nothing behind.
///
/// Anything else — an undeclared transport, a refused OPEN, a short WRITE, a
/// failed COMMIT, a changed verifier, a CLOSE the server rejected, a REMOVE that
/// did not land — is `false`, which the caller reads as
/// [`umbra_core::Durability::Local`]: strictly less, and so incapable of lying.
pub(crate) fn qualify(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    deadline: Deadline,
) -> bool {
    // Gate 1. Checked first because it is free, and because a transport that
    // declares nothing must not have a synthetic write put through it at all.
    if transport.persistence_boundary() != PersistenceBoundary::RemoteServer {
        return false;
    }
    let Ok(name) = ComponentName::new(PROBE_FILE.to_vec()) else {
        return false;
    };
    // `UNCHECKED4`: create it, or adopt the one a crashed predecessor left.
    let Ok(open) = OpenObject::open(
        owners,
        transport,
        private.pin(),
        &name,
        CreateDisposition::OpenOrCreate { mode: 0o600 },
        ShareAccess::WRITE,
        deadline,
    ) else {
        // `UNCHECKED4` creates the name at OPEN time, so an OPEN that returns
        // `Err` may still have created it: `open_with_claim` abandons a
        // *successful* server-side OPEN when the object's identity cannot be
        // established or the owner's seqid is exhausted, and an OPEN whose reply
        // was lost created the name regardless. The boundary is unproven either
        // way, but the name may exist and this is the only chance to take it
        // back — REMOVE of an absent name answers `NFS4ERR_NOENT`, which
        // `unlink` already reports as `false`, so the attempt is free.
        //
        // Without this the module's "the artifact is always removed" guarantee
        // would hold only on the paths that returned an `OpenObject`.
        let _removed = unlink(transport, private, &name, deadline);
        return false;
    };

    let matched = cycle(transport, &open, deadline);

    // The open goes whatever the cycle did: a probe never leaves an open live,
    // and the server may not release a removed name while one is held.
    let closed = release(open, transport, deadline);
    // The artifact goes whatever the open did. This is the orphan the golden
    // layout comparison would otherwise catch on the next `close_run`.
    let removed = unlink(transport, private, &name, deadline);

    // The full cycle is the evidence. A run that proved the COMMIT but could not
    // close or clean up did not complete the cycle, so it does not qualify.
    matched && closed && removed
}

/// CLOSE the probe's open, and report whether the server really released it.
///
/// A refused CLOSE hands the [`OpenFile`](crate::handle::OpenFile) back
/// *because it is still valid* — the seqid was aborted rather than consumed — so
/// dropping it would leave live server-side open state behind. The REMOVE that
/// follows does not release it either: unlinking a name is not closing an open.
/// Since this module promises that a probe never leaves an open live, the
/// returned file is used for exactly what it is handed back for, one more CLOSE.
///
/// One retry, not a loop. The probe is a fixed, small number of extra round
/// trips on every `open_run`, and a server that refuses the same CLOSE twice is
/// not going to yield to a third; spinning here would trade a bounded cost for
/// an unbounded one on the run's critical path. A second refusal is reported as
/// a failed release, which keeps the run unqualified.
///
/// [`CloseOutcome::Abandoned`] gets no retry because there is nothing to retry
/// with: no file comes back, the owner is poisoned, and the server-side state
/// persists until the lease expires. That is a real failure to release and is
/// reported as one — the caller must not read it as a clean close.
fn release(open: OpenObject, transport: &mut dyn RawTransport, deadline: Deadline) -> bool {
    match open.close(transport, deadline) {
        CloseOutcome::Closed(_) => true,
        CloseOutcome::Rejected { file, .. } => {
            matches!(close(file, transport, deadline), CloseOutcome::Closed(_))
        }
        CloseOutcome::Abandoned { .. } => false,
    }
}

/// `WRITE(UNSTABLE)` then `COMMIT`, and whether the verifiers matched.
///
/// Deliberately the transport's own shape helpers rather than
/// [`OpenObject::write`]/[`OpenObject::commit`]: those take a
/// [`ReplayLog`](crate::replay::ReplayLog) and would record a durable intent and
/// a settled outcome for an idempotency key no caller ever issued.
fn cycle(transport: &mut dyn RawTransport, open: &OpenObject, deadline: Deadline) -> bool {
    let Ok(stateid) = open.stateid() else {
        return false;
    };
    let Ok(written) = transport.write(
        open.handle(),
        stateid,
        0,
        Stability::Unstable,
        PROBE_PAYLOAD.to_vec(),
        deadline,
    ) else {
        return false;
    };
    // A short write is a legal reply, and one the probe cannot reason about: the
    // COMMIT below would cover a range the WRITE did not fill.
    if written.count as usize != PROBE_PAYLOAD.len() {
        return false;
    }
    // COMMITted unconditionally, even where the server reported a stability it
    // did not owe a COMMIT for. The cycle being proven is the one every
    // `WriteAt` performs, and the verifier comparison is the whole evidence.
    let Ok(committed) = transport.commit(open.handle(), 0, written.count, deadline) else {
        return false;
    };
    // RFC 7530 §16.4: same verifier, so the server is acknowledging that the
    // unstable bytes reached stable storage. A changed verifier means it lost
    // them — which is exactly the claim this provider must not make.
    committed.verifier == written.verifier
}

/// `PUTFH; REMOVE`, reporting only whether the name is gone.
///
/// The [`crate::namespace`] dispatcher is the seam a *caller's* unlink goes
/// through; it pins the target's identity and proves the removal against the
/// directory's change info, which is the right contract for an object somebody
/// else may be racing. This name is the probe's own, created moments ago in the
/// run's private state, and a removal it cannot prove is reported as an
/// unqualified boundary rather than raised as a namespace failure.
fn unlink(
    transport: &mut dyn RawTransport,
    private: &Anchor,
    name: &ComponentName,
    deadline: Deadline,
) -> bool {
    let call = Compound::new(
        *b"probe",
        vec![
            Nfs4Op::PutFh(private.pin().handle().clone()),
            Nfs4Op::Remove { name: name.clone() },
        ],
    );
    let Ok(reply) = transport.submit(call, deadline) else {
        return false;
    };
    matches!(reply.expect(1), Ok(OpReply::Remove(_)))
}
