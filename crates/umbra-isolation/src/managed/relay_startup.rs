//! Relay barrier and worker-only durable arming before guardian GO.
use super::{
    arming::{RelayArming, Request},
    bootstrap::Bootstrap,
    executable::{Entry, PendingRole},
    guard::Guard,
    invalid,
    startup::Go,
    topology::Purpose,
    Channel,
};
use std::io;
use umbra_core::managed::Role;

/// Retains guardian custody throughout startup and failed arming.
pub struct RelayStartup {
    /// Registered relay and its endpoint assignments.
    pub entry: Entry,
    /// Actual guardian child, retained until runtime adoption.
    pub children: Vec<PendingRole>,
    /// Guard prepared before descendant GO.
    pub guard: Option<Guard>,
    bootstrap: Option<Bootstrap>,
    arming: Option<RelayArming>,
    authorization: Option<Go>,
    ready: bool,
    released: bool,
    failed: bool,
}

impl RelayStartup {
    /// Validate relay role before spawning its separate guardian session.
    pub fn new(entry: Entry) -> io::Result<Self> {
        if entry.role != Role::Relay {
            return Err(invalid("relay startup role mismatch"));
        }
        Ok(Self {
            entry,
            children: Vec::new(),
            guard: None,
            bootstrap: Some(Bootstrap::new(Vec::new(), Vec::new())?),
            arming: None,
            authorization: None,
            ready: false,
            released: false,
            failed: false,
        })
    }

    /// Collect and publish subtree readiness under the inherited deadline.
    pub fn poll_readiness(&mut self, external_caller: bool) -> io::Result<bool> {
        self.step(|state| state.progress_readiness(external_caller))
    }

    fn progress_readiness(&mut self, external_caller: bool) -> io::Result<bool> {
        if self.ready {
            return Ok(true);
        }
        if self.children.is_empty() {
            self.children
                .push(self.entry.spawn_child(Role::Guardian, true)?);
        }
        if !self.bootstrap.as_mut().unwrap().poll(
            &mut self.entry,
            &mut self.children,
            external_caller,
        )? {
            return Ok(false);
        }
        self.ready = self.entry.flush_readiness()?;
        Ok(self.ready)
    }

    /// Request the exact Armed write using the existing worker notification route.
    pub fn begin_arming(&mut self, request: Request) -> io::Result<()> {
        self.step(|state| state.prepare_arming(request))
    }

    fn prepare_arming(&mut self, request: Request) -> io::Result<()> {
        if !self.ready || self.arming.is_some() || request.run != self.entry.launch.run {
            return Err(invalid("relay arming out of order or wrong run"));
        }
        let external = self
            .entry
            .inventory()
            .ok_or_else(|| invalid("relay inventory missing"))?
            .external_caller;
        let matches: Vec<_> = self
            .entry
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.peer == Role::Worker && e.purpose == Purpose::Notification)
            .map(|(index, _)| index)
            .collect();
        let [index] = matches.as_slice() else {
            return Err(invalid("relay worker route missing"));
        };
        let endpoint = self.entry.endpoints.remove(*index);
        self.arming = Some(RelayArming::new(
            Channel::new(endpoint.stream)?,
            request,
            self.entry.readiness_deadline,
            external,
        )?);
        Ok(())
    }

    /// Accept the checked worker response, restore notification ownership, and
    /// release the guardian only after the guard is installed.
    pub fn poll_release(&mut self) -> io::Result<Option<Go>> {
        self.step(Self::progress_release)
    }

    fn progress_release(&mut self) -> io::Result<Option<Go>> {
        if self.released {
            return Ok(self.authorization.clone());
        }
        if self.authorization.is_none() {
            self.authorization = self
                .arming
                .as_mut()
                .ok_or_else(|| invalid("relay not arming"))?
                .poll()?;
            if self.authorization.is_none() {
                return Ok(None);
            }
        }
        if self.guard.is_none() {
            let watch = self
                .bootstrap
                .take()
                .unwrap()
                .into_watch()?
                .ok_or_else(|| invalid("relay startup watch missing"))?;
            let channel = self.arming.take().unwrap().into_channel();
            self.guard = Some(Guard::from_entry_restoring(
                &mut self.entry,
                watch,
                vec![(Role::Worker, channel)],
            )?);
        }
        let go = self.authorization.as_ref().unwrap();
        if !self.children[0].send_authorization(go, self.entry.readiness_deadline)? {
            return Ok(None);
        }
        self.guard
            .as_mut()
            .unwrap()
            .adopt_ready_children(&mut self.children)?;
        self.released = true;
        Ok(self.authorization.clone())
    }

    fn step<T>(&mut self, advance: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        if self.failed {
            return Err(invalid("relay startup already failed"));
        }
        let result = advance(self);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
