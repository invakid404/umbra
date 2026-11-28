use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use umbra_isolation::managed::test_clock::{self, Scope};

#[derive(Clone)]
pub struct SharedClock {
    directory: Arc<tempfile::TempDir>,
    writer: Arc<Mutex<()>>,
}

impl SharedClock {
    pub fn new(value: u64) -> Self {
        let clock = Self {
            directory: Arc::new(
                tempfile::Builder::new()
                    .prefix("u-clock-")
                    .tempdir_in("/tmp")
                    .unwrap(),
            ),
            writer: Arc::new(Mutex::new(())),
        };
        clock.set(value);
        clock
    }
    pub fn path(&self) -> PathBuf {
        self.directory.path().join("clock")
    }
    pub fn set(&self, value: u64) {
        let _lock = self.writer.lock().unwrap();
        self.write(value);
    }
    fn write(&self, value: u64) {
        let pending = self.directory.path().join("next");
        std::fs::write(&pending, value.to_le_bytes()).unwrap();
        std::fs::rename(pending, self.path()).unwrap();
    }
    pub fn advance_to(&self, value: u64) {
        let _lock = self.writer.lock().unwrap();
        let current = u64::from_le_bytes(std::fs::read(self.path()).unwrap().try_into().unwrap());
        self.write(value.max(current));
    }
    pub fn install(&self) -> Scope {
        install(&self.path())
    }
    // An external harness rescue, never a passing production timeout. Advancing
    // to the original deadline lets a regressed guard return without PID signals.
    pub fn observation_bound(&self, original_deadline: u64) -> ObservationBound {
        let done = Arc::new(AtomicBool::new(false));
        let expired = Arc::new(AtomicBool::new(false));
        let done_thread = done.clone();
        let expired_thread = expired.clone();
        let clock = self.clone();
        let watcher = std::thread::spawn(move || {
            let bound = Instant::now() + Duration::from_secs(2);
            while !done_thread.load(Ordering::SeqCst) {
                if Instant::now() >= bound {
                    expired_thread.store(true, Ordering::SeqCst);
                    clock.advance_to(original_deadline);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        ObservationBound {
            done,
            expired,
            watcher: Some(watcher),
        }
    }
}

pub struct ObservationBound {
    done: Arc<AtomicBool>,
    expired: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl ObservationBound {
    pub fn finish(mut self) -> bool {
        self.stop();
        !self.expired.load(Ordering::SeqCst)
    }
    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            watcher.join().unwrap();
        }
    }
}

impl Drop for ObservationBound {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn install(path: &Path) -> Scope {
    let path = path.to_owned();
    test_clock::install(move || {
        let bytes: [u8; 8] = std::fs::read(&path)?
            .try_into()
            .map_err(|_| io::Error::other("invalid shared test clock"))?;
        Ok(u64::from_le_bytes(bytes))
    })
}
