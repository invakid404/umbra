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
    let mut transport = Some(transport);
    guard.run_parent_with_progress(move |ticket| {
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
    })
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
