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
    failed_route: Option<Channel>,
    authorization: Option<Go>,
    ready: bool,
    released: bool,
    failed: bool,
    rollback: Option<super::rollback::Rollback>,
}

impl RelayStartup {
    /// Continue failed cleanup after readiness when GO may have reached a child.
    /// Pending child handles stay here if runtime adoption cannot be validated.
    pub fn fail_runtime(&mut self) -> io::Result<super::guard::Outcome> {
        self.failed = true;
        self.prepare_guard()?;
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
            failed_route: None,
            authorization: None,
            ready: false,
            released: false,
            failed: false,
            rollback: None,
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

    /// Bind the caller's storage identity to the actual startup receipts.
    /// A library caller supplies its installed-watch receipt separately.
    pub fn arm_storage(
        &mut self,
        run_id: umbra_core::RunId,
        caller: Option<super::startup::Ready>,
    ) -> io::Result<()> {
        self.step(|state| {
            if run_id.0.is_nil() {
                return Err(invalid("missing storage run identity"));
            }
            let mut readiness = state
                .entry
                .readiness()
                .ok_or_else(|| invalid("relay readiness missing"))?
                .to_vec();
            if let Some(caller) = caller {
                if caller.participant.role != Role::Caller {
                    return Err(invalid("external readiness is not caller"));
                }
                readiness.push(caller);
            }
            readiness.sort_by_key(|receipt| receipt.participant.role);
            let participants = readiness
                .iter()
                .map(|receipt| {
                    let identity = receipt.participant;
                    Ok(umbra_core::CustodyParticipant {
                        pid: identity.pid,
                        birth: identity
                            .birth
                            .0
                            .checked_mul(1_000_000)
                            .and_then(|value| value.checked_add(identity.birth.1))
                            .ok_or_else(|| invalid("custody birth overflow"))?,
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            let record = umbra_core::CustodyRecord {
                run_id,
                generation: uuid::Uuid::new_v4(),
                sequence: 1,
                state: umbra_core::CustodyPhase::Armed {
                    capacity: state.entry.launch.capacity,
                    startup_nonce: state.entry.launch.run.nonce,
                    participants,
                },
            };
            state.prepare_arming(Request {
                run: state.entry.launch.run,
                readiness,
                record,
            })
        })
    }

    fn prepare_arming(&mut self, request: Request) -> io::Result<()> {
        if !self.ready || self.arming.is_some() || request.run != self.entry.launch.run {
            return Err(invalid("relay arming out of order or wrong run"));
        }
        if !matches!(&request.record.state,
            umbra_core::CustodyPhase::Armed { capacity, .. } if *capacity == self.entry.launch.capacity)
        {
            return Err(invalid("Armed request changed launch capacity"));
        }
        let external = self
            .entry
            .inventory()
            .ok_or_else(|| invalid("relay inventory missing"))?
            .external_caller;
        let retained = self
            .entry
            .readiness()
            .ok_or_else(|| invalid("relay receipts missing"))?;
        let mut proposed: Vec<_> = request
            .readiness
            .iter()
            .filter(|receipt| receipt.participant.role != Role::Caller)
            .cloned()
            .collect();
        let mut retained = retained.to_vec();
        proposed.sort_by_key(|receipt| receipt.participant.role);
        retained.sort_by_key(|receipt| receipt.participant.role);
        if proposed != retained {
            return Err(invalid("Armed request changed subtree readiness"));
        }
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
        self.arming = Some(
            match RelayArming::new_retaining(
                Channel::new(endpoint.stream)?,
                request,
                self.entry.readiness_deadline,
                external,
            ) {
                Ok(arming) => arming,
                Err((error, route)) => {
                    self.failed_route = Some(route);
                    return Err(error);
                }
            },
        );
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
        self.prepare_guard()?;
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

    fn prepare_guard(&mut self) -> io::Result<()> {
        if self.guard.is_none() {
            if !self.ready {
                return Err(invalid("guard before relay readiness"));
            }
            let watch = self
                .bootstrap
                .take()
                .unwrap()
                .into_watch()?
                .ok_or_else(|| invalid("relay startup watch missing"))?;
            let channel = self
                .arming
                .take()
                .map(RelayArming::into_channel)
                .or_else(|| self.failed_route.take());
            self.guard = Some(Guard::from_entry_restoring(
                &mut self.entry,
                watch,
                channel
                    .into_iter()
                    .map(|channel| (Role::Worker, channel))
                    .collect(),
            )?);
        }
        Ok(())
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
