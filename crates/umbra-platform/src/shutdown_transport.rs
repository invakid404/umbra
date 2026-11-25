use super::*;
use crate::shutdown::Closure;
use crate::shutdown::{Ledger, Start};
use std::sync::mpsc;
use std::time::Duration;
use umbra_core::shutdown::{Accepted, Final};
use umbra_core::shutdown::{Nonce, Poll};

#[cfg(test)]
type Clock = std::sync::Arc<std::sync::Mutex<std::time::Instant>>;
#[cfg(test)]
thread_local! { static CLOCK: std::cell::RefCell<Option<Clock>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static CLOCK_TICK: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) }; }
fn now() -> std::time::Instant {
    #[cfg(test)]
    if let Some(clock) = CLOCK.with(|value| value.borrow().clone()) {
        let mut clock = clock.lock().unwrap();
        let current = *clock;
        *clock += CLOCK_TICK.with(|tick| tick.get());
        return current;
    }
    Connection::clock_now()
}

struct Handoff {
    connection: Connection,
    accepted: Accepted,
    ledger: Ledger,
    request_id: u64,
    owner_deadline: std::time::Instant,
}

// No PlatformSession, ABI, native owner or closure capturing one crosses this
// boundary. The static assertion also guards accidental snapshot regressions.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Handoff>();
    send::<Result<Final>>();
};

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Failure {
    None,
    Spawn,
    Handoff,
    TransportPanic,
    Acceptance,
    OwnerPanic,
    Eof,
}
#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<Failure> = const { std::cell::Cell::new(Failure::None) }; }

fn spawn_transport(
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    if FAILURE.with(|value| value.get()) == Failure::Spawn {
        return Err(std::io::Error::other("controlled transport spawn failure"));
    }
    std::thread::Builder::new()
        .name("umbra-shutdown-transport".into())
        .spawn(work)
}

pub(super) fn serve_shutdown(
    mut connection: Connection,
    mut platform: PlatformSession,
    id: u64,
    nonce: Nonce,
) -> Result<()> {
    let initial_deadline = now() + connection.remaining()?;
    let request_timeout = connection.request_timeout();
    let (handoff_tx, handoff_rx) = mpsc::sync_channel::<Handoff>(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (terminal_tx, terminal_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    #[cfg(test)]
    let failure = FAILURE.with(|value| value.get());
    #[cfg(test)]
    let clock = CLOCK.with(|value| value.borrow().clone());
    let worker = match spawn_transport(move || {
        #[cfg(test)]
        CLOCK.with(|value| *value.borrow_mut() = clock);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ready_tx
                .send(())
                .map_err(|_| protocol_error("shutdown owner unavailable"))?;
            #[cfg(test)]
            if failure == Failure::Handoff {
                return Err(protocol_error("controlled handoff receiver loss"));
            }
            let handoff = handoff_rx
                .recv()
                .map_err(|_| protocol_error("shutdown handoff abandoned"))?;
            #[cfg(test)]
            let handoff = {
                let mut handoff = handoff;
                if failure == Failure::TransportPanic {
                    panic!("controlled transport panic");
                }
                if failure == Failure::Acceptance {
                    handoff.connection.invalidate();
                }
                handoff
            };
            serve_polls(handoff, accepted_tx, terminal_rx)
        }));
        let result = match result {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err(protocol_error("shutdown transport panicked"))
            }
        };
        let _ = done_tx.send(result);
    }) {
        Ok(worker) => worker,
        Err(error) => {
            let failure = protocol_error(format!("shutdown transport spawn failed: {error}"));
            let _ = connection.send(&Frame::Response {
                id,
                result: Err(failure.clone()),
            });
            connection.invalidate();
            drop(platform);
            return Err(failure);
        }
    };
    let remaining = || initial_deadline.saturating_duration_since(now());
    if ready_rx.recv_timeout(remaining()).is_err() {
        connection.invalidate();
        drop(handoff_tx);
        drop(platform);
        return Err(protocol_error("shutdown transport did not become ready"));
    }
    let freeze_started = now();
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        platform.control.begin_shutdown(nonce)
    }));
    let (accepted, ledger) = match prepared {
        Ok(Ok(Start::Local { accepted, ledger })) => (accepted, ledger),
        other => {
            let failure = match other {
                Ok(Err(error)) => error,
                Ok(Ok(Start::Remote(_))) => protocol_error("provider backend is not locally owned"),
                Err(payload) => {
                    std::mem::forget(payload);
                    protocol_error("shutdown owner preparation panicked")
                }
                _ => unreachable!(),
            };
            let _ = connection.send(&Frame::Response {
                id,
                result: Err(failure.clone()),
            });
            connection.invalidate();
            drop(handoff_tx);
            drop(platform);
            return Err(failure);
        }
    };
    let owner_deadline = freeze_started
        .checked_add(Duration::from_millis(accepted.remaining_millis))
        .ok_or_else(|| protocol_error("shutdown envelope overflow"))?;
    if let Err(error) = handoff_tx.send(Handoff {
        connection,
        accepted,
        ledger: ledger.clone(),
        request_id: id,
        owner_deadline,
    }) {
        let mut handoff = error.0;
        handoff.connection.invalidate();
        drop(platform);
        return Err(protocol_error("shutdown connection handoff failed"));
    }
    let acceptance = accepted_rx.recv_timeout(remaining());
    // Always destroy on this thread, including an acceptance failure. No
    // WatchedTasks or transport snapshot lock is held during any destructor.
    let destroyed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(platform)));
    let completed_at = now();
    #[cfg(feature = "shutdown-witness")]
    crate::shutdown::witness(serde_json::json!({"event": "owner-terminal"}));
    let terminal = match destroyed {
        Ok(()) => ledger.final_snapshot(),
        Err(payload) => {
            std::mem::forget(payload);
            ledger.invalidate("shutdown owner destruction panicked");
            Err(protocol_error("shutdown owner destruction panicked"))
        }
    };
    let _ = terminal_tx.send((completed_at, terminal));
    if !matches!(acceptance, Ok(Ok(()))) {
        return Err(protocol_error("shutdown acceptance was not written"));
    }
    // All resource-owning fields have already been destroyed. Only the existing
    // per-request transport budget remains. Do not join a live worker.
    let result = done_rx
        .recv_timeout(request_timeout)
        .map_err(|_| protocol_error("shutdown transport did not finish"))?;
    if worker.is_finished() {
        let _ = worker.join();
    }
    result
}

fn serve_polls(
    handoff: Handoff,
    accepted_tx: mpsc::SyncSender<Result<()>>,
    terminal_rx: mpsc::Receiver<(std::time::Instant, Result<Final>)>,
) -> Result<()> {
    let Handoff {
        mut connection,
        accepted,
        ledger,
        request_id,
        owner_deadline: deadline,
    } = handoff;
    #[cfg(test)]
    ledger.transport_thread();
    #[cfg(feature = "shutdown-witness")]
    crate::shutdown::witness(serde_json::json!({"event": "transport-start"}));
    let outcome = (|| {
        let sent = connection.send(&Frame::Response {
            id: request_id,
            result: wire::encode(&Response::ShutdownAccepted(accepted.clone())),
        });
        let _ = accepted_tx.send(sent.clone());
        sent?;
        let mut previous = request_id;
        loop {
            connection.begin();
            // Unlike normal idle request admission, terminal closure has an
            // existing frame deadline and must not wait forever for another poll.
            let Frame::Request { id, payload } = connection.receive::<Frame>()? else {
                return Err(protocol_error("expected shutdown poll request"));
            };
            if id <= previous {
                return Err(protocol_error("unordered shutdown poll"));
            }
            previous = id;
            let Request::PollShutdown {
                nonce,
                generation,
                max_wait_ms,
            } = wire::decode(&payload)?
            else {
                return Err(protocol_error(
                    "only shutdown polls admitted after acceptance",
                ));
            };
            if nonce != accepted.nonce
                || generation != accepted.generation
                || max_wait_ms == 0
                || max_wait_ms > 100
            {
                return Err(protocol_error("invalid shutdown poll identity or budget"));
            }
            let wait = Duration::from_millis(max_wait_ms)
                .min(connection.remaining()?)
                .min(deadline.saturating_duration_since(now()));
            let response = match terminal_rx.recv_timeout(wait) {
                Ok((completed_at, _)) if completed_at > deadline => {
                    Err(protocol_error("shutdown owner-progress failure"))
                }
                Ok((_, final_state)) => {
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    if std::env::var_os("UMBRA_FIX_271_MISSING_FINAL").is_some() {
                        return Err(protocol_error("injected unconfirmed provider closure"));
                    }
                    let final_state = final_state?;
                    #[cfg(feature = "shutdown-witness")]
                    crate::shutdown::witness(serde_json::json!({
                        "event": "transport-final",
                        "nonce": accepted.nonce,
                        "instance": accepted.instance,
                    }));
                    connection.send(&Frame::Response {
                        id,
                        result: wire::encode(&Response::ShutdownPoll(Poll::Final(final_state))),
                    })?;
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(protocol_error("shutdown owner disappeared before final"))
                }
                Err(mpsc::RecvTimeoutError::Timeout) if now() >= deadline => {
                    Err(protocol_error("shutdown owner-progress failure"))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(Response::ShutdownPoll(Poll::Pending {
                    progress: ledger.progress(),
                })),
            };
            let terminal_error = response.as_ref().err().cloned();
            connection.send(&Frame::Response {
                id,
                result: response.and_then(|response| wire::encode(&response)),
            })?;
            if let Some(error) = terminal_error {
                return Err(error);
            }
        }
    })();
    connection.invalidate();
    outcome
}

/// `close_client`'s error text for a Final that does not confirm (#205): the
/// named unconfirmed obligations and diagnostics, then the exit result.
fn unconfirmed_final(final_state: &Final, exit: &impl std::fmt::Debug) -> String {
    format!(
        "{}; exit: {exit:?}",
        crate::shutdown::unconfirmed_obligations(final_state)
    )
}

pub(super) fn close_client(client: &mut Client, nonce: Nonce) -> Closure {
    close_with_owner(client, nonce, None)
}

pub(super) fn close_managed_client(
    client: &mut Client,
    nonce: Nonce,
    owner: &mut dyn ManagedCleanup,
) -> Closure {
    close_with_owner(client, nonce, Some(owner))
}

fn close_with_owner(
    client: &mut Client,
    nonce: Nonce,
    mut owner: Option<&mut dyn ManagedCleanup>,
) -> Closure {
    if let Some(owner) = owner.as_deref_mut() {
        match owner.start() {
            Ok(deadline) => client.connection.cap_cleanup_deadline(deadline),
            Err(error) => {
                owner.failed(&error);
                return Closure::failed(error);
            }
        }
    }
    if owner.is_none() && !client.owns_provider() {
        client.connection.invalidate();
        return Closure::failed(protocol_error("platform provider has no owned exit waiter"));
    }
    let mut proof = false;
    let mut final_to_exit = None;
    let mut collection_attempted = false;
    let result = (|| {
        let Response::ShutdownAccepted(accepted) =
            client.call(&Request::BeginShutdown { nonce })?
        else {
            return Err(protocol_error("expected shutdown acceptance"));
        };
        if accepted.nonce != nonce {
            return Err(protocol_error("shutdown acceptance nonce mismatch"));
        }
        accepted.validate()?;
        let deadline = now()
            .checked_add(Duration::from_millis(accepted.remaining_millis))
            .ok_or_else(|| protocol_error("shutdown envelope overflow"))?;
        let mut previous_progress = 0;
        loop {
            let response = client.call(&Request::PollShutdown {
                nonce,
                generation: accepted.generation,
                max_wait_ms: 100,
            })?;
            match response {
                Response::ShutdownPoll(Poll::Pending { progress }) => {
                    if progress < previous_progress {
                        return Err(protocol_error("shutdown progress moved backwards"));
                    }
                    previous_progress = progress;
                    if now() >= deadline {
                        return Err(protocol_error("shutdown owner-progress failure"));
                    }
                }
                Response::ShutdownPoll(Poll::Final(final_state)) => {
                    // Measure from decoded-Final receipt, including validation.
                    let started = now();
                    proof = accepted.confirms(&final_state)?;
                    let exit = if let Some(owner) = owner.as_deref_mut() {
                        client.connection.invalidate();
                        collection_attempted = true;
                        owner.collect_provider()
                    } else {
                        client.confirm_provider_exit()
                    };
                    let elapsed = now().saturating_duration_since(started);
                    final_to_exit = Some(elapsed);
                    #[cfg(feature = "shutdown-witness")]
                    crate::shutdown::witness(serde_json::json!({
                        "event": "final-to-exit", "nonce": nonce,
                        "timing_origin": "final-decoded",
                        "nanos": elapsed.as_nanos().to_string(), "exit_ok": exit.is_ok(),
                        "proof": proof, "accepted": accepted, "final": final_state,
                    }));
                    if !proof {
                        return Err(protocol_error(unconfirmed_final(&final_state, &exit)));
                    }
                    return exit;
                }
                _ => return Err(protocol_error("invalid shutdown response phase")),
            }
        }
    })();
    match result {
        Ok(_) => Closure {
            tree_terminated: proof,
            error: None,
            final_to_exit,
        },
        Err(mut error) => {
            client.connection.invalidate();
            if let Some(owner) = owner {
                owner.failed(&error);
                if let Err(collection) = if collection_attempted {
                    Ok(Duration::ZERO)
                } else {
                    owner.collect_provider()
                } {
                    error
                        .context
                        .push_str(&format!("; managed provider collection: {collection}"));
                }
            }
            Closure {
                tree_terminated: proof,
                error: Some(error),
                final_to_exit,
            }
        }
    }
}

#[cfg(test)]
#[path = "shutdown_transport_tests.rs"]
mod tests;
