//! Worker-only persistence request and durable authorization response.
use super::{
    boot_nanos, invalid,
    process::identity,
    startup::{Go, Ready, Startup},
    Channel,
};
use std::io;
use umbra_core::managed::{Participant, Role, RunIdentity};

/// Complete readiness and proposed Armed record, supplied before the inherited
/// readiness cutoff. The request does not choose or renew that cutoff.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Request {
    /// Run bound at executable entry.
    pub run: RunIdentity,
    /// Complete role readiness receipts, revalidated against the kernel.
    pub readiness: Vec<Ready>,
    /// Exact Armed record to persist through the authoritative namespace.
    pub record: umbra_core::CustodyRecord,
}

/// Relay side of the existing worker-relay startup exchange. Validates the
/// exact proposed record before forwarding the worker's durable authorization.
pub struct RelayArming {
    channel: Channel,
    request: Request,
    local: Participant,
    readiness_deadline: u64,
    response_deadline: u64,
    sent: bool,
    capacity: u64,
    failed: bool,
    authorization: Option<Go>,
}

impl RelayArming {
    /// Queue one immutable request after validating complete role readiness.
    pub fn new(
        channel: Channel,
        request: Request,
        readiness_deadline: u64,
        external_caller: bool,
    ) -> io::Result<Self> {
        Self::new_retaining(channel, request, readiness_deadline, external_caller)
            .map_err(|(error, _)| error)
    }

    /// Keep the assigned route available for failed startup notification.
    pub fn new_retaining(
        mut channel: Channel,
        request: Request,
        readiness_deadline: u64,
        external_caller: bool,
    ) -> Result<Self, (io::Error, Channel)> {
        let prepared = (|| -> io::Result<_> {
            let local = identity(std::process::id(), Role::Relay)?;
            if !request
                .readiness
                .iter()
                .any(|ready| ready.participant == local)
            {
                return Err(invalid("relay absent from arming readiness"));
            }
            let mut startup = Startup::from_readiness(
                request.run,
                external_caller,
                readiness_deadline,
                request.readiness.clone(),
            )?;
            startup.begin_arming(request.record.clone())?;
            let umbra_core::CustodyPhase::Armed { capacity, .. } = &request.record.state else {
                return Err(invalid("relay arming requires Armed record"));
            };
            let capacity = *capacity;
            let response_deadline = readiness_deadline
                .checked_add(5_000_000_000)
                .ok_or_else(|| invalid("relay arming clock overflow"))?;
            channel.queue(&request)?;
            Ok((local, capacity, response_deadline))
        })();
        let (local, capacity, response_deadline) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err((error, channel)),
        };
        Ok(Self {
            channel,
            request,
            local,
            readiness_deadline,
            response_deadline,
            capacity,
            sent: false,
            failed: false,
            authorization: None,
        })
    }

    /// Progress without renewing readiness, persistence, or GO deadlines.
    pub fn poll(&mut self) -> io::Result<Option<Go>> {
        if self.failed {
            return Err(invalid("relay arming already failed"));
        }
        let result = (|| {
            if let Some(go) = &self.authorization {
                return Ok(Some(go.clone()));
            }
            if !self.sent {
                if !self.channel.flush(self.readiness_deadline)? {
                    return Ok(None);
                }
                self.sent = true;
            }
            let Some(go) = self.channel.receive::<Go>(self.response_deadline)? else {
                return Ok(None);
            };
            go.validate(self.request.run, self.local, self.capacity)?;
            if go.record != self.request.record || go.deadline > self.response_deadline {
                return Err(invalid("worker authorization changed record or cutoff"));
            }
            self.authorization = Some(go.clone());
            Ok(Some(go))
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Return the assigned endpoint for runtime notification or rollback.
    pub fn into_channel(self) -> Channel {
        self.channel
    }
}

/// Owns a dedicated startup exchange. Errors are terminal and a persistence
/// callback is invoked at most once, including after an ambiguous write.
pub struct WorkerArming {
    channel: Channel,
    run: RunIdentity,
    local: Participant,
    capacity: u64,
    external_caller: bool,
    readiness_deadline: u64,
    attempted: bool,
    failed: bool,
    authorization: Option<Go>,
}

impl WorkerArming {
    /// Adopt an existing assigned startup endpoint without creating a new pair.
    pub fn new(
        channel: Channel,
        run: RunIdentity,
        capacity: u64,
        external_caller: bool,
        readiness_deadline: u64,
    ) -> io::Result<Self> {
        run.validate().map_err(invalid)?;
        if capacity == 0 || boot_nanos()? >= readiness_deadline {
            return Err(invalid("invalid worker arming configuration"));
        }
        Ok(Self {
            channel,
            run,
            capacity,
            external_caller,
            readiness_deadline,
            local: identity(std::process::id(), Role::Worker)?,
            attempted: false,
            failed: false,
            authorization: None,
        })
    }

    /// True only after the durable authorization response is fully sent.
    /// The callback must use the authoritative namespace and supplied cutoff.
    pub fn poll(
        &mut self,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<bool> {
        let result = self.progress(persist);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn progress(
        &mut self,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("worker arming already failed"));
        }
        if let Some(go) = &self.authorization {
            return self.channel.flush(go.deadline);
        }
        if self.attempted {
            return Err(invalid("worker persistence already attempted"));
        }
        let Some(request) = self.channel.receive::<Request>(self.readiness_deadline)? else {
            return Ok(false);
        };
        self.attempted = true;
        if request.run != self.run
            || !request
                .readiness
                .iter()
                .any(|ready| ready.participant == self.local)
            || !matches!(&request.record.state, umbra_core::CustodyPhase::Armed { capacity, .. } if *capacity == self.capacity)
        {
            return Err(invalid("worker arming request identity mismatch"));
        }
        let mut startup = Startup::from_readiness(
            self.run,
            self.external_caller,
            self.readiness_deadline,
            request.readiness,
        )?;
        let go = startup.persist_armed(self.run, request.record, persist)?;
        go.validate(self.run, self.local, self.capacity)?;
        self.channel.queue(&go)?;
        let sent = self.channel.flush(go.deadline)?;
        self.authorization = Some(go);
        Ok(sent)
    }

    /// Retain the exchange endpoint for actual-parent rollback after failure.
    pub fn into_channel(self) -> Channel {
        self.channel
    }
}
