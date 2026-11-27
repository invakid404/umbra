//! Managed custody and notification through ordinary children.
//! File latches control work, never certify collection.
use super::cleanup_order;
use super::runtime_clock::{self, SharedClock};
#[path = "runtime_activity.rs"]
mod runtime_activity;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use umbra_core::managed::{
    CleanupPath, Participant, Role, RunIdentity, Start, Ticket, TicketState, ACK_NANOS,
};
use umbra_isolation::managed::{
    boot_nanos,
    guard::{Event, Guard, Outcome},
    inventory::{Inventory, Member},
    owner::Owner,
    process::{identity, identity_with_parent, ProcessWatch, SpawnedChild},
    publication::{Publication, Route},
    test_clock::GuardState,
    Channel,
};

const ROLES: [Role; 6] = [
    Role::Caller,
    Role::Relay,
    Role::Guardian,
    Role::Worker,
    Role::CleanupOwner,
    Role::Provider,
];
const PUBLISHERS: [Role; 4] = [
    Role::Relay,
    Role::Guardian,
    Role::Worker,
    Role::CleanupOwner,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Case {
    Normal,
    OrderEmergency,
    ShutdownError,
    Unwind,
    Cancellation,
    StuckCustodian,
    UncancellableWorker,
    OwnerLost,
    TotalLoss,
    RelayStalled,
    GuardianStalled,
    OwnerDirect,
    ForwardBeforeExit,
}
impl Case {
    fn ordered_failure(self) -> bool {
        matches!(
            self,
            Self::OrderEmergency | Self::ShutdownError | Self::Unwind | Self::Cancellation
        )
    }

    fn acknowledged(self) -> bool {
        matches!(
            self,
            Self::Normal
                | Self::OrderEmergency
                | Self::ShutdownError
                | Self::Unwind
                | Self::Cancellation
                | Self::StuckCustodian
                | Self::UncancellableWorker
                | Self::OwnerLost
        )
    }
    fn stuck_role(self) -> Option<Role> {
        match self {
            Self::StuckCustodian => Some(Role::CleanupOwner),
            Self::UncancellableWorker => Some(Role::Worker),
            _ => None,
        }
    }
    fn failed(self, role: Role) -> bool {
        match self {
            Self::Normal => false,
            Self::OrderEmergency | Self::ShutdownError | Self::Unwind | Self::Cancellation => {
                role != Role::Worker
            }
            Self::StuckCustodian | Self::UncancellableWorker | Self::OwnerLost => {
                matches!(role, Role::Caller | Role::Relay | Role::Guardian)
            }
            _ => true,
        }
    }
    fn origin(self) -> Role {
        if matches!(self, Self::RelayStalled | Self::GuardianStalled) {
            Role::Worker
        } else {
            Role::CleanupOwner
        }
    }
    fn held(self, role: Role) -> bool {
        match self {
            Self::Normal
            | Self::OrderEmergency
            | Self::ShutdownError
            | Self::Unwind
            | Self::Cancellation
            | Self::StuckCustodian
            | Self::UncancellableWorker
            | Self::OwnerLost => false,
            Self::TotalLoss => false,
            Self::RelayStalled => role == Role::Relay,
            Self::GuardianStalled => role == Role::Guardian,
            Self::OwnerDirect => matches!(role, Role::Relay | Role::Guardian | Role::Worker),
            Self::ForwardBeforeExit => matches!(role, Role::Relay | Role::Worker),
        }
    }
    fn delivery(self) -> Option<Role> {
        match self {
            Self::Normal
            | Self::OrderEmergency
            | Self::ShutdownError
            | Self::Unwind
            | Self::Cancellation
            | Self::StuckCustodian
            | Self::UncancellableWorker
            | Self::OwnerLost => None,
            Self::TotalLoss => None,
            Self::RelayStalled => Some(Role::Worker),
            Self::GuardianStalled => Some(Role::Relay),
            Self::OwnerDirect => Some(Role::CleanupOwner),
            Self::ForwardBeforeExit => Some(Role::Guardian),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    run: RunIdentity,
    t0: u64,
    clock: PathBuf,
    case: Case,
    #[serde(default)]
    order: bool,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Reading {
    now: u64,
    state: GuardState,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    role: Role,
    failed: bool,
    waits: Vec<(Participant, i32)>,
    unresolved: usize,
}

fn tick() {
    std::thread::sleep(Duration::from_millis(1));
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn name(prefix: &str, role: Role) -> String {
    format!("{prefix}-{role:?}")
}
fn write(root: &Path, file: &str, value: &impl Serialize) -> io::Result<()> {
    let next = root.join(format!("{file}.next"));
    std::fs::write(&next, serde_json::to_vec(value)?)?;
    std::fs::rename(next, root.join(file))
}
fn read<T: DeserializeOwned>(root: &Path, file: &str) -> io::Result<Option<T>> {
    match std::fs::read(root.join(file)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn wait(root: &Path, mut ready: impl FnMut() -> io::Result<bool>) -> io::Result<()> {
    let bound = Instant::now() + Duration::from_secs(3);
    while !ready()? {
        if root.join("abort").exists() || Instant::now() >= bound {
            return Err(invalid("fixture startup/observation bound"));
        }
        tick();
    }
    Ok(())
}
fn original(config: &Config, owner: Participant) -> Start {
    Start {
        run: config.run,
        origin: owner,
        ticket: Ticket::new(config.t0, CleanupPath::Trusted, 1).unwrap(),
    }
}

/// Rescue uses only this process's actual child PIDs and never sends a signal.
/// Any wait performed here is recorded separately and invalidates qualification.
struct Rescue {
    root: PathBuf,
    config: Config,
    role: Role,
    children: Vec<u32>,
}
impl Drop for Rescue {
    fn drop(&mut self) {
        let mut rescued = Vec::new();
        for pid in &self.children {
            loop {
                let mut status = 0;
                let result = unsafe { libc::waitpid(*pid as i32, &mut status, libc::WNOHANG) };
                if result == *pid as i32 {
                    rescued.push((*pid, status));
                    break;
                }
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.raw_os_error() != Some(libc::ECHILD) {
                        rescued.push((*pid, -1));
                    }
                    break;
                }
                let _ = std::fs::write(self.root.join("abort"), b"abort");
                let _ = std::fs::write(self.root.join("release"), b"release");
                let cutoff = Ticket::new(self.config.t0, CleanupPath::Trusted, 1)
                    .unwrap()
                    .deadline;
                let next = self
                    .config
                    .clock
                    .with_file_name(format!("rescue-{:?}", self.role));
                let _ = std::fs::write(&next, cutoff.to_le_bytes());
                let _ = std::fs::rename(next, &self.config.clock);
                tick();
            }
        }
        let _ = write(&self.root, &name("rescue", self.role), &rescued);
    }
}

fn spawn(root: &Path, role: Role, rescue: &mut Rescue) -> io::Result<SpawnedChild> {
    let exe = std::env::current_exe()?;
    let environment = umbra_isolation::spawn::environment(
        &[umbra_isolation::managed::executable::ENV_ROLE],
        &[
            ("UMBRA_R2_TREE_DIR", root.to_str().unwrap().to_owned()),
            ("UMBRA_R2_TREE_ROLE", serde_json::to_string(&role)?),
        ],
    );
    let child = SpawnedChild::spawn(
        &exe,
        &[
            exe.as_os_str().to_owned(),
            "--exact".into(),
            "runtime_tree_child".into(),
            "--nocapture".into(),
        ],
        &environment,
        false,
        &[],
    )?;
    rescue.children.push(child.pid());
    Ok(child)
}

fn peers(role: Role) -> Vec<Role> {
    use Role::*;
    match role {
        Caller => vec![Relay, Guardian, Worker, CleanupOwner],
        Relay | Guardian => vec![Caller, Worker, CleanupOwner],
        Worker => vec![Caller, Relay, Guardian, CleanupOwner],
        CleanupOwner => vec![Caller, Relay, Guardian, Worker, Provider],
        Provider => vec![CleanupOwner],
    }
}
fn connections(
    root: &Path,
    local: Role,
    listener: &UnixListener,
) -> io::Result<BTreeMap<Role, UnixStream>> {
    let mut streams = BTreeMap::new();
    for peer in peers(local).into_iter().filter(|peer| *peer > local) {
        let mut stream = UnixStream::connect(root.join(name("socket", peer)))?;
        stream.write_all(&[local as u8])?;
        streams.insert(peer, stream);
    }
    let count = peers(local)
        .into_iter()
        .filter(|peer| *peer < local)
        .count();
    wait(root, || {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                let peer = *ROLES
                    .get(byte[0] as usize)
                    .ok_or_else(|| invalid("unknown fixture peer"))?;
                if peer >= local
                    || !peers(local).contains(&peer)
                    || streams.insert(peer, stream).is_some()
                {
                    return Err(invalid("duplicate fixture peer"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        Ok(streams.keys().filter(|peer| **peer < local).count() == count)
    })?;
    Ok(streams)
}
fn participant(inventory: &Inventory, role: Role) -> Participant {
    inventory
        .members
        .iter()
        .find(|member| member.participant.role == role)
        .unwrap()
        .participant
}
fn record(
    root: &Path,
    role: Role,
    state: GuardState,
    prior: &mut Option<Reading>,
) -> io::Result<()> {
    let reading = Reading {
        now: boot_nanos()?,
        state,
    };
    if prior.as_ref() != Some(&reading) {
        write(root, &name("state", role), &reading)?;
        *prior = Some(reading);
    }
    Ok(())
}
fn state(tickets: &TicketState, pending: Vec<Participant>) -> GuardState {
    GuardState {
        ticket: tickets.ticket(),
        receive_deadline: tickets.ticket().map_or(u64::MAX, |t| t.deadline),
        failed: tickets.failed(),
        expired: false,
        stage_events: Vec::new(),
        pending_publication: pending,
    }
}
fn finish_parent(root: &Path, role: Role, outcome: Outcome) -> io::Result<()> {
    let waits: Vec<_> = outcome
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Collected(peer, status) => Some((*peer, *status)),
            _ => None,
        })
        .collect();
    let config: Config = read(root, "config")?.ok_or_else(|| invalid("missing config"))?;
    if config.order {
        use cleanup_order::Stage::*;
        let member: Member =
            read(root, &name("member", role))?.ok_or_else(|| invalid("missing parent identity"))?;
        let mut recorder = cleanup_order::Recorder::new(root, config.run, member.participant);
        for (child, status) in &waits {
            if *status != 0 && !config.case.ordered_failure() {
                return Err(invalid("ordered cleanup child failed"));
            }
            let (stage, dependencies): (_, &[_]) = match child.role {
                Role::Provider if config.case == Case::OrderEmergency => (
                    ProviderParentCollected,
                    &[DebuggerParentCollected, Unconfirmed],
                ),
                Role::Provider => (ProviderParentCollected, &[ResourceCertificate]),
                Role::Worker => (WorkerParentCollected, &[]),
                Role::CleanupOwner => (OwnerParentCollected, &[ProviderParentCollected]),
                Role::Guardian => (
                    GuardianParentCollected,
                    &[WorkerParentCollected, OwnerParentCollected],
                ),
                Role::Relay => (RelayParentCollected, &[GuardianParentCollected]),
                Role::Caller => return Err(invalid("caller cannot be collected child")),
            };
            recorder.record(stage, dependencies, Some(*child), None)?;
        }
        if role == Role::Caller && config.case != Case::OrderEmergency {
            recorder.record(
                NamespaceComplete,
                &[RelayParentCollected, ResourceCertificate],
                None,
                None,
            )?;
        }
    }
    write(
        root,
        &name("receipt", role),
        &Receipt {
            role,
            failed: outcome.failed,
            waits,
            unresolved: outcome.unresolved.len(),
        },
    )?;
    if !outcome.unresolved.is_empty() {
        return Err(invalid("production parent left children unresolved"));
    }
    Ok(())
}

pub fn child() -> io::Result<()> {
    let Some(root) = std::env::var_os("UMBRA_R2_TREE_DIR") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let role: Role = serde_json::from_str(&std::env::var("UMBRA_R2_TREE_ROLE").unwrap())?;
    let config: Config =
        read(&root, "config")?.ok_or_else(|| invalid("missing tree configuration"))?;
    let _clock = runtime_clock::install(&config.clock);
    let mut rescue = Rescue {
        root: root.clone(),
        config: config.clone(),
        role,
        children: Vec::new(),
    };
    let listener = UnixListener::bind(root.join(name("socket", role)))?;
    listener.set_nonblocking(true)?;
    let local = identity(std::process::id(), role)?;
    let parent_role = match role {
        Role::Relay => Role::Caller,
        Role::Guardian => Role::Relay,
        Role::Worker | Role::CleanupOwner => Role::Guardian,
        Role::Provider => Role::CleanupOwner,
        _ => return Err(invalid("invalid child role")),
    };
    let parent = identity(identity_with_parent(local.pid, role)?.1, parent_role)?;
    write(
        &root,
        &name("member", role),
        &Member {
            participant: local,
            parent: Some(parent),
        },
    )?;
    let mut children = Vec::new();
    let mut provider = None;
    if role == Role::Relay {
        children.push(spawn(&root, Role::Guardian, &mut rescue)?);
    }
    if role == Role::Guardian {
        children.push(spawn(&root, Role::Worker, &mut rescue)?);
        children.push(spawn(&root, Role::CleanupOwner, &mut rescue)?);
    }
    if role == Role::CleanupOwner {
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", "runtime_tree_child", "--nocapture"])
            .env(
                "UMBRA_R2_TREE_ROLE",
                serde_json::to_string(&Role::Provider)?,
            )
            .stdin(Stdio::null())
            .spawn()?;
        rescue.children.push(child.id());
        provider = Some(child);
    }
    wait(&root, || Ok(root.join("inventory").exists()))?;
    let inventory: Inventory = read(&root, "inventory")?.unwrap();
    inventory.validate(config.run, true)?;
    let mut streams = connections(&root, role, &listener)?;
    let origin_identity = participant(&inventory, config.case.origin());
    let start = original(&config, origin_identity);
    let mut prior = None;
    if role == Role::Provider {
        let mut channel = Channel::new(streams.remove(&Role::CleanupOwner).unwrap())?;
        let mut tickets = TicketState::new(config.run, 1).map_err(invalid)?;
        write(&root, &name("ready", role), &true)?;
        let mut cancelled = false;
        while !root.join("release").exists() {
            if !cancelled {
                match channel.receive::<Start>(u64::MAX) {
                    Ok(Some(received)) => {
                        tickets
                            .accept(received, origin_identity, local)
                            .map_err(invalid)?;
                    }
                    Ok(None) => {}
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                        cancelled = true;
                    }
                    Err(error) => return Err(error),
                }
            }
            record(&root, role, state(&tickets, Vec::new()), &mut prior)?;
            tick();
        }
        if config.order {
            if config.case == Case::Cancellation {
                wait(&root, || match channel.receive::<Start>(u64::MAX) {
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(true),
                    Err(error) => Err(error),
                    Ok(_) => Ok(false),
                })?;
                write(&root, "trigger-cancellation", &true)?;
            }
            if config.case == Case::Unwind {
                struct Cleanup<'a>(&'a Path, &'a Config, Participant);
                impl Drop for Cleanup<'_> {
                    fn drop(&mut self) {
                        ordered_provider(self.0, self.1, self.2).expect("unwind cleanup");
                    }
                }
                let result = std::panic::catch_unwind(|| {
                    let _cleanup = Cleanup(&root, &config, local);
                    write(&root, "trigger-unwind", &true).unwrap();
                    panic!("scripted operation unwind");
                });
                assert!(result.is_err(), "original unwind must remain failed");
            } else {
                let primary = if config.case == Case::ShutdownError {
                    write(&root, "trigger-shutdown-error", &true)?;
                    Err(invalid("scripted shutdown error"))
                } else {
                    Ok(())
                };
                ordered_provider(&root, &config, local)?;
                if config.case == Case::ShutdownError {
                    assert!(
                        primary.is_err(),
                        "collection must not erase shutdown failure"
                    );
                }
            }
            if config.case.ordered_failure() {
                write(
                    &root,
                    &name("receipt", role),
                    &Receipt {
                        role,
                        failed: true,
                        waits: Vec::new(),
                        unresolved: 0,
                    },
                )?;
                drop(rescue);
                std::process::exit(1);
            }
        }
        return write(
            &root,
            &name("receipt", role),
            &Receipt {
                role,
                failed: cancelled,
                waits: Vec::new(),
                unresolved: 0,
            },
        );
    }
    if role == Role::CleanupOwner {
        let mut provider_channel = Channel::new(streams.remove(&Role::Provider).unwrap())?;
        let cancellation = provider_channel.handoff_stream()?.try_clone()?;
        let routes = streams
            .into_iter()
            .map(|(peer, stream)| Route::new(participant(&inventory, peer), Channel::new(stream)?))
            .collect::<io::Result<Vec<_>>>()?;
        let mut owner = Owner::new(
            local,
            config.run,
            1,
            provider.take().unwrap(),
            cancellation,
            routes,
        )
        .map_err(|(error, _, _, _)| error)?;
        let mut watch = ProcessWatch::new()?;
        for peer in inventory.watches(local)? {
            watch.install(peer)?;
        }
        owner.install_watch(watch, &inventory)?;
        write(&root, &name("ready", role), &true)?;
        wait(&root, || Ok(root.join("go").exists()))?;
        if config.case.origin() == role {
            owner.start(start.clone(), local)?;
        }
        let mut provider_started = false;
        let mut cancelled = false;
        loop {
            if root.join("abort").exists() {
                return Err(invalid("fixture aborted"));
            }
            if config.case == Case::Cancellation && root.join("release").exists() && !cancelled {
                owner.cancel("scripted cancellation after Start".into())?;
                cancelled = true;
            }
            let status = owner.progress()?;
            if owner.test_clock_ticket().is_some() && !provider_started {
                // The scripted provider receives the same accepted origin,
                // including a worker Start forwarded through its actual owner.
                provider_channel.queue(&start)?;
                if !provider_channel.flush(start.ticket.deadline)? {
                    return Err(invalid("fixture provider Start backpressure"));
                }
                provider_started = true;
            }
            if owner.failed() && !cancelled {
                owner.cancel("caller notification acknowledgement lost".into())?;
                cancelled = true;
            }
            record(
                &root,
                role,
                GuardState {
                    ticket: owner.test_clock_ticket(),
                    receive_deadline: owner.test_clock_ticket().map_or(u64::MAX, |t| t.deadline),
                    failed: owner.failed(),
                    expired: owner.expired()?,
                    stage_events: Vec::new(),
                    pending_publication: owner.test_pending_publication(),
                },
                &mut prior,
            )?;
            if let Some(status) = status {
                if !root.join("release").exists() {
                    return Err(invalid("owner claimed collection while provider latched"));
                }
                let provider = participant(&inventory, Role::Provider);
                if config.order {
                    use cleanup_order::Stage::*;
                    cleanup_order::Recorder::new(&root, config.run, local).record(
                        ProviderParentCollected,
                        if config.case == Case::OrderEmergency {
                            &[
                                cleanup_order::Stage::DebuggerParentCollected,
                                cleanup_order::Stage::Unconfirmed,
                            ]
                        } else {
                            &[ResourceCertificate]
                        },
                        Some(provider),
                        None,
                    )?;
                }
                let mut raw = 0;
                let result = unsafe { libc::waitpid(provider.pid as i32, &mut raw, libc::WNOHANG) };
                if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
                    return Err(invalid("owner omitted actual provider parent wait"));
                }
                write(
                    &root,
                    &name(
                        if config.case == Case::OwnerLost {
                            "forensic-wait"
                        } else {
                            "receipt"
                        },
                        role,
                    ),
                    &Receipt {
                        role,
                        failed: owner.failed(),
                        waits: vec![(provider, status.into_raw())],
                        unresolved: 0,
                    },
                )?;
                if config.case != Case::UncancellableWorker {
                    wait(&root, || {
                        Ok(root.join(name("receipt", Role::Worker)).exists())
                    })?;
                }
                if config.case == Case::OwnerLost {
                    // No live child is orphaned. This test-only audit proves
                    // that fact, but supplies no receipt to the managed tree.
                    // The custodian disappears before reporting its result.
                    drop(rescue);
                    unsafe { libc::_exit(1) };
                }
                if config.case == Case::StuckCustodian {
                    // Provider collection is already certified by the real
                    // owner wait and ECHILD check above. Only this now-empty
                    // custodian remains for its actual parent to stop and wait.
                    drop(rescue);
                    write(&root, &name("stuck", role), &true)?;
                    while !root.join(name("unstick", role)).exists() && !root.join("abort").exists()
                    {
                        tick();
                    }
                } else if config.case.ordered_failure() {
                    drop(rescue);
                    std::process::exit(1);
                }
                return Ok(());
            }
            tick();
        }
    }
    // With the guardian held, only the worker may deliver Start to the relay.
    // Keep the redundant owner endpoint open so EOF cannot supply a trigger.
    let mut blocked_owner = Vec::new();
    if config.case == Case::GuardianStalled && role == Role::Relay {
        let incoming = streams.remove(&Role::CleanupOwner).unwrap();
        let (replacement, sink) = UnixStream::pair()?;
        streams.insert(Role::CleanupOwner, replacement);
        blocked_owner.push((incoming, sink));
    }
    let mut routes = streams
        .into_iter()
        .map(|(peer, stream)| Route::new(participant(&inventory, peer), Channel::new(stream)?))
        .collect::<io::Result<Vec<_>>>()?;
    if role == Role::Worker {
        let mut tickets = TicketState::new(config.run, 1).map_err(invalid)?;
        let mut publication = None;
        let mut held = Vec::new();
        write(&root, &name("ready", role), &true)?;
        while config.case.held(role) && !root.join("release").exists() {
            record(&root, role, state(&tickets, Vec::new()), &mut prior)?;
            tick();
        }
        wait(&root, || Ok(root.join("go").exists()))?;
        if config.case.origin() == role {
            let mut publishing = Publication::begin(
                start.clone(),
                local,
                local,
                &mut tickets,
                std::mem::take(&mut routes),
            )?;
            publishing.inventory(
                &inventory
                    .members
                    .iter()
                    .map(|m| m.participant)
                    .collect::<Vec<_>>(),
            );
            publication = Some(publishing);
        }
        loop {
            if root.join("abort").exists() {
                return Err(invalid("fixture aborted"));
            }
            if publication.is_none() {
                let mut accepted = None;
                for (index, route) in routes.iter_mut().enumerate() {
                    if let Some(received) = route.receive_start_from(
                        &mut tickets,
                        local,
                        &inventory
                            .members
                            .iter()
                            .map(|m| m.participant)
                            .collect::<Vec<_>>(),
                        u64::MAX,
                    )? {
                        match route.flush_ack(&received, &mut tickets) {
                            Ok(true) => {}
                            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                                // A deliberately held peer may first run after A.
                                // Keep its original ticket and sticky failure.
                            }
                            Ok(false) => return Err(invalid("fixture peer ACK backpressure")),
                            Err(error) => return Err(error),
                        }
                        accepted = Some((index, received));
                        break;
                    }
                }
                if let Some((index, received)) = accepted {
                    held.push(routes.remove(index));
                    let mut publishing = Publication::begin(
                        received.clone(),
                        received.origin,
                        local,
                        &mut tickets,
                        std::mem::take(&mut routes),
                    )?;
                    publishing.inventory(
                        &inventory
                            .members
                            .iter()
                            .map(|m| m.participant)
                            .collect::<Vec<_>>(),
                    );
                    publication = Some(publishing);
                }
            }
            if let Some(publication) = &mut publication {
                publication.progress(&mut tickets)?;
            }
            record(
                &root,
                role,
                state(
                    &tickets,
                    publication
                        .as_ref()
                        .map_or_else(Vec::new, Publication::test_pending_peers),
                ),
                &mut prior,
            )?;
            if root.join("abort").exists() {
                return Err(invalid("worker fixture observation rescue"));
            }
            if root.join("release").exists()
                && root
                    .join(name(
                        if config.case == Case::OwnerLost {
                            "forensic-wait"
                        } else {
                            "receipt"
                        },
                        Role::CleanupOwner,
                    ))
                    .exists()
            {
                if config.case == Case::UncancellableWorker {
                    drop(rescue);
                    write(&root, &name("stuck", role), &true)?;
                    // Deliberately stop reading cancellation or doing work.
                    // Only the independent parent or invalidating rescue can
                    // release this ordinary-child stand-in.
                    while !root.join(name("unstick", role)).exists() && !root.join("abort").exists()
                    {
                        tick();
                    }
                }
                return write(
                    &root,
                    &name("receipt", role),
                    &Receipt {
                        role,
                        failed: tickets.failed(),
                        waits: Vec::new(),
                        unresolved: 0,
                    },
                );
            }
            tick();
        }
    }
    let mut guard = Guard::new(local, config.run, 1)?;
    guard.install_inventory(&inventory, true)?;
    for route in routes {
        guard.route(route.peer(), route);
    }
    for child in children {
        let peer = inventory
            .members
            .iter()
            .find(|m| m.participant.pid == child.pid())
            .unwrap()
            .participant;
        guard
            .child(peer, child)
            .map_err(|_| invalid("child adoption mismatch"))?;
    }
    write(&root, &name("ready", role), &true)?;
    while config.case.held(role) && !root.join("release").exists() {
        record(&root, role, guard.test_clock_state(), &mut prior)?;
        tick();
    }
    let activity = runtime_activity::Activity::new(&root, &config.clock, false)?;
    let outcome = guard.run_parent_with_test_activity(
        |state| record(&root, role, state, &mut prior),
        |guard| activity.wait(guard),
    );
    let failed = outcome.failed;
    finish_parent(&root, role, outcome)?;
    if (config.case.stuck_role().is_some()
        || (config.case == Case::OwnerLost || config.case.ordered_failure()))
        && failed
    {
        // The helper reports failure only after its actual child waits.
        drop(rescue);
        std::process::exit(1);
    }
    Ok(())
}

pub fn total_publication_loss() -> io::Result<()> {
    run(Case::TotalLoss)
}

fn ordered_provider(root: &Path, config: &Config, local: Participant) -> io::Result<()> {
    use cleanup_order::Stage::*;
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let spawn = || {
        Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Owned)
    };
    let mut program = spawn()?;
    let mut debugger = spawn()?;
    let program_id = identity(program.0.id(), Role::Provider)?;
    let debugger_id = identity(debugger.0.id(), Role::Provider)?;
    write(
        root,
        "order-native-parents",
        &[(program_id, local), (debugger_id, local)],
    )?;
    let rights = UnixStream::pair()?;
    let mut recorder = cleanup_order::Recorder::new(root, config.run, local);
    let hold = |step: &str| {
        write(root, &format!("order-held-{step}"), &true)?;
        wait(root, || {
            Ok(root.join(format!("order-release-{step}")).exists())
        })
    };
    hold("operations")?;
    recorder.record(ProgramOperationsComplete, &[], None, None)?;
    hold("terminal")?;
    let emergency = config.case == Case::OrderEmergency;
    if emergency {
        // This scripted operation's allocation was fixed when the tree started.
        // It is not a new cleanup ticket or a production allowance.
        let cutoff = config.t0 + 500_000_000;
        if boot_nanos()? < cutoff {
            return Err(invalid("emergency before the original operation cutoff"));
        }
        recorder.record(
            Unconfirmed,
            &[ProgramOperationsComplete],
            None,
            Some((ProgramTerminal, cutoff, boot_nanos()?)),
        )?;
        recorder.record(DebuggerCleanupStarted, &[Unconfirmed], None, None)?;
    }
    drop(program.0.stdin.take());
    if !program.0.wait()?.success() {
        return Err(invalid("program failed"));
    }
    recorder.record(ProgramTerminal, &[ProgramOperationsComplete], None, None)?;
    recorder.record(
        ProgramParentCollected,
        &[ProgramTerminal],
        Some(program_id),
        None,
    )?;
    if !emergency {
        recorder.record(
            DebuggerCleanupStarted,
            &[ProgramOperationsComplete, ProgramTerminal],
            None,
            None,
        )?;
    }
    drop(debugger.0.stdin.take());
    if !debugger.0.wait()?.success() {
        return Err(invalid("debugger failed"));
    }
    recorder.record(
        DebuggerParentCollected,
        &[DebuggerCleanupStarted],
        Some(debugger_id),
        None,
    )?;
    hold("rights")?;
    drop(rights);
    recorder.record(RightsReleased, &[DebuggerParentCollected], None, None)?;
    if emergency {
        return Ok(());
    }
    recorder.record(
        ResourceCertificate,
        &[
            ProgramOperationsComplete,
            ProgramTerminal,
            ProgramParentCollected,
            DebuggerParentCollected,
            RightsReleased,
        ],
        None,
        None,
    )
}

pub fn run(case: Case) -> io::Result<()> {
    run_inner(case, false)
}

pub fn run_ordered_trigger(case: Case) -> io::Result<()> {
    assert!(matches!(
        case,
        Case::ShutdownError | Case::Unwind | Case::Cancellation
    ));
    run_inner(case, true)
}

pub fn run_ordered(emergency: bool) -> io::Result<()> {
    run_inner(
        if emergency {
            Case::OrderEmergency
        } else {
            Case::Normal
        },
        true,
    )
}

fn run_inner(case: Case, order: bool) -> io::Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("u-tree-")
        .tempdir_in("/tmp")?;
    let root = directory.path();
    let clock = SharedClock::new(boot_nanos()?);
    let config = Config {
        run: RunIdentity {
            nonce: uuid::Uuid::new_v4(),
            generation: 1,
        },
        t0: boot_nanos()?,
        clock: clock.path(),
        case,
        order,
    };
    clock.set(config.t0);
    let _scope = clock.install();
    write(root, "config", &config)?;
    let mut rescue = Rescue {
        root: root.to_owned(),
        config: config.clone(),
        role: Role::Caller,
        children: Vec::new(),
    };
    let listener = UnixListener::bind(root.join(name("socket", Role::Caller)))?;
    listener.set_nonblocking(true)?;
    let local = identity(std::process::id(), Role::Caller)?;
    write(
        root,
        &name("member", Role::Caller),
        &Member {
            participant: local,
            parent: None,
        },
    )?;
    let relay = spawn(root, Role::Relay, &mut rescue)?;
    wait(root, || {
        Ok(ROLES
            .iter()
            .all(|role| root.join(name("member", *role)).exists()))
    })?;
    let members = ROLES
        .iter()
        .map(|role| read(root, &name("member", *role)).map(Option::unwrap))
        .collect::<io::Result<Vec<_>>>()?;
    let inventory = Inventory {
        run: config.run,
        external_caller: true,
        members,
    };
    inventory.validate(config.run, true)?;
    write(root, "inventory", &inventory)?;
    let mut live_watch = ProcessWatch::new()?;
    for member in &inventory.members {
        if member.participant != local {
            live_watch.install(member.participant)?;
        }
    }
    let streams = connections(root, Role::Caller, &listener)?;
    let mut guard = Guard::new(local, config.run, 1)?;
    guard.install_inventory(&inventory, true)?;
    guard
        .child(participant(&inventory, Role::Relay), relay)
        .map_err(|_| invalid("caller relay adoption"))?;
    // Leave only the case's selected delivery route usable. TotalLoss blocks
    // every route. Production endpoints stay installed and open on both sides.
    let mut blocked = Vec::new();
    for (peer, incoming) in streams {
        if case.acknowledged() || case.delivery() == Some(peer) {
            let peer = participant(&inventory, peer);
            guard.route(peer, Route::new(peer, Channel::new(incoming)?)?);
            continue;
        }
        let (caller, sink) = UnixStream::pair()?;
        let peer = participant(&inventory, peer);
        guard.route(peer, Route::new(peer, Channel::new(caller)?)?);
        blocked.push((incoming, sink));
    }
    wait(root, || {
        Ok(ROLES[1..]
            .iter()
            .all(|role| root.join(name("ready", *role)).exists()))
    })?;
    write(root, "go", &true)?;
    let start = original(&config, participant(&inventory, case.origin()));
    let ticket = start.ticket;
    let ack_checkpoint = config.t0 + ACK_NANOS + 10_000_000;
    if ack_checkpoint >= ticket.cutoffs(1).map_err(invalid)?.cooperative {
        return Err(invalid("checkpoint outside local window"));
    }
    let bound = Instant::now() + Duration::from_secs(3);
    let mut advanced = false;
    let mut released = false;
    let mut rescued = false;
    let mut wrong_caller_state = false;
    let mut caller_notified_before_ack = false;
    let mut order_violation = false;
    let mut stopping_child = false;
    let mut prior = None;
    let activity = runtime_activity::Activity::new(root, &config.clock, true)?;
    let outcome = guard.run_parent_with_test_activity(
        |state| {
            if order {
                use cleanup_order::Stage::*;
                for (step, forbidden) in [
                    ("operations", DebuggerCleanupStarted),
                    ("terminal", DebuggerCleanupStarted),
                    ("rights", ResourceCertificate),
                ] {
                    if root.join(format!("order-held-{step}")).exists()
                        && !root.join(format!("order-release-{step}")).exists()
                    {
                        if cleanup_order::load(root, forbidden)?.is_some() {
                            // Preserve the observation, but release the fixture
                            // so every actual parent still collects its children.
                            order_violation = true;
                        }
                        if case == Case::OrderEmergency && step == "terminal" {
                            clock.advance_to(config.t0 + 500_000_000);
                        }
                        write(root, &format!("order-release-{step}"), &true)?;
                    }
                }
            }
            caller_notified_before_ack |=
                state.ticket == Some(ticket) && boot_nanos()? == config.t0;
            wrong_caller_state |= state.expired
                || (!stopping_child && !state.stage_events.is_empty())
                || match state.ticket {
                    None => {
                        state.receive_deadline != u64::MAX || (advanced && case != Case::TotalLoss)
                    }
                    Some(found) => {
                        case == Case::TotalLoss
                            || found != ticket
                            || state.receive_deadline != ticket.deadline
                    }
                };
            let caller_checkpoint = case == Case::TotalLoss
                || (state.ticket == Some(ticket) && state.failed != case.acknowledged());
            let caller_published = !case.acknowledged() || state.pending_publication.is_empty();
            record(root, Role::Caller, state, &mut prior)?;
            if let Some(role) = case.stuck_role() {
                if released && !stopping_child && root.join(name("stuck", role)).exists() {
                    let child = participant(&inventory, role);
                    if identity(child.pid, child.role)? != child {
                        return Err(invalid("child exited before its parent opportunity"));
                    }
                    stopping_child = true;
                    // Guardian's owner slot follows its worker slot by one second,
                    // within the original four-second helper allocation.
                    let offset = if role == Role::CleanupOwner {
                        1_000_000_000
                    } else {
                        0
                    };
                    clock.advance_to(ticket.cutoffs(1).map_err(invalid)?.fallback + offset);
                }
                if Instant::now() >= bound {
                    rescued = true;
                    std::fs::write(root.join(name("unstick", role)), b"rescue")?;
                    clock.advance_to(ticket.deadline);
                }
            }
            if !rescued && Instant::now() >= bound {
                rescued = true;
                clock.advance_to(ticket.deadline);
                std::fs::write(root.join("abort"), b"rescue")?;
                std::fs::write(root.join("release"), b"rescue")?;
                released = true;
            }
            let states = PUBLISHERS
                .iter()
                .map(|role| read::<Reading>(root, &name("state", *role)))
                .collect::<io::Result<Vec<_>>>()?;
            let provider = read::<Reading>(root, &name("state", Role::Provider))?;
            if !advanced
                && (case == Case::TotalLoss || caller_notified_before_ack)
                && caller_published
                && states.iter().zip(PUBLISHERS).all(|(reading, role)| {
                    reading.as_ref().is_some_and(|r| {
                        r.now == config.t0
                            && r.state.ticket == (!case.held(role)).then_some(ticket)
                            && !r.state.failed
                            && match (case, case.delivery()) {
                                (case, _) if case.acknowledged() => {
                                    r.state.pending_publication.is_empty()
                                }
                                (_, None) => r.state.pending_publication == [local],
                                (_, Some(delivery)) if delivery == role => {
                                    !r.state.pending_publication.contains(&local)
                                }
                                _ => true,
                            }
                    })
                })
                && provider
                    .as_ref()
                    .is_some_and(|r| r.state.ticket == Some(ticket))
            {
                clock.advance_to(ack_checkpoint);
                advanced = true;
            } else if advanced
                && !released
                && caller_checkpoint
                && states.iter().zip(PUBLISHERS).all(|(reading, role)| {
                    reading.as_ref().is_some_and(|r| {
                        r.now == ack_checkpoint
                            && r.state.ticket == (!case.held(role)).then_some(ticket)
                            && r.state.failed == (!case.acknowledged() && !case.held(role))
                            && !r.state.expired
                            && r.state.stage_events.is_empty()
                            && r.state.pending_publication.is_empty()
                    })
                })
                && provider
                    .as_ref()
                    .is_some_and(|r| r.now == ack_checkpoint && r.state.ticket == Some(ticket))
            {
                for member in &inventory.members {
                    if identity(member.participant.pid, member.participant.role)?
                        != member.participant
                    {
                        return Err(invalid("custody lost at ACK checkpoint"));
                    }
                }
                if live_watch.exited()?.is_some() {
                    return Err(invalid(
                        "custodian exited before the original local-window checkpoint",
                    ));
                }
                write(root, "checkpoint", &true)?;
                std::fs::write(root.join("release"), b"release")?;
                released = true;
            }
            Ok(())
        },
        |guard| activity.wait(guard),
    );
    let failed = outcome.failed;
    eprintln!("{case:?} controller: advanced={advanced} released={released} rescued={rescued}");
    for role in ROLES {
        eprintln!(
            "{case:?} {role:?}: {}",
            std::fs::read_to_string(root.join(name("state", role))).unwrap_or_default()
        );
    }
    let caller_starts = outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Start(_)));
    let caller_original = outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Start(received) if *received == start));
    finish_parent(root, Role::Caller, outcome)?;
    drop(rescue);
    if advanced && !released {
        return Err(invalid(
            "custodian exited before the original local-window checkpoint",
        ));
    }
    if case != Case::TotalLoss && !caller_original {
        return Err(invalid(
            "caller missed original Start while notification forwarders were held",
        ));
    }
    if rescued || !advanced || !root.join("checkpoint").exists() {
        return Err(invalid(
            "original-ticket shared-clock checkpoint was not reached",
        ));
    }
    if wrong_caller_state
        || (case == Case::TotalLoss && caller_starts)
        || failed != case.failed(Role::Caller)
    {
        return Err(invalid(
            "unnotified caller invented a deadline or successful outcome",
        ));
    }
    for role in ROLES {
        if case == Case::OwnerLost && role == Role::CleanupOwner {
            let forensic: Receipt = read(root, &name("forensic-wait", role))?
                .ok_or_else(|| invalid("missing owner test-only wait audit"))?;
            let provider = participant(&inventory, Role::Provider);
            if forensic.waits != [(provider, 0)]
                || forensic.unresolved != 0
                || read::<Receipt>(root, &name("receipt", role))?.is_some()
                || read::<Vec<(u32, i32)>>(root, &name("rescue", role))? != Some(Vec::new())
            {
                return Err(invalid("lost owner manufactured a collection receipt"));
            }
            continue;
        }
        if case == Case::UncancellableWorker && role == Role::Worker {
            if read::<Receipt>(root, &name("receipt", role))?.is_some()
                || read::<Vec<(u32, i32)>>(root, &name("rescue", role))? != Some(Vec::new())
                || inventory
                    .members
                    .iter()
                    .any(|member| member.parent.is_some_and(|parent| parent.role == role))
            {
                return Err(invalid(
                    "uncancellable worker produced a cooperative receipt",
                ));
            }
            // Its actual Guardian wait (checked above) is the only collection
            // receipt. The killed worker cannot provide one for itself.
            continue;
        }
        let receipt: Receipt =
            read(root, &name("receipt", role))?.ok_or_else(|| invalid("missing parent receipt"))?;
        let rescued: Vec<(u32, i32)> =
            read(root, &name("rescue", role))?.ok_or_else(|| invalid("missing rescue audit"))?;
        if !rescued.is_empty() || receipt.unresolved != 0 {
            return Err(invalid(
                "harness rescue cannot certify production collection",
            ));
        }
        let expected: Vec<_> = inventory
            .members
            .iter()
            .filter(|m| m.parent.is_some_and(|p| p.role == role))
            .map(|m| m.participant)
            .collect();
        if receipt.waits.len() != expected.len()
            || expected.iter().any(|peer| {
                let status = if case.ordered_failure() && peer.role != Role::Worker {
                    1 << 8
                } else if let Some(stuck) = case.stuck_role() {
                    match peer.role {
                        role if role == stuck => libc::SIGKILL,
                        Role::Guardian | Role::Relay => 1 << 8,
                        _ => 0,
                    }
                } else if case == Case::OwnerLost
                    && matches!(peer.role, Role::CleanupOwner | Role::Guardian | Role::Relay)
                {
                    1 << 8
                } else {
                    0
                };
                !receipt.waits.contains(&(*peer, status))
            })
        {
            return Err(invalid("missing exact expected production parent wait"));
        }
        if PUBLISHERS.contains(&role) && receipt.failed != case.failed(role) {
            return Err(invalid("role failure outcome mismatch"));
        }
        eprintln!(
            "{case:?} actual parent receipt: {}",
            serde_json::to_string(&receipt)?
        );
    }
    if order {
        let trigger = match case {
            Case::ShutdownError => Some("trigger-shutdown-error"),
            Case::Unwind => Some("trigger-unwind"),
            Case::Cancellation => Some("trigger-cancellation"),
            _ => None,
        };
        if let Some(trigger) = trigger {
            assert_eq!(read::<bool>(root, trigger)?, Some(true));
            assert!(failed, "ordered collection erased original failure");
        }
        if order_violation {
            return Err(invalid("cleanup crossed a held barrier"));
        }
        let mut parents: Vec<_> = inventory
            .members
            .iter()
            .filter_map(|m| m.parent.map(|p| (m.participant, p)))
            .collect();
        let native_parents = read::<Vec<(Participant, Participant)>>(root, "order-native-parents")?
            .ok_or_else(|| invalid("missing resource parents"))?;
        if native_parents.len() != 2 {
            return Err(invalid("wrong resource parent count"));
        }
        use cleanup_order::Stage;
        let mut expected_children = vec![
            (Stage::ProgramParentCollected, native_parents[0].0),
            (Stage::DebuggerParentCollected, native_parents[1].0),
        ];
        for (stage, role) in [
            (Stage::ProviderParentCollected, Role::Provider),
            (Stage::WorkerParentCollected, Role::Worker),
            (Stage::OwnerParentCollected, Role::CleanupOwner),
            (Stage::GuardianParentCollected, Role::Guardian),
            (Stage::RelayParentCollected, Role::Relay),
        ] {
            expected_children.push((stage, participant(&inventory, role)));
        }
        parents.extend(native_parents);
        let mut pending = Vec::<cleanup_order::Receipt>::new();
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy();
            if name.starts_with("order-") && name.ends_with(".json") {
                pending.push(serde_json::from_slice(&std::fs::read(path)?)?);
            }
        }
        let mut ordered: Vec<cleanup_order::Receipt> = Vec::new();
        while !pending.is_empty() {
            let index = pending
                .iter()
                .position(|r| {
                    r.predecessors
                        .iter()
                        .all(|id| ordered.iter().any(|p| p.id == *id))
                        && r.id.sequence
                            == 1 + ordered
                                .iter()
                                .filter(|p| p.id.producer == r.id.producer)
                                .count() as u64
                })
                .ok_or_else(|| invalid("order receipts missing causal predecessor"))?;
            ordered.push(pending.remove(index));
        }
        cleanup_order::verify(
            &ordered,
            config.run,
            config.case == Case::OrderEmergency,
            &parents,
            &expected_children,
        )
        .map_err(|error| invalid(&error))?;
        // Exercise the parser against the receipts from these actual processes.
        // A census with the same final empty state cannot recover missing proof.
        if case == Case::Normal {
            use cleanup_order::Stage::*;
            for missing in [ProgramTerminal, DebuggerParentCollected, RightsReleased] {
                let damaged: Vec<_> = ordered
                    .iter()
                    .filter(|r| r.stage != missing)
                    .cloned()
                    .collect();
                if cleanup_order::verify(&damaged, config.run, false, &parents, &expected_children)
                    .is_ok()
                {
                    return Err(invalid("missing proof accepted"));
                }
            }
            for (left, right) in [
                (ProgramParentCollected, DebuggerParentCollected),
                (WorkerParentCollected, OwnerParentCollected),
            ] {
                let mut swapped = ordered.clone();
                let a = swapped.iter().position(|r| r.stage == left).unwrap();
                let b = swapped.iter().position(|r| r.stage == right).unwrap();
                let child = swapped[a].collected;
                swapped[a].collected = swapped[b].collected;
                swapped[b].collected = child;
                if cleanup_order::verify(&swapped, config.run, false, &parents, &expected_children)
                    .is_ok()
                {
                    return Err(invalid(
                        "collection identities swapped across named stages accepted",
                    ));
                }
            }
            let mut stale = ordered.clone();
            stale[0].run.generation += 1;
            if cleanup_order::verify(&stale, config.run, false, &parents, &expected_children)
                .is_ok()
            {
                return Err(invalid("stale generation accepted"));
            }
            let mut forged = ordered.clone();
            let wait = forged
                .iter_mut()
                .find(|r| r.stage == ProviderParentCollected)
                .unwrap();
            wait.id.producer = participant(&inventory, Role::Worker);
            if cleanup_order::verify(&forged, config.run, false, &parents, &expected_children)
                .is_ok()
            {
                return Err(invalid("nonparent collection accepted"));
            }
        }
        eprintln!(
            "ordered cleanup receipts: {}",
            serde_json::to_string(&ordered)?
        );
    }
    Ok(())
}
