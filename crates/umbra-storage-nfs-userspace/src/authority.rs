//! Writer authority and bounded outage recovery for the userspace NFSv4.0 backend.
//!
//! This module owns the two questions the protocol state machine deliberately
//! refuses: *may this session mutate at all*, and *what happens when it is
//! interrupted*. `state::lease::LeaseClock::takeover_by_timeout` and
//! `state::ProtocolState::takeover` both answer `TakeoverRefused` and point
//! here; this is the "here".
//!
//! # One session, one Umbra
//!
//! Admission is product-wide, not per-provider and not per-connection. A run
//! carries one durable [`marker::AdmissionMarker`]; the session that creates it
//! exclusively holds authority until it *cooperatively* releases it. A competing
//! session reads the marker, finds it held, and is denied. There is no timeout
//! path: [`admission::AdmissionControl::acquire`] never consults a clock, and
//! every takeover policy the storage contract can express is refused with a
//! reason rather than silently downgraded.
//!
//! That refusal is the design, not a gap. `docs/design/failure-model.md` states
//! it twice — "A crashed controller leaves an abandoned session: M1/M2 do not
//! autonomously steal its marker" and "No automatic marker deletion or
//! successful close on timeout" — because silence proves nothing about whether
//! the former writer is still issuing I/O. M3 fencing is what will make the
//! question answerable; until then a blocked run is the correct outcome.
//!
//! # What is durable before an acknowledgement
//!
//! [`journal::MutationJournal`] wraps the frozen [`ReplayLog`](crate::replay::ReplayLog)
//! so that a mutation's intent, its payload identity and its committed result
//! identity all reach the log before the caller is told anything. The ticket a
//! caller needs in order to dispatch is minted only by a successful durable
//! admit, so "dispatched without a durable intent" is unrepresentable rather
//! than merely discouraged.
//!
//! # Bounded outage
//!
//! [`outage::OutageMachine`] is the finite state machine over the five states
//! the failure model defines, driven by the crash windows it enumerates. Every
//! transition either resolves or stops; none of them fabricates a success, a
//! release or a clean checkpoint, and the original `NFS4ERR_*` survives every
//! attempt through the frozen [`RetainedError`](crate::error::RetainedError).
//!
//! # Explicitly out of scope
//!
//! Split brain and hard partition. Two hosts holding conflicting *valid*
//! ownership evidence is not resolvable by anything M1 owns: the failure model
//! requires a fence receipt and a cutoff before a winner may be selected, and
//! that authority arrives with M3. [`outage::CrashWindow::SplitBrain`] and
//! [`outage::CrashWindow::HardPartition`] therefore stop both sides and record
//! [`outage::Deferral::M3Fencing`]. Nothing in this module selects a winner,
//! deletes a rival's marker, or treats an increasing epoch as a fence.
//!
//! Binding any of this to a live transport is `m1_integrate`'s seam. Everything
//! here is driven over the frozen facades, and the tests run against the fake.

pub mod admission;
pub mod journal;
pub mod marker;
pub mod outage;
pub mod server_marker;

pub use admission::{
    AdmissionControl, AdmissionOutcome, AdmissionRequest, Admitted, OutstandingIo, ReleaseOutcome,
};
pub use journal::{
    Acknowledged, CommittedResult, DispatchTicket, Durability, KeyStanding, MutationJournal,
    MutationRequest, RecoveryPlan,
};
pub use marker::{
    AdmissionMarker, AdmissionPhase, ExclusiveCreate, MarkerError, MarkerStore, MemoryMarkerStore,
    WriterToken,
};
pub use outage::{
    AdmissionStanding, CrashWindow, Deferral, Evidence, OutageBudget, OutageMachine, RecoveryState,
    StoppedSafely,
};
pub use server_marker::ServerMarkerStore;
