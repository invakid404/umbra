//! External parent measurement; stdout/stderr are retained, never identity inputs.
use super::{events::Events, manifest::Manifest, observation, schedule::Member};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn measure(
    manifest: &Manifest,
    directory: &Path,
    args: &[OsString],
    workload: &str,
    own_session: bool,
    member: Member,
) -> Result<Value, String> {
    let result = measure_inner(manifest, directory, args, workload, own_session, member);
    // Preserve the terminal error beside all raw files even if the scheduler
    // stops before the second member of the pair.
    if directory.exists() {
        let record = json!({"sample": result.as_ref().ok(), "error": result.as_ref().err()});
        std::fs::write(
            directory.join("result.json"),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .map_err(|e| e.to_string())?;
    }
    result
}

fn measure_inner(
    manifest: &Manifest,
    directory: &Path,
    args: &[OsString],
    workload: &str,
    own_session: bool,
    member: Member,
) -> Result<Value, String> {
    std::fs::create_dir(directory).map_err(|e| e.to_string())?;
    let events = directory.join("events");
    std::fs::create_dir(&events).map_err(|e| e.to_string())?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let mut command = Command::new(
        manifest
            .artifacts
            .get(if workload == "admission_only" {
                "admission_driver"
            } else {
                "cli"
            })
            .ok_or("missing invocation artifact")?,
    );
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(directory.join("stdout")).map_err(|e| e.to_string())?)
        .stderr(std::fs::File::create(directory.join("stderr")).map_err(|e| e.to_string())?)
        .env("UMBRA_F299_PERF_EVENTS", &events)
        .env("UMBRA_F299_PERF_MANIFEST_DIGEST", &manifest.digest)
        .env("UMBRA_F299_PERF_RUN_NONCE", &nonce)
        .env("UMBRA_F299_PERF_PAIR_INDEX", member.index.to_string())
        .env("UMBRA_F299_PERF_ORDER", member.order.to_string())
        .env("UMBRA_F299_PERF_WORKLOAD", workload)
        .env("UMBRA_F299_PERF_ROLE", "platform");
    if own_session {
        // SAFETY: setsid and getsid are the only operations in this child hook;
        // no allocation, locking, framework call or destructor is introduced.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getsid(0) == 1 {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
        }
    } else if unsafe { libc::getsid(0) } != 1 {
        return Err("SID-1 prerequisite absent".into());
    }
    let invocation_start = observation::boot_nanos();
    let mut child =
        super::child::OwnedChild::new(command.spawn().map_err(|e| e.to_string())?, member.deadline);
    let pid = child.id();
    let birth = observation::birth(pid);
    let workload_deadline = Instant::now() + Duration::from_secs(60);
    let mut cancelled = false;
    let mut forced = false;
    let cleanup_deadline = workload_deadline + Duration::from_millis(542_600);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        let now = Instant::now();
        if now >= workload_deadline && !cancelled {
            cancelled = true;
            // The direct child has not been collected, so its PID cannot be reused.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        if (now >= cleanup_deadline || now >= member.deadline - Duration::from_secs(1)) && !forced {
            forced = true;
            child.kill().map_err(|e| e.to_string())?;
        }
        if now >= member.deadline {
            return Err("pair deadline exhausted; direct child collection unconfirmed".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let invocation_return = observation::boot_nanos();
    std::fs::write(
        directory.join("parent-receipt.json"),
        serde_json::to_vec(&json!({
            "pid": pid, "birth": birth, "status": status.to_string(),
            "invocation_start": invocation_start, "invocation_return": invocation_return,
            "cancelled": cancelled, "forced": forced, "nonce": nonce,
        }))
        .unwrap(),
    )
    .map_err(|e| e.to_string())?;
    if cancelled || forced || !status.success() {
        return Err(format!(
            "terminal invocation failure: {status}; cancelled={cancelled}; forced={forced}"
        ));
    }
    if matches!(workload, "native_tree5" | "registry_tree5") {
        let stdout =
            std::fs::read_to_string(directory.join("stdout")).map_err(|e| e.to_string())?;
        let mut records: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        records.sort_by_key(|row| row["root"] != true);
        super::tree::validate(&records)?;
    }
    let events = Events::read(&events, &manifest.digest, &nonce)?;
    let intervals = events.intervals(invocation_start, invocation_return)?;
    let guard_evidence = if member.build == super::schedule::Build::Head {
        Some(super::guards::validate(
            &events.rows,
            pid,
            workload == "admission_only",
        )?)
    } else {
        None
    };
    let admission = events
        .rows
        .iter()
        .any(|row| row["event"] == "admission_exchange")
        .then(|| super::admission::components(&events.rows))
        .transpose()?;
    let helper_start = if events
        .rows
        .iter()
        .any(|row| row["event"] == "helpers_ready_go")
    {
        Some(
            events
                .unique_time("helpers_ready_go")?
                .checked_sub(invocation_start)
                .ok_or("helper event predates invocation")?,
        )
    } else {
        None
    };
    if member.build == super::schedule::Build::Head
        && (helper_start.is_none() || admission.as_ref().is_none_or(Vec::is_empty))
    {
        return Err("head sample lacks helper readiness or complete admission exchanges".into());
    }
    if !own_session && helper_start.is_none() && workload != "admission_only" {
        return Err("SID1 baseline lacks helper-start observation".into());
    }
    Ok(
        json!({"pid": pid, "birth": birth, "nonce": nonce, "intervals": intervals,
        "helper_start_ns": helper_start, "admission_ns": admission,
        "guard_failures": guard_evidence.as_ref().map(|v| &v["guard_failures"]),
        "protocol_failures": guard_evidence.as_ref().map(|v| &v["protocol_failures"]),
        "deadline_failures": guard_evidence.as_ref().map(|v| &v["deadline_failures"]),
        "guard_evidence": guard_evidence}),
    )
}
