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
    /// Ordered managed evidence, present only after validated Final and parent wait.
    pub managed: Option<Box<ManagedTerminal>>,
    /// All resource obligations independently confirmed.
    pub tree_terminated: bool,
    /// Closure error; never overwrites a preexisting event-loop primary error.
    pub error: Option<UmbraError>,
    /// Test-side measurement from decoded Final, including validation, to owned exit.
    pub final_to_exit: Option<Duration>,
}

/// Resource evidence retained only after successful managed provider collection.
pub struct ManagedTerminal {
    accepted: Accepted,
    final_state: Final,
    receipt: umbra_core::managed::ProviderReceipt,
}

impl ManagedTerminal {
    pub(crate) fn collected(
        accepted: Accepted,
        final_state: Final,
        receipt: umbra_core::managed::ProviderReceipt,
    ) -> Self {
        Self {
            accepted,
            final_state,
            receipt,
        }
    }

    /// Encode the exact ordered resource evidence for a durable certificate.
    pub fn encode(&self) -> Result<Vec<u8>> {
        umbra_core::provider::encode(&(&self.accepted, &self.final_state, &self.receipt))
    }

    /// Identity of the run whose provider was collected by its actual parent.
    pub fn run(&self) -> umbra_core::managed::RunIdentity {
        self.receipt.run
    }
}

impl Closure {
    /// Missing proof, including failure before acceptance.
    pub fn failed(error: UmbraError) -> Self {
        Self {
            managed: None,
            tree_terminated: false,
            error: Some(error),
            final_to_exit: None,
        }
    }

    /// Validate resource evidence independently of transport exit.
    pub fn from_final(accepted: &Accepted, final_state: &Final) -> Self {
        match accepted.confirms(final_state) {
            Ok(true) => Self {
                managed: None,
                tree_terminated: true,
                error: None,
                final_to_exit: None,
            },
            Ok(false) => Self::failed(invalid(&unconfirmed_obligations(final_state))),
            Err(error) => Self::failed(error),
        }
    }
}

impl crate::PlatformSession {
    /// Consume every platform handle before returning a terminal run verdict.
    pub fn shutdown(self) -> Closure {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.shutdown_inner())) {
            Ok(mut closure) => {
                if !closure.tree_terminated && closure.error.is_none() {
                    closure.error = Some(invalid("shutdown proof missing"));
                }
                closure
            }
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

/// The unconfirmed-obligation message (#205): every entry that is neither
/// confirmed nor never-created, as kind, pid (or `?`) and disposition, then
/// the ledger's own diagnostics. Text only; it decides nothing.
pub(crate) fn unconfirmed_obligations(final_state: &Final) -> String {
    let unconfirmed: Vec<String> = final_state
        .entries
        .iter()
        .filter(|entry| {
            !matches!(
                entry.disposition,
                Disposition::Confirmed(_) | Disposition::NeverCreated
            )
        })
        .map(|entry| {
            let pid = entry
                .identity
                .as_ref()
                .and_then(|identity| identity.pid)
                .map_or_else(|| "?".to_string(), |pid| pid.to_string());
            format!("{:?} pid={pid} {:?}", entry.slot.kind, entry.disposition)
        })
        .collect();
    format!(
        "shutdown ledger has unconfirmed obligations: [{}]; diagnostics: {:?}",
        unconfirmed.join(", "),
        final_state.diagnostics
    )
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
    admission: Option<NativeAdmission>,
}

struct NativeAdmission {
    authority: crate::admission::Admission,
    deadline: Box<dyn Fn() -> Result<u64> + Send>,
    groups: std::collections::BTreeMap<SlotId, crate::admission::ProgramGrant>,
    membership: std::collections::BTreeMap<SlotId, SlotId>,
}

impl NativeAdmission {
    fn reserve(&mut self, slot: &Slot) -> Result<()> {
        if slot.kind == ResourceKind::Tracee || slot.replaces.is_some() {
            let deadline = (self.deadline)()?;
            let mut grant = self.authority.reserve(deadline)?;
            if let Err(error) = grant.enter(deadline) {
                // Only an exact refusal permits this cancellation. Ambiguous
                // entry remains charged by the independent owner.
                let _ = grant.cancel_before_entry(deadline);
                return Err(error);
            }
            self.groups.insert(
                slot.id,
                crate::admission::ProgramGrant::new(grant, slot.id)?,
            );
            self.membership.insert(slot.id, slot.id);
        } else {
            let group = slot
                .parent
                .and_then(|parent| self.membership.get(&parent))
                .copied()
                .ok_or_else(|| invalid("resource without admitted program"))?;
            self.groups
                .get_mut(&group)
                .ok_or_else(|| invalid("missing program charge"))?
                .attach(slot.id)?;
            self.membership.insert(slot.id, group);
        }
        Ok(())
    }

    fn settle(&mut self, id: SlotId, entries: &[Entry]) -> Result<()> {
        let group = *self
            .membership
            .get(&id)
            .ok_or_else(|| invalid("unadmitted resource"))?;
        let creator_terminal = entries
            .iter()
            .find(|entry| entry.slot.id == group)
            .is_some_and(Entry::confirms);
        self.groups
            .get_mut(&group)
            .ok_or_else(|| invalid("missing program charge"))?
            .settle(entries, creator_terminal, &self.deadline)?;
        Ok(())
    }
}

impl State {
    fn settle_admission(&mut self, id: SlotId) -> Result<()> {
        let result = match &mut self.admission {
            Some(admission) => admission.settle(id, &self.entries),
            None => Ok(()),
        };
        if let Err(error) = &result {
            self.poisoned = true;
            self.diagnostics
                .push(format!("admission retirement: {error}"));
        }
        result
    }
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
            admission: None,
        })))
    }

    /// Bind the independent owner ledger before any native reservations.
    /// The deadline callback supplies the caller's absolute operation cutoff;
    /// it must retain the original cleanup cutoff once shutdown starts.
    pub fn install_admission(
        &self,
        authority: crate::admission::Admission,
        deadline: Box<dyn Fn() -> Result<u64> + Send>,
    ) -> Result<()> {
        let mut state = self.state();
        if state.admission.is_some() || !state.entries.is_empty() || state.accepted.is_some() {
            return Err(invalid("late or duplicate native admission installation"));
        }
        state.admission = Some(NativeAdmission {
            authority,
            deadline,
            groups: Default::default(),
            membership: Default::default(),
        });
        Ok(())
    }

    /// Read the retained managed cutoff for work already in progress.
    pub fn admission_cutoff(&self) -> Result<Option<u64>> {
        self.state()
            .admission
            .as_ref()
            .map(|admission| (admission.deadline)())
            .transpose()
    }

    /// Freeze external grants before counting native cleanup obligations.
    pub fn freeze_admission(&self) -> Result<()> {
        let state = self.state();
        if let Some(admission) = &state.admission {
            admission.authority.freeze((admission.deadline)()?)?;
        }
        Ok(())
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
        self.reserve_inner(kind, parent, generation, exec_generation, None)
    }

    /// Atomically reserve an exec replacement while retaining its source right.
    pub fn reserve_replacement(
        &self,
        parent: SlotId,
        generation: u64,
        exec_generation: u64,
        previous: SlotId,
    ) -> Result<Reservation> {
        self.reserve_inner(
            ResourceKind::TaskRight,
            Some(parent),
            generation,
            exec_generation,
            Some(previous),
        )
    }

    fn reserve_inner(
        &self,
        kind: ResourceKind,
        parent: Option<SlotId>,
        generation: u64,
        exec_generation: u64,
        replaces: Option<SlotId>,
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
        if let Some(previous) = replaces {
            let index = previous
                .sequence
                .checked_sub(1)
                .and_then(|sequence| usize::try_from(sequence).ok())
                .ok_or_else(|| invalid("unknown replacement source"))?;
            let old = state
                .entries
                .get(index)
                .filter(|entry| entry.slot.id == previous)
                .ok_or_else(|| invalid("unknown replacement source"))?;
            if old.slot.kind != ResourceKind::TaskRight
                || old.slot.parent != parent
                || old.slot.generation != generation
                || old.slot.exec_generation >= exec_generation
                || !matches!(
                    old.disposition,
                    Disposition::Acquired | Disposition::Settling
                )
            {
                return Err(invalid("replacement source is not a retained right"));
            }
        }
        let id = SlotId {
            instance: state.instance,
            sequence,
        };
        let slot = Slot {
            id,
            parent,
            replaces,
            generation,
            exec_generation,
            kind,
        };
        if let Some(admission) = &mut state.admission {
            admission.reserve(&slot)?;
        }
        state.entries.push(Entry {
            slot,
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
    /// Share this program's accounting authority with a preparation helper.
    pub fn ledger(&self) -> Ledger {
        self.ledger.clone()
    }

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
                let _ = state.settle_admission(self.id);
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
        result?;
        state.settle_admission(self.id)
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

    /// Capture the first birth observation while the acquisition guard owns the
    /// resource. A later conflicting identity poisons the ledger, never replaces it.
    pub fn capture_birth(&self, birth: Option<(u64, u64)>) -> Result<()> {
        let Some(birth) = birth else {
            return Ok(());
        };
        self.update(|entry| {
            let identity = entry
                .identity
                .as_mut()
                .ok_or_else(|| invalid("birth observation before acquisition"))?;
            if identity.birth.is_some_and(|previous| previous != birth) {
                return Err(invalid("resource birth identity changed"));
            }
            identity.birth = Some(birth);
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
pub(crate) fn collected_model_root(ledger: &Ledger) -> SlotId {
    let root = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
    root.acquired(Identity {
        pid: Some(123),
        birth: None,
    })
    .unwrap();
    root.confirm(Proof::Accounted {
        evidence: "unit-model root collection".into(),
        root_owned: true,
        root_collected: true,
    })
    .unwrap();
    root.id()
}

#[cfg(test)]
pub(crate) fn scripted_unconfirmed_final() -> (Accepted, Final) {
    let ledger = Ledger::new();
    let tracee = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
    tracee
        .acquired(Identity {
            pid: Some(4242),
            birth: None,
        })
        .unwrap();
    let _right = ledger
        .reserve(ResourceKind::TaskRight, Some(tracee.id()), 1, 0)
        .unwrap();
    let tracer = ledger
        .reserve(ResourceKind::Tracer, Some(tracee.id()), 1, 0)
        .unwrap();
    tracer
        .acquired(Identity {
            pid: Some(5151),
            birth: None,
        })
        .unwrap();
    tracer
        .confirm(Proof::Collected {
            status: "exit 0".into(),
        })
        .unwrap();
    let accepted = ledger
        .freeze(Nonce::new_v4(), Duration::from_secs(1))
        .unwrap();
    let mut final_state = ledger.final_snapshot().unwrap();
    final_state
        .diagnostics
        .push("poisoned: scripted ownership failure".into());
    (accepted, final_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// #205: the unconfirmed-obligation message names each unconfirmed entry
    /// (kind, pid or `?`, disposition) and keeps the diagnostics; a confirmed
    /// entry is not listed.
    #[test]
    fn the_unconfirmed_message_names_its_obligations() {
        let (accepted, final_state) = scripted_unconfirmed_final();
        let closure = Closure::from_final(&accepted, &final_state);
        assert!(!closure.tree_terminated);
        let message = closure.error.expect("unconfirmed").to_string();
        for named in [
            "Tracee pid=4242 Unconfirmed(\"owner destroyed without terminal proof\")",
            "TaskRight pid=? Unconfirmed(\"owner destroyed without terminal proof\")",
            "poisoned: scripted ownership failure",
        ] {
            assert!(message.contains(named), "{named} missing from: {message}");
        }
        assert!(!message.contains("Tracer pid=5151"), "{message}");
    }
    #[test]
    fn shutdown_late_result_and_retired_history_survive_freeze() {
        let ledger = Ledger::new();
        let root = collected_model_root(&ledger);
        let retired = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
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
            .reserve(ResourceKind::TaskRight, Some(root), 1, 1)
            .unwrap();
        let accepted = ledger
            .freeze(Nonce::new_v4(), Duration::from_secs(1))
            .unwrap();
        assert_eq!(accepted.slots.len(), 3);
        assert!(ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 2)
            .is_err());
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
        assert_eq!(final_state.entries.len(), 3);
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
        let root = collected_model_root(&ledger);
        let slot = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
        slot.never_created().unwrap();
        assert!(slot
            .acquired(Identity {
                pid: Some(123),
                birth: None
            })
            .is_err());
        assert!(ledger.freeze(Nonce::new_v4(), Duration::ZERO).is_err());
    }
    #[test]
    fn shutdown_replacement_links_and_birth_identity_are_retained() {
        let ledger = Ledger::new();
        let tracee = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
        tracee
            .acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
        tracee.capture_birth(Some((10, 20))).unwrap();
        let old = ledger
            .reserve(ResourceKind::TaskRight, Some(tracee.id()), 1, 0)
            .unwrap();
        old.acquired(Identity {
            pid: Some(123),
            birth: Some((10, 20)),
        })
        .unwrap();
        let replacement = ledger
            .reserve_replacement(tracee.id(), 1, 1, old.id())
            .unwrap();
        replacement
            .acquired(Identity {
                pid: Some(123),
                birth: Some((10, 20)),
            })
            .unwrap();
        replacement.confirm(Proof::Released).unwrap();
        tracee
            .confirm(Proof::Accounted {
                evidence: "owned wait control".into(),
                root_owned: true,
                root_collected: true,
            })
            .unwrap();
        let accepted = ledger.freeze(Nonce::new_v4(), Duration::ZERO).unwrap();
        assert_eq!(accepted.slots[2].replaces, Some(old.id()));
        assert!(!accepted
            .confirms(&ledger.final_snapshot().unwrap())
            .unwrap());
        old.confirm(Proof::Released).unwrap();
        let final_state = ledger.final_snapshot().unwrap();
        assert_eq!(
            final_state.entries[0].identity.as_ref().unwrap().birth,
            Some((10, 20))
        );
        assert!(accepted.confirms(&final_state).unwrap());
    }
    #[test]
    fn shutdown_replacement_rejects_invalid_sources_without_reserving() {
        for case in 0..10 {
            let ledger = Ledger::new();
            let root = collected_model_root(&ledger);
            let old = ledger
                .reserve(ResourceKind::TaskRight, Some(root), 1, 0)
                .unwrap();
            old.acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
            let mut previous = old.id();
            match case {
                0 => previous.sequence = 0,
                1 => previous.sequence = u64::MAX,
                2 => previous.sequence += 1,
                3 => previous.instance = Nonce::new_v4(),
                4 => ledger.state().entries[1].slot.kind = ResourceKind::Tracer,
                5 => ledger.state().entries[1].slot.parent = None,
                6 => ledger.state().entries[1].slot.generation = 2,
                7 => ledger.state().entries[1].slot.exec_generation = 1,
                8 => ledger.state().entries[1].disposition = Disposition::Reserved,
                9 => old.confirm(Proof::Released).unwrap(),
                _ => unreachable!(),
            }
            let progress = ledger.state().progress;
            assert!(
                ledger.reserve_replacement(root, 1, 1, previous).is_err(),
                "invalid replacement source case {case}"
            );
            assert_eq!(ledger.state().entries.len(), 2);
            assert_eq!(ledger.state().progress, progress);
        }
        // Both retained dispositions remain eligible; the prior obligation
        // stays present after allocating its replacement.
        for settling in [false, true] {
            let ledger = Ledger::new();
            let root = collected_model_root(&ledger);
            let old = ledger
                .reserve(ResourceKind::TaskRight, Some(root), 1, 0)
                .unwrap();
            old.acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
            if settling {
                ledger.state().entries[1].disposition = Disposition::Settling;
            }
            let replacement = ledger.reserve_replacement(root, 1, 1, old.id()).unwrap();
            assert_eq!(replacement.id().sequence, 3);
            assert_eq!(ledger.state().entries[2].slot.replaces, Some(old.id()));
            assert_eq!(ledger.state().entries[1].slot.id, old.id());
        }
    }
    #[test]
    fn shutdown_birth_reuse_cannot_replace_a_captured_identity() {
        let ledger = Ledger::new();
        let slot = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
        slot.acquired(Identity {
            pid: Some(123),
            birth: Some((10, 20)),
        })
        .unwrap();
        assert!(slot.capture_birth(Some((10, 21))).is_err());
        assert!(ledger.freeze(Nonce::new_v4(), Duration::ZERO).is_err());
    }
}

/// Qualification-only evidence, independent of inventory enumeration.
#[cfg(feature = "shutdown-witness")]
#[doc(hidden)]
pub fn witness(record: serde_json::Value) {
    use std::io::Write;
    static WRITER_INSTANCE: std::sync::OnceLock<Nonce> = std::sync::OnceLock::new();
    let Some(path) = std::env::var_os("UMBRA_FIX_271_EVIDENCE_FILE") else {
        return;
    };
    let mut record = record;
    record["version"] = "shutdown-witness-v1".into();
    record["writer_pid"] = std::process::id().into();
    record["writer_instance"] = WRITER_INSTANCE
        .get_or_init(Nonce::new_v4)
        .to_string()
        .into();
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
