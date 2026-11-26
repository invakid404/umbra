use super::*;

#[test]
fn r2_preaccepted_work_cancel() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use umbra_core::managed::{CleanupPath, RunIdentity, Ticket};
    use umbra_isolation::managed::{
        admission::AdmissionClient,
        boot_nanos,
        cancellation::{Notification, Receiver},
        Channel,
    };
    let run = RunIdentity {
        nonce: umbra_core::shutdown::Nonce::from_u128(1),
        generation: 1,
    };
    let (admission, _authority) = UnixStream::pair().unwrap();
    let client = AdmissionClient::new(run, Channel::new(admission).unwrap()).unwrap();
    let cutoff = client.cutoff();
    let (provider, owner) = UnixStream::pair().unwrap();
    let receiver = Receiver::new(run, 64, Channel::new(provider).unwrap(), cutoff.clone()).unwrap();
    let wakeups = Arc::new(rsp::Wakeups::default());
    let (mut rsp, mut debugger) = Rsp::retirement_test_pair();
    wakeups.add(rsp.wakeup());
    let monitor = ManagedCancellation::new(receiver, wakeups).unwrap();
    let sender = std::thread::spawn(move || {
        debugger
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // Accept the complete ordinary request before publishing on the
        // independent owner route. The main RPC remains waiting for its reply.
        let mut request = [0; b"qCurrentThread".len() + 4];
        debugger.read_exact(&mut request).unwrap();
        assert!(request.starts_with(b"$qCurrentThread#"));
        let ticket = Ticket::new(boot_nanos().unwrap(), CleanupPath::Failed, 64).unwrap();
        let mut owner = Channel::new(owner).unwrap();
        owner.queue(&Notification { run, ticket }).unwrap();
        let limit = boot_nanos().unwrap() + 1_000_000_000;
        while !owner.flush(limit).unwrap() {
            std::thread::yield_now();
        }
        (ticket, debugger, owner)
    });
    let started = Instant::now();
    let error = rsp.request("qCurrentThread").unwrap_err();
    assert!(
        error.to_string().contains("ordinary work cancelled"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "independent wakeup exceeded its bound"
    );
    let (ticket, _debugger, _owner) = sender.join().unwrap();
    assert!(monitor.requested.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(cutoff.clamp(u64::MAX), ticket.cutoffs(64).unwrap().fallback);
    assert!(
        rsp.begin_teardown(),
        "cancellation must retain an aligned RSP stream"
    );
    assert!(rsp
        .teardown_interrupt(Instant::now() + Duration::from_secs(1))
        .is_ok());
    drop(monitor);
}
