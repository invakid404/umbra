//! Ordinary-child workload's separate authoritative admission process.
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use umbra_core::managed::RunIdentity;
use umbra_isolation::managed::{
    admission::{AdmissionClient, AdmissionService},
    Channel,
};

pub fn start(
    deadline: Instant,
) -> (
    umbra_platform::admission::Admission,
    super::child::OwnedChild,
    u64,
) {
    let run = RunIdentity {
        nonce: umbra_core::shutdown::Nonce::new_v4(),
        generation: 1,
    };
    let cutoff = super::observation::boot_nanos() + 60_000_000_000;
    let (mut caller, owner) = UnixStream::pair().unwrap();
    caller
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "r2_performance_admission_owner", "--nocapture"])
        .env(
            "UMBRA_F299_ADMISSION_RUN",
            serde_json::to_string(&run).unwrap(),
        )
        .env("UMBRA_F299_ADMISSION_CUTOFF", cutoff.to_string())
        .stdin(Stdio::from(OwnedFd::from(owner)))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let child = super::child::OwnedChild::new(child, deadline);
    let mut ready = [0];
    caller.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1], "admission owner readiness");
    super::observation::record("helpers_ready_go", Some(u64::from(child.id())));
    let client = AdmissionClient::new(run, Channel::new(caller).unwrap()).unwrap();
    (
        umbra_platform::admission::Admission::new(Box::new(client)),
        child,
        cutoff,
    )
}

pub fn serve() {
    let Some(run) = std::env::var_os("UMBRA_F299_ADMISSION_RUN") else {
        return;
    };
    let run: RunIdentity = serde_json::from_str(run.to_str().unwrap()).unwrap();
    let cutoff: u64 = std::env::var("UMBRA_F299_ADMISSION_CUTOFF")
        .unwrap()
        .parse()
        .unwrap();
    let stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    let mut ready = stream.try_clone().unwrap();
    let mut owner = AdmissionService::new(run, 64, Channel::new(stream).unwrap()).unwrap();
    ready.write_all(&[1]).unwrap();
    drop(ready);
    let mut entered = false;
    loop {
        assert!(
            super::observation::boot_nanos() < cutoff,
            "admission owner deadline"
        );
        owner.poll(cutoff).unwrap();
        entered |= owner.outstanding() != 0;
        if entered && owner.outstanding() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
