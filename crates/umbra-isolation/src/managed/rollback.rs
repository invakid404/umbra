//! Bounded pre-GO bootstrap closure with retained actual-parent custody.
use super::{boot_nanos, executable::PendingRole, invalid, process::SpawnedChild};
use std::io;

/// Closing bootstrap asks descendants to roll back recursively. Expiry reports
/// unresolved custody; it never drops or signals a parent of live descendants.
pub struct Rollback {
    children: Vec<SpawnedChild>,
    statuses: Vec<Option<i32>>,
    deadline: u64,
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
