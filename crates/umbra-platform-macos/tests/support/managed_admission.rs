use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use umbra_core::{shutdown::Nonce, *};
use umbra_platform_macos::MacosTraceBackend;
pub fn install(backend: &mut MacosTraceBackend) -> (Arc<AtomicU64>, std::thread::JoinHandle<u64>) {
    install_with_capacity(backend, 64, false)
}
pub fn install_with_capacity(
    backend: &mut MacosTraceBackend,
    capacity: u64,
    occupied: bool,
) -> (Arc<AtomicU64>, std::thread::JoinHandle<u64>) {
    use umbra_isolation::managed::{
        admission::{AdmissionClient, AdmissionService},
        boot_nanos, Channel,
    };
    let run = umbra_core::managed::RunIdentity {
        nonce: Nonce::from_u128(1),
        generation: 1,
    };
    let (provider, owner) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut client = AdmissionClient::new(run, Channel::new(provider).unwrap()).unwrap();
    let cutoff = client.cutoff();
    let count = Arc::new(AtomicU64::new(0));
    let observed = count.clone();
    let service = std::thread::spawn(move || {
        let mut service =
            AdmissionService::new(run, capacity, Channel::new(owner).unwrap()).unwrap();
        loop {
            let result = service.poll(u64::MAX);
            observed.store(service.outstanding(), Ordering::Release);
            match result {
                Ok(()) => std::thread::sleep(std::time::Duration::from_millis(1)),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return service.outstanding()
                }
                Err(error) => panic!("owner admission failed: {error}"),
            }
        }
    });
    if occupied {
        client
            .exchange(
                AdmissionOperation::Reserve {
                    obligation: Nonce::from_u128(99),
                },
                boot_nanos().unwrap() + 1_000_000_000,
            )
            .unwrap()
            .unwrap();
    }
    backend
        .install_admission(
            umbra_platform::admission::Admission::new(Box::new(client)),
            Box::new(move || {
                let now = boot_nanos().map_err(|e| {
                    UmbraError::new(ErrorKind::Io, "admission clock", e.to_string())
                })?;
                Ok(cutoff.clamp(now.saturating_add(5_000_000_000)))
            }),
        )
        .unwrap();
    (count, service)
}
