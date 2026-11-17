use super::*;
use crate::shutdown::Closure;
use std::time::Duration;
use umbra_core::shutdown::{Nonce, Poll};
use std::sync::mpsc;
use crate::shutdown::{Ledger, Start};
use umbra_core::shutdown::{Accepted, Final};

struct Handoff {
    connection: Connection,
    accepted: Accepted,
    ledger: Ledger,
    request_id: u64,
}

// No PlatformSession, ABI, native owner or closure capturing one crosses this
// boundary. The static assertion also guards accidental snapshot regressions.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Handoff>();
    send::<Result<Final>>();
};

pub(super) fn serve_shutdown(
    mut connection: Connection,
    mut platform: PlatformSession,
    id: u64,
    nonce: Nonce,
) -> Result<()> {
    let initial_deadline = Connection::clock_now() + connection.remaining()?;
    let request_timeout = connection.request_timeout();
    let (handoff_tx, handoff_rx) = mpsc::sync_channel::<Handoff>(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (terminal_tx, terminal_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker = match std::thread::Builder::new()
        .name("umbra-shutdown-transport".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ready_tx.send(()).map_err(|_| protocol_error("shutdown owner unavailable"))?;
                let handoff = handoff_rx.recv()
                    .map_err(|_| protocol_error("shutdown handoff abandoned"))?;
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
        })
    {
        Ok(worker) => worker,
        Err(error) => {
            let failure = protocol_error(format!("shutdown transport spawn failed: {error}"));
            let _ = connection.send(&Frame::Response { id, result: Err(failure.clone()) });
            connection.invalidate();
            drop(platform);
            return Err(failure);
        }
    };
    let remaining = || initial_deadline.saturating_duration_since(Connection::clock_now());
    if ready_rx.recv_timeout(remaining()).is_err() {
        connection.invalidate();
        drop(handoff_tx);
        drop(platform);
        return Err(protocol_error("shutdown transport did not become ready"));
    }
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
            let _ = connection.send(&Frame::Response { id, result: Err(failure.clone()) });
            connection.invalidate();
            drop(handoff_tx);
            drop(platform);
            return Err(failure);
        }
    };
    if let Err(error) = handoff_tx.send(Handoff {
        connection, accepted, ledger: ledger.clone(), request_id: id,
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
    let terminal = match destroyed {
        Ok(()) => ledger.final_snapshot(),
        Err(payload) => {
            std::mem::forget(payload);
            ledger.invalidate("shutdown owner destruction panicked");
            Err(protocol_error("shutdown owner destruction panicked"))
        }
    };
    let _ = terminal_tx.send(terminal);
    if !matches!(acceptance, Ok(Ok(()))) {
        return Err(protocol_error("shutdown acceptance was not written"));
    }
    // All resource-owning fields have already been destroyed. Only the existing
    // per-request transport budget remains. Do not join a live worker.
    let result = done_rx.recv_timeout(request_timeout)
        .map_err(|_| protocol_error("shutdown transport did not finish"))?;
    if worker.is_finished() {
        let _ = worker.join();
    }
    result
}

fn serve_polls(
    handoff: Handoff,
    accepted_tx: mpsc::SyncSender<Result<()>>,
    terminal_rx: mpsc::Receiver<Result<Final>>,
) -> Result<()> {
    let Handoff { mut connection, accepted, ledger, request_id } = handoff;
    let outcome = (|| {
        let sent = connection.send(&Frame::Response {
            id: request_id,
            result: wire::encode(&Response::ShutdownAccepted(accepted.clone())),
        });
        let _ = accepted_tx.send(sent.clone());
        sent?;
        let deadline = Connection::clock_now()
            .checked_add(Duration::from_millis(accepted.remaining_millis))
            .ok_or_else(|| protocol_error("shutdown envelope overflow"))?;
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
            let Request::PollShutdown { nonce, generation, max_wait_ms } = wire::decode(&payload)?
            else {
                return Err(protocol_error("only shutdown polls admitted after acceptance"));
            };
            if nonce != accepted.nonce || generation != accepted.generation
                || max_wait_ms == 0 || max_wait_ms > 100
            {
                return Err(protocol_error("invalid shutdown poll identity or budget"));
            }
            let wait = Duration::from_millis(max_wait_ms)
                .min(connection.remaining()?)
                .min(deadline.saturating_duration_since(Connection::clock_now()));
            let response = match terminal_rx.recv_timeout(wait) {
                Ok(final_state) => {
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    if std::env::var_os("UMBRA_FIX_271_MISSING_FINAL").is_some() {
                        return Err(protocol_error("injected unconfirmed provider closure"));
                    }
                    let final_state = final_state?;
                    connection.send(&Frame::Response {
                        id,
                        result: wire::encode(&Response::ShutdownPoll(Poll::Final(final_state))),
                    })?;
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) =>
                    Err(protocol_error("shutdown owner disappeared before final")),
                Err(mpsc::RecvTimeoutError::Timeout) if Connection::clock_now() >= deadline =>
                    Err(protocol_error("shutdown owner-progress failure")),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(Response::ShutdownPoll(
                    Poll::Pending { progress: ledger.progress() },
                )),
            };
            let terminal_error = response.as_ref().err().cloned();
            connection.send(&Frame::Response {
                id, result: response.and_then(|response| wire::encode(&response)),
            })?;
            if let Some(error) = terminal_error {
                return Err(error);
            }
        }
    })();
    connection.invalidate();
    outcome
}

pub(super) fn close_client(client: &mut Client, nonce: Nonce) -> Closure {
    if !client.owns_provider() {
        client.connection.invalidate();
        return Closure::failed(protocol_error("platform provider has no owned exit waiter"));
    }
    let mut proof = false;
    let mut final_to_exit = None;
    let result = (|| {
        let Response::ShutdownAccepted(accepted) = client.call(&Request::BeginShutdown { nonce })?
        else {
            return Err(protocol_error("expected shutdown acceptance"));
        };
        if accepted.nonce != nonce {
            return Err(protocol_error("shutdown acceptance nonce mismatch"));
        }
        accepted.validate()?;
        let deadline = Connection::clock_now()
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
                    if Connection::clock_now() >= deadline {
                        return Err(protocol_error("shutdown owner-progress failure"));
                    }
                }
                Response::ShutdownPoll(Poll::Final(final_state)) => {
                    proof = accepted.confirms(&final_state)?;
                    let started = Connection::clock_now();
                    let exit = client.confirm_provider_exit();
                    let elapsed = Connection::clock_now().saturating_duration_since(started);
                    final_to_exit = Some(elapsed);
                    #[cfg(feature = "shutdown-witness")]
                    crate::shutdown::witness(serde_json::json!({
                        "event": "final-to-exit", "nonce": nonce,
                        "nanos": elapsed.as_nanos().to_string(), "exit_ok": exit.is_ok(),
                        "proof": proof, "accepted": accepted, "final": final_state,
                    }));
                    if !proof {
                        return Err(protocol_error(format!(
                            "shutdown ledger has unconfirmed obligations: {:?}; exit: {exit:?}",
                            final_state.diagnostics,
                        )));
                    }
                    return exit;
                }
                _ => return Err(protocol_error("invalid shutdown response phase")),
            }
        }
    })();
    match result {
        Ok(duration) => Closure { tree_terminated: proof, error: None, final_to_exit: Some(duration) },
        Err(error) => {
            client.connection.invalidate();
            Closure { tree_terminated: proof, error: Some(error), final_to_exit }
        }
    }
}

#[cfg(test)]
#[path = "shutdown_transport_tests.rs"]
mod tests;
