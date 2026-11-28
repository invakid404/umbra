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
    readiness_queued: bool,
    released: bool,
    failed: bool,
    rollback: Option<super::rollback::Rollback>,
}

impl RelayStartup {
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
        if self.guard.is_none() {
            // Guard construction precedes guardian authorization. Preparation
            // may already have consumed its watch and routes; retain the real
            // child handles and use their original pre-GO rollback allocation.
            while !self.rollback()? {
                self.wait_for_rollback()?;
            }
            return Ok(super::guard::Outcome {
                completion_deadline: Some(
                    self.entry
                        .readiness_deadline
                        .checked_add(9_000_000_000)
                        .ok_or_else(|| invalid("rollback clock overflow"))?,
                ),
                worker_status: None,
                primary: None,
                events: vec![super::guard::Event::Failed(
                    "relay startup failed before guard construction completed".into(),
                )],
                failed: true,
                unresolved: Vec::new(),
            });
        }
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
        self.arming.take();
        self.failed_route.take();
        self.rollback.as_mut().unwrap().poll()
    }

    /// Validate relay role before spawning its guardian.
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
            readiness_queued: false,
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
            self.children.push(
                self.entry
                    .spawn_child(Role::Guardian, dedicated_session())?,
            );
        }
        self.readiness_queued = self.bootstrap.as_mut().unwrap().poll(
            &mut self.entry,
            &mut self.children,
            external_caller,
        )?;
        if !self.readiness_queued {
            return Ok(false);
        }
        self.ready = self.entry.flush_readiness()?;
        Ok(self.ready)
    }

    /// Wait for incomplete readiness publication without delaying local phases.
    pub fn wait_for_readiness(&self) -> io::Result<()> {
        if self.readiness_queued {
            self.entry.wait_for_startup()
        } else {
            self.bootstrap
                .as_ref()
                .unwrap()
                .wait_for_activity(&self.entry, &self.children)
        }
    }

    /// Wait for durable authorization or its delivery under existing cutoffs.
    pub fn wait_for_release(&self) -> io::Result<()> {
        if self.authorization.is_none() {
            self.arming
                .as_ref()
                .ok_or_else(|| invalid("relay not arming"))?
                .wait_for_activity(
                    &self.entry,
                    self.bootstrap
                        .as_ref()
                        .and_then(Bootstrap::watch_descriptor),
                )
        } else {
            self.children
                .first()
                .ok_or_else(|| invalid("relay child already transferred"))?
                .wait_for_activity()
        }
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
        if !self.children[0].send_authorization(go, go.deadline)? {
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
            #[cfg(feature = "test-managed-construction-failure")]
            if std::env::var("UMBRA_TEST_CONSTRUCTION_FAILURE").as_deref() == Ok("relay") {
                return Err(invalid("test hook: relay guard construction failed"));
            }
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

fn dedicated_session() -> bool {
    #[cfg(feature = "test-managed-session")]
    if std::env::var_os("UMBRA_TEST_MANAGED_SESSION").is_some() {
        return true;
    }
    crate::in_launchd_session()
}

#[cfg(all(test, feature = "test-managed-construction-failure"))]
mod construction_tests {
    use super::*;
    use std::{os::unix::net::UnixStream, panic::AssertUnwindSafe, path::Path};

    #[test]
    fn consumed_preparation_collects_guardian_without_panicking() {
        let launch = serde_json::from_str(
            r#"{"version":1,"run":{"nonce":"00000000-0000-4000-8000-000000000001","generation":1},"capacity":64,"dispatch":"admission","payload":[]}"#,
        ).unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 2_000_000_000;
        let mut startup = RelayStartup::new(Entry::root_relay(launch, deadline).unwrap()).unwrap();
        let child = super::super::process::SpawnedChild::spawn(
            Path::new("/bin/sleep"),
            &["sleep".into(), "0.1".into()],
            &[],
            false,
            &[],
        )
        .unwrap();
        let pid = child.pid();
        let (stream, _peer) = UnixStream::pair().unwrap();
        startup.children.push(PendingRole::test_unreleased(
            child,
            stream,
            startup.entry.launch.run,
            deadline,
        ));
        startup.ready = true;
        startup.bootstrap.take();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| startup.fail_runtime()));
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let collected =
            observed == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
        // Baseline recovery panics with the child still retained. Reap the
        // short-lived fixture before asserting, without supplying the proof.
        for child in &mut startup.children {
            while child.child.collect().unwrap().is_none() {
                assert!(super::super::boot_nanos().unwrap() < deadline);
                std::thread::yield_now();
            }
        }
        let outcome = result
            .expect("recovery must not repeat consumed preparation")
            .unwrap();
        assert!(outcome.failed);
        assert!(outcome.unresolved.is_empty());
        assert!(collected, "relay must perform the guardian parent wait");
    }
}
