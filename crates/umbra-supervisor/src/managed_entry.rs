//! Owned registered role dispatch. Failed calls leave parent custody in state.
use crate::{managed_dispatch, managed_input::ManagedRunInput};
use std::io;
use umbra_core::managed::Role;
use umbra_isolation::managed::{
    executable, guardian_startup::GuardianStartup, owner::Service, owner_startup::OwnerStartup,
    relay_startup::RelayStartup, worker_startup::WorkerStartup,
};

enum State {
    Relay(Box<RelayStartup>),
    Guardian(Box<GuardianStartup>),
    Owner(Box<OwnerStartup>, Box<Option<Service>>),
    Worker(Box<Option<WorkerStartup>>),
}

/// Kept alive by the registered executable until its actual children are
/// collected. A failed startup does not transfer custody into an error string.
pub struct RegisteredRole {
    state: State,
    input: Option<ManagedRunInput>,
    external: bool,
    attempted: bool,
    unresolved: Vec<umbra_isolation::managed::process::SpawnedChild>,
}

impl RegisteredRole {
    /// Invoke before ordinary argument parsing in every registered executable.
    pub fn enter() -> io::Result<Option<Self>> {
        let Some(entry) = executable::entry()? else {
            return Ok(None);
        };
        let (external, input) =
            ManagedRunInput::from_registered(&entry.launch).map_err(io::Error::other)?;
        let state = match entry.role {
            Role::Relay => State::Relay(Box::new(RelayStartup::new(entry)?)),
            Role::Guardian => State::Guardian(Box::new(GuardianStartup::new(entry)?)),
            Role::CleanupOwner => State::Owner(Box::new(OwnerStartup::new(entry)), Box::new(None)),
            Role::Worker => State::Worker(Box::new(Some(WorkerStartup::new(entry)?))),
            _ => return Err(io::Error::other("unsupported registered run role")),
        };
        Ok(Some(Self {
            state,
            input: Some(input),
            external,
            attempted: false,
            unresolved: Vec::new(),
        }))
    }

    /// Execute once. Parent startup and service state remain owned on error.
    pub fn run(&mut self) -> io::Result<()> {
        if self.attempted {
            return Err(io::Error::other("registered role already attempted"));
        }
        self.attempted = true;
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| io::Error::other("run input consumed"))?;
        let run_id = input.run_id();
        match &mut self.state {
            State::Relay(startup) => check(
                managed_dispatch::relay(startup, run_id, self.external, None)?,
                &mut self.unresolved,
            ),
            State::Guardian(startup) => check(
                managed_dispatch::guardian(startup, run_id, self.external)?,
                &mut self.unresolved,
            ),
            State::Owner(startup, service) => {
                managed_dispatch::owner(startup, service, input, self.external)
            }
            State::Worker(startup) => {
                managed_dispatch::worker_ready(startup.as_mut().as_mut().unwrap(), input, self.external)?;
                let spec = self.input.take().unwrap().into_spec(None);
                crate::run::run_worker(spec, run_id, startup.take().unwrap())
                    .map_err(io::Error::other)
            }
        }
    }

    /// Finish failed startup using the appropriate retained parent state.
    /// An error preserves unresolved custody in this object for reporting.
    pub fn cleanup_failure(&mut self) -> io::Result<()> {
        match &mut self.state {
            State::Relay(startup) => {
                if startup.guard.is_some() || startup.entry.readiness().is_some() {
                    check_collection(startup.fail_runtime()?, &mut self.unresolved)
                } else {
                    while !startup.rollback()? {
                        tick();
                    }
                    Ok(())
                }
            }
            State::Guardian(startup) => {
                if startup.guard.is_some() {
                    check_collection(startup.fail_runtime()?, &mut self.unresolved)
                } else {
                    while !startup.rollback()? {
                        tick();
                    }
                    Ok(())
                }
            }
            State::Owner(startup, service) => {
                if let Some(service) = service.as_mut() {
                    service.fail("registered owner dispatch failed".to_owned())?;
                    while !service.poll()? {
                        tick();
                    }
                    Ok(())
                } else {
                    while !startup.rollback()? {
                        tick();
                    }
                    Ok(())
                }
            }
            State::Worker(startup) => {
                if let Some(startup) = startup.as_mut() {
                    startup.entry.close_before_go()?;
                }
                Ok(())
            }
        }
    }
}

fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

fn check(
    mut outcome: umbra_isolation::managed::guard::Outcome,
    retained: &mut Vec<umbra_isolation::managed::process::SpawnedChild>,
) -> io::Result<()> {
    retained.append(&mut outcome.unresolved);
    if outcome.failed {
        let failures: Vec<_> = outcome.events.into_iter().filter_map(|event| match event {
            umbra_isolation::managed::guard::Event::Failed(message) => Some(message),
            _ => None,
        }).collect();
        return Err(io::Error::other(format!("managed role failed: {}", failures.join("; "))));
    }
    check_collection(outcome, retained)
}

fn check_collection(
    mut outcome: umbra_isolation::managed::guard::Outcome,
    retained: &mut Vec<umbra_isolation::managed::process::SpawnedChild>,
) -> io::Result<()> {
    retained.append(&mut outcome.unresolved);
    if !retained.is_empty() {
        return Err(io::Error::other("managed role has uncollected children"));
    }
    Ok(())
}
