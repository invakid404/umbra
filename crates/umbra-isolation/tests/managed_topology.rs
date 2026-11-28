//! Ordinary-process qualification of dormant re-entry and parent collection.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        eprintln!("managed topology: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    use umbra_core::managed::{LaunchConfiguration, Role};
    use umbra_isolation::managed::{boot_nanos, executable, topology::Topology};

    if std::env::args().nth(1).as_deref() == Some("--umbra-socket") {
        use umbra_core::provider::{accept, Frame};
        let (mut connection, ()) = accept("handoff-fixture", "platform", |_| {
            Ok(((), [umbra_core::shutdown::CAPABILITY.to_owned()].into()))
        })?;
        let Frame::Request { id, payload } = connection.receive::<Frame>()? else {
            return Err("expected fixture request".into());
        };
        connection.send(&Frame::Response {
            id,
            result: Ok(payload),
        })?;
        // The owner closes its cancellation duplicate after worker RPC succeeds.
        if connection.receive::<Frame>().is_ok() {
            return Err("unexpected second fixture request".into());
        }
        return Ok(());
    }
    if let Some(mut entry) =
        executable::entry().map_err(|e| format!("entry pid {}: {e}", std::process::id()))?
    {
        let evidence = std::path::PathBuf::from(String::from_utf8(entry.launch.payload.clone())?);
        let deadline = entry.readiness_deadline;
        let mut collected = Vec::new();
        if entry.role == Role::Guardian {
            let mut pending_roles = Vec::new();
            for role in [Role::Worker, Role::CleanupOwner] {
                let pending = entry
                    .spawn_child(role, false)
                    .map_err(|e| format!("spawn {role:?}: {e}"))?;
                pending_roles.push(pending);
            }
            for pending in pending_roles {
                collected.push(collect_before_go(pending, deadline)?);
            }
        } else if entry.role == Role::CleanupOwner {
            let endpoint = entry
                .endpoints
                .iter()
                .find(|e| e.peer == Role::Worker)
                .ok_or("owner worker route missing")?;
            collected.push(qualify_handoff(
                entry.launch.run,
                deadline,
                endpoint.stream.try_clone()?,
            )?);
        } else if entry.role == Role::Worker {
            let endpoint = entry
                .endpoints
                .iter()
                .find(|e| e.peer == Role::CleanupOwner)
                .ok_or("worker owner route missing")?;
            receive_handoff(entry.launch.run, deadline, endpoint.stream.try_clone()?)?;
        }
        let expected = match entry.role {
            Role::Guardian => 2,
            Role::Worker => 3,
            Role::CleanupOwner => 5,
            _ => return Err("unexpected role".into()),
        };
        if entry.endpoints.len() != expected {
            return Err("incorrect endpoint assignment".into());
        }
        let record = serde_json::json!({
            "pid": std::process::id(), "parent": entry.parent.pid,
            "collected": collected, "endpoints": expected,
        });
        std::fs::write(
            evidence.join(format!("{:?}.json", entry.role)),
            serde_json::to_vec(&record)?,
        )?;
        entry
            .ready(Vec::new())
            .map_err(|e| format!("ready {:?}: {e}", entry.role))?;
        // This test deliberately ends before GO. Closing the parent's startup
        // channel must release the child without launching session resources.
        loop {
            match entry.await_go(deadline) {
                Ok(false) => std::thread::sleep(std::time::Duration::from_millis(1)),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Ok(true) => return Err("test never authorizes GO".into()),
                Err(error) => return Err(format!("await GO {:?}: {error}", entry.role).into()),
            }
        }
    }

    let evidence = tempfile::tempdir()?;
    let mut launch: LaunchConfiguration = serde_json::from_str(
        r#"{"run":{"nonce":"00000000-0000-0000-0000-000000000001","generation":1},"capacity":64,"dispatch":"topology","payload":[]}"#,
    )?;
    launch.payload = evidence.path().as_os_str().as_encoded_bytes().to_vec();
    let mut topology = Topology::new(false)?;
    let relay_endpoints = topology.take(Role::Relay)?;
    let mut assignments = BTreeMap::new();
    for role in [
        Role::Guardian,
        Role::Worker,
        Role::CleanupOwner,
        Role::Provider,
    ] {
        assignments.insert(role, topology.take(role)?);
    }
    let guardian = executable::spawn_tree(
        Role::Guardian,
        Role::Relay,
        &launch,
        assignments,
        false,
        boot_nanos()? + 2_000_000_000,
    )?;
    let pid = collect_before_go(guardian, boot_nanos()? + 2_000_000_000)?;
    let read = |role| -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_slice(&std::fs::read(
            evidence.path().join(format!("{role}.json")),
        )?)?)
    };
    let guardian = read("Guardian")?;
    let worker = read("Worker")?;
    let owner = read("CleanupOwner")?;
    assert_eq!(guardian["pid"], pid);
    assert_eq!(guardian["parent"], std::process::id());
    assert_eq!(worker["parent"], pid);
    assert_eq!(owner["parent"], pid);
    assert_eq!(
        guardian["collected"],
        serde_json::json!([worker["pid"], owner["pid"]])
    );
    assert_eq!(owner["collected"].as_array().unwrap().len(), 1);
    assert_eq!(relay_endpoints.len(), 2);
    println!("managed topology: parent edges and ordinary-child receipts passed");
    Ok(())
}

#[cfg(target_os = "macos")]
fn collect_before_go(
    mut pending: umbra_isolation::managed::executable::PendingRole,
    deadline: u64,
) -> Result<u32, Box<dyn std::error::Error>> {
    use umbra_isolation::managed::boot_nanos;
    let ready = loop {
        match pending.receive_ready(deadline) {
            Ok(Some(ready)) => break Ok(ready),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Err(error) => break Err(error),
        }
    };
    let mut child = pending.into_child(); // EOF releases the child's before-GO wait.
    let pid = child.pid();
    loop {
        if let Some(status) = child.collect()? {
            ready?;
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                return Err(format!("child {pid} failed: {status}").into());
            }
            return Ok(pid);
        }
        if boot_nanos()? > deadline + 4_000_000_000 {
            return Err(format!("child {pid} exceeded startup collection allocation").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(target_os = "macos")]
fn qualify_handoff(
    run: umbra_core::managed::RunIdentity,
    deadline: u64,
    route: std::os::unix::net::UnixStream,
) -> Result<u32, Box<dyn std::error::Error>> {
    use std::time::Duration;
    use umbra_core::{
        provider::{ProviderDescriptor, PROTOCOL_VERSION},
        BytePath,
    };
    use umbra_isolation::managed::{boot_nanos, owner::Launch, Channel};
    let descriptor = ProviderDescriptor {
        id: "handoff-fixture".into(),
        role: "platform".into(),
        protocol_version: PROTOCOL_VERSION,
        executable: BytePath::new(
            std::env::current_exe()?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        )?,
        capabilities: [umbra_core::shutdown::CAPABILITY.to_owned()].into(),
        options: vec![],
    };
    let mut launch = Launch::default();
    let prepared = launch.connect(&descriptor, deadline);
    let mut child = launch.into_child().ok_or("fixture child not spawned")?;
    let pid = child.id();
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut handoff = prepared?;
        let mut channel = Channel::new(route)?;
        channel.queue(&descriptor)?;
        while !channel.flush(deadline)? {
            std::thread::sleep(Duration::from_millis(1));
        }
        while !handoff.publish(&mut channel, run, deadline)? {
            std::thread::sleep(Duration::from_millis(1));
        }
        loop {
            if let Some(done) = channel.receive::<bool>(deadline)? {
                assert!(done);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            child.try_wait()?.is_none(),
            "owner retains a live provider after handoff"
        );
        handoff
            .into_cancellation()
            .shutdown(std::net::Shutdown::Both)?;
        Ok(())
    })();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if boot_nanos()? >= deadline {
            // Fixture has no acquired native resources. Bound failures and retain its actual wait.
            child.kill()?;
            let status = child.wait()?;
            return Err(
                format!("handoff fixture exceeded startup cutoff: {status}; {result:?}").into(),
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    result?;
    if !status.success() {
        return Err(format!("handoff provider failed: {status}").into());
    }
    Ok(pid)
}

#[cfg(target_os = "macos")]
fn receive_handoff(
    run: umbra_core::managed::RunIdentity,
    deadline: u64,
    route: std::os::unix::net::UnixStream,
) -> Result<(), Box<dyn std::error::Error>> {
    use umbra_core::{managed::Role, provider::ProviderDescriptor};
    use umbra_isolation::managed::{
        process::identity, publication::Route, worker::WorkerCleanup, Channel,
    };
    let mut channel = Channel::new(route)?;
    let descriptor: ProviderDescriptor = loop {
        if let Some(value) = channel.receive(deadline)? {
            break value;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let offer: umbra_isolation::managed::owner::ProviderOffer = loop {
        if let Some(value) = channel.receive(deadline)? {
            break value;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    offer.validate(run)?;
    let (owner, provider, welcome) = (offer.owner, offer.provider, offer.welcome);
    let cleanup = WorkerCleanup::new(
        run,
        identity(std::process::id(), Role::Worker)?,
        owner,
        provider,
        1,
        vec![Route::new(
            owner,
            Channel::new(channel.handoff_stream()?.try_clone()?)?,
        )?],
    )?;
    let mut client = loop {
        if let Some(client) = cleanup.receive_provider(
            channel.handoff_stream()?,
            &welcome,
            &descriptor,
            1000,
            deadline,
        )? {
            break client;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    assert!(!client.owns_provider());
    assert_eq!(
        client.call::<_, String>(&"handoff-roundtrip")?,
        "handoff-roundtrip"
    );
    channel.queue(&true)?;
    while !channel.flush(deadline)? {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(client
        .connection
        .receive::<umbra_core::provider::Frame>()
        .is_err());
    Ok(())
}
