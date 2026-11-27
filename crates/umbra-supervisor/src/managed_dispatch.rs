//! Registered parent role execution, retaining startup custody on errors.
use crate::{
    managed_observer::{CallerObserver, CallerTransport, Forwarder},
    run::RunObserver,
};
use std::io;
use umbra_core::{managed::Role, RunId};
use umbra_isolation::managed::{
    caller_startup::CallerStartup,
    guard::{Guard, Outcome},
    guardian_startup::GuardianStartup,
    relay_startup::RelayStartup,
};

/// Serve the cleanup owner while keeping its service in caller-owned custody
/// on every polling error, including after the provider has been launched.
pub fn owner(
    startup: &mut umbra_isolation::managed::owner_startup::OwnerStartup,
    service: &mut Option<umbra_isolation::managed::owner::Service>,
    input: &crate::managed_input::ManagedRunInput,
    external_caller: bool,
) -> io::Result<()> {
    if service.is_none() {
        let descriptor = input.platform().map_err(io::Error::other)?;
        while !startup.poll(descriptor, external_caller, input.timeout_ms())? {
            startup.wait_for_activity()?;
        }
        umbra_core::launch_stdio::LaunchStdio::release_cli_streams().map_err(io::Error::other)?;
        *service = Some(startup.take_service()?);
    }
    while !service.as_mut().unwrap().poll()? {
        service.as_mut().unwrap().wait_for_activity()?;
    }
    if service.as_ref().unwrap().failed() {
        Err(io::Error::other(
            "cleanup owner collected after failed cleanup",
        ))
    } else {
        Ok(())
    }
}

/// Complete worker readiness before entering staged storage setup.
/// Failed readiness leaves the worker startup in the dispatcher's custody.
pub fn worker_ready(
    startup: &mut umbra_isolation::managed::worker_startup::WorkerStartup,
    input: &crate::managed_input::ManagedRunInput,
    external_caller: bool,
) -> io::Result<()> {
    let descriptor = input.platform().map_err(io::Error::other)?;
    while !startup.poll(descriptor, input.timeout_ms(), external_caller)? {
        startup.wait_for_activity()?;
    }
    while !startup.capture_preparation_readiness()? {
        startup.wait_for_preparation_readiness()?;
    }
    Ok(())
}

/// Run a guardian with its startup owner retained by the executable dispatcher.
/// On startup error, the caller still owns every child through `startup`.
pub fn guardian(
    startup: &mut GuardianStartup,
    run_id: RunId,
    external_caller: bool,
) -> io::Result<Outcome> {
    while !startup.poll(external_caller)? {
        startup.wait_for_activity()?;
    }
    umbra_core::launch_stdio::LaunchStdio::release_cli_streams().map_err(io::Error::other)?;
    let parent = startup.entry.take_exchange()?;
    let child = startup
        .guard
        .as_mut()
        .ok_or_else(|| io::Error::other("guardian guard missing"))?
        .take_exchange(Role::Worker)?;
    let forwarder = Forwarder::new(run_id, child, parent);
    let guard = startup
        .guard
        .take()
        .ok_or_else(|| io::Error::other("guardian guard transferred"))?;
    let (mut outcome, mut forwarder) = forward_retaining(guard, forwarder);
    if outcome.unresolved.is_empty() {
        if let Some(sweep) = umbra_isolation::finish_managed_session() {
            // These are legacy session observations, never parent wait receipts.
            if sweep.kind != umbra_isolation::sweep::Kind::Complete {
                eprintln!("umbra: managed guardian session cleanup: {sweep:?}");
                outcome.failed = true;
                outcome
                    .events
                    .push(umbra_isolation::managed::guard::Event::Failed(format!(
                        "session cleanup {}",
                        sweep.kind.as_str()
                    )));
            } else {
                use std::os::unix::process::ExitStatusExt;
                let status = outcome.events.iter().find_map(|event| match event {
                    umbra_isolation::managed::guard::Event::Collected(participant, status)
                        if participant.role == Role::Worker
                            && !std::process::ExitStatus::from_raw(*status).success() =>
                    {
                        Some(*status)
                    }
                    _ => None,
                });
                if let (Some(status), Some(deadline)) = (status, outcome.completion_deadline) {
                    if let Err(error) = forwarder.report_worker_exit(status, deadline) {
                        outcome.failed = true;
                        outcome
                            .events
                            .push(umbra_isolation::managed::guard::Event::Failed(format!(
                                "worker status report: {error}"
                            )));
                    }
                }
            }
        }
    }
    Ok(outcome)
}

fn forward(guard: Guard, forwarder: Forwarder) -> Outcome {
    let forwarding = std::cell::RefCell::new(Some(forwarder));
    guard.run_parent_with_wait(
        |ticket| {
            let mut forwarder = forwarding.borrow_mut();
            let result = forwarder
                .as_mut()
                .unwrap()
                .poll(ticket.map(|ticket| ticket.deadline));
            if result.is_err() {
                forwarder.take();
            }
            result.map_err(io::Error::other)
        },
        |guard| {
            guard.wait_for_activity(
                forwarding
                    .borrow()
                    .as_ref()
                    .and_then(Forwarder::wait_interest),
            )
        },
    )
}

fn forward_retaining(
    guard: Guard,
    forwarder: Forwarder,
) -> (Outcome, crate::managed_observer::ExitReporter) {
    let forwarding = std::cell::RefCell::new(Some(forwarder));
    let reporter = std::cell::RefCell::new(None);
    let outcome = guard.run_parent_with_wait(
        |ticket| {
            let mut forwarding = forwarding.borrow_mut();
            let result = forwarding
                .as_mut()
                .unwrap()
                .poll(ticket.map(|ticket| ticket.deadline));
            if result.is_err() {
                *reporter.borrow_mut() = Some(forwarding.take().unwrap().into_exit_reporter());
            }
            result.map_err(io::Error::other)
        },
        |guard| {
            let forwarding = forwarding.borrow();
            guard.wait_for_activity(forwarding.as_ref().and_then(Forwarder::wait_interest))
        },
    );
    (
        outcome,
        reporter
            .into_inner()
            .unwrap_or_else(|| forwarding.into_inner().unwrap().into_exit_reporter()),
    )
}

fn observe(guard: Guard, transport: CallerTransport) -> Outcome {
    #[cfg(feature = "probe-native-setup-cancel")]
    let probe = (|| -> io::Result<_> {
        let Some(directory) = std::env::var_os("UMBRA_NATIVE_SETUP_PROBE") else {
            return Ok(None);
        };
        let directory = std::path::PathBuf::from(directory);
        let caller = std::fs::read_to_string(directory.join("caller.pid"))?;
        if caller.parse::<u32>().ok() != Some(std::process::id()) {
            return Ok(None);
        }
        umbra_isolation::managed::test_directory_wake::DirectoryWake::new(&directory).map(Some)
    })();
    let result = transport.result_handle();
    let worker_status = transport.worker_status_handle();
    let observing = std::cell::RefCell::new(Some(transport));
    let mut outcome = guard.run_parent_with_wait(
        |ticket| {
            #[cfg(feature = "probe-native-setup-cancel")]
            let _ = probe
                .as_ref()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let mut transport = observing.borrow_mut();
            #[cfg(feature = "probe-native-setup-cancel")]
            if let Some(directory) = std::env::var_os("UMBRA_NATIVE_SETUP_PROBE") {
                let directory = std::path::PathBuf::from(directory);
                let caller = std::fs::read_to_string(directory.join("caller.pid"))
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok());
                if caller == Some(std::process::id()) && directory.join("setup.json").exists() {
                    let now = umbra_isolation::managed::boot_nanos()?;
                    std::fs::write(directory.join("triggered.json"), now.to_string())?;
                    if let Some(transport) = transport.take() {
                        transport.discard();
                    }
                    return Err(io::Error::other("native setup probe caller cancellation"));
                }
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transport
                    .as_mut()
                    .unwrap()
                    .poll(ticket.map(|ticket| ticket.deadline))
            }));
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => {
                    if let Some(transport) = transport.take() {
                        transport.discard();
                    }
                    Err(io::Error::other(error))
                }
                Err(payload) => {
                    // Collection must continue even if a terminal callback unwinds.
                    std::mem::forget(payload);
                    if let Some(transport) = transport.take() {
                        transport.discard();
                    }
                    Err(io::Error::other("managed terminal observer panicked"))
                }
            }
        },
        |guard| {
            let observing = observing.borrow();
            let application = observing.as_ref().and_then(CallerTransport::wait_interest);
            #[cfg(feature = "probe-native-setup-cancel")]
            if let Ok(Some(probe)) = &probe {
                guard.wait_for_test_activity(application, probe.descriptor())?;
                return probe.drain();
            }
            guard.wait_for_activity(application)
        },
    );
    outcome.worker_status = worker_status.get();
    if outcome.worker_status.is_some() && result.borrow().is_none() {
        outcome.failed = true;
        return outcome;
    }
    match result.borrow().as_ref() {
        Some(Ok(())) => {}
        result => {
            outcome.failed = true;
            let message = match result {
                Some(Err(error)) => {
                    outcome.primary = Some(error.clone());
                    return outcome;
                }
                _ => "worker exited without a final result".to_owned(),
            };
            outcome
                .events
                .push(umbra_isolation::managed::guard::Event::Failed(message));
        }
    }
    outcome
}

/// Execute the relay barrier, exact storage arming and guardian collection.
pub fn relay(
    startup: &mut RelayStartup,
    run_id: RunId,
    external_caller: bool,
    observer: Option<Box<dyn RunObserver>>,
) -> io::Result<Outcome> {
    while !startup.poll_readiness(external_caller)? {
        startup.wait_for_readiness()?;
    }
    umbra_core::launch_stdio::LaunchStdio::release_cli_streams().map_err(io::Error::other)?;
    let caller = if external_caller {
        loop {
            if let Some(receipt) = startup.entry.receive_caller_readiness()? {
                break Some(receipt);
            }
            startup.entry.wait_for_startup()?;
        }
    } else {
        None
    };
    startup.arm_storage(run_id, caller)?;
    while startup.poll_release()?.is_none() {
        startup.wait_for_release()?;
    }
    let child = startup
        .guard
        .as_mut()
        .ok_or_else(|| io::Error::other("relay guard missing"))?
        .take_exchange(Role::Guardian)?;
    if external_caller {
        let parent = startup.entry.take_exchange()?;
        let guard = startup
            .guard
            .take()
            .ok_or_else(|| io::Error::other("relay guard transferred"))?;
        Ok(forward(guard, Forwarder::new(run_id, child, parent)))
    } else {
        let transport = CallerTransport::new(child, CallerObserver::new(run_id, observer));
        let guard = startup
            .guard
            .take()
            .ok_or_else(|| io::Error::other("relay guard transferred"))?;
        Ok(observe(guard, transport))
    }
}

/// Execute the external caller's readiness and actual relay-parent wait loop.
pub fn caller(
    startup: &mut CallerStartup,
    run_id: RunId,
    observer: Option<Box<dyn RunObserver>>,
) -> io::Result<Outcome> {
    while !startup.poll()? {
        startup.wait_for_activity()?;
    }
    let mut guard = startup.take_guard()?;
    let channel = guard.take_exchange(Role::Relay)?;
    Ok(observe(
        guard,
        CallerTransport::new(channel, CallerObserver::new(run_id, observer)),
    ))
}
