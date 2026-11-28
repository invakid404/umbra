//! Qualification-only events on one host boot clock. Included by the provider
//! and external measurement harness so timestamps have identical units.
use std::io::Write;

pub fn boot_nanos() -> u64 {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    // CLOCK_UPTIME_RAW is shared across processes and is not wall time.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) },
        0
    );
    let seconds = u64::try_from(time.tv_sec).expect("negative boot clock");
    let nanos = u64::try_from(time.tv_nsec).expect("negative fractional clock");
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|s| s.checked_add(nanos))
        .expect("boot clock overflow")
}

pub fn birth(pid: u32) -> (u64, u64) {
    // SAFETY: a zeroed proc_bsdinfo is valid output storage, and the kernel
    // receives its exact size. No reference is retained across the call.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    assert_eq!(n, size, "measurement producer identity unavailable");
    (info.pbi_start_tvsec, info.pbi_start_tvusec)
}

pub fn record(event: &str, resource: Option<u64>) {
    record_as("platform", event, resource);
}

pub fn record_as(role: &str, event: &str, resource: Option<u64>) {
    record_details(role, event, resource, serde_json::Value::Null);
}

pub fn record_details(role: &str, event: &str, resource: Option<u64>, details: serde_json::Value) {
    let Some(directory) = std::env::var_os("UMBRA_F299_PERF_EVENTS") else {
        return;
    };
    static WRITERS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let mut writers = WRITERS.lock().expect("measurement writer poisoned");
    let timestamp = boot_nanos();
    let next = writers.entry(role.to_owned()).or_default();
    let sequence = *next;
    *next = next.checked_add(1).expect("measurement sequence exhausted");
    let pid = std::process::id();
    static BIRTH: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    let birth = BIRTH.get_or_init(|| birth(pid));
    let required = |key| std::env::var(key).expect("missing measurement identity");
    let startup_resources = if std::env::var_os("UMBRA_F299_STARTUP_RESOURCES").is_some() {
        let begin = boot_nanos();
        let descriptors = own_descriptors().expect("startup descriptor inventory");
        Some(serde_json::json!({
            "begin": begin, "end": boot_nanos(), "descriptors": descriptors,
            "run_inventory": run_inventory(std::path::Path::new(&directory)),
        }))
    } else {
        None
    };
    let record = serde_json::json!({
        "schema_version": 1,
        "manifest_digest": required("UMBRA_F299_PERF_MANIFEST_DIGEST"),
        "run_nonce": required("UMBRA_F299_PERF_RUN_NONCE"),
        "pair_index": required("UMBRA_F299_PERF_PAIR_INDEX"),
        "order": required("UMBRA_F299_PERF_ORDER"),
        "workload": required("UMBRA_F299_PERF_WORKLOAD"),
        "role": role,
        "pid": pid, "birth": birth, "sequence": sequence, "boot_nanos": timestamp,
        "event": event, "resource": resource, "details": details,
        "startup_resources": startup_resources,
    });
    // Different linked crates can observe distinct roles in one process.
    // Separate streams preserve each observer's sequence and process identity.
    let path = std::path::Path::new(&directory).join(format!("{pid}-{role}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("measurement record file");
    let mut bytes = serde_json::to_vec(&record).expect("measurement encoding");
    bytes.push(b'\n');
    file.write_all(&bytes).expect("measurement write");
}

/// Inspect this process before opening the observation file. No helper or wait.
/// Return descriptor identities as well as counts so stdio is classified exactly.
pub fn own_descriptors() -> std::io::Result<Vec<(i32, u32)>> {
    descriptors_for(std::process::id() as i32)
}

fn descriptors_for(pid: i32) -> std::io::Result<Vec<(i32, u32)>> {
    // SAFETY: a null buffer requests the required byte count.
    let bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let width = std::mem::size_of::<libc::proc_fdinfo>();
    let capacity = bytes as usize / width + 128;
    let mut entries: Vec<libc::proc_fdinfo> = (0..capacity)
        .map(|_| libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        })
        .collect();
    let size = i32::try_from(capacity * width).map_err(std::io::Error::other)?;
    // SAFETY: entries owns size bytes of properly aligned output storage.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr().cast(),
            size,
        )
    };
    if read <= 0 || read >= size || !(read as usize).is_multiple_of(width) {
        return Err(std::io::Error::other(
            "incomplete startup descriptor inventory",
        ));
    }
    let mut result = entries[..read as usize / width]
        .iter()
        .map(|fd| (fd.proc_fd, fd.proc_fdtype))
        .collect::<Vec<_>>();
    result.sort_unstable();
    Ok(result)
}

// Sequential kernel census, not a sum of historical per-process maxima. Keep
// each query interval and any error: an incomplete census cannot prove a peak.
pub fn run_inventory(directory: &std::path::Path) -> serde_json::Value {
    let mut participants = std::collections::BTreeMap::new();
    participants.insert(std::process::id(), birth(std::process::id()));
    // Read and close discovery files before counting descriptors.
    for entry in std::fs::read_dir(directory).expect("inventory directory") {
        let path = entry.expect("inventory entry").path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("inventory discovery");
        if let Some(first) = text.lines().next() {
            if let Ok(row) = serde_json::from_str::<serde_json::Value>(first) {
                if let (Some(pid), Some(sec), Some(usec)) = (
                    row["pid"].as_u64(),
                    row["birth"][0].as_u64(),
                    row["birth"][1].as_u64(),
                ) {
                    participants.insert(pid as u32, (sec, usec));
                }
            }
        }
    }
    let begin = boot_nanos();
    let mut rows = Vec::new();
    for (pid, expected_birth) in participants {
        let start = boot_nanos();
        let descriptors = descriptors_for(pid as i32);
        // Do not call the asserting birth helper for a process which exited.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        // SAFETY: info is valid aligned output storage of size bytes.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        let identity_matches =
            read == size && (info.pbi_start_tvsec, info.pbi_start_tvusec) == expected_birth;
        rows.push(serde_json::json!({
            "pid": pid, "birth": expected_birth, "begin": start, "end": boot_nanos(),
            "identity_matches": identity_matches,
            "descriptors": descriptors.as_ref().ok(),
            "error": descriptors.err().map(|error| error.to_string()),
        }));
    }
    serde_json::json!({"begin": begin, "end": boot_nanos(), "participants": rows})
}
