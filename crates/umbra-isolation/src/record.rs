//! The isolation record (#193 ruling O6; design R-1..R-4).
//!
//! One JSON object per line, appended to
//! `<home>/Library/Logs/umbra/isolation/isolation-<32 hex>[-s<S>].jsonl`. The
//! home directory comes from `getpwuid_r(getuid())`, not `$HOME`, so launchd
//! jobs without `HOME` work. The file name carries a per-run unique id (S
//! recurs across runs).
//!
//! **Contract.** The record is created, and its first line written and
//! `fsync`ed, before anything is launched. Every line is serialized with
//! `serde_json` (checked), written with **one** `write(2)` whose full length is
//! verified, then `fsync(2)`ed (not `F_FULLFSYNC`). The first failure is
//! sticky: the run is not killed for it, but its reported status can never be
//! success (O5).
//!
//! **Retention.** At relay start, keep the newest [`KEEP`] records: delete only
//! *regular files* in that directory whose names match the exact record
//! pattern, oldest first (L39/L40).

use std::path::{Path, PathBuf};

/// Records kept by retention, the new one included.
pub const KEEP: usize = 50;

/// Where record lines go. A trait so tests can inject short writes and fsync
/// failures.
pub trait Sink {
    /// Write `bytes` with one call; `Ok(n)` is the count written.
    fn write_once(&mut self, bytes: &[u8]) -> Result<usize, i32>;
    /// Flush to the device (`fsync`).
    fn sync(&mut self) -> Result<(), i32>;
}

/// A record writer. Never panics; failures are sticky in [`Record::failed`].
pub struct Record<S: Sink> {
    sink: Option<S>,
    failed: Option<i32>,
}

impl<S: Sink> Record<S> {
    /// Wrap a sink.
    pub fn new(sink: S) -> Self {
        Record {
            sink: Some(sink),
            failed: None,
        }
    }
    /// A record that could not be opened: every line fails with `errno`.
    pub fn unavailable(errno: i32) -> Self {
        Record {
            sink: None,
            failed: Some(errno),
        }
    }
    /// The first write, length or fsync failure (errno), if any.
    pub fn failed(&self) -> Option<i32> {
        self.failed
    }
    /// Append one event. `fields` must be a JSON object; `t` and `pid` are added.
    pub fn line(&mut self, event: &str, fields: serde_json::Value) {
        let mut obj = match fields {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        obj.insert("event".into(), event.into());
        obj.insert("t".into(), wall_seconds().into());
        obj.insert("pid".into(), std::process::id().into());
        let mut bytes = match serde_json::to_vec(&serde_json::Value::Object(obj)) {
            Ok(b) => b,
            Err(_) => {
                self.fail(libc_einval());
                return;
            }
        };
        bytes.push(b'\n');
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        match sink.write_once(&bytes) {
            Ok(n) if n == bytes.len() => {}
            Ok(_) => {
                self.fail(libc_eio());
                return;
            }
            Err(e) => {
                self.fail(e);
                return;
            }
        }
        if let Err(e) = sink.sync() {
            self.fail(e);
        }
    }
    fn fail(&mut self, errno: i32) {
        if self.failed.is_none() {
            self.failed = Some(errno);
        }
    }
}

fn libc_eio() -> i32 {
    5
}
fn libc_einval() -> i32 {
    22
}

fn wall_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The record file name for a run: `isolation-<unique>.jsonl` before the
/// session is known, `isolation-<unique>-s<sid>.jsonl` after.
pub fn record_name(unique: &str, sid: Option<i32>) -> String {
    match sid {
        Some(s) => format!("isolation-{unique}-s{s}.jsonl"),
        None => format!("isolation-{unique}.jsonl"),
    }
}

/// Whether `name` is exactly a record name (the only files retention may
/// delete).
pub fn is_record_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("isolation-") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".jsonl") else {
        return false;
    };
    let (unique, sid) = match rest.split_once("-s") {
        Some((u, s)) => (u, Some(s)),
        None => (rest, None),
    };
    let hex_ok = unique.len() == 32
        && unique
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let sid_ok = match sid {
        None => true,
        Some(s) => {
            !s.is_empty()
                && s.len() <= 10
                && s.bytes().all(|b| b.is_ascii_digit())
                && !s.starts_with('0')
        }
    };
    hex_ok && sid_ok
}

/// Retention: keep the newest `keep - 1` existing records (the new one makes
/// `keep`). Deletes only regular files (never symlinks or directories) with an
/// exact record name. Returns the names removed.
pub fn prune(dir: &Path, keep: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut records: Vec<(std::time::SystemTime, String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !is_record_name(&name) {
                return None;
            }
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            if !meta.file_type().is_file() {
                return None;
            }
            Some((meta.modified().ok()?, name, e.path()))
        })
        .collect();
    records.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut removed = vec![];
    for (_, name, path) in records.into_iter().skip(keep.saturating_sub(1)) {
        if std::fs::remove_file(&path).is_ok() {
            removed.push(name);
        }
    }
    removed
}

/// A sink over a raw descriptor (macOS).
#[cfg(target_os = "macos")]
pub struct FdSink(pub std::os::fd::OwnedFd);

#[cfg(target_os = "macos")]
impl Sink for FdSink {
    fn write_once(&mut self, bytes: &[u8]) -> Result<usize, i32> {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: `bytes` is valid for its length; the descriptor is owned.
            let n = unsafe { libc::write(self.0.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            if e != libc::EINTR {
                return Err(e);
            }
        }
    }
    fn sync(&mut self) -> Result<(), i32> {
        use std::os::fd::AsRawFd;
        // SAFETY: an owned, open descriptor.
        if unsafe { libc::fsync(self.0.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        }
    }
}

/// Open (create) a record file for appending, close-on-exec.
#[cfg(target_os = "macos")]
pub fn open(path: &Path) -> Result<FdSink, i32> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map(|f| FdSink(f.into()))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

/// `<home>/Library/Logs/umbra/isolation`, created if missing. The home
/// directory is the password database's, for the real uid.
#[cfg(target_os = "macos")]
pub fn record_dir() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut pwd: libc::passwd = unsafe_zeroed_passwd();
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: valid out-pointers and a buffer of the stated length.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return Err(format!("getpwuid_r: {rc}"));
    }
    // SAFETY: getpwuid_r succeeded, so pw_dir is a NUL-terminated string in buf.
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()))
        .join("Library/Logs/umbra/isolation");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn unsafe_zeroed_passwd() -> libc::passwd {
    // SAFETY: `passwd` is plain old data; all-zero (null pointers) is valid.
    unsafe { std::mem::zeroed() }
}

/// 32 lowercase hex characters from the kernel's entropy source.
#[cfg(target_os = "macos")]
pub fn unique_id() -> Result<String, i32> {
    let mut b = [0u8; 16];
    // SAFETY: a valid 16-byte buffer (getentropy accepts up to 256).
    if unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        out: Vec<u8>,
        short: bool,
        write_err: Option<i32>,
        sync_err: Option<i32>,
    }
    impl Sink for Fake {
        fn write_once(&mut self, bytes: &[u8]) -> Result<usize, i32> {
            if let Some(e) = self.write_err {
                return Err(e);
            }
            let n = if self.short {
                bytes.len() - 1
            } else {
                bytes.len()
            };
            self.out.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn sync(&mut self) -> Result<(), i32> {
            self.sync_err.map_or(Ok(()), Err)
        }
    }
    fn fake() -> Fake {
        Fake {
            out: vec![],
            short: false,
            write_err: None,
            sync_err: None,
        }
    }

    #[test]
    fn lines_are_checked_json_objects() {
        let mut r = Record::new(fake());
        r.line(
            "sweep",
            serde_json::json!({"why": "quote\" and\nnewline", "n": 3}),
        );
        assert_eq!(r.failed(), None);
        let out = String::from_utf8(r.sink.as_ref().unwrap().out.clone()).unwrap();
        assert_eq!(out.matches('\n').count(), 1, "exactly one line");
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["event"], "sweep");
        assert_eq!(v["why"], "quote\" and\nnewline");
    }

    #[test]
    fn a_short_write_is_a_sticky_failure() {
        let mut r = Record::new(Fake {
            short: true,
            ..fake()
        });
        r.line("a", serde_json::json!({}));
        assert_eq!(r.failed(), Some(5));
        r.line("b", serde_json::json!({}));
        assert_eq!(r.failed(), Some(5));
    }

    #[test]
    fn a_write_error_is_kept_first() {
        let mut r = Record::new(Fake {
            write_err: Some(28),
            ..fake()
        });
        r.line("a", serde_json::json!({}));
        assert_eq!(r.failed(), Some(28));
    }

    #[test]
    fn an_fsync_failure_is_a_failure() {
        let mut r = Record::new(Fake {
            sync_err: Some(5),
            ..fake()
        });
        r.line("a", serde_json::json!({}));
        assert_eq!(r.failed(), Some(5));
    }

    #[test]
    fn an_unavailable_record_reports_its_errno() {
        let mut r: Record<Fake> = Record::unavailable(2);
        r.line("a", serde_json::json!({}));
        assert_eq!(r.failed(), Some(2));
    }

    #[test]
    fn record_names_are_exact() {
        let u = "0123456789abcdef0123456789abcdef";
        assert!(is_record_name(&record_name(u, None)));
        assert!(is_record_name(&record_name(u, Some(4321))));
        for bad in [
            "isolation-0123.jsonl",
            "isolation-0123456789ABCDEF0123456789abcdef.jsonl",
            "isolation-0123456789abcdef0123456789abcdef-s.jsonl",
            "isolation-0123456789abcdef0123456789abcdef-s012.jsonl",
            "isolation-0123456789abcdef0123456789abcdef.jsonl.bak",
            "other.jsonl",
        ] {
            assert!(!is_record_name(bad), "{bad}");
        }
    }

    #[test]
    fn retention_deletes_only_old_matching_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        let mut names = vec![];
        for i in 0..55u32 {
            let name = record_name(&format!("{i:032x}"), Some(100 + i as i32));
            let p = dir.path().join(&name);
            std::fs::write(&p, b"{}\n").unwrap();
            let t = now - std::time::Duration::from_secs(1000 - i as u64);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
            names.push(name);
        }
        std::fs::write(dir.path().join("notes.txt"), b"keep").unwrap();
        std::fs::create_dir(dir.path().join(record_name(&"f".repeat(32), None))).unwrap();
        std::os::unix::fs::symlink(
            "/etc/hosts",
            dir.path().join(record_name(&"e".repeat(32), None)),
        )
        .unwrap();
        let removed = prune(dir.path(), KEEP);
        // 55 records, keep 49 so the new one makes 50: the 6 oldest go.
        assert_eq!(removed.len(), 6);
        for n in &names[..6] {
            assert!(removed.contains(n), "{n} should be removed");
        }
        assert!(dir.path().join("notes.txt").exists());
        assert!(dir.path().join(record_name(&"f".repeat(32), None)).is_dir());
        assert!(
            std::fs::symlink_metadata(dir.path().join(record_name(&"e".repeat(32), None))).is_ok()
        );
    }
}
