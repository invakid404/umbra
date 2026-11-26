//! Owner-side independent admission endpoint.
use super::{invalid, Channel};
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use umbra_core::managed::RunIdentity;
use umbra_core::{
    AdmissionError, AdmissionLedger, AdmissionOperation, AdmissionRequest, AdmissionResponse,
};

/// Serializes provider admission with owner freeze; never releases capacity on
/// transport loss or a missing response.
pub struct AdmissionService {
    ledger: AdmissionLedger,
    channel: Channel,
    failed: bool,
    cutoff: u64,
}

/// Shared provider view of the owner's cutoff. Updates can only shorten it.
#[derive(Clone)]
pub struct AdmissionCutoff(Arc<AtomicU64>);

impl AdmissionCutoff {
    /// Retain an independently validated Start ticket or owner phase cutoff.
    /// Later notifications cannot renew the first finite deadline.
    pub fn retain(&self, deadline: u64) -> io::Result<()> {
        if deadline == 0 {
            return Err(invalid("invalid admission cutoff"));
        }
        self.0.fetch_min(deadline, Ordering::AcqRel);
        Ok(())
    }

    /// Clamp a caller's existing operation deadline without creating a timer.
    pub fn clamp(&self, deadline: u64) -> u64 {
        deadline.min(self.0.load(Ordering::Acquire))
    }
}

/// Provider endpoint with at most one unacknowledged transition. A transport
/// failure never turns an ambiguous entry into permission to acquire.
pub struct AdmissionClient {
    run: RunIdentity,
    channel: Channel,
    sequence: u64,
    pending: Option<AdmissionRequest>,
    pending_deadline: Option<u64>,
    failed: bool,
    cutoff: AdmissionCutoff,
}

impl AdmissionClient {
    /// Bind the provider's inherited admission endpoint to the enrolled run.
    pub fn new(run: RunIdentity, channel: Channel) -> io::Result<Self> {
        run.validate().map_err(invalid)?;
        Ok(Self {
            run,
            channel,
            sequence: 0,
            pending: None,
            pending_deadline: None,
            failed: false,
            cutoff: AdmissionCutoff(Arc::new(AtomicU64::new(u64::MAX))),
        })
    }

    /// Share the retained owner cutoff with the native ledger deadline callback.
    pub fn cutoff(&self) -> AdmissionCutoff {
        self.cutoff.clone()
    }

    /// Queue a transition. This is not an acquisition grant: the caller must
    /// receive its matching successful acknowledgement before any side effect.
    pub fn begin(&mut self, operation: AdmissionOperation) -> io::Result<()> {
        if self.failed || self.pending.is_some() {
            return Err(invalid("failed or occupied admission endpoint"));
        }
        let sequence = self.sequence.checked_add(1).ok_or_else(|| {
            self.failed = true;
            invalid("admission sequence exhausted")
        })?;
        let request = AdmissionRequest {
            nonce: self.run.nonce,
            generation: self.run.generation,
            sequence,
            operation,
        };
        if let Err(error) = self.channel.queue(&request) {
            self.failed = true;
            return Err(error);
        }
        self.pending = Some(request);
        Ok(())
    }

    /// Progress under the original operation cutoff. Only a matching response
    /// commits the local sequence; EOF, timeout and mismatches fail permanently.
    pub fn poll(&mut self, deadline: u64) -> io::Result<Option<Result<u64, AdmissionError>>> {
        let deadline = self.cutoff.clamp(
            self.pending_deadline
                .map_or(deadline, |original| original.min(deadline)),
        );
        if self.failed {
            return Err(invalid("admission endpoint already failed"));
        }
        let request = self
            .pending
            .as_ref()
            .ok_or_else(|| invalid("no pending admission operation"))?;
        self.pending_deadline = Some(deadline);
        let result = (|| {
            if !self.channel.flush(deadline)? {
                return Ok(None);
            }
            let Some(response) = self.channel.receive::<AdmissionResponse>(deadline)? else {
                return Ok(None);
            };
            if response.request != *request {
                return Err(invalid("admission acknowledgement mismatch"));
            }
            self.cutoff.retain(response.cutoff)?;
            if super::boot_nanos()? >= self.cutoff.clamp(deadline) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            Ok(Some(response.result))
        })();
        match &result {
            Ok(Some(_)) => {
                self.sequence = request.sequence;
                self.pending = None;
                self.pending_deadline = None;
            }
            Err(_) => self.failed = true,
            Ok(None) => {}
        }
        result
    }
}

impl umbra_core::AdmissionTransport for AdmissionClient {
    fn exchange(
        &mut self,
        operation: AdmissionOperation,
        deadline: u64,
    ) -> io::Result<Result<u64, AdmissionError>> {
        #[cfg(feature = "shutdown-witness")]
        let timing = (std::time::Instant::now(), operation.clone());
        self.begin(operation)?;
        loop {
            if let Some(result) = self.poll(deadline)? {
                #[cfg(feature = "shutdown-witness")]
                {
                    let nanos = u64::try_from(timing.0.elapsed().as_nanos())
                        .expect("admission measurement overflow");
                    crate::performance_observation::record_details(
                        "admission-client",
                        "admission_exchange",
                        None,
                        serde_json::json!({
                            "operation": timing.1,
                            "nanos": nanos,
                            "accepted": result.is_ok(),
                        }),
                    );
                }
                return Ok(result);
            }
            // A provider is thread-affine. Yield while the separate owner
            // services this request; poll always checks the original cutoff.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl AdmissionService {
    /// Adopt the assigned owner-provider admission channel before GO.
    pub fn new(run: RunIdentity, capacity: u64, channel: Channel) -> io::Result<Self> {
        let ledger = AdmissionLedger::new(run.nonce, run.generation, capacity)
            .map_err(|_| invalid("invalid owner admission configuration"))?;
        Ok(Self {
            ledger,
            channel,
            failed: false,
            cutoff: u64::MAX,
        })
    }

    /// One nonblocking request/response step under the caller's absolute cutoff.
    pub fn poll(&mut self, deadline: u64) -> io::Result<()> {
        self.cutoff = self.cutoff.min(deadline);
        let deadline = self.cutoff;
        if self.failed {
            return Err(invalid("owner admission channel already failed"));
        }
        let result: io::Result<()> = (|| {
            if !self.channel.flush(deadline)? {
                return Ok(());
            }
            if let Some(request) = self.channel.receive::<AdmissionRequest>(deadline)? {
                let result = self.ledger.apply(request.clone());
                self.channel.queue(&AdmissionResponse {
                    request,
                    result,
                    cutoff: self.cutoff,
                })?;
                self.channel.flush(deadline)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
            self.ledger.freeze();
            if result.as_ref().is_err_and(|error| {
                error.kind() == io::ErrorKind::UnexpectedEof
                    && self.channel.handoff_stream().is_err()
            }) {
                return Err(invalid("truncated admission exchange"));
            }
        }
        result
    }

    /// Stop new grants and entries before any cancellation or census operation.
    pub fn freeze(&mut self) -> u64 {
        self.ledger.freeze()
    }

    /// Includes all unproven, entered and retiring obligations.
    pub fn outstanding(&self) -> u64 {
        self.ledger.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use umbra_core::shutdown::Nonce as Uuid;
    use umbra_core::TerminalCertificate;

    fn endpoints() -> (AdmissionClient, AdmissionService) {
        let run = RunIdentity {
            nonce: Uuid::from_u128(1),
            generation: 1,
        };
        let (provider, owner) = UnixStream::pair().unwrap();
        (
            AdmissionClient::new(run, Channel::new(provider).unwrap()).unwrap(),
            AdmissionService::new(run, 1, Channel::new(owner).unwrap()).unwrap(),
        )
    }

    fn exchange(
        client: &mut AdmissionClient,
        service: &mut AdmissionService,
        operation: AdmissionOperation,
    ) -> Result<u64, AdmissionError> {
        client.begin(operation).unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 1_000_000_000;
        loop {
            if let Some(result) = client.poll(deadline).unwrap() {
                return result;
            }
            service.poll(deadline).unwrap();
        }
    }

    #[test]
    fn freeze_between_grant_and_entry_retains_charge_until_terminal_proof() {
        let (mut client, mut owner) = endpoints();
        let cutoff = client.cutoff();
        let obligation = Uuid::from_u128(2);
        assert_eq!(
            exchange(
                &mut client,
                &mut owner,
                AdmissionOperation::Reserve { obligation }
            ),
            Ok(1)
        );
        assert_eq!(owner.freeze(), 1);
        let original_cutoff = cutoff.clamp(u64::MAX);
        assert_ne!(original_cutoff, u64::MAX);
        assert_eq!(
            exchange(
                &mut client,
                &mut owner,
                AdmissionOperation::Entering {
                    obligation,
                    resource: Uuid::from_u128(3),
                }
            ),
            Err(AdmissionError::Frozen)
        );
        assert_eq!(owner.outstanding(), 1);
        assert_eq!(cutoff.clamp(u64::MAX), original_cutoff);
        assert_eq!(
            exchange(
                &mut client,
                &mut owner,
                AdmissionOperation::Retiring { obligation }
            ),
            Ok(1)
        );
        assert_eq!(
            exchange(
                &mut client,
                &mut owner,
                AdmissionOperation::Release {
                    obligation,
                    certificate: TerminalCertificate::CancelledBeforeEntry,
                }
            ),
            Ok(0)
        );
    }

    #[test]
    fn lost_entry_reply_preserves_owner_charge() {
        let (mut client, mut owner) = endpoints();
        let obligation = Uuid::from_u128(2);
        exchange(
            &mut client,
            &mut owner,
            AdmissionOperation::Reserve { obligation },
        )
        .unwrap();
        client
            .begin(AdmissionOperation::Entering {
                obligation,
                resource: Uuid::from_u128(3),
            })
            .unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 1_000_000_000;
        assert!(client.poll(deadline).unwrap().is_none());
        owner.poll(deadline).unwrap();
        // The owner committed entry, but the provider never consumed its ACK.
        drop(client);
        // Socket closure becomes observable asynchronously; each poll remains
        // bounded by the same cutoff and must preserve the committed charge.
        let error = loop {
            match owner.poll(deadline) {
                Ok(()) => {
                    assert_eq!(owner.outstanding(), 1);
                    std::thread::yield_now();
                }
                Err(error) => break error,
            }
        };
        assert_ne!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(owner.outstanding(), 1);
        assert_eq!(owner.freeze(), 1);
    }

    #[test]
    fn pending_request_cannot_renew_its_original_deadline() {
        let (mut client, _owner) = endpoints();
        client
            .begin(AdmissionOperation::Reserve {
                obligation: Uuid::from_u128(2),
            })
            .unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 50_000_000;
        assert!(client.poll(deadline).unwrap().is_none());
        while super::super::boot_nanos().unwrap() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            client.poll(u64::MAX).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(client.begin(AdmissionOperation::Freeze).is_err());
    }

    #[test]
    fn independent_start_cutoff_is_shared_and_never_renewed() {
        let (client, _owner) = endpoints();
        let provider = client.cutoff();
        let notification = provider.clone();
        notification.retain(100).unwrap();
        assert_eq!(provider.clamp(u64::MAX), 100);
        notification.retain(200).unwrap();
        assert_eq!(provider.clamp(u64::MAX), 100);
        assert!(notification.retain(0).is_err());
        assert_eq!(provider.clamp(u64::MAX), 100);
        notification.retain(50).unwrap();
        assert_eq!(provider.clamp(75), 50);
        assert_eq!(provider.clamp(25), 25);
    }

    #[test]
    fn mismatched_response_never_authorizes_entry() {
        let run = RunIdentity {
            nonce: Uuid::from_u128(1),
            generation: 1,
        };
        let (provider, owner) = UnixStream::pair().unwrap();
        let mut client = AdmissionClient::new(run, Channel::new(provider).unwrap()).unwrap();
        let mut peer = Channel::new(owner).unwrap();
        client
            .begin(AdmissionOperation::Reserve {
                obligation: Uuid::from_u128(2),
            })
            .unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 1_000_000_000;
        assert!(client.poll(deadline).unwrap().is_none());
        let mut request = peer.receive::<AdmissionRequest>(deadline).unwrap().unwrap();
        request.sequence += 1;
        peer.queue(&AdmissionResponse {
            request,
            result: Ok(1),
            cutoff: u64::MAX,
        })
        .unwrap();
        assert!(peer.flush(deadline).unwrap());
        assert!(client.poll(deadline).is_err());
        assert!(client.begin(AdmissionOperation::Freeze).is_err());
    }

    #[test]
    fn partial_frame_eof_is_not_an_orderly_empty_ledger_close() {
        use std::io::Write;
        let run = RunIdentity {
            nonce: Uuid::from_u128(1),
            generation: 1,
        };
        let (mut provider, owner) = UnixStream::pair().unwrap();
        let mut service = AdmissionService::new(run, 1, Channel::new(owner).unwrap()).unwrap();
        provider.write_all(&[0, 0]).unwrap();
        provider.shutdown(std::net::Shutdown::Write).unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 1_000_000_000;
        let error = loop {
            match service.poll(deadline) {
                Ok(()) => std::thread::yield_now(),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(service.outstanding(), 0);
    }
}
