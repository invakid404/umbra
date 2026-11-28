//! Bounded pre-GO bootstrap closure with retained actual-parent custody.
use super::{boot_nanos, executable::PendingRole, invalid, process::SpawnedChild};
use std::io;

/// Closing bootstrap asks descendants to roll back recursively. Expiry reports
/// unresolved custody; it never drops or signals a parent of live descendants.
pub struct Rollback {
    children: Vec<SpawnedChild>,
    statuses: Vec<Option<i32>>,
    deadline: u64,
    roles: Vec<umbra_core::managed::Role>,
    watch: Option<super::process::ProcessWatch>,
}

impl Rollback {
    /// Move custody into a retained rollback slot, restoring the original
    /// handles when any child has already entered the GO phase.
    pub fn retain(
        children: &mut Vec<PendingRole>,
        slot: &mut Option<Self>,
        deadline: u64,
    ) -> io::Result<()> {
        if slot.is_some() {
            return Ok(());
        }
        match Self::begin(std::mem::take(children), deadline) {
            Ok(rollback) => {
                *slot = Some(rollback);
                Ok(())
            }
            Err((error, retained)) => {
                *children = retained;
                Err(error)
            }
        }
    }

    /// Prepared children share one failure observation; retries reuse each
    /// retained cutoff and an existing rollback slot is never reallocated.
    pub(super) fn retain_prepared(
        children: &mut Vec<PendingRole>,
        slot: &mut Option<Self>,
        original: u64,
    ) -> io::Result<()> {
        if slot.is_some() {
            return Ok(());
        }
        let now = boot_nanos()?;
        let mut cutoff = None;
        for child in children.iter_mut() {
            let child_cutoff = child.preparation_rollback_cutoff(now)?;
            cutoff = Some(cutoff.map_or(child_cutoff, |old: u64| old.min(child_cutoff)));
        }
        Self::retain(children, slot, cutoff.unwrap_or(original))
    }

    pub(super) fn deadline(&self) -> u64 {
        self.deadline
    }

    /// Close all bootstrap channels together, sharing the supplied original
    /// cutoff. A GO already queued makes pre-GO rollback invalid.
    pub fn begin(
        children: Vec<PendingRole>,
        deadline: u64,
    ) -> Result<Self, (io::Error, Vec<PendingRole>)> {
        if children
            .iter()
            .any(|child| !child.permits_rollback(deadline))
        {
            return Err((invalid("invalid pre-GO rollback phase or cutoff"), children));
        }
        Ok(Self {
            statuses: vec![None; children.len()],
            roles: children.iter().map(PendingRole::role).collect(),
            watch: None,
            children: children.into_iter().map(PendingRole::into_child).collect(),
            deadline,
        })
    }

    /// Poll every real child once; no exit watch substitutes for these receipts.
    /// Errors leave all handles owned here for reporting and continued custody.
    pub fn poll(&mut self) -> io::Result<bool> {
        let mut failure = None;
        for (child, status) in self.children.iter_mut().zip(&mut self.statuses) {
            if status.is_none() {
                match child.collect() {
                    Ok(found) => *status = found,
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if self.statuses.iter().all(Option::is_some) {
            return Ok(true);
        }
        if boot_nanos()? >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(false)
    }

    /// Watch actual children without replacing their required parent waits.
    pub fn wait_for_activity(&mut self) -> io::Result<()> {
        if self.poll()? {
            return Ok(());
        }
        if self.watch.is_none() {
            self.watch = Some(super::process::ProcessWatch::new()?);
        }
        let watch = self.watch.as_mut().unwrap();
        for ((child, status), role) in self
            .children
            .iter_mut()
            .zip(&mut self.statuses)
            .zip(&self.roles)
        {
            if status.is_some() {
                continue;
            }
            let installed = super::process::identity(child.pid(), *role).and_then(|participant| {
                if watch.covers(&[participant]) {
                    Ok(())
                } else {
                    watch.install(participant)
                }
            });
            if let Err(error) = installed {
                // Exit can race watch installation. Only the parent receipt resolves it.
                *status = child.collect()?;
                if status.is_none() {
                    return Err(error);
                }
            }
        }
        if self.statuses.iter().all(Option::is_some) {
            return Ok(());
        }
        super::readiness::wait_descriptors(
            &mut [libc::pollfd {
                fd: watch.descriptor(),
                events: libc::POLLIN,
                revents: 0,
            }],
            self.deadline,
        )?;
        while watch.exited()?.is_some() {}
        Ok(())
    }

    /// Actual wait statuses in original child order; absent means unresolved.
    pub fn statuses(&self) -> &[Option<i32>] {
        &self.statuses
    }

    /// Keep uncollected child custody explicit on failure or deadline expiry.
    pub fn into_unresolved(self) -> Vec<SpawnedChild> {
        self.children
            .into_iter()
            .zip(self.statuses)
            .filter_map(|(child, status)| status.is_none().then_some(child))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::managed::Role;

    #[test]
    fn exit_wait_preserves_cutoff_and_actual_child_custody() {
        let child = SpawnedChild::spawn(
            std::path::Path::new("/bin/sleep"),
            &["sleep".into(), "5".into()],
            &[],
            false,
            &[],
        )
        .unwrap();
        let participant = super::super::process::identity(child.pid(), Role::Worker).unwrap();
        let deadline = boot_nanos().unwrap() + 20_000_000;
        let mut rollback = Rollback {
            children: vec![child],
            statuses: vec![None],
            deadline,
            roles: vec![Role::Worker],
            watch: None,
        };
        let waited = rollback.wait_for_activity();
        let finished = boot_nanos().unwrap();
        let expired = rollback.poll();
        let retained = rollback.statuses[0].is_none();
        // End and collect the owned fixture before checking the deadline assertions.
        super::super::process::signal_current(participant, libc::SIGKILL).unwrap();
        let cleanup_cutoff = boot_nanos().unwrap() + 1_000_000_000;
        while rollback.children[0].collect().unwrap().is_none() {
            assert!(boot_nanos().unwrap() < cleanup_cutoff);
            if let Some(watch) = &rollback.watch {
                super::super::readiness::wait_descriptors(
                    &mut [libc::pollfd {
                        fd: watch.descriptor(),
                        events: libc::POLLIN,
                        revents: 0,
                    }],
                    cleanup_cutoff,
                )
                .unwrap();
            }
        }
        waited.unwrap();
        assert_eq!(expired.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(retained);
        assert_eq!(rollback.deadline, deadline);
        assert!(
            finished < deadline + 500_000_000,
            "wait exceeded its original cutoff"
        );
    }
}
