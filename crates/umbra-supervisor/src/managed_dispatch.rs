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

fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

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
            tick();
        }
        umbra_core::launch_stdio::LaunchStdio::release_cli_streams().map_err(io::Error::other)?;
        *service = Some(startup.take_service()?);
    }
    while !service.as_mut().unwrap().poll()? {
        tick();
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
        tick();
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
        tick();
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
    Ok(forward(guard, forwarder))
}

fn forward(guard: Guard, forwarder: Forwarder) -> Outcome {
    let mut forwarder = Some(forwarder);
    guard.run_parent_with_progress(move |ticket| {
        let result = forwarder
            .as_mut()
            .unwrap()
            .poll(ticket.map(|ticket| ticket.deadline));
        if result.is_err() {
            // Closing both exchange ends wakes the worker and its real parent;
            // the independent guard continues collecting its actual children.
            forwarder.take();
        }
        result.map_err(io::Error::other)
    })
}

fn observe(guard: Guard, transport: CallerTransport) -> Outcome {
    let result = transport.result_handle();
    let mut transport = Some(transport);
    let mut outcome = guard.run_parent_with_progress(move |ticket| {
        #[cfg(feature = "probe-native-setup-cancel")]
        if let Some(directory) = std::env::var_os("UMBRA_NATIVE_SETUP_PROBE") {
            let directory = std::path::PathBuf::from(directory);
            let caller = std::fs::read_to_string(directory.join("caller.pid"))
                .ok()
                .and_then(|value| value.parse::<u32>().ok());
            if caller == Some(std::process::id()) && directory.join("setup.json").exists() {
                let now = umbra_isolation::managed::boot_nanos()?;
                std::fs::write(directory.join("triggered.json"), now.to_string())?;
                transport.take();
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
                transport.take();
                Err(io::Error::other(error))
            }
            Err(payload) => {
                // Collection must continue even if a terminal callback unwinds.
                std::mem::forget(payload);
                transport.take();
                Err(io::Error::other("managed terminal observer panicked"))
            }
        }
    });
    match result.borrow().as_ref() {
        Some(Ok(())) => {}
        result => {
            outcome.failed = true;
            let message = match result {
                Some(Err(error)) => error.to_string(),
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
        tick();
    }
    umbra_core::launch_stdio::LaunchStdio::release_cli_streams().map_err(io::Error::other)?;
    let caller = if external_caller {
        loop {
            if let Some(receipt) = startup.entry.receive_caller_readiness()? {
                break Some(receipt);
            }
            tick();
        }
    } else {
        None
    };
    startup.arm_storage(run_id, caller)?;
    while startup.poll_release()?.is_none() {
        tick();
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
        tick();
    }
    let mut guard = startup.take_guard()?;
    let channel = guard.take_exchange(Role::Relay)?;
    Ok(observe(
        guard,
        CallerTransport::new(channel, CallerObserver::new(run_id, observer)),
    ))
}
