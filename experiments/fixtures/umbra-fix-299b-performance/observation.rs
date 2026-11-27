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
        "startup_resources": if std::env::var_os("UMBRA_F299_STARTUP_RESOURCES").is_some() {
            Some(own_descriptors().expect("startup descriptor inventory"))
        } else { None },
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
    let pid = std::process::id() as i32;
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
    if read <= 0 || read >= size || read as usize % width != 0 {
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
