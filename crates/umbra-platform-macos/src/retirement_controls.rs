//! R7: actual backend interfaces with controlled RSP transport, no debugger.
use super::*;
use crate::ownership::{Owned, Witness};
use std::io::Read;

fn slot(generation: u64, done: bool) -> (Session, std::net::TcpStream) {
    let pid = std::process::id() as u64;
    let (rsp, peer) = Rsp::retirement_test_pair();
    (
        Session {
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
fn retirement_release_owns_all_resources() {
    let mut backend = MacosTraceBackend::default();
    backend.watchdog = Some(Watchdog {
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
    register(&mut backend, retiring);
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
    assert!(task.upgrade().is_none(), "both Task owners must release");
    assert!(
        backend
            .watchdog
            .as_ref()
            .unwrap()
            .tasks
            .lock()
            .unwrap()
            .tasks
            .is_empty()
    );
    assert_eq!(crate::ownership::remaining(pid), Vec::<&str>::new());
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0, "RSP socket must close");
}
#[test]
fn retirement_quiesce_resume_preserves_tombstones_and_live_sibling() {
    let mut backend = MacosTraceBackend::default();
    backend.watchdog = Some(Watchdog {
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
    register(&mut backend, live);
    let (old, _old_peer) = slot(101, true);
    register(&mut backend, old);
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
    register(&mut backend, pending_quiesce);
    backend.quiesce(root).unwrap();
    assert!(
        backend.sessions[2].live.is_none(),
        "quiesce must service retirement"
    );
    assert_eq!(facts(&backend.sessions[1]), before);
    let (pending_resume, _r_peer) = slot(103, true);
    register(&mut backend, pending_resume);
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
}
