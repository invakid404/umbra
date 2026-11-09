//! Test-only, acknowledged fault rendezvous; absent from normal builds.
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    time::Duration,
};
use umbra_core::Result;

pub(crate) fn acquired(pid: i32, birth: Option<(u64, u64)>, registered: bool) -> Result<()> {
    let Some(socket) = std::env::var_os("UMBRA_FIX_243_PROBE_SOCKET") else {
        return Ok(());
    };
    let birth = birth.ok_or_else(|| crate::error("spawn probe", "birth identity unavailable"))?;
    let mut stream = UnixStream::connect(socket).map_err(|e| crate::error("spawn probe", e))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| crate::error("spawn probe", e))?;
    let record = serde_json::json!({
        "phase": "task-acquired", "watchdog_registered": registered,
        "provider": std::process::id(), "child": pid,
        "start_sec": birth.0, "start_usec": birth.1,
    });
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
        b'E' => Err(crate::error(
            "spawn probe",
            "injected attach failure after registration",
        )),
        _ => Err(crate::error("spawn probe", "unknown fault command")),
    }
}
