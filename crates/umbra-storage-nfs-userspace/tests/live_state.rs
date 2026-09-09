//! The `raw_join` integration suite: `raw_state`'s protocol state machine driven
//! over `raw_rpc`'s live `LibnfsRawTransport`.
//!
//! Every test here skips unless `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names a
//! live NFSv4.0 server. Tests that restart the server additionally need
//! `UMBRA_NFS_FIXTURE_CONTAINER=<docker container name>`; without it they skip
//! rather than silently proving less.
//!
//! **Run single-threaded.** Several tests restart the shared fixture, so
//! `-- --test-threads=1` is required for the results to mean anything.
//!
//! Nothing here mounts anything, touches `~/umbra-scratch`, or reaches
//! `Storage`. The live transport stays inside this suite and
//! `integration::StateSession`; binding it into the provider is `m1_integrate`'s
//! seam.
#![cfg(feature = "transport-raw")]

use std::sync::{Arc, Mutex};

use umbra_core::{IdempotencyKey, OperationId};
use umbra_storage_nfs_userspace::error::{FacadeError, ReplayError};
use umbra_storage_nfs_userspace::fake::FakeTransport;
use umbra_storage_nfs_userspace::handle::FileHandle;
use umbra_storage_nfs_userspace::integration::{identity_for, Backend, StateSession};
use umbra_storage_nfs_userspace::replay::VerifierMatch;
use umbra_storage_nfs_userspace::state::lease::{EpochVerdict, LeaseStanding, RenewOutcome};
use umbra_storage_nfs_userspace::state::open_owner::{OpenOutcome, OpenRequest};
use umbra_storage_nfs_userspace::state::reclaim::SurrenderCause;
use umbra_storage_nfs_userspace::transport::raw::RawTransportConfig;
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, DirCookie, DirVerifier, FaultAction, FaultContext,
    FaultPlan, FaultPoint, OpCode, OpenHow, RawTransport, ReadDirRequest, ShareAccess, ShareDeny,
    Stability, Verifier,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn deadline() -> Deadline {
    Deadline { millis: 15_000 }
}

/// The live fixture, or `None` when this run has no server.
fn fixture() -> Option<RawTransportConfig> {
    let target = std::env::var("UMBRA_NFS_RAW_FIXTURE").ok()?;
    let (host, port) = target.rsplit_once(':')?;
    let mut config = RawTransportConfig::loopback(port.parse().ok()?);
    config.host = host.to_owned();
    config.limits.default_deadline = deadline();
    Some(config)
}

/// The fixture's container name, for the tests that restart the server.
fn container() -> Option<String> {
    std::env::var("UMBRA_NFS_FIXTURE_CONTAINER").ok()
}

/// Run one `docker` subcommand against the fixture container.
fn docker(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("docker {args:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Wait until the server answers `PUTROOTFH; GETFH` again, or give up.
///
/// Returns the number of milliseconds waited, so a test can report how long the
/// restart actually took rather than asserting against a guess.
fn wait_until_serving(config: &RawTransportConfig, budget_millis: u64) -> Result<u64, String> {
    let step = 250;
    let mut waited = 0;
    while waited < budget_millis {
        if let Ok(mut transport) =
            umbra_storage_nfs_userspace::transport::raw::LibnfsRawTransport::connect(config.clone())
        {
            if transport.root_filehandle(deadline()).is_ok() {
                return Ok(waited);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(step));
        waited += step;
    }
    Err(format!("server did not come back within {budget_millis}ms"))
}

/// A fault plan that injects nothing and records every operation dispatched.
///
/// Used to prove an operation actually reached the wire. An assertion about
/// observable state alone cannot distinguish "the server demanded OPEN_CONFIRM
/// and we sent it" from "the server never demanded it".
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<OpCode>>>,
}

impl Recorder {
    fn ops(&self) -> Vec<OpCode> {
        self.seen.lock().expect("recorder lock").clone()
    }
}

impl FaultPlan for Recorder {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if point == FaultPoint::AfterDispatch {
            self.seen.lock().expect("recorder lock").push(context.op);
        }
        FaultAction::Proceed
    }
}

/// Resolve `/export/<parts...>` from the pseudo-root.
fn resolve(
    transport: &mut dyn RawTransport,
    root: &FileHandle,
    parts: &[&[u8]],
) -> Result<FileHandle, FacadeError> {
    let mut cursor = root.clone();
    for part in parts {
        let name = ComponentName::new(part.to_vec()).expect("component name");
        cursor = transport
            .lookup(&cursor, &name, AttrMask::IDENTITY, deadline())?
            .0;
    }
    Ok(cursor)
}

/// `NFS4ERR_GRACE` (RFC 7530): the server is replaying its recovery window and
/// refuses any open that is not a reclaim.
const NFS4ERR_GRACE: u32 = 10013;

/// Drive an anchored OPEN, waiting out the server's grace period if it is in one.
///
/// A server that has just restarted answers `NFS4ERR_GRACE` to a `CLAIM_NULL`
/// OPEN until its grace window closes. That is correct behaviour, not a failure,
/// and `OpenOutcome::Rejected` hands the owner lease back with its seqid already
/// resolved per RFC 7530 §9.1.7, so retrying is safe. The wait is bounded: a
/// server still in grace after the budget is a real failure.
fn open_waiting_out_grace(
    session: &mut StateSession,
    request: &OpenRequest,
    budget_millis: u64,
) -> OpenOutcome {
    let step = 1_000;
    let mut waited = 0;
    let mut lease = {
        let state = session.state();
        state
            .incarnation()
            .expect("an incarnation")
            .open_owners()
            .allocate()
            .expect("allocate an open owner")
    };
    loop {
        let outcome = {
            let (state, transport) = session.split();
            state
                .incarnation()
                .expect("an incarnation")
                .open_owners()
                .open(lease, transport, request, deadline())
        };
        match outcome {
            OpenOutcome::Rejected { lease: back, error }
                if error.status().map(|s| s.0) == Some(NFS4ERR_GRACE) && waited < budget_millis =>
            {
                if waited == 0 {
                    eprintln!("     (server in grace; waiting it out, budget {budget_millis}ms)");
                }
                lease = back;
                std::thread::sleep(std::time::Duration::from_millis(step));
                waited += step;
            }
            settled => {
                if waited > 0 {
                    eprintln!("     (grace lifted after {waited}ms)");
                }
                return settled;
            }
        }
    }
}

/// A live session with its client id established and adopted.
fn live_session(config: &RawTransportConfig, label: &str) -> (StateSession, FileHandle) {
    let mut session =
        StateSession::over_libnfs(config.clone(), identity_for(label, Verifier([0x5A; 8])))
            .expect("connect the live transport");
    assert_eq!(session.backend(), Backend::Libnfs);
    let root = session.root(deadline()).expect("PUTROOTFH; GETFH");
    session
        .establish_and_adopt(&root, 0, deadline())
        .expect("SETCLIENTID; SETCLIENTID_CONFIRM against the live server");
    (session, root)
}

// ---------------------------------------------------------------------------
// Backend parity: the join itself
// ---------------------------------------------------------------------------

/// The same lifecycle code establishes a client over either backend.
///
/// This is the join asserted directly: one function body, two implementations of
/// the frozen `RawTransport`, and the session reports which one answered.
#[test]
fn the_same_lifecycle_code_drives_the_fake_and_the_live_transport() {
    fn establish_over(session: &mut StateSession) -> Backend {
        let root = session.root(deadline()).expect("root filehandle");
        session
            .establish_and_adopt(&root, 0, deadline())
            .expect("establish a client incarnation");
        assert!(
            session.state().incarnation().is_some(),
            "an adopted incarnation is present"
        );
        session.backend()
    }

    let mut over_fake = StateSession::over_fake(
        FakeTransport::new(),
        identity_for("parity-fake", Verifier([0x5A; 8])),
    );
    assert_eq!(establish_over(&mut over_fake), Backend::Fake);

    let Some(config) = fixture() else {
        eprintln!("skipped live half: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let mut over_live =
        StateSession::over_libnfs(config, identity_for("parity-live", Verifier([0x5A; 8])))
            .expect("connect the live transport");
    assert_eq!(establish_over(&mut over_live), Backend::Libnfs);
}

// ---------------------------------------------------------------------------
// Criterion 1 — anchored OPEN with real OPEN_CONFIRM sequencing
// ---------------------------------------------------------------------------

/// Anchored OPEN (parent + name) through the raw stack, with the OPEN_CONFIRM
/// the server actually demands, then CLOSE.
///
/// The recorder proves what reached the wire: OPEN must appear, and when the
/// server sets `confirm_required`, OPEN_CONFIRM must appear too. A second OPEN
/// under the same open-owner must *not* need confirming, which is what makes the
/// first confirmation a real protocol event rather than an unconditional step.
#[test]
fn an_anchored_open_confirms_when_the_server_demands_it() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let (mut session, root) = live_session(&config, "c1-open-confirm");

    let recorder = Recorder::default();
    session
        .transport()
        .install_faults(Box::new(recorder.clone()));

    let probe =
        resolve(session.transport(), &root, &[b"export", b"probe"]).expect("resolve /export/probe");

    let request = OpenRequest {
        parent: probe.clone(),
        name: ComponentName::new(b"read.txt".to_vec()).unwrap(),
        how: OpenHow::NoCreate,
        share_access: ShareAccess::READ,
        share_deny: ShareDeny::NONE,
    };

    let outcome = open_waiting_out_grace(&mut session, &request, 150_000);

    let file = match outcome {
        OpenOutcome::Opened(file) => file,
        other => panic!("anchored OPEN did not settle as opened: {other:?}"),
    };

    let ops = recorder.ops();
    assert!(
        ops.contains(&OpCode::Open),
        "OPEN must reach the wire, saw {ops:?}"
    );
    let confirmed_on_the_wire = ops.contains(&OpCode::OpenConfirm);
    eprintln!(
        "c1: anchored OPEN via parent+name; OPEN_CONFIRM dispatched = {confirmed_on_the_wire}; ops = {ops:?}"
    );
    assert!(
        confirmed_on_the_wire,
        "this server sets confirm_required on a first OPEN per owner, so \
         OPEN_CONFIRM must have been dispatched; saw {ops:?}"
    );

    // The open is usable: a confirmed stateid is what lets a READ be issued.
    let stateid = file.stateid().expect("a confirmed open yields a stateid");
    let read = session
        .transport()
        .read(file.handle(), stateid, 0, 64, deadline())
        .expect("READ under the confirmed open stateid");
    assert_eq!(read.data, b"hello-umbra-raw-rpc\n");

    // Identity was re-proven *after* OPEN, not merely somewhere in the trace:
    // LOOKUP also carries a GETATTR, so a bare `contains` would pass without the
    // post-OPEN identity proof ever happening.
    let open_at = ops
        .iter()
        .position(|op| *op == OpCode::Open)
        .expect("OPEN is in the trace");
    assert!(
        ops[open_at + 1..].contains(&OpCode::GetAttr),
        "identity must be re-proven with GETATTR after OPEN, saw {ops:?}"
    );

    let closed = {
        let (_, transport) = session.split();
        umbra_storage_nfs_userspace::state::open_owner::close(file, transport, deadline())
    };
    assert!(
        matches!(
            closed,
            umbra_storage_nfs_userspace::state::open_owner::CloseOutcome::Closed(_)
        ),
        "CLOSE settled as closed, saw {closed:?}"
    );
}

// ---------------------------------------------------------------------------
// Criterion 2 — bounded READDIR paging with cookieverf continuity
// ---------------------------------------------------------------------------

/// Bounded READDIR paging over a directory that does not fit in one page.
///
/// Asserts three things a single page cannot show: that paging actually took
/// more than one round trip, that the cookie verifier is byte-identical on every
/// page (continuity), and that entries do not repeat across pages.
#[test]
fn bounded_readdir_paging_keeps_cookieverf_continuity() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let mut session =
        StateSession::over_libnfs(config, identity_for("c2-readdir", Verifier([0x5A; 8])))
            .expect("connect the live transport");
    let root = session.root(deadline()).expect("root filehandle");
    let pagedir = resolve(session.transport(), &root, &[b"export", b"pagedir"])
        .expect("resolve /export/pagedir");

    let mut cookie = DirCookie(0);
    let mut verifier = DirVerifier([0; 8]);
    let mut first_verifier: Option<DirVerifier> = None;
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;

    loop {
        let page = session
            .transport()
            .readdir(
                &pagedir,
                ReadDirRequest {
                    // Deliberately small so the server must page.
                    cookie,
                    verifier,
                    dir_count: 512,
                    max_count: 1024,
                    attrs: AttrMask::IDENTITY,
                },
                deadline(),
            )
            .expect("READDIR page");
        pages += 1;

        match first_verifier {
            None => first_verifier = Some(page.verifier),
            Some(seen) => assert_eq!(
                page.verifier, seen,
                "cookieverf changed mid-listing at page {pages}: the cursor would be invalid"
            ),
        }

        for entry in &page.entries {
            names.push(entry.name.as_bytes().to_vec());
            cookie = entry.cookie;
        }
        verifier = page.verifier;

        if page.eof {
            break;
        }
        assert!(pages < 200, "READDIR did not terminate within 200 pages");
    }

    assert!(
        pages > 1,
        "the fixture directory must not fit in one page, or continuity is untested (pages={pages})"
    );

    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "an entry was returned on more than one page"
    );
    assert!(
        names.len() >= 120,
        "expected the 120 seeded entries, saw {}",
        names.len()
    );
    eprintln!(
        "c2: {} entries across {pages} pages, cookieverf {:?} stable throughout",
        names.len(),
        first_verifier.expect("at least one page")
    );
}

// ---------------------------------------------------------------------------
// Criterion 3 — WRITE UNSTABLE -> COMMIT, verifier match, change across restart
// ---------------------------------------------------------------------------

/// WRITE UNSTABLE then COMMIT, with the verifiers matching before a restart and
/// changing after a hard one, surfaced as a typed retained error.
///
/// The hard restart is `docker kill` (SIGKILL) followed by `docker start`, which
/// is what makes the write verifier change: the server lost the unstable data it
/// had not committed.
#[test]
fn unstable_write_then_commit_matches_and_detects_verifier_change_across_restart() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let Some(container) = container() else {
        eprintln!("skipped: UMBRA_NFS_FIXTURE_CONTAINER is unset");
        return;
    };

    let (mut session, root) = live_session(&config, "c3-verifier");
    let probe =
        resolve(session.transport(), &root, &[b"export", b"probe"]).expect("resolve /export/probe");

    let request = OpenRequest {
        parent: probe.clone(),
        name: ComponentName::new(b"write.txt".to_vec()).unwrap(),
        how: OpenHow::NoCreate,
        share_access: ShareAccess::BOTH,
        share_deny: ShareDeny::NONE,
    };
    let outcome = open_waiting_out_grace(&mut session, &request, 150_000);
    let file = match outcome {
        OpenOutcome::Opened(file) => file,
        other => panic!("OPEN for write did not settle as opened: {other:?}"),
    };
    let stateid = file.stateid().expect("a confirmed open yields a stateid");

    // --- pre-restart: UNSTABLE write, then COMMIT, verifiers must match -------
    let payload = b"umbra-unstable-write-payload".to_vec();
    let write = session
        .transport()
        .write(
            file.handle(),
            stateid,
            0,
            Stability::Unstable,
            payload.clone(),
            deadline(),
        )
        .expect("WRITE UNSTABLE");
    assert_eq!(
        write.count as usize,
        payload.len(),
        "the fixture accepted a short write; the criterion needs the whole payload"
    );
    assert_eq!(
        write.committed,
        Stability::Unstable,
        "the server answered a stability stronger than requested, so COMMIT proves nothing"
    );

    let key = IdempotencyKey("umbra-c3-unstable-write".into());
    let operation = OperationId(uuid::Uuid::from_u128(0xC3C3_0001));

    let commit = session
        .transport()
        .commit(file.handle(), 0, payload.len() as u32, deadline())
        .expect("COMMIT");

    let matched = VerifierMatch::compare(Some(write.verifier), commit.verifier);
    assert_eq!(
        matched,
        VerifierMatch::Match,
        "pre-restart writeverf {:?} and commitverf {:?} must match",
        write.verifier,
        commit.verifier
    );
    assert!(matched.into_result().is_ok());
    eprintln!(
        "c3 pre-restart: writeverf={:?} commitverf={:?} -> Match",
        write.verifier, commit.verifier
    );

    // --- hard restart: SIGKILL, then start -----------------------------------
    let pre_restart_verifier = write.verifier;
    docker(&["kill", "--signal", "SIGKILL", &container]).expect("SIGKILL the fixture");
    docker(&["start", &container]).expect("restart the fixture");
    let waited = wait_until_serving(&config, 90_000).expect("fixture returns to service");
    eprintln!("c3: fixture came back after {waited}ms");

    // --- post-restart: a fresh write's verifier must differ ------------------
    let (mut after, root_after) = live_session(&config, "c3-verifier-after");
    let probe_after = resolve(after.transport(), &root_after, &[b"export", b"probe"])
        .expect("resolve /export/probe after restart");
    let request_after = OpenRequest {
        parent: probe_after,
        name: ComponentName::new(b"write.txt".to_vec()).unwrap(),
        how: OpenHow::NoCreate,
        share_access: ShareAccess::BOTH,
        share_deny: ShareDeny::NONE,
    };
    // The server just restarted, so it is in grace and refuses a CLAIM_NULL open
    // until that window closes. Ganesha's configured grace is 90s.
    let outcome = open_waiting_out_grace(&mut after, &request_after, 150_000);
    let file_after = match outcome {
        OpenOutcome::Opened(file) => file,
        other => panic!("post-restart OPEN did not settle as opened: {other:?}"),
    };
    let stateid_after = file_after.stateid().expect("a confirmed open");

    let write_after = after
        .transport()
        .write(
            file_after.handle(),
            stateid_after,
            0,
            Stability::Unstable,
            payload.clone(),
            deadline(),
        )
        .expect("WRITE UNSTABLE after restart");

    // The verifier the *old* write recorded, compared against what the server
    // reports now, is the detection the failure model turns on.
    let changed = VerifierMatch::compare(Some(pre_restart_verifier), write_after.verifier);
    eprintln!(
        "c3 post-restart: recorded={:?} observed={:?} -> {:?}",
        pre_restart_verifier, write_after.verifier, changed
    );
    assert!(
        matches!(changed, VerifierMatch::Changed { .. }),
        "a hard server restart must change the write verifier; recorded={:?} observed={:?}",
        pre_restart_verifier,
        write_after.verifier
    );

    // --- the typed retained error reaches the replay facade -------------------
    let error = changed
        .into_result()
        .expect_err("a changed verifier is an error");
    assert!(
        matches!(
            error,
            FacadeError::Replay(ReplayError::VerifierChanged { .. })
        ),
        "the change must surface as ReplayError::VerifierChanged, saw {error:?}"
    );

    let retained = after
        .state()
        .retained_errors()
        .retain(operation, &key, error.clone(), false)
        .expect("retain the verifier change against its key");
    assert!(
        matches!(
            retained.error(),
            FacadeError::Replay(ReplayError::VerifierChanged { .. })
        ),
        "the retained record keeps the typed replay error"
    );
    assert!(
        !retained.is_durable(),
        "this layer has no durable replay log; durability is authority_recovery's"
    );
    eprintln!("c3: retained -> {retained}");
}

// ---------------------------------------------------------------------------
// Criterion 4 — lease idle past the period, OP_RENEW, and reconnect
// ---------------------------------------------------------------------------

/// Idle past the server's lease period with the renewal cadence running, then
/// prove the state is still alive; a reconnect then invalidates the epoch.
///
/// "Idle" here means no *application* traffic. The state machine's own lease
/// maintenance keeps running, which is what `OP_RENEW` is for: NFSv4.0 entitles
/// the server to expire a client that says nothing for a whole lease period, so
/// a client that wants state to survive an idle longer than the lease must renew
/// inside it. `renew_if_idle` fires only from `DueForRenewal` (lease/2) onward,
/// so polling more often than that costs nothing.
///
/// The idle is real elapsed time against a server whose `Lease_Lifetime` is 60s,
/// so this test takes over a minute by construction. `UMBRA_NFS_IDLE_SECONDS`
/// overrides it for a faster (and weaker) run.
#[test]
fn lease_idle_past_the_period_is_kept_alive_by_op_renew_and_reconnect_invalidates_the_epoch() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let idle_seconds: u64 = std::env::var("UMBRA_NFS_IDLE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(65);

    let (mut session, root) = live_session(&config, "c4-lease");
    let lease_millis = session
        .state()
        .incarnation()
        .expect("an incarnation")
        .lease()
        .lease_millis();
    eprintln!(
        "c4: server lease period = {lease_millis}ms; idling {idle_seconds}s with renewal running"
    );
    assert!(
        lease_millis >= 60_000,
        "the fixture advertises a {lease_millis}ms lease; the criterion assumes 60s"
    );

    let epoch_before = session.epoch().expect("connected");
    let started = std::time::Instant::now();
    let step = std::time::Duration::from_secs(5);
    let mut renewals = 0;
    let mut standings = Vec::new();

    // Idle. Nothing but lease maintenance is sent.
    while started.elapsed().as_secs() < idle_seconds {
        std::thread::sleep(step);
        let now = started.elapsed().as_millis() as u64;
        let standing = session
            .state()
            .incarnation()
            .expect("an incarnation")
            .lease()
            .standing(now);
        let outcome = {
            let (state, transport) = session.split();
            state.renew_if_idle(transport, now, deadline())
        };
        if let Some(outcome) = outcome {
            standings.push((now / 1000, standing, format!("{outcome:?}")));
            assert!(
                matches!(outcome, RenewOutcome::Renewed),
                "OP_RENEW at {}s (standing {standing:?}) must renew, saw {outcome:?}",
                now / 1000
            );
            renewals += 1;
        }
    }

    assert!(
        renewals > 0,
        "an idle longer than the lease must have triggered at least one OP_RENEW"
    );
    eprintln!(
        "c4: {renewals} renewal(s) across {}s idle: {standings:?}",
        started.elapsed().as_secs()
    );
    assert!(
        started.elapsed().as_secs() >= idle_seconds,
        "the idle must actually exceed the configured period"
    );

    // The state genuinely survived: the connection is unchanged and the client
    // id is still one the server honours.
    let during_idle = session.observe();
    assert!(
        matches!(during_idle, Some(EpochVerdict::Unchanged(_))),
        "no reconnect happened during the idle, saw {during_idle:?}"
    );
    let final_renew = {
        let now = started.elapsed().as_millis() as u64;
        let (state, transport) = session.split();
        let incarnation = state.incarnation().expect("an incarnation");
        let client = incarnation.client().clone();
        incarnation
            .lease()
            .renew(transport, &client, now, deadline())
    };
    assert!(
        matches!(final_renew, RenewOutcome::Renewed),
        "after an idle longer than the lease the client id is still alive, saw {final_renew:?}"
    );
    let probe = resolve(session.transport(), &root, &[b"export", b"probe"])
        .expect("the session is still usable after the idle");
    assert!(!probe.as_bytes().is_empty());
    eprintln!(
        "c4: state alive after {}s idle",
        started.elapsed().as_secs()
    );

    // --- reconnect invalidates the epoch -------------------------------------
    let epoch_after = session.reconnect().expect("reconnect");
    assert!(
        epoch_after.0 > epoch_before.0,
        "reconnect must strictly advance the connection generation ({} -> {})",
        epoch_before.0,
        epoch_after.0
    );

    let verdict = session.observe();
    assert!(
        matches!(verdict, Some(EpochVerdict::Changed { .. })),
        "state built on the old generation must be reported as Changed, saw {verdict:?}"
    );
    assert!(
        !verdict.expect("a verdict").state_survives(),
        "state must not survive a generation change"
    );

    let incarnation = session.state().incarnation().expect("an incarnation");
    assert!(
        !incarnation.client().is_valid_for(epoch_after),
        "a client confirmed on epoch {} must not validate on epoch {}",
        epoch_before.0,
        epoch_after.0
    );
    eprintln!(
        "c4: reconnect {} -> {}, verdict={verdict:?}, old client invalid on the new epoch",
        epoch_before.0, epoch_after.0
    );
}

/// The companion fact that makes the renewal above load-bearing: the same idle
/// with **no** renewal expires the client, and that is reported as client loss
/// with the server's verbatim status rather than papered over.
///
/// Skipped unless `UMBRA_NFS_RUN_EXPIRY=1`, because it costs another full lease
/// period and proves a property of the server, not of the join.
#[test]
fn an_unrenewed_idle_past_the_lease_is_reported_as_client_loss() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    if std::env::var("UMBRA_NFS_RUN_EXPIRY").ok().as_deref() != Some("1") {
        eprintln!("skipped: set UMBRA_NFS_RUN_EXPIRY=1 to run the expiry companion");
        return;
    }
    let idle_seconds: u64 = 65;
    let (mut session, _root) = live_session(&config, "c4-expiry");

    std::thread::sleep(std::time::Duration::from_secs(idle_seconds));
    let now = idle_seconds * 1_000;

    let standing = session
        .state()
        .incarnation()
        .expect("an incarnation")
        .lease()
        .standing(now);
    assert_eq!(
        standing,
        LeaseStanding::PossiblyExpired,
        "past the whole lease the standing is PossiblyExpired — a statement about \
         what we know, not about what the server did"
    );

    let outcome = {
        let (state, transport) = session.split();
        state.renew_if_idle(transport, now, deadline())
    }
    .expect("the lease was due, so a renewal was attempted");
    eprintln!("c4-expiry: unrenewed {idle_seconds}s idle -> {outcome:?}");
    match outcome {
        RenewOutcome::ClientLost(error) => {
            assert_eq!(
                error.status().map(|s| s.0),
                Some(10011),
                "the verbatim NFS4ERR_EXPIRED is retained, not folded into a neighbour"
            );
        }
        other => panic!("an unrenewed idle past the lease must report client loss, saw {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Criterion 5 — v4.0 CLAIM_PREVIOUS reclaim in grace, safe give-up outside it
// ---------------------------------------------------------------------------

/// After a graceful restart the server is in grace and `CLAIM_PREVIOUS` recovers
/// the open; against a server that is not in grace the same reclaim gives up
/// safely with the verbatim `NFS4ERR_NO_GRACE`.
#[test]
fn claim_previous_reclaims_in_grace_and_gives_up_safely_outside_it() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let Some(container) = container() else {
        eprintln!("skipped: UMBRA_NFS_FIXTURE_CONTAINER is unset");
        return;
    };

    // --- give-up path first: this server is not in grace ---------------------
    {
        let (mut session, root) = live_session(&config, "c5-no-grace");
        let probe = resolve(session.transport(), &root, &[b"export", b"probe"])
            .expect("resolve /export/probe");
        let request = OpenRequest {
            parent: probe,
            name: ComponentName::new(b"read.txt".to_vec()).unwrap(),
            how: OpenHow::NoCreate,
            share_access: ShareAccess::READ,
            share_deny: ShareDeny::NONE,
        };
        let outcome = open_waiting_out_grace(&mut session, &request, 150_000);
        let file = match outcome {
            OpenOutcome::Opened(file) => file,
            other => panic!("OPEN did not settle as opened: {other:?}"),
        };

        // Reconnect and re-establish, then reclaim against a server that never
        // restarted and is therefore out of grace.
        session.reconnect().expect("reconnect");
        let previous = vec![file];
        let (_, report) = {
            let (state, transport) = session.split();
            state
                .reestablish_and_reclaim(transport, &root, &previous, 0, deadline())
                .expect("re-establish after reconnect")
        };
        eprintln!("c5 out-of-grace: {report:?}");
        assert!(
            !report.is_complete(),
            "a reclaim outside grace cannot complete"
        );
        let surrendered = report
            .surrendered
            .first()
            .expect("the open was surrendered, not silently dropped");
        assert_eq!(
            surrendered.cause,
            SurrenderCause::GraceLifted,
            "outside grace the server answers NFS4ERR_NO_GRACE and the open is surrendered"
        );
        assert!(
            surrendered.cause.is_safe_stop(),
            "a surrender is a safe stop, never an upgrade to CLAIM_NULL"
        );
        assert_eq!(
            surrendered.status().map(|s| s.0),
            Some(10033),
            "the verbatim NFS4ERR_NO_GRACE status is retained"
        );
    }

    // --- in-grace path: graceful restart, then reclaim -----------------------
    let (mut session, root) = live_session(&config, "c5-in-grace");
    let probe =
        resolve(session.transport(), &root, &[b"export", b"probe"]).expect("resolve /export/probe");
    let request = OpenRequest {
        parent: probe,
        name: ComponentName::new(b"read.txt".to_vec()).unwrap(),
        how: OpenHow::NoCreate,
        share_access: ShareAccess::READ,
        share_deny: ShareDeny::NONE,
    };
    let outcome = open_waiting_out_grace(&mut session, &request, 150_000);
    let file = match outcome {
        OpenOutcome::Opened(file) => file,
        other => panic!("OPEN did not settle as opened: {other:?}"),
    };

    let recorder = Recorder::default();

    // Graceful restart: the server records its clients and comes back in grace.
    docker(&["restart", &container]).expect("gracefully restart the fixture");
    let waited = wait_until_serving(&config, 90_000).expect("fixture returns to service");
    eprintln!("c5: fixture restarted gracefully, serving after {waited}ms");

    session.reconnect().expect("reconnect after the restart");
    session
        .transport()
        .install_faults(Box::new(recorder.clone()));

    let previous = vec![file];
    let (_, report) = {
        let (state, transport) = session.split();
        state
            .reestablish_and_reclaim(transport, &root, &previous, 0, deadline())
            .expect("re-establish after the restart")
    };
    eprintln!("c5 in-grace: {report:?}");

    let ops = recorder.ops();
    assert!(
        ops.iter()
            .all(|op| (*op as u32) <= OpCode::ReleaseLockOwner as u32),
        "the reclaim path dispatched an operation outside NFSv4.0: {ops:?}"
    );
    assert!(
        !ops.iter().any(|op| format!("{op:?}").contains("Reclaim")),
        "RECLAIM_COMPLETE is a v4.1 operation and must never be dispatched: {ops:?}"
    );

    assert!(
        report.is_complete(),
        "the server restarted into grace, so CLAIM_PREVIOUS must recover the open; report={report:?}"
    );
    assert_eq!(
        report.recovered.len(),
        1,
        "exactly the one open that was live must come back"
    );
}

// ---------------------------------------------------------------------------
// Integration fault matrix — the state machine over the LIVE transport
// ---------------------------------------------------------------------------

/// A plan that fires one action once, at one point, for one operation.
///
/// `ScriptedFault` fires on the first match, and at every point except
/// `AfterDispatch` the fault context carries the COMPOUND's subject operation,
/// so targeting by opcode is what puts the fault on the intended transition
/// rather than on whatever plumbing happened to go first.
struct DelayedFault {
    point: FaultPoint,
    op: Option<OpCode>,
    skip: u32,
    action: Option<FaultAction>,
}

impl DelayedFault {
    fn plan(
        point: FaultPoint,
        op: Option<OpCode>,
        action: FaultAction,
        skip: u32,
    ) -> Box<dyn FaultPlan> {
        Box::new(Self {
            point,
            op,
            skip,
            action: Some(action),
        })
    }
}

impl FaultPlan for DelayedFault {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if point != self.point || self.op.is_some_and(|op| op != context.op) {
            return FaultAction::Proceed;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return FaultAction::Proceed;
        }
        self.action.take().unwrap_or(FaultAction::Proceed)
    }
}

/// The transitions this layer can fault meaningfully over a live server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
    SetClientId,
    SetClientIdConfirm,
    Open,
    OpenConfirm,
    Close,
    Renew,
}

impl Transition {
    fn label(self) -> &'static str {
        match self {
            Self::SetClientId => "SETCLIENTID",
            Self::SetClientIdConfirm => "SETCLIENTID_CONFIRM",
            Self::Open => "OPEN",
            Self::OpenConfirm => "OPEN_CONFIRM",
            Self::Close => "CLOSE",
            Self::Renew => "OP_RENEW",
        }
    }

    /// The opcode a fault must target to land on this transition.
    fn target(self) -> Option<OpCode> {
        match self {
            Self::SetClientId => Some(OpCode::SetClientId),
            Self::SetClientIdConfirm => Some(OpCode::SetClientIdConfirm),
            Self::Open => Some(OpCode::Open),
            Self::OpenConfirm => Some(OpCode::OpenConfirm),
            Self::Close => Some(OpCode::Close),
            Self::Renew => Some(OpCode::Renew),
        }
    }
}

/// Run one matrix cell and return a human-readable observation, or the reason
/// the cell's invariant was violated.
///
/// The invariant asserted in every cell is one-directional and the same one
/// `raw_state` settled on: **the client may never claim more than the transport
/// proved.** A fault may legitimately produce success, a server rejection or an
/// unknown outcome; what must hold in all three is that anything the session
/// reports as established is actually honoured by the server, and anything it
/// does not establish is not silently treated as established.
fn run_live_cell(
    config: &RawTransportConfig,
    transition: Transition,
    point: FaultPoint,
    action: FaultAction,
    label: &str,
) -> Result<String, String> {
    let identity = identity_for(label, Verifier([0x5A; 8]));
    let mut session = StateSession::over_libnfs(config.clone(), identity)
        .map_err(|error| format!("connect: {error}"))?;
    let root = session
        .root(deadline())
        .map_err(|error| format!("root filehandle: {error}"))?;

    // Faults on the client-id transitions are installed before establishing;
    // faults on the open/renew transitions need an established client first.
    let install = |session: &mut StateSession| {
        session.transport().install_faults(DelayedFault::plan(
            point,
            transition.target(),
            action,
            0,
        ));
    };

    match transition {
        Transition::SetClientId | Transition::SetClientIdConfirm => {
            install(&mut session);
            let established = session.establish_and_adopt(&root, 0, deadline());
            // Invariant: if we claim an incarnation, the server must honour it.
            match established {
                Ok(()) => {
                    let now = 0;
                    let outcome = {
                        let (state, transport) = session.split();
                        let incarnation = state.incarnation().ok_or("adopted but absent")?;
                        let client = incarnation.client().clone();
                        incarnation
                            .lease()
                            .renew(transport, &client, now, deadline())
                    };
                    // Clear the fault before judging: a one-shot fault may have
                    // been spent on this very RENEW.
                    match outcome {
                        RenewOutcome::Renewed => {
                            Ok("established; server honours the client id".into())
                        }
                        RenewOutcome::Refused(_) | RenewOutcome::Unknown(_) => Ok(format!(
                            "established; renew inconclusive under the fault: {outcome:?}"
                        )),
                        RenewOutcome::ClientLost(error) => Err(format!(
                            "claimed an established client the server does not know: {error}"
                        )),
                    }
                }
                Err(error) => {
                    if session.state().incarnation().is_some() {
                        return Err("reported failure but left an adopted incarnation".into());
                    }
                    Ok(format!("not established: {error}"))
                }
            }
        }
        Transition::Open | Transition::OpenConfirm | Transition::Close => {
            session
                .establish_and_adopt(&root, 0, deadline())
                .map_err(|error| format!("establish before the fault: {error}"))?;
            let probe = resolve(session.transport(), &root, &[b"export", b"probe"])
                .map_err(|error| format!("resolve probe: {error}"))?;
            let request = OpenRequest {
                parent: probe,
                name: ComponentName::new(b"read.txt".to_vec()).unwrap(),
                how: OpenHow::NoCreate,
                share_access: ShareAccess::READ,
                share_deny: ShareDeny::NONE,
            };
            // Sampled *before* the attempt: an owner burned by this OPEN is the
            // difference, and reading it afterwards would compare a number with
            // itself.
            let burned_before = session
                .state()
                .incarnation()
                .ok_or("no incarnation")?
                .open_owners()
                .burned();
            install(&mut session);
            let outcome = open_waiting_out_grace(&mut session, &request, 150_000);

            match outcome {
                OpenOutcome::Opened(file) => {
                    // Invariant: a claimed open must be usable against the server.
                    let stateid = file
                        .stateid()
                        .map_err(|error| format!("opened but no stateid: {error}"))?;
                    let read = session
                        .transport()
                        .read(file.handle(), stateid, 0, 8, deadline());
                    let observation = match read {
                        Ok(_) => "opened; stateid honoured by the server".to_string(),
                        Err(error) => {
                            return Err(format!(
                                "claimed a confirmed open the server rejects: {error}"
                            ))
                        }
                    };
                    if transition == Transition::Close {
                        let closed = {
                            let (_, transport) = session.split();
                            umbra_storage_nfs_userspace::state::open_owner::close(
                                file,
                                transport,
                                deadline(),
                            )
                        };
                        return Ok(format!("{observation}; close -> {}", close_label(&closed)));
                    }
                    Ok(observation)
                }
                OpenOutcome::Unconfirmed { file, error } => {
                    if file.stateid().is_ok() {
                        return Err("unconfirmed open handed out a usable stateid".into());
                    }
                    Ok(format!("unconfirmed, stateid refused: {error}"))
                }
                OpenOutcome::Rejected { lease, error } => {
                    if lease.next_seqid().is_none() {
                        return Err("a rejected open returned a lease with no usable seqid".into());
                    }
                    Ok(format!("rejected, lease reusable: {error}"))
                }
                OpenOutcome::Abandoned { error } => {
                    let burned_after = session
                        .state()
                        .incarnation()
                        .ok_or("no incarnation")?
                        .open_owners()
                        .burned();
                    if burned_after <= burned_before {
                        return Err("abandoned an open without burning its owner".into());
                    }
                    Ok(format!("abandoned, owner burned: {error}"))
                }
            }
        }
        Transition::Renew => {
            session
                .establish_and_adopt(&root, 0, deadline())
                .map_err(|error| format!("establish before the fault: {error}"))?;
            // The lease was created at t=0, so judging it at t=0 cannot tell a
            // credited lease from a brand new one — both read `Fresh`. Renewing
            // at 60% of the period makes the two states distinguishable: a
            // credited lease reads `Fresh` (idle 0), an uncredited one still
            // reads `DueForRenewal` (idle 0.6 x lease).
            let lease_millis = session
                .state()
                .incarnation()
                .ok_or("no incarnation")?
                .lease()
                .lease_millis();
            let now = lease_millis * 6 / 10;
            install(&mut session);
            let outcome = {
                let (state, transport) = session.split();
                let incarnation = state.incarnation().ok_or("no incarnation")?;
                let client = incarnation.client().clone();
                incarnation
                    .lease()
                    .renew(transport, &client, now, deadline())
            };
            let credited = session
                .state()
                .incarnation()
                .ok_or("no incarnation")?
                .lease()
                .standing(now);
            // Invariant: only a real renewal credits the lease.
            match outcome {
                RenewOutcome::Renewed => {
                    if credited != LeaseStanding::Fresh {
                        return Err(format!(
                            "a successful renewal did not credit the lease: standing {credited:?}"
                        ));
                    }
                    Ok(format!("renewed; standing {credited:?}"))
                }
                other => {
                    if credited == LeaseStanding::Fresh {
                        return Err(format!(
                            "a failed renewal credited the lease anyway: {other:?}"
                        ));
                    }
                    Ok(format!(
                        "not renewed, lease uncredited ({credited:?}): {other:?}"
                    ))
                }
            }
        }
    }
}

fn close_label(outcome: &umbra_storage_nfs_userspace::state::open_owner::CloseOutcome) -> String {
    use umbra_storage_nfs_userspace::state::open_owner::CloseOutcome;
    match outcome {
        CloseOutcome::Closed(_) => "closed".into(),
        CloseOutcome::Rejected { .. } => "rejected".into(),
        CloseOutcome::Abandoned { .. } => "abandoned".into(),
    }
}

/// Every state transition survives every fault, over the **live** transport.
///
/// This is the fake matrix's live counterpart. It matters because the fake never
/// consults `FaultPoint::OnConnection` and acts on each action at only one point;
/// `LibnfsRawTransport` consults all five points, so cells that are inert against
/// the fake are real here.
#[test]
fn every_state_transition_survives_every_fault_over_the_live_transport() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    let transitions = [
        Transition::SetClientId,
        Transition::SetClientIdConfirm,
        Transition::Open,
        Transition::OpenConfirm,
        Transition::Close,
        Transition::Renew,
    ];
    let points = [
        FaultPoint::BeforeDispatch,
        FaultPoint::AfterDispatch,
        FaultPoint::BeforeReturn,
        FaultPoint::OnDeadline,
        FaultPoint::OnConnection,
    ];
    let actions = [
        FaultAction::Fail(
            umbra_storage_nfs_userspace::error::TransportError::Disconnected {
                epoch: umbra_storage_nfs_userspace::transport::ConnectionEpoch(1),
                detail: "injected by the integration matrix".into(),
            },
        ),
        FaultAction::Substitute(umbra_storage_nfs_userspace::error::Nfs4Status(10013)),
        FaultAction::ShortWrite(2),
        FaultAction::RotateVerifier(umbra_storage_nfs_userspace::transport::WriteVerifier(
            [0xAB; 8],
        )),
        FaultAction::DropReply,
    ];

    let mut cells = 0;
    let mut failures = Vec::new();
    for transition in transitions {
        for point in points {
            for action in &actions {
                cells += 1;
                let label = format!("fm-{cells}");
                let result = run_live_cell(&config, transition, point, action.clone(), &label);
                match result {
                    Ok(observation) => println!(
                        "PASS | {:<20} x {:<14} x {:<16} | {observation}",
                        transition.label(),
                        format!("{point:?}"),
                        action_label(action),
                    ),
                    Err(reason) => {
                        println!(
                            "FAIL | {:<20} x {:<14} x {:<16} | {reason}",
                            transition.label(),
                            format!("{point:?}"),
                            action_label(action),
                        );
                        failures.push(format!(
                            "{} x {point:?} x {}: {reason}",
                            transition.label(),
                            action_label(action)
                        ));
                    }
                }
            }
        }
    }

    println!(
        "\nintegration fault matrix (live transport): {} cells, {} PASS, {} FAIL",
        cells,
        cells - failures.len(),
        failures.len()
    );
    assert_eq!(
        cells, 150,
        "6 transitions x 5 fault points x 5 fault actions"
    );
    assert!(
        failures.is_empty(),
        "{} of {cells} live cells violated their invariant:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn action_label(action: &FaultAction) -> &'static str {
    match action {
        FaultAction::Proceed => "Proceed",
        FaultAction::Fail(_) => "Fail",
        FaultAction::Substitute(_) => "Substitute",
        FaultAction::ShortWrite(_) => "ShortWrite",
        FaultAction::RotateVerifier(_) => "RotateVerifier",
        FaultAction::DropReply => "DropReply",
    }
}
