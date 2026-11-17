use super::*;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId as HostThread;
use umbra_core::shutdown::{Identity, Proof, ResourceKind};

struct Affine {
    _not_send: Rc<()>,
    ledger: Ledger,
    slot: Option<crate::shutdown::Reservation>,
    log: Arc<Mutex<Vec<(&'static str, HostThread)>>>,
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    envelope: Duration,
}
impl Affine {
    fn record(&self, event: &'static str) {
        self.log.lock().unwrap().push((event, std::thread::current().id()));
    }
}
impl Drop for Affine {
    fn drop(&mut self) {
        self.record("drop-entered");
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        self.slot.take().unwrap().confirm(Proof::Collected { status: "exit 0".into() }).unwrap();
        self.record("drop-finished");
    }
}
impl TraceBackend for Affine {
    fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> { unreachable!() }
    fn next_event(&mut self) -> Result<TraceEvent> { unreachable!() }
    fn poll_event(&mut self, _: Duration) -> Result<crate::EventPoll> { unreachable!() }
    fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> { unreachable!() }
    fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> { unreachable!() }
    fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> { unreachable!() }
    fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> { unreachable!() }
    fn resume(&mut self, _: ResumeCommand) -> Result<()> { unreachable!() }
}
impl TraceControl for Affine {
    fn capabilities(&self) -> PlatformCapabilities { self.record("normal"); PlatformCapabilities::default() }
    fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> { unreachable!() }
    fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> { self.record("terminate"); Ok(()) }
    fn begin_shutdown(&mut self, nonce: Nonce) -> Result<Start> {
        self.record("begin");
        Ok(Start::Local { accepted: self.ledger.freeze(nonce, self.envelope)?, ledger: self.ledger.clone() })
    }
}
struct Abi(Arc<Mutex<Vec<(&'static str, HostThread)>>>);
impl Drop for Abi {
    fn drop(&mut self) { self.0.lock().unwrap().push(("abi-drop", std::thread::current().id())); }
}
impl SyscallAbi for Abi {
    fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> { unreachable!() }
    fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> { unreachable!() }
    fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> { unreachable!() }
}

fn call(connection: &mut Connection, id: u64, request: Request) -> Result<Response> {
    connection.begin();
    connection.send(&Frame::Request { id, payload: wire::encode(&request)? })?;
    match connection.receive()? {
        Frame::Response { id: actual, result } if actual == id => wire::decode(&result?),
        _ => panic!("unexpected frame"),
    }
}

fn exercise(envelope: Duration, invalid_poll: bool) {
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let owner_log = log.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let owner = std::thread::spawn(move || {
        let ledger = Ledger::new();
        let slot = ledger.reserve(ResourceKind::Tracer, None, 1, 0).unwrap();
        slot.acquired(Identity { pid: Some(123), birth: None }).unwrap();
        let backend = Affine { _not_send: Rc::new(()), ledger, slot: Some(slot),
            log: owner_log.clone(), entered: entered_tx, release: release_rx, envelope };
        backend.record("constructed");
        serve_session(Connection::new(right, Duration::from_secs(2)), PlatformSession {
            control: Box::new(backend), abi: Box::new(Abi(owner_log)),
        })
    });
    let owner_id = owner.thread().id();
    let mut connection = Connection::new(left, Duration::from_secs(2));
    assert!(matches!(call(&mut connection, 1, Request::Capabilities).unwrap(), Response::Capabilities(_)));
    let nonce = Nonce::new_v4();
    let Response::ShutdownAccepted(accepted) = call(&mut connection, 2, Request::BeginShutdown { nonce }).unwrap()
    else { panic!("acceptance required") };
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let poll = |nonce| Request::PollShutdown { nonce, generation: accepted.generation, max_wait_ms: 1 };
    let result = call(&mut connection, 3, poll(if invalid_poll { Nonce::new_v4() } else { nonce }));
    if envelope.is_zero() || invalid_poll {
        let error = match result { Err(error) => error, Ok(_) => panic!("failed closure returned success") };
        if !invalid_poll { assert!(error.context.contains("owner-progress failure"), "{error:?}"); }
        release_tx.send(()).unwrap();
        assert!(owner.join().unwrap().is_err());
    } else {
        assert!(matches!(result.unwrap(), Response::ShutdownPoll(Poll::Pending { .. })));
        assert!(!log.lock().unwrap().iter().any(|(event, _)| *event == "drop-finished"));
        release_tx.send(()).unwrap();
        let mut id = 4;
        loop {
            match call(&mut connection, id, poll(nonce)).unwrap() {
                Response::ShutdownPoll(Poll::Pending { .. }) => id += 1,
                Response::ShutdownPoll(Poll::Final(final_state)) => {
                    assert!(accepted.confirms(&final_state).unwrap());
                    assert!(log.lock().unwrap().iter().any(|(event, _)| *event == "abi-drop"));
                    break;
                }
                _ => panic!("wrong shutdown phase"),
            }
        }
        owner.join().unwrap().unwrap();
    }
    let events = log.lock().unwrap();
    for (_, thread) in events.iter() { assert_eq!(*thread, owner_id); }
    assert_eq!(events.iter().filter(|(event, _)| *event == "drop-finished").count(), 1);
}

#[test]
fn shutdown_thread_affinity() { exercise(Duration::from_secs(1), false); }
#[test]
fn shutdown_drop_latch_envelope_fails_closed() { exercise(Duration::ZERO, false); }
#[test]
fn shutdown_wrong_nonce_still_destroys_on_owner() { exercise(Duration::from_secs(1), true); }
