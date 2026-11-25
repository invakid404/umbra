//! Live pre-GO inventory, watch installation and rollback through actual parents.
#[cfg(not(target_os = "macos"))]
fn main() {}
#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        eprintln!("managed startup: {error}");
        std::process::exit(1);
    }
}
#[cfg(target_os = "macos")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        collections::BTreeMap,
        process::{Command, Stdio},
        time::Duration,
    };
    use umbra_core::managed::{LaunchConfiguration, Role};
    use umbra_isolation::managed::{
        boot_nanos, executable,
        inventory::{Inventory, Member},
        process::{identity, ProcessWatch},
        topology::Topology,
    };
    let tick = || std::thread::sleep(Duration::from_millis(1));
    if let Some(mut entry) = executable::entry()? {
        let deadline = entry.readiness_deadline;
        let mut children = Vec::new();
        let mut members = Vec::new();
        let mut provider = None;
        let mut cancellation = None;
        if entry.role == Role::Guardian {
            for role in [Role::Worker, Role::CleanupOwner] {
                children.push(entry.spawn_child(role, false)?);
            }
        } else if entry.role == Role::CleanupOwner {
            let input = if entry.launch.payload.is_empty() {
                Stdio::piped()
            } else {
                let (control, input) = std::os::unix::net::UnixStream::pair()?;
                cancellation = Some(control);
                Stdio::from(std::os::fd::OwnedFd::from(input))
            };
            let child = Command::new("/bin/cat")
                .stdin(input)
                .stdout(Stdio::null())
                .spawn()?;
            members.push(Member {
                participant: identity(child.id(), Role::Provider)?,
                parent: Some(identity(std::process::id(), Role::CleanupOwner)?),
            });
            provider = Some(child);
        }
        let local = identity(std::process::id(), entry.role)?;
        let mut receipts = Vec::new();
        if let Some(child) = &provider {
            receipts.push(umbra_isolation::managed::startup::Ready {
                run: entry.launch.run,
                participant: identity(child.id(), Role::Provider)?,
                parent: Some(local),
                watched: Vec::new(),
            });
        }
        let mut bootstrap = umbra_isolation::managed::bootstrap::Bootstrap::new(members, receipts)?;
        while !bootstrap.poll(&mut entry, &mut children, false)? {
            tick();
        }
        let mut _watch = bootstrap.into_watch()?;
        assert!(
            entry.worker_boundary().is_err(),
            "no fail-stop authority before GO"
        );
        let watched = entry.inventory().unwrap().watches(local)?;
        let mut roles: Vec<_> = watched.iter().map(|peer| peer.role).collect();
        roles.sort();
        let mut expected = match entry.role {
            Role::Guardian => vec![Role::Worker, Role::CleanupOwner, Role::Relay],
            Role::CleanupOwner => vec![Role::Provider, Role::Worker, Role::Guardian, Role::Relay],
            Role::Worker => Vec::new(),
            _ => return Err("unexpected fixture role".into()),
        };
        expected.sort();
        assert_eq!(roles, expected);
        assert_eq!(_watch.is_some(), entry.role != Role::Worker);
        if let Some(watch) = &_watch {
            assert!(watch.covers(&watched));
        }
        let mut owner_runtime = None;
        let mut parent_runtime = if !entry.launch.payload.is_empty() && entry.role == Role::Guardian
        {
            Some(umbra_isolation::managed::guard::Guard::from_entry(
                &mut entry,
                _watch.take().unwrap(),
            )?)
        } else {
            None
        };
        let mut worker_routes = None;
        if let Some(control) = cancellation.take() {
            let mut owner = match umbra_isolation::managed::owner::Owner::new(
                local,
                entry.launch.run,
                entry.launch.capacity,
                provider.take().unwrap(),
                control,
                Vec::new(),
            ) {
                Ok(owner) => owner,
                Err((error, mut child, control, _)) => {
                    drop(control);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            };
            owner.install_watch(_watch.take().unwrap(), entry.inventory().unwrap())?;
            assert!(owner.progress()?.is_none(), "provider exited before GO");
            owner_runtime = Some(owner);
        }
        if entry.role == Role::Worker {
            // Flush readiness before waiting for the relay's complete inventory.
            assert!(!entry.await_go(deadline)?);
            let index = entry
                .endpoints
                .iter()
                .position(|e| e.peer == Role::Relay)
                .unwrap();
            let endpoint = entry.endpoints.remove(index);
            let mut arming = umbra_isolation::managed::arming::WorkerArming::new(
                umbra_isolation::managed::Channel::new(endpoint.stream)?,
                entry.launch.run,
                entry.launch.capacity,
                false,
                deadline,
            )?;
            let mut attempted = false;
            loop {
                let result = arming.poll(|record, cutoff| {
                    assert!(matches!(
                        record.state,
                        umbra_core::CustodyPhase::Armed { .. }
                    ));
                    assert!(cutoff > boot_nanos()?);
                    attempted = true;
                    if entry.launch.dispatch == "inventory-go" {
                        return persist_and_replay(record);
                    }
                    Err(std::io::Error::other(
                        "injected worker persistence ambiguity",
                    ))
                });
                match result {
                    Ok(false) => tick(),
                    Ok(true) if entry.launch.dispatch == "inventory-go" => break,
                    Ok(true) => return Err("failed persistence emitted GO".into()),
                    Err(error) if entry.launch.dispatch == "inventory-go" => {
                        return Err(error.into())
                    }
                    Err(_) => break,
                }
            }
            assert!(
                attempted,
                "complete readiness must reach the worker callback"
            );
            if entry.launch.dispatch != "inventory-go" {
                assert!(arming
                    .poll(|_, _| panic!("persistence retried after failure"))
                    .is_err());
            }
            if entry.launch.dispatch == "inventory-go" {
                let mut channels =
                    umbra_isolation::managed::channels::RuntimeChannels::from_entry(&mut entry)?;
                channels.insert(Role::Relay, arming.into_channel())?;
                worker_routes = Some(channels.finish()?);
            } else {
                drop(arming.into_channel());
            }
        }
        loop {
            match entry.await_go(deadline + 5_000_000_000) {
                Ok(false) => tick(),
                Err(e)
                    if e.kind() == std::io::ErrorKind::UnexpectedEof
                        && entry.launch.dispatch != "inventory-go" =>
                {
                    break
                }
                Ok(true) if entry.launch.dispatch == "inventory-go" => {
                    assert_eq!(entry.worker_boundary().is_ok(), entry.role == Role::Worker);
                    for child in &mut children {
                        while !entry.forward_go(child, deadline + 5_000_000_000)? {
                            tick();
                        }
                    }
                    break;
                }
                Ok(true) => return Err("fixture never authorizes GO".into()),
                Err(e) => return Err(e.into()),
            }
        }
        if !entry.launch.payload.is_empty() && entry.role == Role::Worker {
            struct Marker(std::path::PathBuf);
            impl Drop for Marker {
                fn drop(&mut self) {
                    std::fs::write(&self.0, b"unwound").unwrap();
                }
            }
            let _marker = Marker(String::from_utf8(entry.launch.payload.clone())?.into());
            let inventory = entry.inventory().unwrap().clone();
            let participant = |role| {
                inventory
                    .members
                    .iter()
                    .find(|m| m.participant.role == role)
                    .unwrap()
                    .participant
            };
            let mut routes = worker_routes.take().unwrap();
            for route in &mut routes {
                route
                    .channel()
                    .handoff_stream()?
                    .shutdown(std::net::Shutdown::Both)?;
            }
            let mut cleanup = umbra_isolation::managed::worker::WorkerCleanup::new(
                entry.launch.run,
                local,
                participant(Role::CleanupOwner),
                participant(Role::Provider),
                entry.launch.capacity,
                routes,
            )?;
            cleanup.install_inventory(&inventory, false)?;
            let failure = umbra_core::UmbraError::new(
                umbra_core::ErrorKind::ProtocolMismatch,
                "fixture",
                "injected total publication loss",
            );
            entry.worker_boundary()?.fail_stop(&mut cleanup, &failure)?;
        }
        if let Some(mut owner) = owner_runtime {
            let result = (|| -> std::io::Result<()> {
                loop {
                    if let Some(status) = owner.progress()? {
                        assert!(status.success());
                        assert!(owner.failed(), "worker loss must remain a failed run");
                        assert!(owner
                            .failures()
                            .iter()
                            .any(|reason| reason.contains("Worker exit")));
                        return Ok(());
                    }
                    if boot_nanos()? >= deadline + 5_000_000_000 {
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                    tick();
                }
            })();
            let mut child = owner.into_child();
            if result.is_err() {
                let _ = child.kill();
            }
            child.wait()?;
            result?;
        }
        if let Some(mut guard) = parent_runtime.take() {
            guard.adopt_ready_children(&mut children)?;
            let outcome = guard.run_parent();
            assert!(
                outcome.failed,
                "worker fail-stop cannot produce parent success"
            );
            assert!(outcome.unresolved.is_empty());
            let mut collected = 0;
            for event in outcome.events {
                if let umbra_isolation::managed::guard::Event::Collected(peer, status) = event {
                    assert!(libc::WIFEXITED(status));
                    assert_eq!(
                        libc::WEXITSTATUS(status),
                        i32::from(peer.role == Role::Worker)
                    );
                    collected += 1;
                }
            }
            assert_eq!(collected, 2);
        }
        if entry.launch.dispatch != "inventory-go" {
            rollback(std::mem::take(&mut children), deadline + 4_000_000_000)?;
        }
        for child in children {
            let worker = entry.inventory().unwrap().members.iter().any(|member| {
                member.participant.role == Role::Worker
                    && member.participant.pid == child.child.pid()
            });
            collect_status(
                child,
                deadline + 4_000_000_000,
                i32::from(worker && !entry.launch.payload.is_empty()),
            )?;
        }
        if let Some(mut child) = provider {
            drop(child.stdin.take());
            loop {
                if let Some(status) = child.try_wait()? {
                    assert!(status.success());
                    break;
                }
                if boot_nanos()? >= deadline + 4_000_000_000 {
                    return Err("provider rollback expired".into());
                }
                tick();
            }
        }
        return Ok(());
    }
    let mut launch: LaunchConfiguration = serde_json::from_str(
        r#"{"run":{"nonce":"00000000-0000-0000-0000-000000000001","generation":1},"capacity":64,"dispatch":"inventory","payload":[]}"#,
    )?;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("worker-destructor");
    if std::env::var_os("UMBRA_MANAGED_FAIL_STOP").is_some() {
        launch.dispatch = "inventory-go".into();
        launch.payload = marker.to_str().unwrap().as_bytes().to_vec();
    } else if std::env::var_os("UMBRA_MANAGED_STARTUP_SUCCESS").is_some() {
        launch.dispatch = "inventory-go".into();
    }
    let mut startup = umbra_isolation::managed::startup::Startup::new(launch.run, false)?;
    let deadline = startup.readiness_deadline();
    let mut topology = Topology::new(false)?;
    let mut relay_endpoints = topology.take(Role::Relay)?;
    let worker_index = relay_endpoints
        .iter()
        .position(|e| e.peer == Role::Worker)
        .unwrap();
    let worker_channel =
        umbra_isolation::managed::Channel::new(relay_endpoints.remove(worker_index).stream)?;
    let mut assignments = BTreeMap::new();
    for role in [
        Role::Guardian,
        Role::Worker,
        Role::CleanupOwner,
        Role::Provider,
    ] {
        assignments.insert(role, topology.take(role)?);
    }
    let mut guardian = executable::spawn_tree(
        Role::Guardian,
        Role::Relay,
        &launch,
        assignments,
        false,
        deadline,
    )?;
    let mut members = vec![Member {
        participant: identity(std::process::id(), Role::Relay)?,
        parent: None,
    }];
    loop {
        if let Some(found) = guardian.receive_members()? {
            members.extend(found);
            break;
        }
        tick();
    }
    let inventory = Inventory {
        run: launch.run,
        external_caller: false,
        members,
    };
    inventory.validate(launch.run, false)?;
    let mut watch = ProcessWatch::new()?;
    for peer in inventory.watches(identity(std::process::id(), Role::Relay)?)? {
        watch.install(peer)?;
    }
    while !guardian.send_inventory(&inventory, false)? {
        tick();
    }
    let mut receipts = loop {
        if let Some(receipts) = guardian.receive_readiness(deadline)? {
            break receipts;
        }
        tick();
    };
    assert_eq!(receipts.len(), 4);
    let local = identity(std::process::id(), Role::Relay)?;
    receipts.push(umbra_isolation::managed::startup::Ready {
        run: launch.run,
        participant: local,
        parent: None,
        watched: inventory.watches(local)?,
    });
    for receipt in &receipts {
        startup.accept(receipt.clone())?;
    }
    assert!(startup.complete());
    assert!(!startup.go());
    let mut members = inventory.members.clone();
    members.sort_by_key(|member| member.participant.role);
    let record = umbra_core::CustodyRecord {
        run_id: umbra_core::RunId(launch.run.nonce),
        generation: launch.run.nonce,
        sequence: 1,
        state: umbra_core::CustodyPhase::Armed {
            capacity: launch.capacity,
            startup_nonce: launch.run.nonce,
            participants: members
                .iter()
                .map(|member| umbra_core::CustodyParticipant {
                    pid: member.participant.pid,
                    birth: member.participant.birth.0 * 1_000_000 + member.participant.birth.1,
                })
                .collect(),
        },
    };
    let mut attempted = false;
    let mut relay_channels =
        umbra_isolation::managed::channels::RuntimeChannels::new(local, inventory.clone())?;
    for endpoint in relay_endpoints {
        relay_channels.insert(
            endpoint.peer,
            umbra_isolation::managed::Channel::new(endpoint.stream)?,
        )?;
    }
    let mut relay_runtime = umbra_isolation::managed::guard::Guard::from_startup(
        local,
        &inventory,
        launch.capacity,
        watch,
    )?;
    let mut arming = umbra_isolation::managed::arming::RelayArming::new(
        worker_channel,
        umbra_isolation::managed::arming::Request {
            run: launch.run,
            readiness: receipts,
            record: record.clone(),
        },
        deadline,
        false,
    )?;
    loop {
        match arming.poll() {
            Ok(None) => tick(),
            Ok(Some(go)) if launch.dispatch == "inventory-go" => {
                go.validate(launch.run, local, launch.capacity)?;
                assert_eq!(go.record, record);
                while !guardian.send_authorization(&go, go.deadline)? {
                    tick();
                }
                if launch.payload.is_empty() {
                    collect(guardian, go.deadline + 4_000_000_000)?;
                } else {
                    relay_channels.insert(Role::Worker, arming.into_channel())?;
                    for route in relay_channels.finish()? {
                        relay_runtime.route(route.peer(), route);
                    }
                    relay_runtime.adopt_ready_children(&mut vec![guardian])?;
                    let outcome = relay_runtime.run_parent();
                    assert!(outcome.failed);
                    assert!(outcome.unresolved.is_empty());
                    assert!(outcome.events.iter().any(|event| matches!(
                        event,
                        umbra_isolation::managed::guard::Event::Collected(peer, status)
                            if peer.role == Role::Guardian && libc::WIFEXITED(*status)
                                && libc::WEXITSTATUS(*status) == 0
                    )));
                }
                assert!(!marker.exists(), "worker fail-stop ran a destructor");
                println!(
                    "managed startup: durable worker journal replay and GO propagation passed"
                );
                return Ok(());
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof
                    && launch.dispatch != "inventory-go" =>
            {
                break
            }
            Ok(Some(_)) => return Err("worker authorized GO after persistence failure".into()),
            Err(error) => return Err(error.into()),
        }
    }
    let result = startup.persist_armed(launch.run, record.clone(), |persisted, cutoff| {
        assert_eq!(persisted, &record);
        assert!(cutoff > boot_nanos()?);
        attempted = true;
        Err(std::io::Error::other(
            "injected ambiguous persistence error",
        ))
    });
    assert!(attempted, "complete readiness reaches persistence");
    assert!(result.is_err());
    assert!(!startup.go());
    assert!(startup.authorization(launch.run).is_err());
    assert!(
        startup
            .commit_armed(&record, umbra_core::Sequence(9))
            .is_err(),
        "late receipt cannot clear failed startup"
    );
    rollback(vec![guardian], deadline + 4_000_000_000)?;
    println!(
        "managed startup: complete inventory, required watches and recursive parent waits passed"
    );
    Ok(())
}
#[cfg(target_os = "macos")]
fn persist_and_replay(record: &umbra_core::CustodyRecord) -> std::io::Result<umbra_core::Sequence> {
    use std::os::unix::ffi::OsStrExt;
    use umbra_core::*;
    use umbra_journal::Journal;
    let storage = tempfile::tempdir()?;
    let mut request = JournalOpenRequest {
        control: JournalControlBinding {
            run_id: record.run_id,
            directory: PhysicalPath(
                BytePath::new(storage.path().as_os_str().as_bytes().to_vec())
                    .map_err(std::io::Error::other)?,
            ),
        },
        access: JournalAccess::Writer(JournalWriterAuthority {
            run_id: record.run_id,
            writer_instance_id: "managed-startup-worker".into(),
            host_fingerprint: "test".into(),
            supervisor_version: "test".into(),
            acquired_at: 0,
            renewed_at: 0,
            lease_epoch: LeaseEpoch(1),
            fencing: JournalFencingEvidence::Fenced {
                mechanism: "private-test-directory".into(),
                evidence: vec![],
            },
        }),
        format: JournalFormatPolicy {
            readable_versions: vec![1],
            write_version: 1,
        },
    };
    let mut journal = umbra_journal_file::FileJournal::new();
    journal.open(&request).map_err(std::io::Error::other)?;
    let sequence = journal
        .append(&JournalRecord {
            format_version: 1,
            sequence: Sequence(0),
            operation_id: OperationId(record.generation),
            writer_epoch: LeaseEpoch(1),
            payload: JournalPayload::Custody(record.clone()),
        })
        .map_err(std::io::Error::other)?;
    journal.flush(sequence).map_err(std::io::Error::other)?;
    journal.close().map_err(std::io::Error::other)?;
    request.access = JournalAccess::ReadOnly;
    let mut journal = umbra_journal_file::FileJournal::new();
    let recovered = journal.open(&request).map_err(std::io::Error::other)?;
    assert_eq!(
        recovered.custody.as_ref().map(|state| &state.record),
        Some(record)
    );
    journal.close().map_err(std::io::Error::other)?;
    Ok(sequence)
}
#[cfg(target_os = "macos")]
fn rollback(
    children: Vec<umbra_isolation::managed::executable::PendingRole>,
    deadline: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rollback = match umbra_isolation::managed::rollback::Rollback::begin(children, deadline)
    {
        Ok(rollback) => rollback,
        Err((error, children)) => {
            for child in children {
                collect(child, deadline)?;
            }
            return Err(error.into());
        }
    };
    while !rollback.poll()? {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    for status in rollback.statuses() {
        let status = status.ok_or("rollback lacks parent wait receipt")?;
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }
    assert!(rollback.into_unresolved().is_empty());
    Ok(())
}

#[cfg(target_os = "macos")]
fn collect(
    pending: umbra_isolation::managed::executable::PendingRole,
    deadline: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    collect_status(pending, deadline, 0)
}
#[cfg(target_os = "macos")]
fn collect_status(
    pending: umbra_isolation::managed::executable::PendingRole,
    deadline: u64,
    expected: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = pending.into_child();
    loop {
        if let Some(status) = child.collect()? {
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != expected {
                return Err(format!("helper failed: {status}").into());
            }
            return Ok(());
        }
        if umbra_isolation::managed::boot_nanos()? >= deadline {
            return Err("helper rollback expired".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
