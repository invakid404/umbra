//! Qualification-only events on one host boot clock. Included by the provider
//! and external measurement harness so timestamps have identical units.
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

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
    let Some(directory) = std::env::var_os("UMBRA_F299_PERF_EVENTS") else {
        return;
    };
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _writer = WRITER.lock().expect("measurement writer poisoned");
    let timestamp = boot_nanos();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
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
        "event": event, "resource": resource,
    });
    // One file per producer; shared stderr is never an identity oracle.
    let path = std::path::Path::new(&directory).join(format!("{pid}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("measurement record file");
    let mut bytes = serde_json::to_vec(&record).expect("measurement encoding");
    bytes.push(b'\n');
    file.write_all(&bytes).expect("measurement write");
}
