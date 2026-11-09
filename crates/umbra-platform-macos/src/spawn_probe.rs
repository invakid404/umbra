//! Test-only, acknowledged fault rendezvous; absent from normal builds.
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    time::Duration,
};
use umbra_core::Result;

pub(crate) fn acquired(pid: i32, birth: Option<(u64, u64)>, registered: bool) -> Result<()> {
    if rendezvous(
        "task-acquired",
        pid,
        birth,
        serde_json::json!({"watchdog_registered": registered}),
    )? {
        return Err(crate::error(
            "spawn probe",
            "injected attach failure after registration",
        ));
    }
    Ok(())
}

pub(crate) fn rendezvous(
    phase: &str,
    pid: i32,
    birth: Option<(u64, u64)>,
    detail: serde_json::Value,
) -> Result<bool> {
    let selected =
        std::env::var("UMBRA_FIX_243_PROBE_PHASE").unwrap_or_else(|_| "task-acquired".into());
    if selected != phase && !(selected == "policy-ready" && phase == "inserted") {
        return Ok(false);
    }
    let Some(socket) = std::env::var_os("UMBRA_FIX_243_PROBE_SOCKET") else {
        return Ok(false);
    };
    let birth = birth.ok_or_else(|| crate::error("spawn probe", "birth identity unavailable"))?;
    let mut stream = UnixStream::connect(socket).map_err(|e| crate::error("spawn probe", e))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| crate::error("spawn probe", e))?;
    let mut record = serde_json::json!({
        "phase": phase,
        "provider": std::process::id(), "child": pid,
        "start_sec": birth.0, "start_usec": birth.1,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(detail.as_object().unwrap().clone());
    writeln!(stream, "{record}").map_err(|e| crate::error("spawn probe", e))?;
    let mut command = [0];
    stream
        .read_exact(&mut command)
        .map_err(|e| crate::error("spawn probe", e))?;
    match command[0] {
        b'P' => {
            // SAFETY: the test acknowledged the independent child identity.
            // This kills only this test's provider, deliberately without Drop.
            unsafe { libc::_exit(86) }
        }
        b'E' => Ok(true),
        b'C' => Ok(false),
        _ => Err(crate::error("spawn probe", "unknown fault command")),
    }
}
