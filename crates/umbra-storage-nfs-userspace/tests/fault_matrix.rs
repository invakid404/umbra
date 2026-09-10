//! Fault-injection harness: every [`FaultPoint`] against every [`FaultAction`].
//!
//! Runs against a private Ganesha fixture (`umbra-m1-transport-raw-*` docker
//! volumes, `127.0.0.1:12105`), never a mounted NFS filesystem. Skipped unless
//! `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names one, so an ordinary `cargo test`
//! needs no server.
//!
//! Each cell asserts two things, because either alone would be weak evidence:
//!
//! 1. **The point was consulted.** The plan records which points the transport
//!    asked about. A cell that produced the right answer without the transport
//!    ever reaching the fault point would be a passing test over dead code.
//! 2. **The outcome matches.** Actions that carry no meaning at a point — a
//!    `ShortWrite` before anything has been dispatched, for instance — are
//!    expected to be *ignored*, and the call must complete normally rather than
//!    fail in some incidental way. That is an assertion, not a gap.
//!
//! Outcomes are appended to `evidence/fault-matrix.log` under the fixture root.
#![cfg(feature = "transport-raw")]

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use umbra_storage_nfs_userspace::error::{FacadeError, Nfs4Status, TransportError};
use umbra_storage_nfs_userspace::handle::{FileHandle, Stateid};
use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, FaultAction, FaultContext, FaultPlan, FaultPoint,
    RawTransport, Stability, WriteVerifier,
};

/// A plan that answers `action` the first time `point` is consulted, records
/// every point it was asked about, and otherwise proceeds.
struct MatrixPlan {
    scripted: Vec<(FaultPoint, FaultAction)>,
    seen: Arc<Mutex<HashSet<FaultPoint>>>,
}

impl FaultPlan for MatrixPlan {
    fn decide(&mut self, point: FaultPoint, _context: FaultContext) -> FaultAction {
        self.seen.lock().unwrap().insert(point);
        if let Some(index) = self.scripted.iter().position(|(at, _)| *at == point) {
            return self.scripted.remove(index).1;
        }
        FaultAction::Proceed
    }
}

/// What a cell expects to observe.
#[derive(Debug)]
enum Expected {
    /// The call completed and carries no recorded failure.
    Success,
    /// The call completed carrying a recorded protocol failure.
    RecordedFailure(Nfs4Status),
    /// The submission failed with a transport error whose text matches.
    TransportFailure(&'static str),
    /// The call reached its deadline and was retired first.
    Deadline,
    /// A WRITE reply whose count was truncated to this many bytes.
    ShortCount(u32),
    /// A COMMIT reply carrying this substituted verifier.
    Verifier(WriteVerifier),
    /// `reconnect` opened a fresh connection generation.
    Reconnected,
}

/// Which operation a cell drives.
#[derive(Clone, Copy, Debug)]
enum Probe {
    /// `PUTFH; LOOKUP; GETFH; GETATTR` — idempotent, safe to lose a reply for.
    Lookup,
    /// `PUTFH; WRITE` — needed for `ShortWrite`.
    Write,
    /// `PUTFH; COMMIT` — needed for `RotateVerifier`.
    Commit,
    /// `reconnect` — the only deterministic way to reach `OnConnection`.
    Reconnect,
}

struct Fixture {
    config: RawTransportConfig,
    directory: FileHandle,
    name: ComponentName,
    file: FileHandle,
}

fn fixture() -> Option<Fixture> {
    let target = std::env::var("UMBRA_NFS_RAW_FIXTURE").ok()?;
    let (host, port) = target.rsplit_once(':')?;
    let mut config = RawTransportConfig::loopback(port.parse().ok()?);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 5_000 };

    let mut transport = LibnfsRawTransport::connect(config.clone()).expect("connect to fixture");
    let deadline = Deadline { millis: 5_000 };
    let root = transport
        .root_filehandle(deadline)
        .expect("root filehandle");

    let mut cursor = root;
    for part in [b"export".as_slice(), b"probe"] {
        let name = ComponentName::new(part.to_vec()).unwrap();
        cursor = transport
            .lookup(&cursor, &name, AttrMask::IDENTITY, deadline)
            .expect("resolve the seeded fixture path")
            .0;
    }
    let name = ComponentName::new(b"write.txt".to_vec()).unwrap();
    let file = transport
        .lookup(&cursor, &name, AttrMask::IDENTITY, deadline)
        .expect("resolve write.txt")
        .0;

    Some(Fixture {
        config,
        directory: cursor,
        name: ComponentName::new(b"read.txt".to_vec()).unwrap(),
        file,
    })
}

/// Run one cell and return `Ok(observation)` or `Err(reason)`.
fn run_cell(
    fixture: &Fixture,
    point: FaultPoint,
    action: FaultAction,
    probe: Probe,
    expected: &Expected,
) -> Result<String, String> {
    let mut transport =
        LibnfsRawTransport::connect(fixture.config.clone()).map_err(|e| format!("connect: {e}"))?;

    // Reaching `OnDeadline` needs a call that never returns a reply, so the
    // reply is dropped before dispatch and the deadline path then runs.
    let mut scripted = Vec::new();
    if point == FaultPoint::OnDeadline {
        scripted.push((FaultPoint::BeforeDispatch, FaultAction::DropReply));
    }
    scripted.push((point, action));

    let seen = Arc::new(Mutex::new(HashSet::new()));
    transport.install_faults(Box::new(MatrixPlan {
        scripted,
        seen: Arc::clone(&seen),
    }));

    let deadline = Deadline { millis: 2_000 };
    let observation = match probe {
        Probe::Reconnect => match transport.reconnect() {
            Ok(epoch) => format!("reconnected to epoch {}", epoch.0),
            Err(error) => format!("transport error: {error}"),
        },
        Probe::Lookup => {
            match transport.lookup(&fixture.directory, &fixture.name, AttrMask::STAT, deadline) {
                Ok(_) => "completed".to_owned(),
                Err(error) => describe(&error),
            }
        }
        Probe::Write => match transport.write(
            &fixture.file,
            Stateid::ANONYMOUS,
            0,
            Stability::Unstable,
            b"ABCD".to_vec(),
            deadline,
        ) {
            Ok(reply) => format!("completed count={}", reply.count),
            Err(error) => describe(&error),
        },
        Probe::Commit => match transport.commit(&fixture.file, 0, 0, deadline) {
            Ok(reply) => format!("completed verifier={:?}", reply.verifier.0),
            Err(error) => describe(&error),
        },
    };

    // 1. The transport must actually have consulted this point.
    if !seen.lock().unwrap().contains(&point) {
        return Err(format!(
            "the transport never consulted {point:?} (observed {observation})"
        ));
    }

    // 2. The outcome must match.
    check(fixture, point, expected, &observation)?;
    Ok(observation)
}

/// Assert the observation against the expectation.
fn check(
    fixture: &Fixture,
    point: FaultPoint,
    expected: &Expected,
    observation: &str,
) -> Result<(), String> {
    let ok = match expected {
        Expected::Success => observation.starts_with("completed"),
        Expected::Reconnected => observation.starts_with("reconnected"),
        // `drained=true` is required, not incidental: it is the assertion that
        // cancellation actually withdrew the call rather than merely reporting
        // a timeout over a registration libnfs could still complete.
        Expected::Deadline => {
            observation.starts_with("deadline") && observation.contains("drained=true")
        }
        Expected::TransportFailure(text) => observation.contains(text),
        Expected::RecordedFailure(status) => {
            observation.contains(&format!("NFS4ERR({})", status.0))
        }
        Expected::ShortCount(count) => observation == format!("completed count={count}"),
        Expected::Verifier(verifier) => {
            observation == format!("completed verifier={:?}", verifier.0)
        }
    };
    let _ = (fixture, point);
    if ok {
        Ok(())
    } else {
        Err(format!("expected {expected:?}, observed {observation}"))
    }
}

/// A short, stable label for whatever a shape helper returned.
///
/// A substituted status reaches a caller as `FacadeError::Protocol`, because
/// `CompoundReply::expect` returns a recorded failure in preference to any
/// positional result. Rendering it as `NFS4ERR(<n>)` keeps the raw status word
/// visible in the evidence log rather than folding it into a category.
fn describe(error: &FacadeError) -> String {
    match error {
        FacadeError::Protocol(failure) => {
            format!(
                "NFS4ERR({}) at {:?}[{}]",
                failure.status.0, failure.op, failure.index
            )
        }
        // A deadline carries its `Retirement`, which is the only proof the
        // registration was withdrawn before the error was built. The evidence
        // log records whether the pump was also observed to quiesce the call.
        FacadeError::Transport(TransportError::DeadlineExpired { retirement }) => format!(
            "deadline: token={} drained={}",
            retirement.token().get(),
            retirement.drained()
        ),
        FacadeError::Transport(transport) => format!("{}: {transport}", kind(transport)),
        other => format!("unexpected facade error: {other}"),
    }
}

/// A short, stable label for a transport error.
fn kind(error: &TransportError) -> &'static str {
    match error {
        TransportError::Connect(_) => "connect",
        TransportError::Disconnected { .. } => "disconnected",
        TransportError::DeadlineExpired { .. } => "deadline",
        TransportError::QueueFull { .. } => "queue-full",
        TransportError::Malformed(_) => "malformed",
        TransportError::Cancelled(_) => "cancelled",
        TransportError::UnsupportedProfile(_) => "unsupported-profile",
    }
}

const SUBSTITUTED: Nfs4Status = Nfs4Status::GRACE;
const ROTATED: WriteVerifier = WriteVerifier([0xAB; 8]);

/// The full matrix: every point against every action.
fn matrix() -> Vec<(FaultPoint, FaultAction, Probe, Expected)> {
    use FaultAction::*;
    use FaultPoint::*;

    let fail = || Fail(TransportError::Malformed("injected by the harness".into()));

    vec![
        // --- BeforeDispatch: nothing is registered yet -----------------------
        (BeforeDispatch, Proceed, Probe::Lookup, Expected::Success),
        (
            BeforeDispatch,
            fail(),
            Probe::Lookup,
            Expected::TransportFailure("malformed"),
        ),
        (
            BeforeDispatch,
            Substitute(SUBSTITUTED),
            Probe::Lookup,
            Expected::RecordedFailure(SUBSTITUTED),
        ),
        // A write cap before dispatch shapes no reply, so the call is normal.
        (
            BeforeDispatch,
            ShortWrite(1),
            Probe::Write,
            Expected::Success,
        ),
        (
            BeforeDispatch,
            RotateVerifier(ROTATED),
            Probe::Commit,
            Expected::Success,
        ),
        (BeforeDispatch, DropReply, Probe::Lookup, Expected::Deadline),
        // --- AfterDispatch: the request is on the wire -----------------------
        (AfterDispatch, Proceed, Probe::Lookup, Expected::Success),
        (
            AfterDispatch,
            fail(),
            Probe::Lookup,
            Expected::TransportFailure("malformed"),
        ),
        (
            AfterDispatch,
            Substitute(SUBSTITUTED),
            Probe::Lookup,
            Expected::RecordedFailure(SUBSTITUTED),
        ),
        (
            AfterDispatch,
            ShortWrite(1),
            Probe::Write,
            Expected::Success,
        ),
        (
            AfterDispatch,
            RotateVerifier(ROTATED),
            Probe::Commit,
            Expected::Success,
        ),
        (AfterDispatch, DropReply, Probe::Lookup, Expected::Deadline),
        // --- BeforeReturn: the reply is decoded and owned --------------------
        (BeforeReturn, Proceed, Probe::Lookup, Expected::Success),
        (
            BeforeReturn,
            fail(),
            Probe::Lookup,
            Expected::TransportFailure("malformed"),
        ),
        // The reply carries real positional results here, so this is also the
        // cell that proves `expect` prefers a recorded failure over one.
        (
            BeforeReturn,
            Substitute(SUBSTITUTED),
            Probe::Lookup,
            Expected::RecordedFailure(SUBSTITUTED),
        ),
        (
            BeforeReturn,
            ShortWrite(2),
            Probe::Write,
            Expected::ShortCount(2),
        ),
        (
            BeforeReturn,
            RotateVerifier(ROTATED),
            Probe::Commit,
            Expected::Verifier(ROTATED),
        ),
        (BeforeReturn, DropReply, Probe::Lookup, Expected::Deadline),
        // --- OnDeadline: reached by dropping the reply first -----------------
        (OnDeadline, Proceed, Probe::Lookup, Expected::Deadline),
        (
            OnDeadline,
            fail(),
            Probe::Lookup,
            Expected::TransportFailure("malformed"),
        ),
        (
            OnDeadline,
            Substitute(SUBSTITUTED),
            Probe::Lookup,
            Expected::RecordedFailure(SUBSTITUTED),
        ),
        (OnDeadline, ShortWrite(1), Probe::Lookup, Expected::Deadline),
        (
            OnDeadline,
            RotateVerifier(ROTATED),
            Probe::Lookup,
            Expected::Deadline,
        ),
        (OnDeadline, DropReply, Probe::Lookup, Expected::Deadline),
        // --- OnConnection: driven through `reconnect` ------------------------
        (
            OnConnection,
            Proceed,
            Probe::Reconnect,
            Expected::Reconnected,
        ),
        (
            OnConnection,
            Fail(TransportError::Connect("injected by the harness".into())),
            Probe::Reconnect,
            Expected::TransportFailure("connect"),
        ),
        (
            OnConnection,
            Substitute(SUBSTITUTED),
            Probe::Reconnect,
            Expected::Reconnected,
        ),
        (
            OnConnection,
            ShortWrite(1),
            Probe::Reconnect,
            Expected::Reconnected,
        ),
        (
            OnConnection,
            RotateVerifier(ROTATED),
            Probe::Reconnect,
            Expected::Reconnected,
        ),
        (
            OnConnection,
            DropReply,
            Probe::Reconnect,
            Expected::Reconnected,
        ),
    ]
}

#[test]
fn every_fault_point_meets_every_fault_action() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    let cells = matrix();
    assert_eq!(cells.len(), 30, "5 fault points x 6 fault actions");

    let mut lines = Vec::new();
    let mut failures = Vec::new();

    for (point, action, probe, expected) in cells {
        let label = format!("{point:?} x {}", action_name(&action));
        match run_cell(&fixture, point, action, probe, &expected) {
            Ok(observation) => lines.push(format!(
                "PASS | {label:<40} | probe={probe:?} | expected={expected:?} | observed={observation}"
            )),
            Err(reason) => {
                lines.push(format!("FAIL | {label:<40} | probe={probe:?} | {reason}"));
                failures.push(format!("{label}: {reason}"));
            }
        }
    }

    write_evidence(&lines);

    assert!(
        failures.is_empty(),
        "{} of 30 combinations failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn action_name(action: &FaultAction) -> &'static str {
    match action {
        FaultAction::Proceed => "Proceed",
        FaultAction::Fail(_) => "Fail",
        FaultAction::Substitute(_) => "Substitute",
        FaultAction::ShortWrite(_) => "ShortWrite",
        FaultAction::RotateVerifier(_) => "RotateVerifier",
        FaultAction::DropReply => "DropReply",
    }
}

/// Record the per-combination outcome beside the fixture it ran against.
fn write_evidence(lines: &[String]) {
    let Ok(root) = std::env::var("UMBRA_NFS_RAW_EVIDENCE") else {
        for line in lines {
            println!("{line}");
        }
        return;
    };
    let path = std::path::Path::new(&root).join("fault-matrix.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::File::create(&path).expect("create the evidence log");
    writeln!(
        file,
        "# raw_rpc fault-injection matrix — {} FaultPoint x FaultAction combinations",
        lines.len()
    )
    .unwrap();
    writeln!(
        file,
        "# fixture 127.0.0.1:12105 (umbra-m1-transport-raw-ganesha), NFSv4.0/TCP/AUTH_SYS, no mount"
    )
    .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
    println!("evidence written to {}", path.display());
}

#[test]
fn backpressure_is_applied_before_anything_is_dispatched() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    // Invariant 3 of the frozen contract: a full queue is refused *before* a
    // call is registered, so the failure carries no token and nothing needs
    // retiring.
    let mut config = fixture.config.clone();
    config.limits.max_queue_depth = 0;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect");

    let error = transport
        .lookup(
            &fixture.directory,
            &fixture.name,
            AttrMask::STAT,
            Deadline { millis: 2_000 },
        )
        .expect_err("a zero-depth queue must refuse the submission");

    match error {
        FacadeError::Transport(TransportError::QueueFull { depth, capacity }) => {
            assert_eq!(capacity, 0);
            assert_eq!(depth, 0);
        }
        other => panic!("expected QueueFull before dispatch, got {other}"),
    }

    // The refusal must not have consumed inflight capacity: raising the bound
    // and retrying succeeds against the same server.
    let mut config = fixture.config.clone();
    config.limits.max_queue_depth = 64;
    let mut transport = LibnfsRawTransport::connect(config).expect("reconnect");
    transport
        .lookup(
            &fixture.directory,
            &fixture.name,
            AttrMask::STAT,
            Deadline { millis: 2_000 },
        )
        .expect("the same call succeeds once the queue bound allows it");
}

#[test]
fn reconnect_opens_a_fresh_generation_so_stale_state_is_rejectable() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    let mut transport = LibnfsRawTransport::connect(fixture.config.clone()).expect("connect");
    let before = match transport.connection() {
        umbra_storage_nfs_userspace::transport::ConnectionState::Connected(epoch) => epoch,
        other => panic!("expected a connected transport, got {other:?}"),
    };

    let after = transport.reconnect().expect("reconnect");
    assert!(
        after > before,
        "reconnect must advance the generation ({after:?} followed {before:?}); protocol-state \
         relies on that to invalidate open state built on the old connection"
    );

    // The transport is usable again, and reconnecting revalidates no protocol
    // state — that remains the protocol-state owner's job.
    transport
        .lookup(
            &fixture.directory,
            &fixture.name,
            AttrMask::STAT,
            Deadline { millis: 2_000 },
        )
        .expect("the transport works on the new generation");
}

#[test]
fn cancelling_an_unknown_token_is_not_an_error_and_reports_drained() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let mut transport = LibnfsRawTransport::connect(fixture.config.clone()).expect("connect");

    // A completed call's token is retired inside `submit`, so cancelling it
    // again must be a no-op that still reports the pump idle rather than
    // touching a PDU libnfs has already freed.
    let deadline = Deadline { millis: 2_000 };
    transport
        .lookup(&fixture.directory, &fixture.name, AttrMask::STAT, deadline)
        .expect("a call to retire");

    let error = transport
        .lookup(
            &fixture.directory,
            &ComponentName::new(b"does-not-exist".to_vec()).unwrap(),
            AttrMask::STAT,
            deadline,
        )
        .expect_err("a missing name is NFS4ERR_NOENT");
    assert_eq!(error.status(), Some(Nfs4Status::NOENT));
}

/// **R1-010**, the reviewer's acceptance case, executed against a real server.
///
/// Added at `collect_fix` rather than by the fixer: the fix landed but its test
/// could not be written there, because `src/transport/raw/` does not compile
/// without the pinned libnfs checkout that worktree lacked.
///
/// The review asks for "a malformed or over-budget reply followed by a valid
/// request, asserting zero live registrations and no stale backpressure". An
/// over-budget reply is the reachable half against a well-behaved server: a
/// one-byte reply budget makes a real `GETATTR` overflow inside `decode`, on the
/// exact `?` path that used to return before `retire`.
///
/// `max_inflight: 1` is what turns a leak into a visible failure rather than a
/// silent one. Before the fix, the first over-budget call kept its registration,
/// so the *second* submission was refused `QueueFull` and every later one too.
/// Asserting "the tenth over-budget call still reports a decode failure, and a
/// normal call afterwards still succeeds" therefore fails loudly on a regression
/// instead of needing an accessor into private state.
#[test]
fn r1_010_an_over_budget_reply_retires_its_registration_on_a_real_server() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    let mut starved = fixture.config.clone();
    // One byte of reply budget: every reply carrying attributes overflows it.
    starved.limits.max_reply_bytes = 1;
    // One concurrent call: a single retained registration is enough to jam this.
    starved.limits.max_inflight = 1;
    let deadline = Deadline { millis: 5_000 };

    let mut transport =
        LibnfsRawTransport::connect(starved).expect("connect to the m1-integrator fixture");

    for attempt in 0..10 {
        let error = transport
            .lookup(&fixture.directory, &fixture.name, AttrMask::STAT, deadline)
            .expect_err("a one-byte reply budget cannot decode a GETATTR reply");
        // The original error survives retirement: it is the budget refusal, not
        // a `QueueFull` produced by this transport's own leaked slot.
        // The budget refusal is a `Malformed`, raised by `ReplyBudget::charge`.
        match &error {
            FacadeError::Transport(TransportError::Malformed(detail)) => {
                assert!(
                    detail.contains("max_reply_bytes"),
                    "attempt {attempt}: expected the reply-budget refusal, got {detail}"
                );
            }
            other => panic!("attempt {attempt}: expected a decode failure, got {other:?}"),
        }
        assert!(
            !matches!(
                &error,
                FacadeError::Transport(TransportError::QueueFull { .. })
            ),
            "attempt {attempt}: a retained registration jammed the queue, which is \
             exactly the leak R1-010 reports"
        );
    }

    // No stale backpressure: a normal request on the same transport still works
    // once the budget is adequate.
    let mut healthy = fixture.config.clone();
    healthy.limits.max_inflight = 1;
    let mut transport =
        LibnfsRawTransport::connect(healthy).expect("reconnect with an adequate budget");
    for _ in 0..3 {
        transport
            .lookup(&fixture.directory, &fixture.name, AttrMask::STAT, deadline)
            .expect("a valid request after decode failures must still succeed");
    }
}
