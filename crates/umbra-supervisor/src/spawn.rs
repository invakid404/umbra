//! Validation of kernel-only plans against a frozen logical descriptor table.
use std::collections::BTreeMap;
use umbra_core::{
    ErrorKind, ProcessContext, Result, SpawnAction, SpawnAuthorization, SpawnPlan, SpawnToken,
    TaskId, UmbraError,
};

#[derive(Default)]
pub(crate) struct PendingSpawns {
    active: BTreeMap<TaskId, (SpawnAuthorization, SpawnPlan)>,
    last: BTreeMap<TaskId, u64>,
    epoch: u64,
}

fn invalid() -> UmbraError {
    UmbraError::new(
        ErrorKind::InvalidState,
        "supervisor.spawn",
        "stale or mismatched spawn token",
    )
}

impl PendingSpawns {
    pub(crate) fn prepare(
        &mut self,
        token: SpawnToken,
        plan: SpawnPlan,
        context: &ProcessContext,
    ) -> Result<SpawnAuthorization> {
        if token.parent != context.task
            || token.exec_generation != context.exec_generation
            || self.active.contains_key(&token.parent)
            || self
                .last
                .get(&token.parent)
                .is_some_and(|last| token.sequence <= *last)
        {
            return Err(invalid());
        }
        validate(&plan, context)?;
        self.epoch = self.epoch.checked_add(1).ok_or_else(invalid)?;
        let authorization = SpawnAuthorization {
            token,
            digest: plan.digest,
            table_generation: self.epoch,
        };
        self.last.insert(token.parent, token.sequence);
        self.active.insert(token.parent, (authorization, plan));
        Ok(authorization)
    }

    pub(crate) fn plan(&self, authorization: SpawnAuthorization) -> Result<&SpawnPlan> {
        let (stored, plan) = self
            .active
            .get(&authorization.token.parent)
            .ok_or_else(invalid)?;
        if *stored != authorization {
            return Err(invalid());
        }
        Ok(plan)
    }

    pub(crate) fn consume(&mut self, authorization: SpawnAuthorization) -> Result<SpawnPlan> {
        self.plan(authorization)?;
        Ok(self.active.remove(&authorization.token.parent).unwrap().1)
    }
}

pub(crate) fn validate(plan: &SpawnPlan, context: &ProcessContext) -> Result<()> {
    validate_references(plan, |number| {
        number >= 0 && context.fds.contains_key(&umbra_core::TracedFd(number))
    })
}

fn validate_references(plan: &SpawnPlan, mut virtual_fd: impl FnMut(i32) -> bool) -> Result<()> {
    if plan.actions.len() > 64 {
        return Err(UmbraError::new(
            ErrorKind::InvalidState,
            "supervisor.spawn",
            "spawn malformed packed descriptor",
        ));
    }
    // No admitted action creates a logical descriptor. Checking every operand
    // against the frozen table therefore also covers all earlier action effects:
    // an action cannot close a virtual slot and disguise a later collision.
    for action in &plan.actions {
        if let Some(diagnostic) = action.unsupported_diagnostic() {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "supervisor.spawn",
                diagnostic,
            ));
        }
        let collides = match *action {
            SpawnAction::Close(fd) | SpawnAction::Inherit(fd) => virtual_fd(fd),
            SpawnAction::Dup2(source, destination) => virtual_fd(source) || virtual_fd(destination),
            SpawnAction::Unsupported(_) => unreachable!("refused above"),
        };
        if collides {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "supervisor.spawn",
                "spawn virtual descriptor action",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_action_refusals_follow_caller_order() {
        for (actions, diagnostic) in [
            (
                vec![SpawnAction::Dup2(4096, 100), SpawnAction::Unsupported(5)],
                "spawn virtual descriptor action",
            ),
            (
                vec![SpawnAction::Unsupported(5), SpawnAction::Dup2(4096, 100)],
                "spawn unsupported action CHDIR",
            ),
        ] {
            let plan = SpawnPlan {
                digest: [0; 32],
                actions,
                close_on_exec_default: false,
            };
            assert!(validate_references(&plan, |fd| fd == 4096)
                .unwrap_err()
                .to_string()
                .contains(diagnostic));
        }
    }

    #[test]
    fn authorization_is_single_use_thread_bound_and_cleared_after_error() {
        use umbra_core::{Architecture, BytePath, TaskIdentity, ThreadId};
        let task = TaskId(TaskIdentity {
            native_id: 11,
            generation: 3,
        });
        let thread = ThreadId(TaskIdentity {
            native_id: 12,
            generation: 3,
        });
        let context = ProcessContext {
            task,
            parent: None,
            architecture: Architecture::Aarch64,
            abi: "test".into(),
            exec_generation: 7,
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
            root: BytePath::new(b"/".to_vec()).unwrap(),
            fds: BTreeMap::new(),
        };
        let mut token = SpawnToken {
            run_generation: 1,
            parent: task,
            exec_generation: 7,
            thread,
            sequence: 1,
        };
        let plan = SpawnPlan {
            digest: [42; 32],
            actions: vec![SpawnAction::Close(-1)],
            close_on_exec_default: false,
        };
        let mut pending = PendingSpawns::default();
        let authorization = pending.prepare(token, plan.clone(), &context).unwrap();
        assert!(pending.prepare(token, plan.clone(), &context).is_err());
        for field in 0..6 {
            let mut wrong = authorization;
            match field {
                0 => wrong.token.thread.0.native_id += 1,
                1 => wrong.token.exec_generation += 1,
                2 => wrong.token.sequence += 1,
                3 => wrong.digest[0] ^= 1,
                4 => wrong.table_generation += 1,
                5 => wrong.token.run_generation += 1,
                _ => unreachable!(),
            }
            assert!(pending.consume(wrong).is_err());
            assert_eq!(pending.plan(authorization).unwrap(), &plan);
        }
        assert_eq!(pending.consume(authorization).unwrap(), plan);
        assert!(pending.consume(authorization).is_err());
        assert!(pending.prepare(token, plan.clone(), &context).is_err());
        token.sequence += 1;
        let next = pending.prepare(token, plan.clone(), &context).unwrap();
        assert_ne!(next.table_generation, authorization.table_generation);
        assert!(pending.plan(authorization).is_err());
        assert_eq!(pending.consume(next).unwrap(), plan);
        token.sequence += 1;
        token.exec_generation += 1;
        assert!(pending.prepare(token, plan, &context).is_err());
    }

    #[test]
    fn concurrent_parents_complete_out_of_order_without_crossing_policies() {
        use umbra_core::{Architecture, BytePath, TaskIdentity, ThreadId};
        let mut pending = PendingSpawns::default();
        let mut authorizations = Vec::new();
        for parent in 1..=16 {
            let task = TaskId(TaskIdentity {
                native_id: parent,
                generation: 1,
            });
            let context = ProcessContext {
                task,
                parent: None,
                architecture: Architecture::Aarch64,
                abi: "test".into(),
                exec_generation: 0,
                cwd: BytePath::new(b"/".to_vec()).unwrap(),
                root: BytePath::new(b"/".to_vec()).unwrap(),
                fds: BTreeMap::new(),
            };
            let token = SpawnToken {
                run_generation: 1,
                parent: task,
                exec_generation: 0,
                thread: ThreadId(TaskIdentity {
                    native_id: parent + 100,
                    generation: 1,
                }),
                sequence: 1,
            };
            let plan = SpawnPlan {
                digest: [parent as u8; 32],
                actions: vec![SpawnAction::Close(parent as i32)],
                close_on_exec_default: parent % 2 == 0,
            };
            let auth = pending.prepare(token, plan.clone(), &context).unwrap();
            let mut competing = token;
            competing.thread.0.native_id += 1000;
            competing.sequence += 1;
            assert!(pending.prepare(competing, plan.clone(), &context).is_err());
            assert_eq!(pending.plan(auth).unwrap(), &plan);
            authorizations.push((auth, plan));
        }
        let mut crossed = authorizations[0].0;
        crossed.token.parent = authorizations[1].0.token.parent;
        assert!(pending.consume(crossed).is_err());
        for (auth, plan) in authorizations.into_iter().rev() {
            assert_eq!(pending.consume(auth).unwrap(), plan);
            assert!(pending.consume(auth).is_err());
        }
        assert!(pending.active.is_empty());
    }

    #[test]
    fn every_source_destination_and_earlier_close_is_checked() {
        for actions in [
            vec![SpawnAction::Close(4096)],
            vec![SpawnAction::Inherit(4096)],
            vec![SpawnAction::Dup2(4096, 1)],
            vec![SpawnAction::Dup2(1, 4096)],
            vec![SpawnAction::Close(4096), SpawnAction::Dup2(1, 4096)],
            vec![SpawnAction::Dup2(4, 1), SpawnAction::Inherit(4096)],
        ] {
            let plan = SpawnPlan {
                digest: [0; 32],
                actions,
                close_on_exec_default: false,
            };
            assert!(validate_references(&plan, |fd| fd == 4096)
                .unwrap_err()
                .to_string()
                .contains("spawn virtual descriptor action"));
        }
        let plan = SpawnPlan {
            digest: [0; 32],
            actions: vec![
                SpawnAction::Close(3),
                SpawnAction::Dup2(-1, 3),
                SpawnAction::Inherit(3),
            ],
            close_on_exec_default: true,
        };
        validate_references(&plan, |fd| fd == 4096).unwrap();
    }
}

#[cfg(all(feature = "mutation-probe-spawn-lifecycle", unix))]
pub(crate) fn before_ready(child: TaskId) -> Result<()> {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    if std::env::var("UMBRA_FIX_243_PROBE_PHASE").as_deref() != Ok("policy-ready") {
        return Ok(());
    }
    let socket = std::env::var_os("UMBRA_FIX_243_PROBE_SOCKET").ok_or_else(invalid)?;
    let mut stream = UnixStream::connect(socket).map_err(|_| invalid())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|_| invalid())?;
    writeln!(stream, "policy-ready {}", child.0.native_id).map_err(|_| invalid())?;
    let mut command = [0];
    stream.read_exact(&mut command).map_err(|_| invalid())?;
    if command[0] != b'E' {
        return Err(invalid());
    }
    if matches!(
        std::env::var("UMBRA_FIX_243_PROBE_TRIGGER").as_deref(),
        Ok("deadline" | "session-drop" | "backend-drop" | "terminate")
    ) {
        // Exercise the provider at Ready receipt, before caller delivery.
        return Ok(());
    }
    if std::env::var("UMBRA_FIX_243_PROBE_TRIGGER").as_deref() == Ok("watchdog") {
        // The fixture configures the provider's real watchdog to ten seconds.
        // Keep policy Ready withheld until that registered owner has expired.
        std::thread::sleep(Duration::from_secs(11));
    }
    Err(UmbraError::new(
        ErrorKind::InvalidState,
        "spawn probe",
        "injected policy-before-Ready failure",
    ))
}
