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

#[derive(serde::Serialize, serde::Deserialize)]
enum Response {
    PersistenceStarted { run: RunIdentity, deadline: u64 },
    Authorized(Go),
}

/// Relay side of the existing worker-relay startup exchange. Validates the
/// exact proposed record before forwarding the worker's durable authorization.
pub struct RelayArming {
    channel: Channel,
    request: Request,
    local: Participant,
    readiness_deadline: u64,
    response_deadline: Option<u64>,
    sent: bool,
    capacity: u64,
    failed: bool,
    authorization: Option<Go>,
}

impl RelayArming {
    /// Wait for the request write or response using that phase's original cutoff.
    pub(super) fn wait_for_activity(
        &self,
        entry: &super::executable::Entry,
        watch: Option<i32>,
    ) -> io::Result<()> {
        entry.wait_for_preparation_route(
            &self.channel,
            watch,
            if self.sent {
                self.response_deadline.unwrap_or(u64::MAX)
            } else {
                self.readiness_deadline
            },
        )
    }

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
            startup.validate_arming_record(&request.record)?;
            let umbra_core::CustodyPhase::Armed { capacity, .. } = &request.record.state else {
                return Err(invalid("relay arming requires Armed record"));
            };
            let capacity = *capacity;
            channel.queue(&request)?;
            Ok((local, capacity))
        })();
        let (local, capacity) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err((error, channel)),
        };
        Ok(Self {
            channel,
            request,
            local,
            readiness_deadline,
            response_deadline: None,
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
            let Some(response) = self
                .channel
                .receive::<Response>(self.response_deadline.unwrap_or(u64::MAX))?
            else {
                return Ok(None);
            };
            let go = match response {
                Response::PersistenceStarted { run, deadline } => {
                    let now = boot_nanos()?;
                    if self.response_deadline.is_some()
                        || run != self.request.run
                        || deadline <= now
                        || deadline > now.saturating_add(5_000_000_000)
                    {
                        return Err(invalid("invalid persistence allocation"));
                    }
                    self.response_deadline = Some(deadline);
                    return Ok(None);
                }
                Response::Authorized(go) => go,
            };
            go.validate(self.request.run, self.local, self.capacity)?;
            if go.record != self.request.record || Some(go.deadline) != self.response_deadline {
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
    persistence_deadline: Option<u64>,
    attempted: bool,
    failed: bool,
    authorization: Option<Go>,
    validated: Option<(Startup, umbra_core::CustodyRecord)>,
}

impl WorkerArming {
    pub(super) fn persistence_deadline(&self) -> io::Result<u64> {
        self.persistence_deadline
            .ok_or_else(|| invalid("persistence not started"))
    }

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
            persistence_deadline: None,
            attempted: false,
            failed: false,
            authorization: None,
            validated: None,
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
        if !self.capture_readiness()? {
            return Ok(false);
        }
        if self.persistence_deadline.is_none() {
            let (startup, record) = self.validated.as_mut().unwrap();
            let deadline = startup.begin_arming(record.clone())?;
            self.persistence_deadline = Some(deadline);
            self.channel.queue(&Response::PersistenceStarted {
                run: self.run,
                deadline,
            })?;
        }
        let deadline = self.persistence_deadline.unwrap();
        if !self.channel.flush(deadline)? {
            return Ok(false);
        }
        self.attempted = true;
        let (mut startup, record) = self.validated.take().unwrap();
        let sequence = persist(&record, deadline)?;
        startup.commit_armed(&record, sequence)?;
        let go = startup.authorization(self.run)?;
        go.validate(self.run, self.local, self.capacity)?;
        self.channel.queue(&Response::Authorized(go.clone()))?;
        let sent = self.channel.flush(go.deadline)?;
        self.authorization = Some(go);
        Ok(sent)
    }

    /// Wait for complete readiness without extending the inherited cutoff.
    pub fn wait_for_readiness(&self) -> io::Result<()> {
        self.channel.wait_until(self.readiness_deadline)
    }

    /// Receive and validate complete readiness before namespace preparation.
    /// This preserves the original cutoff; it grants no durable authorization.
    pub fn capture_readiness(&mut self) -> io::Result<bool> {
        if self.failed || self.attempted {
            return Err(invalid("readiness capture after persistence or failure"));
        }
        if self.validated.is_some() {
            return Ok(true);
        }
        let result = self.capture_readiness_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn capture_readiness_inner(&mut self) -> io::Result<bool> {
        let Some(request) = self.channel.receive::<Request>(self.readiness_deadline)? else {
            return Ok(false);
        };
        if request.run != self.run
            || !request
                .readiness
                .iter()
                .any(|ready| ready.participant == self.local)
            || !matches!(&request.record.state, umbra_core::CustodyPhase::Armed { capacity, .. } if *capacity == self.capacity)
        {
            return Err(invalid("worker arming request identity mismatch"));
        }
        let startup = Startup::from_readiness(
            self.run,
            self.external_caller,
            self.readiness_deadline,
            request.readiness,
        )?;
        self.validated = Some((startup, request.record));
        Ok(true)
    }

    /// Retain the exchange endpoint for actual-parent rollback after failure.
    pub fn into_channel(self) -> Channel {
        self.channel
    }
}
