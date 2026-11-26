//! Real process-generation races with a retained peer descriptor, not EOF.
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use umbra_core::managed_handoff;
use umbra_platform_macos::{probe_owner_watch, probe_watch_registration_error, WatchCheckpoint};

const NAME: &str = "r2_native_watch_registration_race";

pub fn peer(path: &Path) {
    let stream = UnixStream::connect(path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !managed_handoff::send(&stream, &stream).unwrap() {
        assert!(Instant::now() < deadline, "peer handoff timed out");
        std::thread::yield_now();
    }
    // The parent releases stdin at the chosen registration boundary. Bound
    // this helper independently so an assertion cannot strand a live child.
    let mut input = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(unsafe { libc::poll(&mut input, 1, 10_000) } > 0);
    assert_eq!(std::io::stdin().read(&mut [0u8; 1]).unwrap(), 0);
}

struct Peer(Child);
impl Peer {
    fn stop(&mut self) {
        drop(self.0.stdin.take());
        assert!(self.0.wait().unwrap().success(), "peer parent wait failed");
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

fn connected() -> (Peer, UnixStream, UnixStream) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owner.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = Peer(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("UMBRA_NATIVE_WATCH_PEER", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "peer connection timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("peer connection failed: {e}"),
        }
    };
    let retained = loop {
        if let Some(retained) = managed_handoff::receive(&stream).unwrap() {
            break retained;
        }
        assert!(Instant::now() < deadline, "peer receipt timed out");
        std::thread::sleep(Duration::from_millis(1));
    };
    (peer, stream, retained)
}

pub fn run() {
    use WatchCheckpoint::*;
    for exit_at in [
        Some(BeforeRegistration),
        Some(AfterRegistration),
        Some(BeforeGo),
        None,
    ] {
        let (mut peer, stream, mut retained) = connected();
        let mut socket_pid = 0i32;
        let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut socket_pid as *mut i32).cast(),
                &mut length,
            )
        };
        eprintln!(
            "native watch peer: child={} socket={socket_pid} query={result}",
            peer.0.id()
        );
        let mut eof_check = stream.try_clone().unwrap();
        eof_check.set_nonblocking(true).unwrap();
        let mut reached = Vec::new();
        let observed = probe_owner_watch(peer.0.id(), stream, |point| {
            reached.push(point);
            if Some(point) == exit_at {
                peer.stop();
            }
        });
        // Every assertion follows the real parent wait, including mutations.
        peer.stop();
        assert_eq!(result, 0, "socket peer credential query failed");
        assert_eq!(socket_pid as u32, peer.0.id(), "wrong socket peer");
        if let Some(point) = exit_at {
            assert!(
                reached.contains(&point),
                "race boundary was not reached: {observed:?}"
            );
        }
        assert_eq!(
            eof_check.read(&mut [0u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "retained endpoint incorrectly allowed EOF"
        );
        // Writing through the retained descriptor changes LOCAL_PEERPID on
        // this kernel. Prove its connection only after observing the watch.
        retained.write_all(b"I").unwrap();
        let mut identity_byte = [0];
        eof_check.read_exact(&mut identity_byte).unwrap();
        assert_eq!(identity_byte, *b"I");
        match exit_at {
            Some(BeforeRegistration | AfterRegistration) => {
                assert!(observed.is_err(), "dead owner admitted: {observed:?}");
                assert!(!reached.contains(&BeforeGo), "dead owner reached GO");
            }
            Some(BeforeGo) => assert!(
                observed.unwrap(),
                "process exit was lost while peer endpoint remained open"
            ),
            None => assert!(!observed.unwrap(), "live owner falsely reported dead"),
        }
        eprintln!("native watch: exit={exit_at:?} boundaries={reached:?}; parent waited, no EOF");
    }
    assert!(
        probe_watch_registration_error().is_err(),
        "kernel registration error was ignored"
    );
}
