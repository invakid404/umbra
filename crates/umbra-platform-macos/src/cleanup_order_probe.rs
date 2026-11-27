//! Test-only native cleanup observations. A timed-out latch invalidates the test.
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A per-test, per-process observation fault. Never changes the operation
/// directed at the program or the deadline governing cleanup.
pub(crate) fn withhold_terminal(pid: i32) -> bool {
    std::env::var_os("UMBRA_NATIVE_CLEANUP_ORDER").is_some_and(|root| {
        std::path::PathBuf::from(root)
            .join(format!("withhold-terminal-{pid}"))
            .is_file()
    })
}

pub(crate) fn record(stage: &str, details: serde_json::Value, hold: bool) {
    let Some(root) = std::env::var_os("UMBRA_NATIVE_CLEANUP_ORDER") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let _writer = WRITER.lock().unwrap();
    let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    let producer = umbra_isolation::managed::process::identity(
        std::process::id(),
        umbra_core::managed::Role::Provider,
    )
    .unwrap();
    let run = std::fs::read_to_string(root.join("run")).unwrap();
    let record = serde_json::json!({
        "producer": producer, "run": run, "sequence": sequence,
        "predecessor": sequence.checked_sub(1).filter(|s| *s != 0),
        "stage": stage, "details": details,
    });
    let path = root.join(format!("{sequence:04}-{stage}.json"));
    let pending = path.with_extension("pending");
    std::fs::write(&pending, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::rename(pending, path).unwrap();
    if hold {
        let release = root.join(format!("release-{sequence}-{stage}"));
        let observation = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !release.exists() {
            if std::time::Instant::now() >= observation {
                std::fs::write(root.join("invalid-observation"), stage).unwrap();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
