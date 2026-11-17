//! Persistent acquisition accounting. This stores values, never native owners.
//! The owner thread must destroy resources before exposing a final snapshot.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use umbra_core::shutdown::{
    Accepted, Disposition, Entry, Final, Identity, Nonce, Proof, ResourceKind, Slot, SlotId,
};
use umbra_core::{ErrorKind, Result, UmbraError};

/// Closure preparation distinguishes native owner destruction from remote exit.
pub enum Start {
    /// Resource owners must be destroyed on their original thread before final.
    Local {
        /// Frozen inventory.
        accepted: Accepted,
        /// Non-resource accounting handle retained through destruction.
        ledger: Ledger,
    },
    /// The remote proxy has already validated final and waited for provider exit.
    Remote(Closure),
}

/// Run-end evidence, keeping ledger proof separate from exit-wait success.
pub struct Closure {
    /// All resource obligations independently confirmed.
    pub tree_terminated: bool,
    /// Closure error; never overwrites a preexisting event-loop primary error.
    pub error: Option<UmbraError>,
    /// Test-side measurement from validated Final to owned provider exit.
    pub final_to_exit: Option<Duration>,
}

impl Closure {
    /// Missing proof, including failure before acceptance.
    pub fn failed(error: UmbraError) -> Self {
        Self {
            tree_terminated: false,
            error: Some(error),
            final_to_exit: None,
        }
    }

    /// Validate resource evidence independently of transport exit.
    pub fn from_final(accepted: &Accepted, final_state: &Final) -> Self {
        match accepted.confirms(final_state) {
            Ok(true) => Self {
                tree_terminated: true,
                error: None,
                final_to_exit: None,
            },
            Ok(false) => Self::failed(invalid("shutdown ledger has unconfirmed obligations")),
            Err(error) => Self::failed(error),
        }
    }
}

impl crate::PlatformSession {
    /// Consume every platform handle before returning a terminal run verdict.
    pub fn shutdown(self) -> Closure {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.shutdown_inner())) {
            Ok(closure) => closure,
            Err(payload) => {
                std::mem::forget(payload);
                Closure::failed(invalid("shutdown owner panicked"))
            }
        }
    }

    fn shutdown_inner(self) -> Closure {
        let crate::PlatformSession { mut control, abi } = self;
        // Remote proxies share a Client: release the ABI before taking exclusive
        // control ownership. Native ABI destruction stays on this owner thread.
        drop(abi);
        let start = control.begin_shutdown(Nonce::new_v4());
        match start {
            Ok(Start::Local { accepted, ledger }) => {
                drop(control);
                match ledger.final_snapshot() {
                    Ok(final_state) => Closure::from_final(&accepted, &final_state),
                    Err(error) => Closure::failed(error),
                }
            }
            Ok(Start::Remote(closure)) => {
                drop(control);
                closure
            }
            Err(error) => {
                drop(control);
                Closure::failed(error)
            }
        }
    }
}

fn invalid(message: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, "platform.shutdown", message)
}

#[cfg(test)]
type ThreadLog = Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>>;

struct State {
    #[cfg(test)]
    thread_log: Option<ThreadLog>,
    instance: Nonce,
    entries: Vec<Entry>,
    accepted: Option<Accepted>,
    progress: u64,
    diagnostics: Vec<String>,
    poisoned: bool,
}

/// A shared accounting handle that survives all resource-owning objects.
#[derive(Clone)]
pub struct Ledger(Arc<Mutex<State>>);

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    /// Start a new provider instance, not a new launch in an existing provider.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            #[cfg(test)]
            thread_log: None,
            instance: Nonce::new_v4(),
            entries: Vec::new(),
            accepted: None,
            progress: 0,
            diagnostics: Vec::new(),
            poisoned: false,
        })))
    }

    #[cfg(test)]
    pub(crate) fn record_threads(&self, log: ThreadLog) {
        self.state().thread_log = Some(log);
    }
    #[cfg(test)]
    pub(crate) fn transport_thread(&self) {
        if let Some(log) = &self.state().thread_log {
            log.lock()
                .unwrap()
                .push(("transport", std::thread::current().id()));
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|error| {
            let mut state = error.into_inner();
            state.poisoned = true;
            state
        })
    }

    /// Reject ordinary work after the frozen inventory boundary.
    pub fn admit(&self) -> Result<()> {
        let state = self.state();
        if state.accepted.is_some() || state.poisoned {
            Err(invalid("operation after closure or ledger failure"))
        } else {
            Ok(())
        }
    }

    /// Reserve before a creation-capable operation or resume. No post-freeze
    /// reservation is admitted, including from late acquisition results.
    pub fn reserve(
        &self,
        kind: ResourceKind,
        parent: Option<SlotId>,
        generation: u64,
        exec_generation: u64,
    ) -> Result<Reservation> {
        let mut state = self.state();
        if state.accepted.is_some() || state.poisoned {
            return Err(invalid(
                "resource acquisition after closure or ledger failure",
            ));
        }
        let sequence = u64::try_from(state.entries.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| invalid("reservation sequence exhausted"))?;
        if parent.is_some_and(|parent| {
            parent.instance != state.instance || parent.sequence == 0 || parent.sequence >= sequence
        }) {
            return Err(invalid("invalid acquisition parent"));
        }
        let id = SlotId {
            instance: state.instance,
            sequence,
        };
        state.entries.push(Entry {
            slot: Slot {
                id,
                parent,
                generation,
                exec_generation,
                kind,
            },
            identity: None,
            disposition: Disposition::Reserved,
        });
        state.progress = state.progress.saturating_add(1);
        Ok(Reservation {
            ledger: self.clone(),
            id,
        })
    }

    /// Freeze the complete reservation set. The caller supplies only the
    /// remaining existing windows, never an added lifetime or idle limit.
    pub fn freeze(&self, nonce: Nonce, remaining: Duration) -> Result<Accepted> {
        let mut state = self.state();
        if state.accepted.is_some() || state.poisoned {
            return Err(invalid("duplicate shutdown or poisoned ledger"));
        }
        let accepted = Accepted {
            nonce,
            instance: state.instance,
            generation: state.progress,
            high_water: state.entries.len() as u64,
            slots: state
                .entries
                .iter()
                .map(|entry| entry.slot.clone())
                .collect(),
            remaining_millis: u64::try_from(remaining.as_millis())
                .map_err(|_| invalid("cleanup schedule overflow"))?,
        };
        accepted.validate()?;
        state.accepted = Some(accepted.clone());
        Ok(accepted)
    }

    /// Current owner progress. Repeated reads cannot advance it.
    pub fn progress(&self) -> u64 {
        self.state().progress
    }

    /// Record an independent lifecycle inconsistency. It cannot be hidden by
    /// agreement between accepted and final serialized lists.
    pub fn invalidate(&self, diagnostic: impl Into<String>) {
        let mut state = self.state();
        state.poisoned = true;
        state.diagnostics.push(diagnostic.into());
    }

    /// Copy the terminal inventory after owner destruction. Unsettled
    /// reservations remain explicit failures; this never manufactures proof.
    pub fn final_snapshot(&self) -> Result<Final> {
        let state = self.state();
        let accepted = state
            .accepted
            .as_ref()
            .ok_or_else(|| invalid("shutdown not accepted"))?;
        if state.poisoned {
            return Err(invalid("ledger ownership invariant failed"));
        }
        let entries = state
            .entries
            .iter()
            .cloned()
            .map(|mut entry| {
                if matches!(
                    entry.disposition,
                    Disposition::Reserved | Disposition::Acquired | Disposition::Settling
                ) {
                    entry.disposition =
                        Disposition::Unconfirmed("owner destroyed without terminal proof".into());
                }
                entry
            })
            .collect();
        Ok(Final {
            nonce: accepted.nonce,
            generation: accepted.generation,
            entries,
            diagnostics: state.diagnostics.clone(),
        })
    }
}

/// An acquisition token retained with the resource, including on error paths.
/// Dropping a token does not erase its slot or confirm the resource.
#[derive(Clone)]
pub struct Reservation {
    ledger: Ledger,
    id: SlotId,
}

impl Reservation {
    /// Settle a failed creation only when no resource was ever returned.
    /// Failures after acquisition preserve the existing owner and disposition.
    pub fn creation_failed(&self) {
        let mut state = self.ledger.state();
        if let Some(entry) = state.entries.get_mut((self.id.sequence - 1) as usize) {
            if entry.slot.id == self.id
                && entry.identity.is_none()
                && matches!(entry.disposition, Disposition::Reserved)
            {
                entry.disposition = Disposition::NeverCreated;
                state.progress = state.progress.saturating_add(1);
            }
        }
    }
    /// Immutable resource identity for transfer and independent witnesses.
    pub fn id(&self) -> SlotId {
        self.id
    }

    fn update(&self, f: impl FnOnce(&mut Entry) -> Result<()>) -> Result<()> {
        let mut state = self.ledger.state();
        let index =
            usize::try_from(self.id.sequence - 1).map_err(|_| invalid("unknown reservation"))?;
        let entry = state
            .entries
            .get_mut(index)
            .ok_or_else(|| invalid("unknown reservation"))?;
        if entry.slot.id != self.id {
            state.poisoned = true;
            return Err(invalid("reservation identity mismatch"));
        }
        if matches!(
            entry.disposition,
            Disposition::Confirmed(_) | Disposition::NeverCreated
        ) {
            state.poisoned = true;
            return Err(invalid("terminal reservation changed"));
        }
        let result = f(entry);
        if result.is_err() {
            state.poisoned = true;
        }
        state.progress = state.progress.saturating_add(1);
        result
    }

    /// Record the returned identity before any subsequent fallible operation.
    pub fn acquired(&self, identity: Identity) -> Result<()> {
        self.update(|entry| {
            if entry.identity.is_some() || !matches!(entry.disposition, Disposition::Reserved) {
                return Err(invalid("resource acquired more than once"));
            }
            entry.identity = Some(identity);
            entry.disposition = Disposition::Acquired;
            Ok(())
        })
    }

    /// The creation API definitively returned without creating a resource.
    pub fn never_created(&self) -> Result<()> {
        self.update(|entry| {
            if entry.identity.is_some() {
                return Err(invalid("acquired resource marked never-created"));
            }
            entry.disposition = Disposition::NeverCreated;
            Ok(())
        })
    }

    /// Preserve a terminal observation. Proof kind is independently checked
    /// against the immutable obligation when validating the final inventory.
    pub fn confirm(&self, proof: Proof) -> Result<()> {
        self.update(|entry| {
            if entry.identity.is_none() {
                return Err(invalid("unacquired resource confirmed"));
            }
            entry.disposition = Disposition::Confirmed(proof);
            Ok(())
        })
    }

    /// Retain failure and resource ownership for the existing emergency path.
    pub fn unconfirmed(&self, diagnostic: impl Into<String>) -> Result<()> {
        let diagnostic = diagnostic.into();
        self.update(|entry| {
            entry.disposition = Disposition::Unconfirmed(diagnostic);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_late_result_and_retired_history_survive_freeze() {
        let ledger = Ledger::new();
        let retired = ledger.reserve(ResourceKind::Tracer, None, 1, 0).unwrap();
        retired
            .acquired(Identity {
                pid: Some(10),
                birth: None,
            })
            .unwrap();
        retired
            .confirm(Proof::Collected {
                status: "exit 0".into(),
            })
            .unwrap();
        let late = ledger
            .reserve(ResourceKind::TaskRight, Some(retired.id()), 1, 1)
            .unwrap();
        let accepted = ledger
            .freeze(Nonce::new_v4(), Duration::from_secs(1))
            .unwrap();
        assert_eq!(accepted.slots.len(), 2);
        assert!(ledger.reserve(ResourceKind::Tracer, None, 1, 2).is_err());
        assert!(!accepted
            .confirms(&ledger.final_snapshot().unwrap())
            .unwrap());
        late.acquired(Identity {
            pid: Some(11),
            birth: None,
        })
        .unwrap();
        late.confirm(Proof::Released).unwrap();
        let final_state = ledger.final_snapshot().unwrap();
        assert_eq!(final_state.entries.len(), 2);
        assert!(accepted.confirms(&final_state).unwrap());
    }
    #[test]
    fn shutdown_failed_acquisition_is_not_erased() {
        let ledger = Ledger::new();
        let pending = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
        let accepted = ledger.freeze(Nonce::new_v4(), Duration::ZERO).unwrap();
        drop(pending);
        let final_state = ledger.final_snapshot().unwrap();
        assert_eq!(final_state.entries.len(), 1);
        assert!(!accepted.confirms(&final_state).unwrap());
    }
    #[test]
    fn shutdown_terminal_slot_cannot_be_rewritten() {
        let ledger = Ledger::new();
        let slot = ledger.reserve(ResourceKind::Tracer, None, 1, 0).unwrap();
        slot.never_created().unwrap();
        assert!(slot
            .acquired(Identity {
                pid: Some(123),
                birth: None
            })
            .is_err());
        assert!(ledger.freeze(Nonce::new_v4(), Duration::ZERO).is_err());
    }
}

/// Qualification-only evidence, independent of inventory enumeration.
#[cfg(feature = "shutdown-witness")]
#[doc(hidden)]
pub fn witness(record: serde_json::Value) {
    use std::io::Write;
    let Some(path) = std::env::var_os("UMBRA_FIX_271_EVIDENCE_FILE") else {
        return;
    };
    let mut record = record;
    record["version"] = "shutdown-witness-v1".into();
    record["writer_pid"] = std::process::id().into();
    record["thread"] = format!("{:?}", std::thread::current().id()).into();
    let mut bytes = serde_json::to_vec(&record).expect("qualification evidence encoding");
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("qualification evidence file");
    file.write_all(&bytes)
        .expect("qualification evidence write");
}
