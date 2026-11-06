use notify::{RecursiveMode, Watcher};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

// Deliberately use the raw read/write entry points, never buffered stdio.
fn open(path: &Path, flags: i32) -> i32 {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(name.as_ptr(), flags, 0o600) };
    assert!(fd >= 0, "open: {}", std::io::Error::last_os_error());
    fd
}
fn write(fd: i32, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        assert!(n > 0, "write: {}", std::io::Error::last_os_error());
        bytes = &bytes[n as usize..];
    }
}
fn close(fd: i32) {
    assert_eq!(unsafe { libc::close(fd) }, 0);
}
struct Report {
    fd: i32,
    seq: usize,
    nonce: String,
    start: Instant,
}
impl Report {
    fn record(&mut self, phase: &str, data: Value) {
        let bytes = serde_json::to_vec(&json!({
            "version":1,"seq":self.seq,"nonce":self.nonce,
            "pid":std::process::id(),"us":self.start.elapsed().as_micros(),
            "phase":phase,"data":data
        }))
        .unwrap();
        write(self.fd, format!("{}:", bytes.len()).as_bytes());
        write(self.fd, &bytes);
        write(self.fd, b"\n");
        self.seq += 1;
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4, "mode root nonce");
    let root = Path::new(&args[2]);
    let mut report = Report {
        fd: open(
            &root.join("evidence/result"),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
        ),
        seq: 0,
        nonce: args[3].clone(),
        start: Instant::now(),
    };
    report.record(
        "BEGIN",
        json!({"mode":args[1],"backend":format!("{:?}",notify::RecommendedWatcher::kind())}),
    );
    report.record("channel", json!("C17-CHANNEL"));
    let callbacks = Arc::new(Mutex::new((Vec::new(), false)));
    let queue = callbacks.clone();
    let clock = report.start;
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let mut q = queue.lock().unwrap();
        if q.0.len() == 4096 {
            q.1 = true;
            return;
        }
        q.0.push((clock.elapsed().as_micros(), event));
    });
    report.record(
        "constructor",
        json!({"ok":watcher.is_ok(),"error":watcher.as_ref().err().map(|e|format!("{e:?}"))}),
    );
    let mut watcher = watcher.unwrap();
    if args[1] == "register" {
        let watched = root.join("work");
        let registration = watcher.watch(&watched, RecursiveMode::NonRecursive);
        report.record("registration", json!({"ok":registration.is_ok(),"error":registration.as_ref().err().map(|e|format!("{e:?}"))}));
        registration.unwrap();
        report.record("READY", json!(true));
        let path = watched.join(format!("{}.txt", args[3]));
        let fd = open(&path, libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY);
        write(fd, args[3].as_bytes());
        close(fd);
        let acknowledged = report.start.elapsed().as_micros();
        report.record("mutation", json!({"ack_us":acknowledged,"bytes":args[3]}));
        report.record("observation-start", json!(acknowledged));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        report.record("observation-end", json!(report.start.elapsed().as_micros()));
        let fd = open(&path, libc::O_RDONLY);
        let mut bytes = vec![0u8; args[3].len() + 1];
        let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        assert!(n >= 0);
        bytes.truncate(n as usize);
        close(fd);
        report.record("read", json!(bytes));
        let mut entries: Vec<_> = std::fs::read_dir(&watched)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        report.record("list", json!(entries));
    } else {
        assert_eq!(args[1], "construct");
        report.record("workload", json!("NOT_APPLICABLE"));
    }
    drop(watcher);
    let q = callbacks.lock().unwrap();
    for (us, event) in &q.0 {
        let value = match event {
            Ok(event) => json!({"us":us,"paths":event.paths,"kind":format!("{:?}",event.kind)}),
            Err(error) => json!({"us":us,"error":format!("{error:?}")}),
        };
        report.record("callback", value);
    }
    report.record("teardown", json!({"dropped":true,"overflow":q.1}));
    report.record("COMPLETE", json!({"records":report.seq + 1}));
    close(report.fd);
}
