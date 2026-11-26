//! Ordinary-child D1 fixture. File latches control work, never certify collection.
use super::runtime_clock::{self, SharedClock};
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

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    run: RunIdentity,
    t0: u64,
    clock: PathBuf,
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
    let waits = outcome
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Collected(peer, status) => Some((*peer, *status)),
            _ => None,
        })
        .collect();
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
    let owner_identity = participant(&inventory, Role::CleanupOwner);
    let start = original(&config, owner_identity);
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
                            .accept(received, owner_identity, local)
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
        owner.start(start.clone(), local)?;
        // Scripted provider work starts under the same original local ticket.
        provider_channel.queue(&start)?;
        if !provider_channel.flush(start.ticket.deadline)? {
            return Err(invalid("fixture provider Start backpressure"));
        }
        let mut cancelled = false;
        loop {
            if root.join("abort").exists() {
                return Err(invalid("fixture aborted"));
            }
            let status = owner.progress()?;
            if owner.failed() && !cancelled {
                owner.cancel("caller notification acknowledgement lost".into())?;
                cancelled = true;
            }
            record(
                &root,
                role,
                GuardState {
                    ticket: owner.test_clock_ticket(),
                    receive_deadline: start.ticket.deadline,
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
                let mut raw = 0;
                let result = unsafe { libc::waitpid(provider.pid as i32, &mut raw, libc::WNOHANG) };
                if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
                    return Err(invalid("owner omitted actual provider parent wait"));
                }
                write(
                    &root,
                    &name("receipt", role),
                    &Receipt {
                        role,
                        failed: owner.failed(),
                        waits: vec![(provider, status.into_raw())],
                        unresolved: 0,
                    },
                )?;
                wait(&root, || {
                    Ok(root.join(name("receipt", Role::Worker)).exists())
                })?;
                return Ok(());
            }
            tick();
        }
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
                        if !route.flush_ack(&received, &mut tickets)? {
                            return Err(invalid("fixture peer ACK backpressure"));
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
            if root.join("release").exists()
                && root.join(name("receipt", Role::CleanupOwner)).exists()
            {
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
    let outcome =
        guard.run_parent_with_test_observer(|state| record(&root, role, state, &mut prior));
    finish_parent(&root, role, outcome)
}

pub fn total_publication_loss() -> io::Result<()> {
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
    // Black-hole every caller route in both directions. Production caller
    // endpoints remain installed and open, with no Start or ACK crossing.
    let mut blocked = Vec::new();
    for (peer, incoming) in streams {
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
    let ticket = original(&config, participant(&inventory, Role::CleanupOwner)).ticket;
    let ack_checkpoint = config.t0 + ACK_NANOS + 10_000_000;
    if ack_checkpoint >= ticket.cutoffs(1).map_err(invalid)?.cooperative {
        return Err(invalid("checkpoint outside local window"));
    }
    let bound = Instant::now() + Duration::from_secs(3);
    let mut advanced = false;
    let mut released = false;
    let mut rescued = false;
    let mut wrong_caller_state = false;
    let mut prior = None;
    let outcome = guard.run_parent_with_test_observer(|state| {
        wrong_caller_state |= state.ticket.is_some()
            || state.receive_deadline != u64::MAX
            || state.expired
            || !state.stage_events.is_empty();
        record(root, Role::Caller, state, &mut prior)?;
        if !released && Instant::now() >= bound {
            rescued = true;
            clock.advance_to(ticket.deadline);
            std::fs::write(root.join("release"), b"rescue")?;
            released = true;
        }
        let states = PUBLISHERS
            .iter()
            .map(|role| read::<Reading>(root, &name("state", *role)))
            .collect::<io::Result<Vec<_>>>()?;
        let provider = read::<Reading>(root, &name("state", Role::Provider))?;
        if !advanced
            && states.iter().all(|reading| {
                reading.as_ref().is_some_and(|r| {
                    r.now == config.t0
                        && r.state.ticket == Some(ticket)
                        && !r.state.failed
                        && r.state.pending_publication == [local]
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
            && states.iter().all(|reading| {
                reading.as_ref().is_some_and(|r| {
                    r.now == ack_checkpoint
                        && r.state.ticket == Some(ticket)
                        && r.state.failed
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
                if identity(member.participant.pid, member.participant.role)? != member.participant
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
    });
    let failed = outcome.failed;
    eprintln!("D1 controller: advanced={advanced} released={released} rescued={rescued}");
    for role in ROLES {
        eprintln!(
            "D1 {role:?}: {}",
            std::fs::read_to_string(root.join(name("state", role))).unwrap_or_default()
        );
    }
    let caller_starts = outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Start(_)));
    finish_parent(root, Role::Caller, outcome)?;
    drop(rescue);
    if advanced && !released {
        return Err(invalid(
            "custodian exited before the original local-window checkpoint",
        ));
    }
    if rescued || !advanced || !root.join("checkpoint").exists() {
        return Err(invalid("full D1 shared-clock checkpoint was not reached"));
    }
    if wrong_caller_state || caller_starts || !failed {
        return Err(invalid(
            "unnotified caller invented a deadline or successful outcome",
        ));
    }
    for role in ROLES {
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
            || expected
                .iter()
                .any(|peer| !receipt.waits.contains(&(*peer, 0)))
        {
            return Err(invalid("missing exact successful production parent wait"));
        }
        if PUBLISHERS.contains(&role) && !receipt.failed {
            return Err(invalid("publication failure was not sticky"));
        }
        eprintln!(
            "D1 actual parent receipt: {}",
            serde_json::to_string(&receipt)?
        );
    }
    Ok(())
}
