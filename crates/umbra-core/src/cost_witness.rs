//! Explicit qualification observer; the default macro emits no instructions.

/// Record an allocation, close, spawn, or collection boundary in a cost build.
#[cfg(feature = "cost-witness")]
#[macro_export]
macro_rules! cost_snapshot {
    ($event:expr, $child:expr, $keep:expr) => {
        $crate::cost_witness::snapshot($event, $child, $keep, None)
    };
    ($event:expr, $child:expr, $keep:expr, $status:expr) => {
        $crate::cost_witness::snapshot($event, $child, $keep, $status)
    };
}

/// Ordinary builds neither evaluate the arguments nor invoke an observer.
#[cfg(not(feature = "cost-witness"))]
#[macro_export]
macro_rules! cost_snapshot {
    ($($argument:tt)*) => {};
}

/// Run and collect the explicit observer at selected boundaries without output pipes.
///
/// Its own process and descriptors are excluded from the census. These builds
/// measure resource counts, not latency, and must never supply D6 samples.
/// Startup allocation hooks are ignored: the first managed snapshot is after
/// worker GO. These readings do not measure the peak during startup.
#[cfg(feature = "cost-witness")]
pub fn snapshot(event: &str, child: Option<u32>, keep: &[i32], status: Option<i32>) {
    if !event.starts_with("control-")
        && !matches!(
            event,
            "driver-before-spawn"
                | "driver-after-collection"
                | "cli-return"
                | "library-before-prepare"
                | "library-after-return"
                | "role-go-Worker"
                | "run-ready"
                | "helper-collected"
                | "startup-rollback-closed"
        )
    {
        return;
    }
    if std::env::var_os("UMBRA_COST_OBSERVER").is_none() {
        return;
    }
    if let Err(error) = observe(event, child, keep, status) {
        // A failed measurement must not unwind across a newly spawned child
        // before its normal custody transfer. The reading is invalid instead.
        use std::io::Write;
        eprintln!("cost witness failed at {event}: {error}");
        if let Some(directory) = std::env::var_os("UMBRA_COST_DIRECTORY") {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::path::PathBuf::from(directory).join("errors"))
            {
                let _ = writeln!(file, "{event}: {error}");
            }
        }
    }
}

#[cfg(feature = "cost-witness")]
fn observe(
    event: &str,
    child: Option<u32>,
    keep: &[i32],
    status: Option<i32>,
) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let missing = || std::io::Error::other("missing cost observer configuration");
    let observer = std::env::var_os("UMBRA_COST_OBSERVER").ok_or_else(missing)?;
    let _serial = SERIAL
        .lock()
        .map_err(|_| std::io::Error::other("cost observer lock poisoned"))?;
    let directory =
        std::path::PathBuf::from(std::env::var_os("UMBRA_COST_DIRECTORY").ok_or_else(missing)?);
    let gate = directory.join(format!("gate-{}", std::process::id()));
    let pending = gate.with_extension("pending");
    match std::fs::remove_file(&gate) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let keep = keep
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut observer = Command::new(observer)
        .arg(&directory)
        .arg(event)
        .arg(std::process::id().to_string())
        .arg(child.unwrap_or(0).to_string())
        .arg(keep)
        .arg(status.map_or_else(|| "null".into(), |value| value.to_string()))
        .arg(&gate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    // spawn() has returned and released its temporary stdio descriptors.
    // Publish only after write() has closed the temporary file as well.
    if let Err(error) = std::fs::write(&pending, []).and_then(|()| std::fs::rename(&pending, &gate))
    {
        let _ = observer.kill();
        let _ = observer.wait();
        return Err(error);
    }
    let result = observer.wait()?;
    std::fs::remove_file(gate)?;
    if !result.success() {
        return Err(std::io::Error::other(format!(
            "cost observer failed at {event}: {result}"
        )));
    }
    Ok(())
}
