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
    stalled: bool,
    forwarded: usize,
    failed: bool,
    rollback: Option<super::rollback::Rollback>,
}

impl GuardianStartup {
    /// Wait for retained children during pre-GO rollback.
    pub fn wait_for_rollback(&mut self) -> io::Result<()> {
        self.rollback
            .as_mut()
            .ok_or_else(|| invalid("rollback not started"))?
            .wait_for_activity()
    }

    /// Continue failed cleanup after readiness when GO may have reached a child.
    /// Pending child handles stay here if runtime adoption cannot be validated.
    pub fn fail_runtime(&mut self) -> io::Result<super::guard::Outcome> {
        self.failed = true;
        let guard = self
            .guard
            .as_mut()
            .ok_or_else(|| invalid("runtime guard not prepared"))?;
        guard.adopt_ready_children(&mut self.children)?;
        guard.notify_failure()?;
        Ok(self.guard.take().unwrap().run_parent())
    }

    /// Progress pre-GO rollback under the original readiness plus helper cutoff.
    /// Timeout keeps the rollback slot and every uncollected child owned here.
    pub fn rollback(&mut self) -> io::Result<bool> {
        self.failed = true;
        let deadline = self
            .entry
            .readiness_deadline
            .checked_add(9_000_000_000)
            .ok_or_else(|| invalid("rollback clock overflow"))?;
        super::rollback::Rollback::retain(&mut self.children, &mut self.rollback, deadline)?;
        self.entry.close_before_go()?;
        self.guard.take();
        self.bootstrap.take();
        self.rollback.as_mut().unwrap().poll()
    }

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
            stalled: false,
            forwarded: 0,
            failed: false,
            rollback: None,
        })
    }

    /// Advance until child GO delivery and runtime custody transfer complete.
    pub fn poll(&mut self, external_caller: bool) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("guardian startup already failed"));
        }
        let phase = self.phase;
        let result = self.progress(external_caller);
        self.stalled = self.phase == phase;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Local transitions continue immediately; only incomplete exchanges wait.
    pub fn wait_for_activity(&self) -> io::Result<()> {
        if !self.stalled {
            return Ok(());
        }
        match self.phase {
            1 => self
                .bootstrap
                .as_ref()
                .unwrap()
                .wait_for_activity(&self.entry, &self.children),
            2 => self
                .entry
                .wait_for_preparation(self.guard.as_ref().map(Guard::watch_descriptor)),
            3 => self.children[self.forwarded].wait_for_activity(),
            _ => Err(invalid("guardian wait outside startup")),
        }
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
                if self.entry.await_go(u64::MAX)? {
                    self.phase = 3;
                }
            }
            3 => {
                while let Some(child) = self.children.get_mut(self.forwarded) {
                    if !self
                        .entry
                        .forward_go(child, self.entry.authorization_deadline()?)?
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
