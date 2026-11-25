//! Guardian startup with retained worker and cleanup-owner child custody.
use super::{
    bootstrap::Bootstrap,
    executable::{Entry, PendingRole},
    guard::Guard,
    invalid,
};
use std::io;
use umbra_core::managed::Role;

/// Every failed step leaves actual child handles available for rollback.
pub struct GuardianStartup {
    /// Registered guardian and remaining bootstrap authority.
    pub entry: Entry,
    /// Children not yet transferred to the runtime guard.
    pub children: Vec<PendingRole>,
    /// Runtime guard retains children once startup has completed.
    pub guard: Option<Guard>,
    bootstrap: Option<Bootstrap>,
    phase: u8,
    forwarded: usize,
    failed: bool,
}

impl GuardianStartup {
    /// Validate the role before spawning either child.
    pub fn new(entry: Entry) -> io::Result<Self> {
        if entry.role != Role::Guardian {
            return Err(invalid("guardian startup role mismatch"));
        }
        Ok(Self {
            entry,
            children: Vec::new(),
            guard: None,
            bootstrap: Some(Bootstrap::new(Vec::new(), Vec::new())?),
            phase: 0,
            forwarded: 0,
            failed: false,
        })
    }

    /// Advance until child GO delivery and runtime custody transfer complete.
    pub fn poll(&mut self, external_caller: bool) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("guardian startup already failed"));
        }
        let result = self.progress(external_caller);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn progress(&mut self, external_caller: bool) -> io::Result<bool> {
        match self.phase {
            0 => {
                if self.children.is_empty() {
                    self.children
                        .push(self.entry.spawn_child(Role::Worker, false)?);
                }
                self.children
                    .push(self.entry.spawn_child(Role::CleanupOwner, false)?);
                self.phase = 1;
            }
            1 => {
                if self.bootstrap.as_mut().unwrap().poll(
                    &mut self.entry,
                    &mut self.children,
                    external_caller,
                )? {
                    let watch = self
                        .bootstrap
                        .take()
                        .unwrap()
                        .into_watch()?
                        .ok_or_else(|| invalid("guardian startup watch missing"))?;
                    self.guard = Some(Guard::from_entry(&mut self.entry, watch)?);
                    self.phase = 2;
                }
            }
            2 => {
                if self.entry.await_go(self.entry.readiness_deadline)? {
                    self.phase = 3;
                }
            }
            3 => {
                while let Some(child) = self.children.get_mut(self.forwarded) {
                    if !self
                        .entry
                        .forward_go(child, self.entry.readiness_deadline)?
                    {
                        return Ok(false);
                    }
                    self.forwarded += 1;
                }
                self.guard
                    .as_mut()
                    .unwrap()
                    .adopt_ready_children(&mut self.children)?;
                self.phase = 4;
            }
            4 => return Ok(true),
            _ => unreachable!(),
        }
        Ok(self.phase == 4)
    }
}
