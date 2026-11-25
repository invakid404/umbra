//! A terminal failure ends the series; every attempted member is retained.
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Build {
    Base,
    Head,
}
impl Build {
    fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Head => "head",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Member {
    pub warmup: bool,
    pub index: usize,
    pub order: usize,
    pub build: Build,
    pub deadline: Instant,
}

pub fn run(
    directory: &Path,
    paired: bool,
    mut invocation: impl FnMut(Member) -> Result<Value, String>,
) -> Result<(), String> {
    // Refuse overwrite even when an earlier run produced only a partial record.
    std::fs::create_dir(directory).map_err(|e| format!("fresh evidence directory: {e}"))?;
    let mut records = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("samples.jsonl"))
        .map_err(|e| e.to_string())?;
    for batch in 0..33 {
        let warmup = batch < 3;
        let index = if warmup { batch } else { batch - 3 };
        let deadline = Instant::now() + Duration::from_secs(900);
        let order = if index % 2 == 0 {
            [Build::Base, Build::Head]
        } else {
            [Build::Head, Build::Base]
        };
        let builds: &[Build] = if paired { &order } else { &[Build::Base] };
        for (position, &build) in builds.iter().enumerate() {
            let member = Member {
                warmup,
                index,
                order: position,
                build,
                deadline,
            };
            let result = invocation(member);
            let expired = Instant::now() >= deadline;
            let record = json!({"warmup": warmup, "index": index,
                "order": position, "build": build.name(), "pair_expired": expired,
                "sample": result.as_ref().ok(), "error": result.as_ref().err()});
            serde_json::to_writer(&mut records, &record).map_err(|e| e.to_string())?;
            records.write_all(b"\n").map_err(|e| e.to_string())?;
            records.sync_all().map_err(|e| e.to_string())?;
            result?;
            if expired {
                return Err("pair watchdog expired".into());
            }
        }
    }
    Ok(())
}
