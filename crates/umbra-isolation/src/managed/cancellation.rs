//! Independent provider cancellation with the original cleanup ticket.
use super::{admission::AdmissionCutoff, invalid, Channel};
use std::io;
use umbra_core::managed::{RunIdentity, Ticket};

#[derive(serde::Serialize, serde::Deserialize)]
/// Owner's cancellation request on its dedicated inherited route.
pub struct Notification {
    /// Enrolled run.
    pub run: RunIdentity,
    /// Original committed cleanup allocation.
    pub ticket: Ticket,
}

/// Provider side of the dedicated cancellation route.
pub struct Receiver {
    run: RunIdentity,
    capacity: u64,
    channel: Channel,
    cutoff: AdmissionCutoff,
}

impl Receiver {
    /// Bind the inherited endpoint to run identity and shared cutoff.
    pub fn new(
        run: RunIdentity,
        capacity: u64,
        channel: Channel,
        cutoff: AdmissionCutoff,
    ) -> io::Result<Self> {
        run.validate().map_err(invalid)?;
        umbra_core::managed::CleanupPath::Failed
            .allocation_nanos(capacity)
            .map_err(invalid)?;
        Ok(Self {
            run,
            capacity,
            channel,
            cutoff,
        })
    }

    /// Validate and retain the ticket before making cancellation observable.
    pub fn poll(&mut self, deadline: u64) -> io::Result<bool> {
        let Some(message) = self.channel.receive::<Notification>(deadline)? else {
            return Ok(false);
        };
        if message.run != self.run {
            return Err(invalid("cancellation run mismatch"));
        }
        message.ticket.validate(self.capacity).map_err(invalid)?;
        self.cutoff.retain(
            message
                .ticket
                .cutoffs(self.capacity)
                .map_err(invalid)?
                .fallback,
        )?;
        Ok(true)
    }
}
