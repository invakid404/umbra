//! Content-addressed, verified ad-hoc signed twins; originals are never modified.
use crate::{error, Options};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use umbra_core::Result;
pub const ENTITLEMENTS: &str = include_str!("../ent.plist");
pub fn cached<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.operation == "admission" || error.operation == "platform.shutdown" => {
            Err(error)
        }
        Err(_) => Ok(None),
    }
}
pub fn command_with_ledger(
    command: &mut Command,
    deadline: Instant,
    ledger: Option<&umbra_platform::shutdown::Ledger>,
) -> Result<Output> {
    use umbra_core::shutdown::{Identity, Proof, ResourceKind};
    let slot = ledger
        .map(|ledger| ledger.reserve(ResourceKind::Tracee, None, 1, 0))
        .transpose()?;
    umbra_core::cost_snapshot!("preparation-command-before", None, &[]);
    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            if let Some(slot) = &slot {
                slot.creation_failed();
            }
            return Err(error("signing command", e));
        }
    };
    umbra_core::cost_snapshot!("preparation-command-after", Some(child.id()), &[]);
    if let Some(slot) = &slot {
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(serde_json::json!({
            "event": "acquired",
            "kind": "Tracee",
            "pid": child.id(),
            "preparation_helper": true,
        }));
        if let Err(error) = slot.acquired(Identity {
            pid: Some(child.id() as i32),
            birth: crate::native::bsd_info(child.id() as i32)
                .map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec)),
        }) {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                let _ = slot.confirm(Proof::Accounted {
                    evidence: format!("helper wait after registration failure: {status}"),
                    root_owned: true,
                    root_collected: true,
                });
            }
            return Err(error);
        }
    }
    let confirm = |status: std::process::ExitStatus| -> Result<()> {
        umbra_core::cost_snapshot!("preparation-command-collected", None, &[]);
        if let Some(slot) = &slot {
            slot.confirm(Proof::Accounted {
                evidence: format!("preparation helper parent wait: {status}"),
                root_owned: true,
                root_collected: true,
            })?;
        }
        Ok(())
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(cause) => {
                let _ = child.kill();
                if let Ok(status) = child.wait() {
                    confirm(status)?;
                }
                return Err(error("signing command wait", cause));
            }
        }
        let expired = (|| -> Result<bool> {
            if Instant::now() >= deadline {
                return Ok(true);
            }
            let cutoff = ledger
                .map(|ledger| ledger.admission_cutoff())
                .transpose()?
                .flatten();
            match cutoff {
                Some(cutoff) => Ok(crate::clock::boot_nanos()? >= cutoff),
                None => Ok(false),
            }
        })();
        if !matches!(expired, Ok(false)) {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                confirm(status)?;
            }
            expired?;
            return Err(error("watchdog", "signing command timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The parent wait is the terminal evidence. Preserve it even when reading
    // the helper's captured output subsequently fails.
    confirm(status)?;
    let output = child
        .wait_with_output()
        .map_err(|e| error("signing command", e))?;
    if !output.status.success() {
        return Err(error(
            "signing command",
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(output)
}
fn hash(
    path: &Path,
    deadline: Instant,
    ledger: Option<&umbra_platform::shutdown::Ledger>,
) -> Result<String> {
    let out = command_with_ledger(
        Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(path),
        deadline,
        ledger,
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let digest = text
        .split_whitespace()
        .next()
        .ok_or_else(|| error("sha256", "no digest"))?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error("sha256", "invalid digest"));
    }
    Ok(digest.to_owned())
}
fn verify(
    path: &Path,
    deadline: Instant,
    ledger: Option<&umbra_platform::shutdown::Ledger>,
) -> Result<()> {
    command_with_ledger(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(path),
        deadline,
        ledger,
    )?;
    let out = command_with_ledger(
        Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(path),
        deadline,
        ledger,
    )?;
    let text = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<String>();
    for key in [
        "com.apple.security.get-task-allow",
        "com.apple.security.cs.allow-jit",
        "com.apple.security.cs.allow-unsigned-executable-memory",
    ] {
        if !text.contains(&format!("<key>{key}</key><true/>")) {
            return Err(error("codesign", "missing twin entitlement"));
        }
    }
    Ok(())
}
pub fn resign(
    source: &Path,
    options: &Options,
    deadline: Instant,
    ledger: Option<&umbra_platform::shutdown::Ledger>,
) -> Result<PathBuf> {
    let source = fs::canonicalize(source).map_err(|e| error("twin source", e))?;
    let root = match &options.twin_cache {
        Some(p) => p.clone(),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| error("twin cache", "HOME unset"))?,
        )
        .join("Library/Caches/umbra/twins"),
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .map_err(|e| error("twin cache", e))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|e| error("twin cache", e))?;
    let root = fs::canonicalize(&root).map_err(|e| error("twin cache", e))?;
    let digest = hash(&source, deadline, ledger)?;
    // Executables may exec/spawn their already resigned self; avoid chains of twins.
    if source.starts_with(&root) {
        let meta = source.with_file_name(format!(
            "{}.sha256",
            source.file_name().unwrap().to_string_lossy()
        ));
        if fs::read_to_string(meta).is_ok_and(|s| s.trim() == digest)
            && cached(verify(&source, deadline, ledger))?.is_some()
        {
            return Ok(source);
        }
    }
    let folder = root.join(&digest);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(|e| error("twin cache", e))?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))
        .map_err(|e| error("twin cache", e))?;
    let name = source
        .file_name()
        .ok_or_else(|| error("twin", "missing basename"))?;
    let twin = folder.join(name);
    let mut meta_name = name.to_os_string();
    meta_name.push(".sha256");
    let meta = folder.join(meta_name);
    if let Ok(saved) = fs::read_to_string(&meta) {
        if cached(hash(&twin, deadline, ledger))?.is_some_and(|h| h == saved.trim())
            && cached(verify(&twin, deadline, ledger))?.is_some()
        {
            return Ok(twin);
        }
    }
    let staging = folder.join(format!(
        ".sign-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&staging).map_err(|e| error("twin stage", e))?;
    let result = (|| {
        let candidate = staging.join(name);
        fs::copy(&source, &candidate).map_err(|e| error("twin copy", e))?;
        if hash(&candidate, deadline, ledger)? != digest {
            return Err(error("twin", "source changed while copying"));
        }
        let ent = staging.join("ent.plist");
        fs::write(&ent, ENTITLEMENTS).map_err(|e| error("entitlements", e))?;
        command_with_ledger(
            Command::new("/usr/bin/codesign")
                .args(["-f", "-s", "-", "--entitlements"])
                .arg(&ent)
                .arg("--preserve-metadata=identifier,flags,runtime")
                .arg(&candidate),
            deadline,
            ledger,
        )?;
        verify(&candidate, deadline, ledger)?;
        let signed_hash = hash(&candidate, deadline, ledger)?;
        fs::write(staging.join("metadata"), signed_hash).map_err(|e| error("twin metadata", e))?;
        fs::rename(candidate, &twin).map_err(|e| error("twin publish", e))?;
        fs::rename(staging.join("metadata"), meta).map_err(|e| error("twin metadata", e))?;
        Ok(twin)
    })();
    let _ = fs::remove_dir_all(staging);
    result
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use umbra_core::shutdown::Nonce;
    use umbra_core::{
        AdmissionError, AdmissionLedger, AdmissionOperation, AdmissionRequest, AdmissionTransport,
    };
    struct Transport {
        ledger: Arc<Mutex<AdmissionLedger>>,
        sequence: u64,
    }
    #[test]
    fn helper_wait_obeys_managed_cutoff_before_local_timeout() {
        let authority = Arc::new(Mutex::new(
            AdmissionLedger::new(Nonce::from_u128(1), 1, 1).unwrap(),
        ));
        let ledger = umbra_platform::shutdown::Ledger::new();
        let cutoff = crate::clock::boot_nanos().unwrap() + 50_000_000;
        ledger
            .install_admission(
                umbra_platform::admission::Admission::new(Box::new(Transport {
                    ledger: authority.clone(),
                    sequence: 0,
                })),
                Box::new(move || Ok(cutoff)),
            )
            .unwrap();
        let started = Instant::now();
        let result = command_with_ledger(
            Command::new("/bin/sleep").arg("2"),
            started + Duration::from_secs(5),
            Some(&ledger),
        );
        assert!(result.is_err(), "managed cutoff must interrupt the helper");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            authority.lock().unwrap().current(),
            0,
            "parent wait must retire the interrupted helper"
        );
    }
    impl AdmissionTransport for Transport {
        fn exchange(
            &mut self,
            operation: AdmissionOperation,
            _: u64,
        ) -> std::io::Result<std::result::Result<u64, AdmissionError>> {
            self.sequence += 1;
            Ok(self.ledger.lock().unwrap().apply(AdmissionRequest {
                nonce: Nonce::from_u128(1),
                generation: 1,
                sequence: self.sequence,
                operation,
            }))
        }
    }
    fn ledger() -> (
        umbra_platform::shutdown::Ledger,
        Arc<Mutex<AdmissionLedger>>,
    ) {
        let authority = Arc::new(Mutex::new(
            AdmissionLedger::new(Nonce::from_u128(1), 1, 1).unwrap(),
        ));
        let ledger = umbra_platform::shutdown::Ledger::new();
        ledger
            .install_admission(
                umbra_platform::admission::Admission::new(Box::new(Transport {
                    ledger: authority.clone(),
                    sequence: 0,
                })),
                Box::new(|| Ok(u64::MAX)),
            )
            .unwrap();
        (ledger, authority)
    }
    #[test]
    fn helper_charge_recycles_after_real_parent_wait() {
        let (ledger, authority) = ledger();
        for executable in ["/usr/bin/false", "/umbra/nonexistent/helper"] {
            assert!(command_with_ledger(
                &mut Command::new(executable),
                Instant::now() + Duration::from_secs(2),
                Some(&ledger),
            )
            .is_err());
            assert_eq!(authority.lock().unwrap().current(), 0);
        }
        for _ in 0..3 {
            command_with_ledger(
                &mut Command::new("/usr/bin/true"),
                Instant::now() + Duration::from_secs(2),
                Some(&ledger),
            )
            .unwrap();
            assert_eq!(authority.lock().unwrap().current(), 0);
        }
        assert!(command_with_ledger(
            Command::new("/bin/sleep").arg("2"),
            Instant::now() + Duration::from_millis(20),
            Some(&ledger)
        )
        .is_err());
        assert_eq!(authority.lock().unwrap().current(), 0);
        let accepted = ledger.freeze(Nonce::new_v4(), Duration::ZERO).unwrap();
        assert!(accepted
            .confirms(&ledger.final_snapshot().unwrap())
            .unwrap());
    }
    #[test]
    fn helper_refusal_precedes_spawn_side_effect() {
        let (ledger, authority) = ledger();
        authority.lock().unwrap().freeze();
        let path = std::env::temp_dir().join(format!("umbra-refused-helper-{}", Nonce::new_v4()));
        let result = command_with_ledger(
            Command::new("/usr/bin/touch").arg(&path),
            Instant::now() + Duration::from_secs(2),
            Some(&ledger),
        );
        let exists = path.exists();
        if exists {
            let _ = std::fs::remove_file(&path);
        }
        assert!(result.is_err());
        assert!(!exists, "refused helper must not execute");
        assert_eq!(authority.lock().unwrap().current(), 0);
    }
}
