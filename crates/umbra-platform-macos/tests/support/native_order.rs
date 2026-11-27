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
        Self::start_case(false)
    }

    pub fn start_emergency() -> Option<Self> {
        Self::start_case(true)
    }

    fn start_case(emergency_case: bool) -> Option<Self> {
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
                    if stage == "barrier-held" && !emergency_case {
                        use umbra_isolation::managed::process::{identity, ProcessWatch};
                        let debugger = event["details"]["debugger"].as_u64().unwrap() as u32;
                        let observation = (|| -> std::io::Result<bool> {
                            let peer = identity(debugger, umbra_core::managed::Role::Provider)?;
                            let mut watch = ProcessWatch::new()?;
                            watch.install(peer)?;
                            Ok(watch.exited()?.is_none())
                        })();
                        if !matches!(observation, Ok(true)) {
                            errors.push(format!(
                                "debugger {debugger} ended before barrier release: {observation:?}"
                            ));
                        }
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

    pub fn assert_held(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let errors = thread.join().unwrap();
            assert!(errors.is_empty(), "native held barrier crossed: {errors:?}");
        }
    }

    pub fn directory(&self) -> &std::path::Path {
        &self.root
    }

    pub fn finish(self, final_state: &umbra_core::shutdown::Final) {
        self.finish_case(final_state, None);
    }

    pub fn finish_case(
        mut self,
        final_state: &umbra_core::shutdown::Final,
        emergency: Option<i32>,
    ) {
        self.stop.store(true, Ordering::Release);
        self.assert_held();
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
            .position(|e| {
                e["stage"] == "barrier-held"
                    && emergency.is_none_or(|pid| e["details"]["pid"] == pid)
            })
            .unwrap();
        let cleanup = events
            .iter()
            .position(|e| {
                e["stage"] == "debugger-cleanup"
                    && emergency.is_none_or(|pid| e["details"]["pid"] == pid)
            })
            .unwrap();
        let collected = events
            .iter()
            .position(|e| {
                e["stage"] == "debugger-collected"
                    && emergency.is_none_or(|pid| e["details"]["pid"] == pid)
            })
            .unwrap();
        assert!(barrier < cleanup && cleanup < collected);
        assert_eq!(events[barrier]["details"]["confirmed"], emergency.is_none());
        assert_eq!(events[cleanup]["details"]["emergency"], emergency.is_some());
        assert_eq!(events[collected]["details"]["collected"], true);
        if emergency.is_some() {
            assert_eq!(events[cleanup]["details"]["allocation_expired"], true);
            let before = &events[cleanup]["details"]["helper_before_request"];
            assert!(
                before["running"] == true || before["normal_exit"] == true,
                "helper ended by signal before cleanup request, or observation failed: {before}"
            );
            let raw = events[collected]["details"]["raw_wait_status"]
                .as_i64()
                .expect("actual parent raw wait status") as i32;
            if before["normal_exit"] == true {
                assert!(libc::WIFEXITED(raw), "helper parent status: {raw}");
                assert_eq!(
                    i64::from(libc::WEXITSTATUS(raw)),
                    before["status"].as_i64().unwrap()
                );
            }
        }
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
