//! R7: actual backend interfaces with controlled RSP transport, no debugger.
use super::*;
use crate::ownership::{Owned, Witness};
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
thread_local! {
    static DROP_REPORTS: std::cell::RefCell<Option<Vec<TeardownReport>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn capture_drop(reports: &[TeardownReport]) {
    DROP_REPORTS.with(|slot| {
        if let Some(captured) = slot.borrow_mut().as_mut() {
            *captured = reports.to_vec();
        }
    });
}

fn slot(generation: u64, done: bool) -> (Session, std::net::TcpStream) {
    let pid = std::process::id() as u64;
    let (rsp, peer) = Rsp::retirement_test_pair();
    (
        Session {
            ownership: None,
            id: TaskId(TaskIdentity {
                native_id: pid,
                generation,
            }),
            thread: tid(generation, generation),
            parent: Some(0),
            stopped: true,
            done,
            reaped: true,
            birth: None,
            torn_down: false,
            exit_evidence: Some(BarrierEvidence::ReapedByPlatform),
            exit_report: Some("W00".into()),
            exec_generation: 0,
            #[cfg(feature = "signal-witness")]
            signal_probe_fired: false,
            signals: crate::signals::Delivery::default(),
            policy_exec_generation: 0,
            initial: false,
            retirement: done.then(|| crate::retirement::Retirement::new(crate::clock::now())),
            retirement_report: None,
            live: Some(Box::new(LiveSession {
                _witness: Witness::new(pid, "body"),
                task: Arc::new(Task::acquire(pid as i32).unwrap()),
                rsp,
                twin: PathBuf::new(),
                interposer: None,
                spawn: None,
                breaks: Owned::new(BTreeMap::new(), pid, "breaks"),
                pending: Owned::new(BTreeMap::new(), pid, "pending"),
                waiting: Owned::new(vec![], pid, "waiting"),
                entry: Owned::new(BTreeMap::new(), pid, "entry"),
                registers: Owned::new(vec![], pid, "registers"),
                scratch: Owned::new(vec![], pid, "scratch"),
            })),
        },
        peer,
    )
}
fn register(backend: &mut MacosTraceBackend, s: Session) {
    backend.watchdog.as_ref().unwrap().add(s.task.clone());
    backend.sessions.push(s);
}

#[test]
fn f284_sibling_window_stop_refuses_before_delivery() {
    signal_window_stop(false);
}

#[test]
fn f284_signal_cause_precedes_return_pc() {
    signal_window_stop(true);
}

fn signal_window_stop(owner_stopped: bool) {
    use std::io::Write;
    let mut owned = SelfTaskBackend(MacosTraceBackend::default());
    let (mut session, mut peer) = slot(100, false);
    let stopped = session.thread;
    let sibling = if owner_stopped {
        stopped
    } else {
        tid(stopped.0.native_id + 1, stopped.0.generation)
    };
    assert_eq!(stopped == sibling, owner_stopped);
    session.pending.insert(
        sibling,
        Pending {
            thread: sibling,
            entry: 0,
            entry_breakpoint: Breakpoint {
                original: [0; 4],
                raw: false,
            },
            gate: 0,
            kind: ReturnKind::Syscall,
            hardware: false,
            creation: None,
        },
    );
    owned.0.sessions.push(session);
    // Controlled register reply puts PC at zero, matching the window gate.
    // Exercise both ownership cases without changing the genuine cause field.
    peer.write_all(b"$00#60").unwrap();
    let failure = owned
        .0
        .stop(0, format!("T1ethread:{:x};", stopped.0.native_id), false)
        .unwrap_err();
    assert!(failure.to_string().contains("signal-during-return-window"));
    assert!(owned.0.events.is_empty(), "no signal or syscall event");
    assert!(!owned.0.sessions[0].signals.pending());
    assert!(owned.0.sessions[0].pending.contains_key(&sibling));
    drop(owned);
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).unwrap();
    let sent = std::str::from_utf8(&sent).unwrap();
    assert!(sent.starts_with("$g;thread:"));
    assert_eq!(sent.matches('$').count(), 1, "no resume or syscall packet");
}

#[test]
fn f284_backend_rejects_duplicate_and_cross_exec_tokens() {
    let mut owned = SelfTaskBackend(MacosTraceBackend::default());
    let (session, mut peer) = slot(100, false);
    let task = session.id;
    let thread = session.thread;
    owned.0.sessions.push(session);
    let token = owned.0.sessions[0]
        .signals
        .issue(task, thread, 0, 30)
        .unwrap();
    let command = || ResumeCommand {
        thread,
        mode: ResumeMode::Signal(token),
        signal: Some(30),
    };
    owned.0.resume(command()).unwrap();
    // Model the next externally stopped state, preserving the consumed token.
    owned.0.sessions[0].stopped = true;
    assert!(owned.0.resume(command()).is_err());
    let stale = owned.0.sessions[0]
        .signals
        .issue(task, thread, 0, 30)
        .unwrap();
    // Deliberately leave authority present: this isolates the generation
    // comparison from the separate invalidation at an exec notification.
    owned.0.sessions[0].exec_generation = 1;
    assert!(owned
        .0
        .resume(ResumeCommand {
            thread,
            mode: ResumeMode::Signal(stale),
            signal: Some(30),
        })
        .is_err());
    drop(owned);
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = Vec::new();
    peer.read_to_end(&mut bytes).unwrap();
    let sent = std::str::from_utf8(&bytes).unwrap();
    assert!(sent.starts_with("$vCont;C1e:64;c#"));
    assert_eq!(sent.matches('$').count(), 1, "exactly one delivery packet");
}

#[test]
fn f284_transport_timeout() {
    signal_transport_failure(0, "timeout");
}

#[test]
fn f284_transport_disconnect() {
    signal_transport_failure(0, "disconnect");
}

#[test]
fn f284_transport_partial() {
    signal_transport_failure(5, "partial send");
}

#[test]
fn f284_transport_completed() {
    signal_transport_failure(usize::MAX, "possibly completed send");
}

fn signal_transport_failure(accepted: usize, reason: &'static str) {
    let mut owned = SelfTaskBackend(MacosTraceBackend::default());
    let (mut session, mut peer) = slot(100, false);
    let thread = session.thread;
    let token = session.signals.issue(session.id, thread, 0, 30).unwrap();
    owned.0.sessions.push(session);
    crate::rsp::SIGNAL_WRITE_ATTEMPTS.with(|count| count.set(0));
    crate::rsp::SIGNAL_WRITE_FAULT.with(|fault| fault.set(Some((accepted, reason))));
    let failure = owned
        .0
        .resume(ResumeCommand {
            thread,
            mode: ResumeMode::Signal(token),
            signal: Some(30),
        })
        .unwrap_err();
    assert!(failure.to_string().contains(reason));
    assert_eq!(
        crate::rsp::SIGNAL_WRITE_ATTEMPTS.with(|count| count.get()),
        1
    );
    assert!(!owned.0.sessions[0].stopped, "failed send leaves an outstanding resume");
    assert!(owned
        .0
        .resume(ResumeCommand {
            thread,
            mode: ResumeMode::Continue,
            signal: None,
        })
        .unwrap_err()
        .to_string()
        .contains("signal-delivery-closed"));
    drop(owned);
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = Vec::new();
    peer.read_to_end(&mut bytes).unwrap();
    let body = b"vCont;C1e:64;c";
    let checksum = body.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    let packet = format!("${}#{checksum:02x}", std::str::from_utf8(body).unwrap());
    assert_eq!(bytes, packet.as_bytes()[..accepted.min(packet.len())]);
}
// Controlled slots hold send rights to this test process. Clear the registry
// and mark sessions done before backend Drop, including assertion unwinding.
struct SelfTaskBackend(MacosTraceBackend);
impl Drop for SelfTaskBackend {
    fn drop(&mut self) {
        if let Some(watchdog) = &self.0.watchdog {
            watchdog
                .tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tasks
                .clear();
        }
        for session in &mut self.0.sessions {
            session.done = true;
        }
    }
}
#[test]
fn retirement_self_task_registry_is_cleared_on_unwind() {
    let watched = Arc::new(Mutex::new(WatchedTasks::default()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut owned = SelfTaskBackend(MacosTraceBackend::default());
        owned.0.watchdog = Some(Watchdog {
            wakeups: Arc::new(rsp::Wakeups::default()),
            tasks: watched.clone(),
            cancel: None,
            worker: None,
        });
        let (live, _peer) = slot(105, false);
        register(&mut owned.0, live);
        assert_eq!(watched.lock().unwrap().tasks.len(), 1);
        panic!("controlled assertion unwind with a registered self Task");
    }));
    assert!(result.is_err());
    assert!(watched.lock().unwrap().tasks.is_empty());
}
#[test]
fn retirement_release_owns_all_resources() {
    let mut owned = SelfTaskBackend(MacosTraceBackend::default());
    let backend = &mut owned.0;
    backend.watchdog = Some(Watchdog {
        wakeups: Arc::new(rsp::Wakeups::default()),
        tasks: Arc::new(Mutex::new(WatchedTasks::default())),
        cancel: None,
        worker: None,
    });
    let (mut retiring, mut peer) = slot(104, true);
    let pid = retiring.id.0.native_id;
    let thread = retiring.thread;
    let breakpoint = Breakpoint {
        original: [0; 4],
        raw: false,
    };
    retiring.breaks.insert(1, breakpoint.clone());
    retiring.pending.insert(
        thread,
        Pending {
            creation: None,
            thread,
            entry: 1,
            entry_breakpoint: breakpoint,
            gate: 2,
            kind: ReturnKind::Wait,
            hardware: false,
        },
    );
    retiring.entry.insert(thread, 1);
    retiring.waiting.push(0);
    retiring.registers.push((1, 1));
    retiring.scratch.push((1, 1, 1));
    let task = Arc::downgrade(&retiring.task);
    crate::ownership::task(pid, &retiring.task);
    register(backend, retiring);
    let mut before = crate::ownership::remaining(pid);
    before.sort_unstable();
    assert_eq!(
        before,
        [
            "body",
            "breaks",
            "entry",
            "pending",
            "registers",
            "scratch",
            "task",
            "waiting"
        ]
    );
    backend.service_retirements();
    assert!(backend.sessions[0].live.is_none());
    assert_eq!(crate::ownership::remaining(pid), Vec::<&str>::new());
    assert!(task.upgrade().is_none(), "both Task owners must release");
    assert!(backend
        .watchdog
        .as_ref()
        .unwrap()
        .tasks
        .lock()
        .unwrap()
        .tasks
        .is_empty());
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0, "RSP socket must close");
}
#[test]
fn retirement_quiesce_resume_preserves_tombstones_and_live_sibling() {
    fn register_owned(backend: &mut MacosTraceBackend, mut session: Session) {
        let parent = session.parent.map(|index| {
            backend.sessions[index]
                .ownership
                .as_ref()
                .unwrap()
                .tracee
                .id()
        });
        let tracee = backend
            .ledger
            .reserve(
                umbra_core::shutdown::ResourceKind::Tracee,
                parent,
                session.id.0.generation,
                0,
            )
            .unwrap();
        tracee
            .acquired(umbra_core::shutdown::Identity {
                pid: Some(session.id.0.native_id as i32),
                birth: None,
            })
            .unwrap();
        session.ownership = Some(TraceOwnership {
            ledger: backend.ledger.clone(),
            tracee,
        });
        register(backend, session);
    }
    let mut owned = SelfTaskBackend(MacosTraceBackend::default());
    let backend = &mut owned.0;
    backend.watchdog = Some(Watchdog {
        wakeups: Arc::new(rsp::Wakeups::default()),
        tasks: Arc::new(Mutex::new(WatchedTasks::default())),
        cancel: None,
        worker: None,
    });
    let (mut live, mut peer) = slot(100, false);
    live.parent = None;
    // Prevent an assertion unwind from treating this controlled transport as a
    // real live inferior. The test owns a send right to itself, not a tracee.
    live.torn_down = true;
    let root = ProcessHandle(live.id);
    let thread = live.thread;
    let live_task = live.task.clone();
    register_owned(backend, live);
    let (old, _old_peer) = slot(101, true);
    register_owned(backend, old);
    backend.service_retirements();
    assert!(backend.sessions[1].live.is_none());
    let facts = |s: &Session| {
        (
            s.id,
            s.parent,
            s.exit_report.clone(),
            s.exit_evidence.clone(),
            s.reaped,
        )
    };
    let before = facts(&backend.sessions[1]);
    let (pending_quiesce, _q_peer) = slot(102, true);
    register_owned(backend, pending_quiesce);
    backend.quiesce(root).unwrap();
    assert!(
        backend.sessions[2].live.is_none(),
        "quiesce must service retirement"
    );
    assert_eq!(facts(&backend.sessions[1]), before);
    let (pending_resume, _r_peer) = slot(103, true);
    register_owned(backend, pending_resume);
    backend
        .resume(ResumeCommand {
            thread,
            mode: ResumeMode::Continue,
            signal: None,
        })
        .unwrap();
    assert!(
        backend.sessions[3].live.is_none(),
        "resume must service retirement"
    );
    assert_eq!(facts(&backend.sessions[1]), before);
    assert!(!backend.sessions[0].stopped, "sibling resumed");
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut packet = [0; 5];
    peer.read_exact(&mut packet).unwrap();
    assert_eq!(&packet, b"$c#63");
    let registry = backend.watchdog.as_ref().unwrap().tasks.lock().unwrap();
    assert_eq!(registry.tasks.len(), 1);
    assert!(Arc::ptr_eq(&registry.tasks[0], &live_task));
    drop(registry);
    backend.sessions[0].done = true;
    // These controlled sessions own rights to this test process. Clear only
    // their fake kill registry before exercising the production drain path.
    backend
        .watchdog
        .as_ref()
        .unwrap()
        .tasks
        .lock()
        .unwrap()
        .tasks
        .clear();
    backend
        .terminate(root, TerminationPolicy::Immediate)
        .unwrap();
    let umbra_platform::shutdown::Start::Local { accepted, ledger } = backend
        .begin_shutdown(umbra_core::shutdown::Nonce::new_v4())
        .unwrap()
    else {
        panic!("native owner")
    };
    assert_eq!(
        accepted.slots.len(),
        4,
        "retired child history survives quiesce/resume/terminate"
    );
    assert!(accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap());
}

#[test]
fn retirement_quiesce_services_exit_after_pending_entry() {
    let mut backend = MacosTraceBackend::default();
    let (mut pending, _peer) = slot(201, true);
    pending.rsp.retirement_wait_gate(1);
    let root = ProcessHandle(pending.id);
    let owner = Arc::downgrade(&pending.task);
    backend.sessions.push(pending);
    backend.quiesce(root).unwrap();
    assert!(
        backend.sessions[0].live.is_none(),
        "exit pass must release the newly available owned wait"
    );
    assert!(owner.upgrade().is_none());
}

fn killed_pending(generation: u64) -> (Session, std::net::TcpStream) {
    let (mut session, peer) = slot(generation, true);
    session.rsp.retirement_live_child();
    session.rsp.retirement_wait_gate(usize::MAX);
    let start = crate::clock::now() - Duration::from_secs(2);
    let mut retirement = crate::retirement::Retirement::new(start);
    retirement.service(start, Ok(()), &mut session.rsp);
    retirement.service(
        start + crate::retirement::TRACER_BOUND,
        Ok(()),
        &mut session.rsp,
    );
    assert!(retirement.outcome().is_none());
    assert!(retirement.overrun.is_some());
    session.retirement = Some(retirement);
    (session, peer)
}
#[test]
fn retirement_backend_late_reap_terminates_ok() {
    let mut backend = MacosTraceBackend::default();
    let (mut session, _peer) = killed_pending(202);
    let root = ProcessHandle(session.id);
    let owner = Arc::downgrade(&session.task);
    session.rsp.retirement_wait_gate(1);
    backend.sessions.push(session);
    backend.service_retirements();
    assert!(owner.upgrade().is_some());
    backend
        .terminate(root, TerminationPolicy::Immediate)
        .unwrap();
    assert!(owner.upgrade().is_none());
    assert!(backend.failed_retirements.is_empty());
    assert!(backend.teardown_reports[0]
        .steps
        .iter()
        .any(|step| matches!(step, TeardownStep::RetirementOverrun { .. })));
    assert!(backend.teardown_reports[0].steps.iter().any(|step| matches!(step,
        TeardownStep::Retirement(crate::FinishOutcome::KilledThenReaped(status)) if status.signal() == Some(libc::SIGKILL))));
}
#[test]
fn retirement_backend_exhausted_drain_retains_child_and_failure() {
    let mut backend = MacosTraceBackend::default();
    let (session, _peer) = killed_pending(203);
    let root = ProcessHandle(session.id);
    let tracer = session.rsp.tracer_pid();
    let owner = Arc::downgrade(&session.task);
    backend.sessions.push(session);
    backend.service_retirements();
    assert!(backend
        .terminate(root, TerminationPolicy::Immediate)
        .is_err());
    assert!(owner.upgrade().is_some());
    assert_eq!(backend.failed_retirements.len(), 1);
    assert_eq!(backend.failed_retirements[0].rsp.tracer_pid(), tracer);
    let original = backend.teardown_reports[0].steps.clone();
    assert!(original.iter().any(|step| matches!(
        step,
        TeardownStep::Retirement(crate::FinishOutcome::BoundExhausted(_))
    )));
    assert!(original
        .iter()
        .any(|step| matches!(step, TeardownStep::RetirementOverrun { .. })));
    backend.failed_retirements[0].rsp.retirement_wait_gate(0);
    backend.finalize_failed_retirements();
    assert!(backend.teardown_reports[0].steps.starts_with(&original));
    assert!(backend.teardown_reports[0].steps.iter().any(
        |step| matches!(step, TeardownStep::EmergencyTracer(attempt) if attempt.outcome.success())
    ));
    assert!(backend.failed_retirements[0].torn_down);
    let report = backend.teardown_reports[0].clone();
    backend.finalize_failed_retirements();
    assert_eq!(
        backend.teardown_reports[0], report,
        "emergency attempt is once only"
    );
    drop(backend);
    assert!(owner.upgrade().is_none());
}
#[test]
fn retirement_drop_unconfirmed_resource_owner_records_emergency() {
    let mut backend = MacosTraceBackend::default();
    let (mut session, mut peer) = slot(204, true);
    session.exit_evidence = None;
    session.exit_report = None;
    session.reaped = false;
    session.retirement = Some(crate::retirement::Retirement::new(
        crate::clock::now() - Duration::from_secs(2),
    ));
    let root = ProcessHandle(session.id);
    let owner = Arc::downgrade(&session.task);
    backend.sessions.push(session);
    backend.service_retirements();
    assert!(backend
        .terminate(root, TerminationPolicy::Immediate)
        .is_err());
    assert!(owner.upgrade().is_some());
    let original = backend.teardown_reports[0].steps.clone();
    backend.finalize_failed_retirements();
    let report = &backend.teardown_reports[0];
    assert!(report.unconfirmed);
    assert!(report.steps.starts_with(&original));
    let warning = report
        .steps
        .iter()
        .position(|step| matches!(step, TeardownStep::Warning(_)))
        .unwrap();
    let finish = report
        .steps
        .iter()
        .position(|step| matches!(step, TeardownStep::EmergencyTracer(_)))
        .unwrap();
    assert!(warning < finish);
    drop(backend);
    assert!(owner.upgrade().is_none());
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[test]
fn retirement_actual_backend_drop_merges_failed_owner_report() {
    for confirmed in [true, false] {
        let mut backend = MacosTraceBackend::default();
        let (mut session, mut peer) = slot(301, true);
        session.rsp.retirement_live_child();
        if !confirmed {
            session.exit_evidence = None;
            session.exit_report = None;
            session.reaped = false;
        }
        // A recorded terminal failure owns real resources before actual Drop.
        let failure = crate::FinishOutcome::UnconfirmedBarrier("retained original failure".into());
        let report = TeardownReport {
            pid: session.id.0.native_id as i32,
            identity: session.birth,
            tracer: session.rsp.tracer_pid(),
            steps: vec![TeardownStep::Retirement(failure)],
            timed_out: true,
            unconfirmed: !confirmed,
            elapsed: Duration::ZERO,
        };
        let owner = Arc::downgrade(&session.task);
        session.retirement_report = Some(report.clone());
        backend.teardown_reports.push(report.clone());
        backend.failed_retirements.push(session);
        DROP_REPORTS.with(|slot| *slot.borrow_mut() = Some(vec![]));
        drop(backend);
        let reports = DROP_REPORTS.with(|slot| slot.borrow_mut().take().unwrap());
        assert_eq!(reports.len(), 1);
        assert!(reports[0].steps.starts_with(&report.steps));
        let emergency = reports[0]
            .steps
            .iter()
            .find_map(|step| match step {
                TeardownStep::EmergencyTracer(attempt) => Some(attempt),
                _ => None,
            })
            .expect("actual backend Drop must merge emergency results before field destruction");
        assert!(
            matches!(&emergency.outcome, crate::FinishOutcome::KilledThenReaped(status) if status.signal() == Some(libc::SIGKILL))
        );
        assert!(emergency.operations.iter().any(|operation| matches!(
            operation,
            crate::retirement::TracerOperation::Kill(Ok(()))
        )));
        assert!(owner.upgrade().is_none());
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
#[test]
fn retirement_simultaneous_exits_release_independent_owners() {
    let mut backend = MacosTraceBackend::default();
    let mut owners = vec![];
    let mut peers = vec![];
    for generation in 310..313 {
        let (session, peer) = slot(generation, true);
        owners.push(Arc::downgrade(&session.task));
        peers.push(peer);
        backend.sessions.push(session);
    }
    backend.service_retirements();
    assert!(owners.iter().all(|owner| owner.upgrade().is_none()));
    assert!(backend.sessions.iter().all(|s| s.live.is_none()));
    assert_eq!(backend.teardown_reports.len(), 3);
    backend.service_retirements();
    assert_eq!(backend.teardown_reports.len(), 3);
}
#[test]
fn retirement_root_exit_waits_for_pending_child_without_losing_events() {
    let mut backend = MacosTraceBackend::default();
    let (root, _root_peer) = slot(320, true);
    let root_id = root.id;
    let (mut child, _child_peer) = slot(321, true);
    let child_id = child.id;
    child.rsp.retirement_wait_gate(1);
    backend.sessions.extend([root, child]);
    backend.events.extend([
        TraceEvent::Exit {
            task: root_id,
            status: ExitStatus::Code(0),
        },
        TraceEvent::Exit {
            task: child_id,
            status: ExitStatus::Code(7),
        },
    ]);
    let first = backend.poll_event(Duration::from_millis(1)).unwrap();
    assert!(
        matches!(first, umbra_platform::EventPoll::Event(TraceEvent::Exit { task, status: ExitStatus::Code(7) }) if task == child_id)
    );
    assert!(backend.sessions[1].live.is_some());
    let second = backend.poll_event(Duration::from_millis(1)).unwrap();
    assert!(
        matches!(second, umbra_platform::EventPoll::Event(TraceEvent::Exit { task, .. }) if task == root_id)
    );
    assert!(backend.sessions[1].live.is_none());
    assert!(backend.events.is_empty());
}
#[test]
fn retirement_slow_gap_preserves_history_for_the_next_child() {
    // Assertion unwinding must never signal the self-Task used by the live root.
    struct SafeBackend(MacosTraceBackend);
    impl Drop for SafeBackend {
        fn drop(&mut self) {
            crate::clock::SCRIPT.with(|clock| clock.set(None));
            for session in &mut self.0.sessions {
                session.done = true;
            }
        }
    }
    let mut owned = SafeBackend(MacosTraceBackend::default());
    let backend = &mut owned.0;
    let (mut root, _root_peer) = slot(329, false);
    root.parent = None;
    root.torn_down = true;
    let root_owner = Arc::downgrade(&root.task);
    backend.sessions.push(root);
    let (first, _peer) = slot(330, true);
    backend.sessions.push(first);
    backend.service_retirements();
    let old = backend.sessions[1].id;
    let future = crate::clock::now() + Duration::from_secs(60 * 60);
    let (mut later, _later_peer) = slot(331, true);
    later.retirement = Some(crate::retirement::Retirement::new(future));
    backend.sessions.push(later);
    crate::clock::SCRIPT.with(|clock| clock.set(Some((future, future))));
    backend.service_retirements();
    crate::clock::SCRIPT.with(|clock| clock.set(None));
    assert!(!backend.sessions[0].done);
    assert!(backend.sessions[0].live.is_some());
    assert!(root_owner.upgrade().is_some());
    assert_eq!(backend.sessions[1].id, old);
    assert_eq!(backend.sessions.len(), 3);
    assert!(backend.sessions[1..].iter().all(|s| s.live.is_none()));
    assert_eq!(backend.teardown_reports.len(), 2);
}

#[test]
fn retirement_barrier_keeps_real_owners() {
    let mut backend = MacosTraceBackend::default();
    let (mut session, mut peer) = slot(340, true);
    session.exit_evidence = None;
    session.exit_report = None;
    session.reaped = false;
    let owner = Arc::downgrade(&session.task);
    backend.sessions.push(session);
    backend.service_retirements();
    let retained = backend.sessions[0].live.is_some() && owner.upgrade().is_some();
    peer.set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let socket = peer.read(&mut [0]);
    // Settle the genuine obligation before assertions, including mutation failures.
    backend.sessions[0].exit_evidence = Some(BarrierEvidence::ReapedByPlatform);
    backend.service_retirements();
    assert!(
        retained,
        "unconfirmed barrier must retain the actual Task and body"
    );
    assert!(
        matches!(socket, Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "unconfirmed barrier must retain the socket: {socket:?}"
    );
    assert!(
        owner.upgrade().is_none(),
        "confirmation must release the owner"
    );
}
