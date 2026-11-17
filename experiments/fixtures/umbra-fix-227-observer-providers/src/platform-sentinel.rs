//! Handshake-compatible sentinel: failed preparation must never request Launch.
use umbra_core::{provider as wire, *};
use umbra_platform::provider::{Request, Response};
fn main() {
    let executable = std::env::current_exe().unwrap();
    let root = executable.parent().unwrap();
    let caps = PlatformCapabilities {
        architectures: vec![Architecture::Aarch64],
        capabilities: [
            umbra_platform::dirents::DARWIN_ARM64_ABI.to_owned(),
            capabilities::PLATFORM_SANDBOXED_LAUNCH_V1.to_owned(),
            capabilities::PLATFORM_SYSCALL_REWRITE_V1.to_owned(),
            capabilities::PLATFORM_INTERPOSE_V1.to_owned(),
            capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.to_owned(),
            shutdown::CAPABILITY.to_owned(),
        ]
        .into_iter()
        .collect(),
    };
    let (mut connection, ()) =
        wire::accept("macos", "platform", |_| Ok(((), caps.capabilities.clone()))).unwrap();
    let ledger = umbra_platform::shutdown::Ledger::new();
    let mut accepted: Option<shutdown::Accepted> = None;
    loop {
        connection.begin();
        let Ok(wire::Frame::Request { id, payload }) = connection.receive_request() else {
            break;
        };
        let request: Request = wire::decode(&payload).unwrap();
        let mut terminal = false;
        let result = match request {
            Request::BeginShutdown { nonce } if accepted.is_none() => {
                let snapshot = ledger.freeze(nonce, std::time::Duration::ZERO).unwrap();
                accepted = Some(snapshot.clone());
                Ok(Response::ShutdownAccepted(snapshot))
            }
            Request::PollShutdown {
                nonce,
                generation,
                max_wait_ms,
            } if accepted
                .as_ref()
                .is_some_and(|a| a.nonce == nonce && a.generation == generation)
                && (1..=100).contains(&max_wait_ms) =>
            {
                terminal = true;
                Ok(Response::ShutdownPoll(shutdown::Poll::Final(
                    ledger.final_snapshot().unwrap(),
                )))
            }
            Request::Capabilities => {
                std::fs::write(root.join("platform-capabilities"), b"Capabilities\n").unwrap();
                Ok(Response::Capabilities(caps.clone()))
            }
            Request::Launch(_) => {
                std::fs::write(root.join("platform-started"), b"unexpected Launch\n").unwrap();
                Err(UmbraError::new(
                    ErrorKind::InvalidState,
                    "fixture.launch",
                    "unexpected Launch",
                ))
            }
            _ => Err(UmbraError::new(
                ErrorKind::InvalidState,
                "fixture.request",
                "unexpected request",
            )),
        };
        connection
            .send(&wire::Frame::Response {
                id,
                result: result.and_then(|r| wire::encode(&r)),
            })
            .unwrap();
        if terminal {
            break;
        }
    }
}
