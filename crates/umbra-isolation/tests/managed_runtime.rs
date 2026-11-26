//! Hosted macOS checks of production cleanup protocols with ordinary children.
#![cfg(target_os = "macos")]

use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::{Duration, Instant},
};
use umbra_core::managed::{CleanupPath, Participant, Role, RunIdentity, Start, Ticket, ACK_NANOS};
use umbra_isolation::managed::{
    boot_nanos,
    guard::{Event, Guard},
    process::{identity, SpawnedChild},
    publication::Route,
    Channel,
};

/// This child deliberately never acknowledges Start. Its release latch belongs
/// to the test; that latch is not a production collection receipt.
#[test]
fn runtime_child() {
    let Some(directory) = std::env::var_os("UMBRA_R2_PEER_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    let mut stream = UnixStream::connect(directory.join("s")).unwrap();
    stream.write_all(b"R").unwrap();
    let bound = Instant::now() + Duration::from_secs(3);
    while !directory.join("release").exists() {
        assert!(Instant::now() < bound, "test release latch not opened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Peer {
    directory: tempfile::TempDir,
    child: Option<SpawnedChild>,
    participant: Participant,
    channel: Option<Channel>,
}

impl Peer {
    fn spawn(role: Role) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("u-r2-")
            .tempdir_in("/tmp")
            .unwrap();
        let listener = UnixListener::bind(directory.path().join("s")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = std::env::current_exe().unwrap();
        let environment = umbra_isolation::spawn::environment(
            &[umbra_isolation::managed::executable::ENV_ROLE],
            &[(
                "UMBRA_R2_PEER_DIR",
                directory.path().to_str().unwrap().to_owned(),
            )],
        );
        let child = SpawnedChild::spawn(
            &executable,
            &[
                executable.as_os_str().to_owned(),
                "--exact".into(),
                "runtime_child".into(),
                "--nocapture".into(),
            ],
            &environment,
            false,
            &[],
        )
        .unwrap();
        let participant = identity(child.pid(), role).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "child rendezvous expired");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut ready = [0];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        Self {
            directory,
            child: Some(child),
            participant,
            channel: Some(Channel::new(stream).unwrap()),
        }
    }
    fn release(&self) -> std::io::Result<()> {
        std::fs::write(self.directory.path().join("release"), b"release")
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.release();
        if let Some(child) = self.child.as_mut() {
            // Setup-failure cleanup by the same actual parent.
            while matches!(child.collect(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn parent_ack_loss(role: Role, child_roles: &[Role]) {
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), role).unwrap();
    let mut guard = Guard::new(local, run, 1).unwrap();
    let mut peers: Vec<_> = child_roles.iter().copied().map(Peer::spawn).collect();
    for peer in &mut peers {
        guard.watch(peer.participant).unwrap();
        guard.route(
            peer.participant,
            Route::new(peer.participant, peer.channel.take().unwrap()).unwrap(),
        );
        assert!(guard
            .child(peer.participant, peer.child.take().unwrap())
            .is_ok());
    }
    guard.notify_failure().unwrap();
    let mut original = None;
    let mut held_past_ack = false;
    let mut outcome = guard.run_parent_with_progress(|ticket| {
        let ticket = ticket.ok_or_else(|| std::io::Error::other("missing original ticket"))?;
        if let Some(prior) = original {
            if ticket != prior {
                return Err(std::io::Error::other("ticket renewed"));
            }
        } else {
            original = Some(ticket);
        }
        let now = boot_nanos()?;
        if !held_past_ack && now >= ticket.started + ACK_NANOS + 10_000_000 {
            if now
                >= ticket
                    .cutoffs(1)
                    .map_err(std::io::Error::other)?
                    .cooperative
            {
                return Err(std::io::Error::other(
                    "observation missed local cleanup window",
                ));
            }
            for peer in &peers {
                if identity(peer.participant.pid, peer.participant.role)? != peer.participant {
                    return Err(std::io::Error::other("child custody lost at ACK expiry"));
                }
            }
            held_past_ack = true;
            for peer in &peers {
                peer.release()?;
            }
        }
        Ok(())
    });
    // Recover test children before asserting, without crediting these rescue
    // waits as successful production collection.
    for peer in &peers {
        peer.release().unwrap();
    }
    let unresolved = outcome.unresolved.len();
    for child in &mut outcome.unresolved {
        while child.collect().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(held_past_ack, "parent stopped at combined ACK expiry");
    assert!(outcome.failed);
    assert_eq!(
        unresolved, 0,
        "production parent did not collect its children"
    );
    assert!(
        outcome
            .events
            .iter()
            .any(|event| matches!(event, Event::Failed(message) if message.contains("timed out"))),
        "missing ACK-expiry failure"
    );
    for peer in &peers {
        let receipts: Vec<_> = outcome
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Collected(participant, status) if *participant == peer.participant => {
                    Some(*status)
                }
                _ => None,
            })
            .collect();
        assert_eq!(receipts, [0], "missing exact successful parent wait");
    }
}

#[test]
fn r2_guardian_ack_loss_preserves_child_collection() {
    parent_ack_loss(Role::Guardian, &[Role::Worker, Role::CleanupOwner]);
}

#[test]
fn r2_relay_ack_loss_preserves_child_collection() {
    parent_ack_loss(Role::Relay, &[Role::Guardian]);
}

#[test]
fn r2_ack_loss_preserves_owner_provider_custody() {
    use umbra_isolation::managed::owner::Owner;
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::CleanupOwner).unwrap();
    let mut silent = Peer::spawn(Role::Guardian);
    let directory = tempfile::Builder::new()
        .prefix("u-owner-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(directory.path().join("s")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_child", "--nocapture"])
        .env("UMBRA_R2_PEER_DIR", directory.path())
        .spawn()
        .unwrap();
    let participant = identity(provider.id(), Role::Provider).unwrap();
    let bound = Instant::now() + Duration::from_secs(2);
    let mut cancellation = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < bound);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("{error}"),
        }
    };
    cancellation.set_nonblocking(false).unwrap();
    cancellation
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut ready = [0];
    cancellation.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    let route = Route::new(silent.participant, silent.channel.take().unwrap()).unwrap();
    let mut owner = Owner::new(local, run, 1, provider, cancellation, vec![route])
        .unwrap_or_else(|(error, ..)| panic!("{error}"));
    let ticket = Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap();
    owner
        .start(
            Start {
                run,
                origin: local,
                ticket,
            },
            local,
        )
        .unwrap();
    let mut held_past_ack = false;
    let result = (|| -> std::io::Result<_> {
        loop {
            if let Some(status) = owner.progress()? {
                return Ok(status);
            }
            let now = boot_nanos()?;
            if !held_past_ack && now >= ticket.started + ACK_NANOS + 10_000_000 {
                if now
                    >= ticket
                        .cutoffs(1)
                        .map_err(std::io::Error::other)?
                        .cooperative
                {
                    return Err(std::io::Error::other("local cleanup window expired"));
                }
                if identity(participant.pid, Role::Provider)? != participant {
                    return Err(std::io::Error::other("provider lost at ACK expiry"));
                }
                held_past_ack = true;
                std::fs::write(directory.path().join("release"), b"release")?;
            }
            if Instant::now() >= bound {
                return Err(std::io::Error::other("test observation bound expired"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    let failed = owner.failed();
    let timed_out = owner
        .failures()
        .iter()
        .any(|failure| failure.contains("timed out"));
    let mut provider = owner.into_child();
    std::fs::write(directory.path().join("release"), b"release").unwrap();
    silent.release().unwrap();
    // ECHILD here distinguishes the owner's real wait from a fabricated exit
    // status. Any wait performed below for test rescue cannot satisfy it.
    let mut raw_status = 0;
    // SAFETY: this is our retained child PID and writable status storage.
    let kernel_wait =
        unsafe { libc::waitpid(participant.pid as i32, &mut raw_status, libc::WNOHANG) };
    let already_collected =
        kernel_wait == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    let _ = provider.wait();
    assert!(held_past_ack, "owner abandoned provider at ACK expiry");
    assert!(
        result.unwrap().success(),
        "provider did not exit successfully"
    );
    assert!(failed && timed_out, "publication failure was erased");
    assert!(
        already_collected,
        "production owner omitted the actual parent wait"
    );
}
