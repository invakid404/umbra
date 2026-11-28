//! The fixture uses production re-entry and provider launch, with ordinary children.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    time::Duration,
};
use umbra_core::managed::{LaunchConfiguration, Role};
use umbra_isolation::managed::{
    boot_nanos,
    executable::{self, Entry, PendingRole},
    owner::Launch,
    topology::{Endpoint, Purpose, Topology},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Children {
    items: Vec<PendingRole>,
    deadline: u64,
}
impl std::ops::Deref for Children {
    type Target = Vec<PendingRole>;
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}
impl std::ops::DerefMut for Children {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.items
    }
}
impl Drop for Children {
    fn drop(&mut self) {
        for child in self.items.drain(..) {
            if let Err(error) = collect(child, self.deadline) {
                eprintln!("fixture parent collection failed: {error}");
            }
        }
    }
}
#[derive(Default)]
struct Provider(Launch);
impl Provider {
    fn collect(&mut self) -> Result<()> {
        if let Some(mut child) = std::mem::take(&mut self.0).into_child() {
            if !child.wait()?.success() {
                return Err("provider parent wait failed".into());
            }
        }
        Ok(())
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        if let Err(error) = self.collect() {
            eprintln!("fixture provider collection failed: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    dev: i32,
    ino: u64,
    mode: u16,
    socket: u64,
}
unsafe extern "C" {
    fn umbra_inventory_socket(
        pid: i32,
        fd: i32,
        identity: *mut u64,
        domain: *mut i32,
        kind: *mut i32,
    ) -> i32;
}
fn socket_details(fd: i32) -> io::Result<(u64, i32, i32)> {
    let (mut id, mut domain, mut kind) = (0, 0, 0);
    if unsafe {
        umbra_inventory_socket(
            std::process::id() as i32,
            fd,
            &mut id,
            &mut domain,
            &mut kind,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((id, domain, kind))
}
fn identity(fd: i32) -> io::Result<Identity> {
    let mut id = stat_identity(fd)?;
    if id.mode & libc::S_IFMT == libc::S_IFSOCK {
        id.socket = socket_details(fd)?.0;
    }
    Ok(id)
}
fn stat_identity(fd: i32) -> io::Result<Identity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // Also used in the fork child: no allocation or destructors.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Identity {
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        socket: 0,
    })
}

#[derive(Serialize, Deserialize)]
struct Original {
    owner: Role,
    peer: Role,
    purpose: Purpose,
    identity: Identity,
    nonce: [u8; 16],
}
#[derive(Serialize, Deserialize)]
struct Input {
    directory: PathBuf,
    originals: Vec<Original>,
    caller_file: Option<Identity>,
}
fn prepare(directory: PathBuf, roles: &BTreeMap<Role, Vec<Endpoint>>) -> Result<Input> {
    let mut originals = Vec::new();
    for (&owner, endpoints) in roles {
        for endpoint in endpoints {
            let nonce = *uuid::Uuid::new_v4().as_bytes();
            (&endpoint.stream).write_all(&nonce)?;
            originals.push(Original {
                owner,
                peer: endpoint.peer,
                purpose: endpoint.purpose,
                identity: identity(endpoint.stream.as_raw_fd())?,
                nonce,
            });
        }
    }
    Ok(Input {
        directory,
        originals,
        caller_file: None,
    })
}

fn challenge(input: &Input, role: Role, endpoints: &[Endpoint], errors: &mut Vec<String>) {
    for endpoint in endpoints {
        let own = input.originals.iter().find(|row| {
            row.owner == role && row.peer == endpoint.peer && row.purpose == endpoint.purpose
        });
        let opposite = input.originals.iter().find(|row| {
            row.owner == endpoint.peer && row.peer == role && row.purpose == endpoint.purpose
        });
        let checked = (|| -> Result<()> {
            let own = own.ok_or("unexpected endpoint assignment")?;
            let opposite = opposite.ok_or("missing opposite endpoint")?;
            if identity(endpoint.stream.as_raw_fd())? != own.identity {
                return Err("assigned endpoint identity differs from original".into());
            }
            endpoint
                .stream
                .set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut bytes = [0; 16];
            (&endpoint.stream).read_exact(&mut bytes)?;
            if bytes != opposite.nonce {
                return Err("endpoint nonce challenge mismatch".into());
            }
            Ok(())
        })();
        if let Err(error) = checked {
            errors.push(error.to_string());
        }
    }
}

fn extra_allowed(fd: i32, id: Identity, extras: &[(i32, Identity)]) -> bool {
    extras.contains(&(fd, id))
}

fn duplicate_extra_socket_requires_its_own_descriptor_allowance() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let intentional = stream.try_clone().unwrap();
    let unexpected = stream.try_clone().unwrap();
    let id = identity(stream.as_raw_fd()).unwrap();
    let extras = [(stream.as_raw_fd(), id), (intentional.as_raw_fd(), id)];
    assert!(extra_allowed(stream.as_raw_fd(), id, &extras));
    assert!(extra_allowed(
        intentional.as_raw_fd(),
        identity(intentional.as_raw_fd()).unwrap(),
        &extras
    ));
    assert!(
        !extra_allowed(
            unexpected.as_raw_fd(),
            identity(unexpected.as_raw_fd()).unwrap(),
            &extras
        ),
        "unlisted third copy of an allowed RPC socket must be rejected"
    );
}

fn inspect(
    input: &Input,
    role: Role,
    endpoints: &[Endpoint],
    extra: &[i32],
    baseline: &[Identity],
    errors: &mut Vec<String>,
) -> Result<()> {
    let expected: Vec<_> = input
        .originals
        .iter()
        .filter(|row| row.owner == role)
        .map(|row| row.identity)
        .collect();
    let extras: Vec<_> = extra
        .iter()
        .map(|fd| identity(*fd).map(|id| (*fd, id)))
        .collect::<io::Result<_>>()?;
    let mut found = Vec::new();
    for fd in 3..unsafe { libc::getdtablesize() } {
        let id = match identity(fd) {
            Ok(id) => id,
            Err(error) if error.raw_os_error() == Some(libc::EBADF) => continue,
            Err(error) => return Err(error.into()),
        };
        if id.mode & libc::S_IFMT != libc::S_IFSOCK {
            continue;
        }
        if expected.contains(&id) {
            found.push(id);
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC == 0 {
                errors.push(format!("{role:?}: managed endpoint {fd} lacks CLOEXEC"));
            }
        } else if !extra_allowed(fd, id, &extras) && !baseline.contains(&id) {
            let details = socket_details(fd);
            errors.push(format!("{role:?}: unexpected socket {fd}: {id:?}, domain/type={details:?}, owning process={}", std::process::id()));
        }
    }
    if found.len() != expected.len() || expected.iter().any(|id| !found.contains(id)) {
        errors.push(format!(
            "{role:?}: exact endpoint identity inventory differs"
        ));
    }
    if endpoints.len() != expected.len() {
        errors.push(format!("{role:?}: role assignment size differs"));
    }
    let mut fork_extras: Vec<_> = extras.iter().map(|(_, id)| *id).collect();
    fork_extras.extend_from_slice(baseline);
    fork_boundary(input, &fork_extras, errors)?;
    std::fs::write(
        input.directory.join(format!("{role:?}.json")),
        serde_json::to_vec(errors)?,
    )?;
    Ok(())
}

fn fork_boundary(input: &Input, extras: &[Identity], errors: &mut Vec<String>) -> Result<()> {
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_read_timeout(Some(Duration::from_secs(2)))?;
    let fd = writer.as_raw_fd();
    let plan = umbra_isolation::spawn::ClosePlan::capture(&[fd])?;
    let limit = unsafe { libc::getdtablesize() };
    let forbidden: Vec<_> = input
        .originals
        .iter()
        .map(|row| row.identity)
        .chain(extras.iter().copied())
        .collect();
    let mut reply = [0u8; 17];
    reply[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let expected = reply;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        // Only preallocated memory and async-signal-safe syscalls until _exit.
        if unsafe { plan.close_in_child() }.is_err() {
            reply[16] = 1;
        }
        for candidate in 3..limit {
            if let Ok(id) = stat_identity(candidate) {
                if candidate != fd
                    || forbidden.iter().any(|item| {
                        item.dev == id.dev
                            && item.ino == id.ino
                            && item.mode == id.mode
                            && item.socket == 0
                    })
                {
                    reply[16] = 2;
                }
            }
        }
        let n = unsafe { libc::write(fd, reply.as_ptr().cast(), reply.len()) };
        unsafe { libc::_exit(if n == reply.len() as isize { 0 } else { 1 }) };
    }
    drop(writer);
    let read = reader.read_exact(&mut reply);
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            break;
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(io::Error::last_os_error().into());
    }
    if read.is_err() || reply != expected || status != 0 {
        errors.push(format!(
            "fork boundary retained an endpoint or lost its nonce: {reply:?}, {status}"
        ));
    }
    Ok(())
}

fn tick(deadline: u64) -> Result<()> {
    if boot_nanos()? >= deadline {
        return Err("fixture readiness deadline expired".into());
    }
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}
fn ready(pending: &mut PendingRole, deadline: u64) -> Result<()> {
    while pending.receive_ready(deadline)?.is_none() {
        tick(deadline)?;
    }
    Ok(())
}
fn collect(pending: PendingRole, deadline: u64) -> Result<()> {
    let mut child = pending.into_child();
    loop {
        if let Some(status) = child.collect()? {
            if status != 0 {
                return Err(format!("parent collected failed child: {status}").into());
            }
            return Ok(());
        }
        tick(deadline + 4_000_000_000)?;
    }
}

fn role(mut entry: Entry) -> Result<()> {
    let input: Input = serde_json::from_slice(&entry.launch.payload)?;
    let mut errors = Vec::new();
    if let Some(caller_file) = input.caller_file {
        for fd in 3..unsafe { libc::getdtablesize() } {
            if identity(fd).ok() == Some(caller_file) {
                errors.push(format!("{:?}: inherited caller file at {fd}", entry.role));
            }
        }
    }
    challenge(&input, entry.role, &entry.endpoints, &mut errors);
    let mut children = Children {
        items: Vec::new(),
        deadline: entry.readiness_deadline,
    };
    match entry.role {
        Role::Relay => children.push(entry.spawn_child(Role::Guardian, false)?),
        Role::Guardian => {
            children.push(entry.spawn_child(Role::Worker, false)?);
            children.push(entry.spawn_child(Role::CleanupOwner, false)?);
        }
        _ => {}
    }
    let mut extras: Vec<_> = entry.probe_bootstrap_fd().into_iter().collect();
    extras.extend(children.iter().map(PendingRole::probe_bootstrap_fd));
    let mut provider = Provider::default();
    let mut provider_startup = None;
    let mut rpc = None;
    let mut connection = None;
    if entry.role == Role::CleanupOwner {
        use umbra_core::provider::{ProviderDescriptor, PROTOCOL_VERSION};
        let descriptor = ProviderDescriptor {
            id: "endpoint-inventory".into(),
            role: "platform".into(),
            protocol_version: PROTOCOL_VERSION,
            executable: umbra_core::BytePath::new(
                std::env::current_exe()?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
            )?,
            options: b"{}".to_vec(),
            capabilities: Default::default(),
        };
        let (handoff, startup) = entry.connect_provider(&mut provider.0, &descriptor, 5_000)?;
        let mut stream = handoff.into_cancellation();
        stream.write_all(entry.launch.run.nonce.as_bytes())?;
        extras.extend([stream.as_raw_fd(), startup.probe_bootstrap_fd()]);
        rpc = Some(stream);
        provider_startup = Some(startup);
        while !input.directory.join("Provider.json").exists() {
            tick(entry.readiness_deadline)?;
        }
    } else if entry.role == Role::Provider {
        let (accepted, ()) =
            umbra_core::provider::accept("endpoint-inventory", "platform", |_| {
                Ok(((), Default::default()))
            })?;
        let mut stream = accepted.observation_stream()?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut nonce = [0; 16];
        stream.read_exact(&mut nonce)?;
        if &nonce != entry.launch.run.nonce.as_bytes() {
            errors.push("provider RPC nonce mismatch".into());
        }
        // Authorize exactly the connection and its intentional observation copy.
        extras.extend([
            stream.as_raw_fd(),
            accepted.observation_descriptor().as_raw_fd(),
        ]);
        rpc = Some(stream);
        connection = Some(accepted);
    }
    for child in children.iter_mut() {
        ready(child, entry.readiness_deadline)?;
    }
    inspect(
        &input,
        entry.role,
        &entry.endpoints,
        &extras,
        &[],
        &mut errors,
    )?;
    entry.ready(Vec::new())?;
    loop {
        match entry.await_go(entry.readiness_deadline) {
            Ok(false) => tick(entry.readiness_deadline)?,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            other => return Err(format!("fixture never sends GO: {other:?}").into()),
        }
    }
    drop(provider_startup);
    drop(rpc);
    drop(connection);
    while let Some(child) = children.pop() {
        collect(child, entry.readiness_deadline)?;
    }
    provider.collect()?;
    Ok(())
}

pub fn run() -> Result<()> {
    duplicate_extra_socket_requires_its_own_descriptor_allowance();
    if let Some(entry) = executable::entry()? {
        return role(entry);
    }
    let directory = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("missing evidence directory")?,
    );
    let library = std::env::args().nth(2).as_deref() == Some("library");
    let root = if library { Role::Caller } else { Role::Relay };
    // Model descriptors supplied by the invoking process without close-on-exec.
    // Keep them alive throughout the topology reading and actual collection.
    let inherited = if std::env::args().nth(3).as_deref() == Some("inherited") {
        let pair = UnixStream::pair()?;
        let file = std::fs::File::create(directory.join("caller-file"))?;
        for fd in [pair.0.as_raw_fd(), pair.1.as_raw_fd(), file.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        Some((pair, file))
    } else {
        None
    };
    let mut baseline = Vec::new();
    for fd in 3..unsafe { libc::getdtablesize() } {
        match identity(fd) {
            Ok(id) => baseline.push(id),
            Err(error) if error.raw_os_error() == Some(libc::EBADF) => {}
            Err(error) => return Err(error.into()),
        }
    }
    std::fs::write(
        directory.join("root-baseline.json"),
        serde_json::to_vec(&baseline)?,
    )?;
    let mut topology = Topology::new(library)?;
    let mut roles = BTreeMap::new();
    for role in [
        Role::Relay,
        Role::Guardian,
        Role::Worker,
        Role::CleanupOwner,
        Role::Provider,
    ] {
        roles.insert(role, topology.take(role)?);
    }
    if library {
        roles.insert(Role::Caller, topology.take(Role::Caller)?);
    }
    let mut input = prepare(directory, &roles)?;
    input.caller_file = inherited
        .as_ref()
        .map(|(_, file)| identity(file.as_raw_fd()))
        .transpose()?;
    let launch = LaunchConfiguration {
        run: umbra_core::managed::RunIdentity {
            nonce: uuid::Uuid::new_v4(),
            generation: 1,
        },
        capacity: 64,
        dispatch: "endpoint-inventory".into(),
        payload: serde_json::to_vec(&input)?,
    };
    let deadline =
        umbra_isolation::managed::startup::Startup::new(launch.run, library)?.readiness_deadline();
    let endpoints = roles.remove(&root).unwrap();
    let mut errors = Vec::new();
    challenge(&input, root, &endpoints, &mut errors);
    let mut child = executable::spawn_tree(
        if library { Role::Relay } else { Role::Guardian },
        root,
        &launch,
        roles,
        false,
        deadline,
    )?;
    let ready_result = ready(&mut child, deadline);
    let inspection = if ready_result.is_ok() {
        inspect(
            &input,
            root,
            &endpoints,
            &[child.probe_bootstrap_fd()],
            &baseline,
            &mut errors,
        )
    } else {
        Ok(())
    };
    let collected = collect(child, deadline);
    ready_result?;
    inspection?;
    collected?;
    for role in [
        Role::Relay,
        Role::Guardian,
        Role::Worker,
        Role::CleanupOwner,
        Role::Provider,
    ] {
        let found: Vec<String> = serde_json::from_slice(&std::fs::read(
            input.directory.join(format!("{role:?}.json")),
        )?)?;
        errors.extend(found);
    }
    if !errors.is_empty() {
        return Err(format!("exact endpoint inventory failed: {errors:?}").into());
    }
    println!(
        "endpoint inventory: {root:?}, exact identities/nonces/CLOEXEC/fork exclusion; all direct parents waited"
    );
    Ok(())
}
