//! Owned registered role dispatch. Failed calls leave parent custody in state.
use crate::{managed_dispatch, managed_input::ManagedRunInput};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use umbra_core::managed::Role;
use umbra_isolation::managed::{
    executable, guardian_startup::GuardianStartup, owner::Service, owner_startup::OwnerStartup,
    relay_startup::RelayStartup, worker_startup::WorkerStartup,
};
static REGISTERED: AtomicBool = AtomicBool::new(false);

enum State {
    Caller(Box<umbra_isolation::managed::caller_startup::CallerStartup>),
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
    parent_finished: bool,
    unresolved: Vec<umbra_isolation::managed::process::SpawnedChild>,
    observer: Option<Box<dyn crate::run::RunObserver>>,
}

impl RegisteredRole {
    /// Invoke before ordinary argument parsing in every registered executable.
    pub fn enter() -> io::Result<Option<Self>> {
        let Some(entry) = executable::entry()? else {
            REGISTERED.store(true, Ordering::Relaxed);
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
        REGISTERED.store(true, Ordering::Relaxed);
        Ok(Some(Self {
            state,
            input: Some(input),
            external,
            attempted: false,
            parent_finished: false,
            unresolved: Vec::new(),
            observer: None,
        }))
    }

    /// Prepare CLI relay mode or external library caller mode. The executable
    /// must call `enter` before argument parsing so every child can re-enter it.
    pub fn prepare(
        spec: crate::run::RunSpec,
        external: bool,
        capacity: u64,
    ) -> umbra_core::Result<Self> {
        let failure = |error: String| {
            umbra_core::UmbraError::new(
                umbra_core::ErrorKind::InvalidState,
                "managed.launch",
                error,
            )
        };
        if !REGISTERED.load(Ordering::Relaxed) {
            return Err(failure(
                "executable has not registered managed role dispatch".into(),
            ));
        }
        crate::run::validate(&spec)?;
        let (input, observer) = ManagedRunInput::split(spec);
        let launch = input.registered_launch(external, capacity)?;
        let deadline = umbra_isolation::managed::startup::Startup::new(launch.run, external)
            .map_err(|e| failure(e.to_string()))?
            .readiness_deadline();
        let state = if external {
            State::Caller(Box::new(
                umbra_isolation::managed::caller_startup::CallerStartup::spawn(launch, deadline)
                    .map_err(|e| failure(e.to_string()))?,
            ))
        } else {
            let entry = executable::Entry::root_relay(launch, deadline)
                .map_err(|e| failure(e.to_string()))?;
            State::Relay(Box::new(
                RelayStartup::new(entry).map_err(|e| failure(e.to_string()))?,
            ))
        };
        Ok(Self {
            state,
            input: Some(input),
            external,
            attempted: false,
            parent_finished: false,
            unresolved: Vec::new(),
            observer,
        })
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
            State::Caller(startup) => check(
                managed_dispatch::caller(startup, run_id, self.observer.take())?,
                &mut self.unresolved,
                &mut self.parent_finished,
            ),
            State::Relay(startup) => check(
                managed_dispatch::relay(startup, run_id, self.external, self.observer.take())?,
                &mut self.unresolved,
                &mut self.parent_finished,
            ),
            State::Guardian(startup) => check(
                managed_dispatch::guardian(startup, run_id, self.external)?,
                &mut self.unresolved,
                &mut self.parent_finished,
            ),
            State::Owner(startup, service) => {
                managed_dispatch::owner(startup, service, input, self.external)
            }
            State::Worker(startup) => {
                managed_dispatch::worker_ready(
                    startup.as_mut().as_mut().unwrap(),
                    input,
                    self.external,
                )?;
                let spec = self.input.take().unwrap().into_spec(None);
                crate::run::run_worker(spec, run_id, startup.take().unwrap())
                    .map_err(io::Error::other)
            }
        }
    }

    /// Finish failed startup using the appropriate retained parent state.
    /// An error preserves unresolved custody in this object for reporting.
    pub fn cleanup_failure(&mut self) -> io::Result<()> {
        if self.parent_finished {
            return if self.unresolved.is_empty() { Ok(()) } else { Err(io::Error::other("managed children remain uncollected")) };
        }
        match &mut self.state {
            State::Caller(startup) => match startup.rollback() {
                Ok(true) => Ok(()),
                Ok(false) => {
                    while !startup.rollback()? {
                        tick();
                    }
                    Ok(())
                }
                Err(_) => check_collection(startup.fail_runtime()?, &mut self.unresolved),
            },
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

/// Execute a registered run while preserving its primary failure through cleanup.
pub fn run(
    spec: crate::run::RunSpec,
    external_caller: bool,
    capacity: u64,
) -> umbra_core::Result<()> {
    let mut role = RegisteredRole::prepare(spec, external_caller, capacity)?;
    execute(&mut role).map_err(|error| {
        umbra_core::UmbraError::new(
            umbra_core::ErrorKind::InvalidState,
            "managed.run",
            error.to_string(),
        )
    })
}

/// Execute an entered helper or a prepared root, completing failure cleanup.
pub fn execute(role: &mut RegisteredRole) -> io::Result<()> {
    match role.run() {
        Ok(()) => Ok(()),
        Err(primary) => match role.cleanup_failure() {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(io::Error::other(format!("{primary}; cleanup: {cleanup}"))),
        },
    }
}

fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

fn check(
    mut outcome: umbra_isolation::managed::guard::Outcome,
    retained: &mut Vec<umbra_isolation::managed::process::SpawnedChild>,
    finished: &mut bool,
) -> io::Result<()> {
    *finished = true;
    retained.append(&mut outcome.unresolved);
    if outcome.failed {
        let failures: Vec<_> = outcome
            .events
            .into_iter()
            .filter_map(|event| match event {
                umbra_isolation::managed::guard::Event::Failed(message) => Some(message),
                _ => None,
            })
            .collect();
        return Err(io::Error::other(format!(
            "managed role failed: {}",
            failures.join("; ")
        )));
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
