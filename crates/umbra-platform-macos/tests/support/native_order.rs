use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub struct Observation {
    root: PathBuf,
    run: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Vec<String>>>,
}

fn records(root: &std::path::Path) -> Vec<Value> {
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
        .collect()
}

impl Observation {
    pub fn start() -> Option<Self> {
        let root = PathBuf::from(std::env::var_os("UMBRA_NATIVE_CLEANUP_ORDER")?);
        std::fs::create_dir(&root).expect("fresh native order directory");
        let run = uuid::Uuid::new_v4().to_string();
        std::fs::write(root.join("run"), &run).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let watching = stop.clone();
        let directory = root.clone();
        let witness = PathBuf::from(std::env::var_os("UMBRA_FIX_271_EVIDENCE_FILE").unwrap());
        let thread = std::thread::spawn(move || {
            let mut errors = Vec::new();
            let mut checked = std::collections::BTreeSet::new();
            while !watching.load(Ordering::Acquire) {
                let seen = records(&directory);
                for event in &seen {
                    let stage = event["stage"].as_str().unwrap();
                    if !matches!(stage, "barrier-held" | "right-held")
                        || !checked.insert(event["sequence"].as_u64().unwrap())
                    {
                        continue;
                    }
                    let forbidden = if stage == "barrier-held" {
                        "debugger-cleanup"
                    } else {
                        "right-released"
                    };
                    if std::fs::read_to_string(&witness)
                        .unwrap_or_default()
                        .lines()
                        .any(|line| {
                            serde_json::from_str::<Value>(line).is_ok_and(|event| {
                                event["event"] == "transport-final"
                                    || event["event"] == "final-to-exit"
                            })
                        })
                    {
                        errors.push(format!("final proof crossed held {stage}"));
                    }
                    if seen.iter().any(|later| {
                        later["stage"] == forbidden
                            && (if stage == "barrier-held" {
                                later["details"]["pid"] == event["details"]["pid"]
                            } else {
                                later["details"]["slot"] == event["details"]["slot"]
                            })
                    }) {
                        errors.push(format!("{forbidden} crossed held {stage}"));
                    }
                    // Always release the test latch, even after a violation:
                    // the actual owner must finish collection before assertions.
                    std::fs::write(
                        directory.join(format!(
                            "release-{}-{stage}",
                            event["sequence"].as_u64().unwrap()
                        )),
                        b"release",
                    )
                    .unwrap();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            errors
        });
        Some(Self {
            root,
            run,
            stop,
            thread: Some(thread),
        })
    }

    pub fn finish(mut self, final_state: &umbra_core::shutdown::Final) {
        self.stop.store(true, Ordering::Release);
        assert!(
            self.thread.take().unwrap().join().unwrap().is_empty(),
            "native held barrier crossed"
        );
        assert!(
            !self.root.join("invalid-observation").exists(),
            "controller missed native latch"
        );
        let events = records(&self.root);
        assert!(!events.is_empty());
        let producer = &events[0]["producer"];
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["run"], self.run);
            assert_eq!(&event["producer"], producer);
            assert_eq!(event["sequence"], (index + 1) as u64);
            assert_eq!(
                event["predecessor"].as_u64(),
                (index > 0).then_some(index as u64)
            );
        }
        let barrier = events
            .iter()
            .position(|e| e["stage"] == "barrier-held")
            .unwrap();
        let cleanup = events
            .iter()
            .position(|e| e["stage"] == "debugger-cleanup")
            .unwrap();
        let collected = events
            .iter()
            .position(|e| e["stage"] == "debugger-collected")
            .unwrap();
        assert!(barrier < cleanup && cleanup < collected);
        assert_eq!(events[barrier]["details"]["confirmed"], true);
        assert_eq!(events[cleanup]["details"]["emergency"], false);
        assert_eq!(events[collected]["details"]["collected"], true);
        assert_eq!(
            events[barrier]["details"]["debugger"],
            events[collected]["details"]["debugger"]
        );
        assert_eq!(
            events[barrier]["details"]["birth"],
            events[collected]["details"]["birth"]
        );
        for entry in final_state
            .entries
            .iter()
            .filter(|e| e.slot.kind == umbra_core::shutdown::ResourceKind::TaskRight)
        {
            let id = serde_json::to_value(entry.slot.id).unwrap();
            let held = events
                .iter()
                .position(|e| e["stage"] == "right-held" && e["details"]["slot"] == id)
                .unwrap();
            let released = events
                .iter()
                .position(|e| e["stage"] == "right-released" && e["details"]["slot"] == id)
                .unwrap();
            assert!(held < released);
            assert_eq!(events[released]["details"]["result"], 0);
            assert!(entry.confirms());
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for event in records(&self.root) {
            let stage = event["stage"].as_str().unwrap();
            let _ = std::fs::write(
                self.root.join(format!(
                    "release-{}-{stage}",
                    event["sequence"].as_u64().unwrap()
                )),
                b"rescue",
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
