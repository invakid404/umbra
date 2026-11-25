//! Same-executable role re-entry with explicit descriptor ownership.
use super::{
    invalid,
    process::{identity, identity_with_parent, SpawnedChild},
    topology::{Endpoint, Purpose},
};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use umbra_core::managed::{LaunchConfiguration, Participant, Role};

/// Private role-dispatch configuration; removed before running worker code.
pub const ENV_ROLE: &str = "UMBRA_MANAGED_ROLE";

#[derive(serde::Serialize, serde::Deserialize)]
struct Descriptor {
    owner: Role,
    peer: Role,
    purpose: Purpose,
    fd: i32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Spec {
    role: Role,
    parent: Participant,
    launch: LaunchConfiguration,
    endpoints: Vec<Descriptor>,
    bootstrap: i32,
    readiness_deadline: u64,
}

/// Adopted child configuration ready for the executable's registered dispatch.
pub struct Entry {
    /// Role assigned by the real parent.
    pub role: Role,
    /// Birth-validated actual parent.
    pub parent: Participant,
    /// Shared serializable launch input.
    pub launch: LaunchConfiguration,
    /// Original shared readiness cutoff, inherited without renewal.
    pub readiness_deadline: u64,
    /// Only this role's owned endpoints.
    pub endpoints: Vec<Endpoint>,
    /// Endpoints held only until this role spawns its descendants.
    pub descendants: BTreeMap<Role, Vec<Endpoint>>,
    bootstrap: Option<super::Channel>,
    authorization: Option<super::startup::Go>,
    announced: Option<super::inventory::Announcement>,
    inventory: Option<super::inventory::Inventory>,
}

/// Spawn a helper from this executable; no caller-supplied executable fallback.
/// Consumed endpoint copies close in the parent after the atomic spawn boundary.
pub fn spawn(
    role: Role,
    parent_role: Role,
    launch: &LaunchConfiguration,
    endpoints: Vec<Endpoint>,
    new_session: bool,
    readiness_deadline: u64,
) -> io::Result<PendingRole> {
    spawn_tree(
        role,
        parent_role,
        launch,
        BTreeMap::from([(role, endpoints)]),
        new_session,
        readiness_deadline,
    )
}

/// Spawn with explicit assignments for this role and its descendants.
pub fn spawn_tree(
    role: Role,
    parent_role: Role,
    launch: &LaunchConfiguration,
    assignments: BTreeMap<Role, Vec<Endpoint>>,
    new_session: bool,
    readiness_deadline: u64,
) -> io::Result<PendingRole> {
    if super::boot_nanos()? >= readiness_deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    if assignments.keys().any(|owner| !in_subtree(role, *owner)) {
        return Err(invalid("endpoint assignment outside child subtree"));
    }
    launch.validate().map_err(invalid)?;
    if !matches!(
        (parent_role, role),
        (Role::Caller, Role::Relay)
            | (Role::Relay, Role::Guardian)
            | (Role::Guardian, Role::Worker | Role::CleanupOwner)
    ) {
        return Err(invalid("invalid managed helper parent edge"));
    }
    let parent = identity(std::process::id(), parent_role)?;
    let (parent_bootstrap, child_bootstrap) = UnixStream::pair()?;
    let parent_bootstrap = super::Channel::new(parent_bootstrap)?;
    let spec = Spec {
        bootstrap: child_bootstrap.as_raw_fd(),
        readiness_deadline,
        role,
        parent,
        launch: launch.clone(),
        endpoints: assignments
            .iter()
            .flat_map(|(owner, endpoints)| {
                endpoints.iter().map(move |endpoint| Descriptor {
                    owner: *owner,
                    peer: endpoint.peer,
                    purpose: endpoint.purpose,
                    fd: endpoint.stream.as_raw_fd(),
                })
            })
            .collect(),
    };
    let encoded = serde_json::to_string(&spec).map_err(io::Error::other)?;
    if encoded.len() > umbra_core::managed::MAX_FRAME {
        return Err(invalid("managed re-entry configuration too large"));
    }
    let executable = std::env::current_exe()?;
    let args = [executable.as_os_str().to_owned()];
    let environment =
        crate::spawn::environment(&[ENV_ROLE, crate::ENV_GUARDIAN], &[(ENV_ROLE, encoded)]);
    let mut fds: Vec<_> = spec.endpoints.iter().map(|endpoint| endpoint.fd).collect();
    fds.push(spec.bootstrap);
    let child = SpawnedChild::spawn(&executable, &args, &environment, new_session, &fds)?;
    Ok(PendingRole {
        child,
        bootstrap: parent_bootstrap,
        role,
        run: launch.run,
        ready: false,
        member: None,
        parent,
        inventory_sent: None,
        go_queued: None,
        capacity: launch.capacity,
        readiness_deadline,
    })
}

/// Call before ordinary argument parsing. Validate the inherited configuration
/// and consume each listed descriptor once, restoring close-on-exec immediately.
pub fn entry() -> io::Result<Option<Entry>> {
    let Some(encoded) = std::env::var_os(ENV_ROLE) else {
        return Ok(None);
    };
    std::env::remove_var(ENV_ROLE);
    let encoded = encoded
        .to_str()
        .ok_or_else(|| invalid("managed configuration is not UTF-8"))?;
    if encoded.len() > umbra_core::managed::MAX_FRAME {
        return Err(invalid("managed re-entry configuration too large"));
    }
    let spec: Spec = serde_json::from_str(encoded).map_err(io::Error::other)?;
    if spec.bootstrap <= 2 {
        return Err(invalid("invalid bootstrap descriptor"));
    }
    // SAFETY: check validity before taking exclusive ownership of this inherited descriptor.
    if unsafe { libc::fcntl(spec.bootstrap, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the private startup descriptor is adopted once and excluded from all endpoint assignments below.
    let bootstrap = unsafe { UnixStream::from_raw_fd(spec.bootstrap) };
    // SAFETY: owned descriptor, integer flags.
    if unsafe { libc::fcntl(bootstrap.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    validate_socket(&bootstrap)?;
    let bootstrap = super::Channel::new(bootstrap)?;
    let mut seen = std::collections::BTreeSet::from([spec.bootstrap]);
    if spec.endpoints.len() > 22
        || spec.endpoints.iter().any(|endpoint| {
            endpoint.fd <= 2 || !seen.insert(endpoint.fd) || !in_subtree(spec.role, endpoint.owner)
        })
    {
        return Err(invalid("invalid managed descriptor inventory"));
    }
    let mut assignments: BTreeMap<Role, Vec<Endpoint>> = BTreeMap::new();
    for endpoint in spec.endpoints {
        // SAFETY: F_GETFD checks the raw number without transferring ownership.
        if unsafe { libc::fcntl(endpoint.fd, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: this process was exec'd with these nonstandard descriptors;
        // uniqueness was checked above and ownership is taken exactly once.
        let stream = unsafe { UnixStream::from_raw_fd(endpoint.fd) };
        // SAFETY: an exclusively owned descriptor, with integer flags.
        if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        validate_socket(&stream)?;
        assignments
            .entry(endpoint.owner)
            .or_default()
            .push(Endpoint {
                peer: endpoint.peer,
                purpose: endpoint.purpose,
                stream,
            });
    }
    spec.launch.validate().map_err(invalid)?;
    let (_, parent_pid) = identity_with_parent(std::process::id(), spec.role)?;
    if parent_pid != spec.parent.pid || identity(parent_pid, spec.parent.role)? != spec.parent {
        return Err(invalid("managed re-entry parent identity mismatch"));
    }
    Ok(Some(Entry {
        role: spec.role,
        parent: spec.parent,
        launch: spec.launch,
        readiness_deadline: spec.readiness_deadline,
        endpoints: assignments.remove(&spec.role).unwrap_or_default(),
        descendants: assignments,
        bootstrap: Some(bootstrap),
        authorization: None,
        announced: None,
        inventory: None,
    }))
}

fn validate_socket(stream: &UnixStream) -> io::Result<()> {
    // A peer may already have exited during startup. Validate this endpoint's
    // type and address family without requiring the peer to remain connected.
    stream.local_addr()?;
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: valid integer output buffer and length for SO_TYPE.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if kind != libc::SOCK_STREAM {
        return Err(invalid("managed endpoint is not a stream"));
    }
    Ok(())
}

fn in_subtree(root: Role, candidate: Role) -> bool {
    root == candidate
        || match root {
            Role::Relay => matches!(
                candidate,
                Role::Guardian | Role::Worker | Role::CleanupOwner | Role::Provider
            ),
            Role::Guardian => matches!(
                candidate,
                Role::Worker | Role::CleanupOwner | Role::Provider
            ),
            Role::CleanupOwner => candidate == Role::Provider,
            _ => false,
        }
}

impl Entry {
    /// Obtain fail-stop authority only for this registered, GO-authorized worker.
    pub fn worker_boundary(&self) -> io::Result<super::fail_stop::WorkerBoundary> {
        let local = identity(std::process::id(), self.role)?;
        if self.role != Role::Worker
            || !self
                .authorization
                .as_ref()
                .is_some_and(|go| go.run == self.launch.run)
            || !self.inventory.as_ref().is_some_and(|inventory| {
                inventory
                    .members
                    .iter()
                    .any(|member| member.participant == local)
            })
        {
            return Err(invalid("worker boundary requires enrolled worker GO"));
        }
        Ok(super::fail_stop::WorkerBoundary::new(
            local,
            self.launch.run,
        ))
    }

    /// Move one child's entire subtree into its spawn, closing parent copies.
    pub fn spawn_child(&mut self, role: Role, new_session: bool) -> io::Result<PendingRole> {
        let roles: Vec<_> = self
            .descendants
            .keys()
            .copied()
            .filter(|candidate| in_subtree(role, *candidate))
            .collect();
        let assignments = roles
            .into_iter()
            .map(|role| (role, self.descendants.remove(&role).unwrap()))
            .collect();
        spawn_tree(
            role,
            self.role,
            &self.launch,
            assignments,
            new_session,
            self.readiness_deadline,
        )
    }
}

/// Parent ownership of a helper while the startup barrier is closed.
pub struct PendingRole {
    /// Actual child handle retained through readiness failures and rollback.
    pub child: SpawnedChild,
    bootstrap: super::Channel,
    role: Role,
    run: umbra_core::managed::RunIdentity,
    ready: bool,
    member: Option<super::inventory::Member>,
    parent: Participant,
    inventory_sent: Option<super::inventory::Inventory>,
    go_queued: Option<super::startup::Go>,
    capacity: u64,
    readiness_deadline: u64,
}
impl PendingRole {
    pub(super) fn permits_rollback(&self, deadline: u64) -> bool {
        self.go_queued.is_none()
            && self
                .readiness_deadline
                .checked_add(9_000_000_000)
                .is_some_and(|maximum| deadline <= maximum)
    }

    pub(super) fn guard_identity(&self) -> io::Result<(Participant, Participant)> {
        let member = self
            .member
            .as_ref()
            .ok_or_else(|| invalid("child not announced"))?;
        if !self.ready || member.parent != Some(self.parent) {
            return Err(invalid("child not ready for runtime custody"));
        }
        Ok((member.participant, self.parent))
    }

    /// Receive the child's initial identity before distributing the full topology.
    pub fn receive_members(&mut self) -> io::Result<Option<Vec<super::inventory::Member>>> {
        if self.member.is_some() || self.ready {
            return Err(invalid("identity announcement already consumed"));
        }
        let Some(announcement) = self
            .bootstrap
            .receive::<super::inventory::Announcement>(self.readiness_deadline)?
        else {
            return Ok(None);
        };
        let root = identity(self.child.pid(), self.role)?;
        announcement.validate(self.run, root, self.parent)?;
        self.member = announcement
            .members
            .iter()
            .find(|m| m.participant == root)
            .cloned();
        Ok(Some(announcement.members))
    }

    /// Publish the same complete validated topology on every descendant route.
    pub fn send_inventory(
        &mut self,
        inventory: &super::inventory::Inventory,
        external_caller: bool,
    ) -> io::Result<bool> {
        inventory.validate(self.run, external_caller)?;
        let member = self
            .member
            .as_ref()
            .ok_or_else(|| invalid("inventory before child identity"))?;
        if !inventory.members.contains(member) || self.ready {
            return Err(invalid("inventory excludes child or follows readiness"));
        }
        if self
            .inventory_sent
            .as_ref()
            .is_some_and(|prior| prior != inventory)
        {
            return Err(invalid("startup inventory changed"));
        }
        if self.inventory_sent.is_none() {
            self.bootstrap.queue(inventory)?;
            self.inventory_sent = Some(inventory.clone());
        }
        self.bootstrap.flush(self.readiness_deadline)
    }

    /// Receive readiness under the caller's shared startup deadline.
    pub fn receive_ready(&mut self, deadline: u64) -> io::Result<Option<super::startup::Ready>> {
        Ok(self.receive_readiness(deadline)?.and_then(|receipts| {
            receipts
                .into_iter()
                .find(|r| r.participant.role == self.role)
        }))
    }

    /// Collect all readiness receipts for this child's subtree.
    pub fn receive_readiness(
        &mut self,
        deadline: u64,
    ) -> io::Result<Option<Vec<super::startup::Ready>>> {
        if self.ready {
            return Err(invalid("readiness already consumed"));
        }
        if self.member.is_some() && self.inventory_sent.is_none() {
            return Err(invalid("readiness before inventory distribution"));
        }
        let Some(receipts) = self
            .bootstrap
            .receive::<Vec<super::startup::Ready>>(deadline.min(self.readiness_deadline))?
        else {
            return Ok(None);
        };
        let root = identity(self.child.pid(), self.role)?;
        if let Some(inventory) = &self.inventory_sent {
            inventory.validate_readiness(root, &receipts)?;
        } else if receipts.len() != 1
            || receipts[0].run != self.run
            || receipts[0].participant != root
        {
            return Err(invalid("child readiness identity mismatch"));
        }
        self.ready = true;
        Ok(Some(receipts))
    }
    /// Publish GO only after durable Armed.
    pub fn send_go(
        &mut self,
        startup: &super::startup::Startup,
        deadline: u64,
    ) -> io::Result<bool> {
        self.send_authorization(&startup.authorization(self.run)?, deadline)
    }

    /// Forward a worker's durable authorization without creating a new cutoff.
    /// The caller must receive it on the enrolled worker's startup exchange.
    pub fn send_authorization(
        &mut self,
        authorization: &super::startup::Go,
        deadline: u64,
    ) -> io::Result<bool> {
        if !self.ready {
            return Err(invalid("GO before readiness"));
        }
        authorization.validate(
            self.run,
            identity(self.child.pid(), self.role)?,
            self.capacity,
        )?;
        if self
            .go_queued
            .as_ref()
            .is_some_and(|prior| prior != authorization)
        {
            return Err(invalid("GO authorization changed during publication"));
        }
        if self.go_queued.is_none() {
            self.bootstrap.queue(authorization)?;
            self.go_queued = Some(authorization.clone());
        }
        self.bootstrap.flush(deadline.min(authorization.deadline))
    }
    /// Close bootstrap and retain the actual child collection obligation.
    pub fn into_child(self) -> SpawnedChild {
        self.child
    }
}
impl Entry {
    /// Announce this role before any installed-watch readiness assertion.
    pub fn announce_members(
        &mut self,
        descendants: &[super::inventory::Member],
    ) -> io::Result<bool> {
        let local = identity(std::process::id(), self.role)?;
        let mut members = vec![super::inventory::Member {
            participant: local,
            parent: Some(self.parent),
        }];
        members.extend_from_slice(descendants);
        let announcement = super::inventory::Announcement {
            run: self.launch.run,
            members,
        };
        announcement.validate(self.launch.run, local, self.parent)?;
        if self
            .announced
            .as_ref()
            .is_some_and(|prior| prior != &announcement)
        {
            return Err(invalid("subtree announcement changed"));
        }
        let bootstrap = self
            .bootstrap
            .as_mut()
            .ok_or_else(|| invalid("startup already closed"))?;
        if self.announced.is_none() {
            bootstrap.queue(&announcement)?;
            self.announced = Some(announcement);
        }
        bootstrap.flush(self.readiness_deadline)
    }

    /// Accept the parent's complete topology under the original readiness cutoff.
    pub fn receive_inventory(&mut self, external_caller: bool) -> io::Result<bool> {
        if self.announced.is_none() || self.inventory.is_some() {
            return Err(invalid("inventory in wrong startup phase"));
        }
        let bootstrap = self
            .bootstrap
            .as_mut()
            .ok_or_else(|| invalid("startup already closed"))?;
        if !bootstrap.flush(self.readiness_deadline)? {
            return Ok(false);
        }
        let Some(inventory) =
            bootstrap.receive::<super::inventory::Inventory>(self.readiness_deadline)?
        else {
            return Ok(false);
        };
        inventory.validate(self.launch.run, external_caller)?;
        let member = super::inventory::Member {
            participant: identity(std::process::id(), self.role)?,
            parent: Some(self.parent),
        };
        if !inventory.members.contains(&member) {
            return Err(invalid("received inventory excludes local identity"));
        }
        if self.announced.as_ref().is_some_and(|announcement| {
            announcement
                .members
                .iter()
                .any(|member| !inventory.members.contains(member))
        }) {
            return Err(invalid("received inventory changed announced subtree"));
        }
        self.inventory = Some(inventory);
        Ok(true)
    }

    /// Validated topology available for watch installation and child distribution.
    pub fn inventory(&self) -> Option<&super::inventory::Inventory> {
        self.inventory.as_ref()
    }

    /// Forward only the authorization this role accepted from its own parent.
    pub fn forward_go(&self, child: &mut PendingRole, deadline: u64) -> io::Result<bool> {
        let authorization = self
            .authorization
            .as_ref()
            .ok_or_else(|| invalid("forward GO before acceptance"))?;
        child.send_authorization(authorization, deadline)
    }

    /// Queue this role's identity and installed watch inventory.
    pub fn ready(&mut self, watched: Vec<Participant>) -> io::Result<()> {
        self.ready_subtree(watched, Vec::new())
    }

    /// Report this role and its validated descendant readiness receipts together.
    pub fn ready_subtree(
        &mut self,
        watched: Vec<Participant>,
        mut descendants: Vec<super::startup::Ready>,
    ) -> io::Result<()> {
        if self.announced.is_some() && self.inventory.is_none() {
            return Err(invalid("readiness before inventory"));
        }
        if let Some(inventory) = &self.inventory {
            let required = inventory.watches(identity(std::process::id(), self.role)?)?;
            if required.iter().any(|p| !watched.contains(p)) {
                return Err(invalid("required startup watch missing"));
            }
        }
        if super::boot_nanos()? >= self.readiness_deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let ready = super::startup::Ready {
            run: self.launch.run,
            participant: identity(std::process::id(), self.role)?,
            parent: Some(self.parent),
            watched,
        };
        let local = ready.participant;
        descendants.insert(0, ready);
        if let Some(inventory) = &self.inventory {
            inventory.validate_readiness(local, &descendants)?;
        }
        self.bootstrap
            .as_mut()
            .ok_or_else(|| invalid("bootstrap already closed"))?
            .queue(&descendants)
    }
    /// Advance readiness and accept matching GO under the original deadline.
    pub fn await_go(&mut self, deadline: u64) -> io::Result<bool> {
        let bootstrap = self
            .bootstrap
            .as_mut()
            .ok_or_else(|| invalid("GO already consumed"))?;
        if !bootstrap.flush(deadline)? {
            return Ok(false);
        }
        let Some(run) = bootstrap.receive::<super::startup::Go>(deadline)? else {
            return Ok(false);
        };
        run.validate(
            self.launch.run,
            identity(std::process::id(), self.role)?,
            self.launch.capacity,
        )?;
        self.authorization = Some(run);
        self.bootstrap.take();
        Ok(true)
    }
}
