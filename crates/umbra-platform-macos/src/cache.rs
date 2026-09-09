//! Content-addressed, verified ad-hoc signed twins; originals are never modified.
use crate::{error, Options};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use umbra_core::Result;
pub const ENTITLEMENTS: &str = include_str!("../ent.plist");
pub fn command(command: &mut Command, deadline: Instant) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| error("signing command", e))?;
    loop {
        if child
            .try_wait()
            .map_err(|e| error("signing command", e))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error("watchdog", "signing command timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
fn hash(path: &Path, deadline: Instant) -> Result<String> {
    let out = command(
        Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(path),
        deadline,
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
fn verify(path: &Path, deadline: Instant) -> Result<()> {
    command(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(path),
        deadline,
    )?;
    let out = command(
        Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(path),
        deadline,
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
pub fn resign(source: &Path, options: &Options, deadline: Instant) -> Result<PathBuf> {
    let source = fs::canonicalize(source).map_err(|e| error("twin source", e))?;
    let root = match &options.twin_cache {
        Some(p) => p.clone(),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| error("twin cache", "HOME unset"))?,
        )
        .join("Library/Caches/umbra/twins"),
    };
    fs::create_dir_all(&root).map_err(|e| error("twin cache", e))?;
    let root = fs::canonicalize(&root).map_err(|e| error("twin cache", e))?;
    let digest = hash(&source, deadline)?;
    // Executables may exec/spawn their already resigned self; avoid chains of twins.
    if source.starts_with(&root) {
        let meta = source.with_file_name(format!(
            "{}.sha256",
            source.file_name().unwrap().to_string_lossy()
        ));
        if fs::read_to_string(meta).is_ok_and(|s| s.trim() == digest)
            && verify(&source, deadline).is_ok()
        {
            return Ok(source);
        }
    }
    let folder = root.join(&digest);
    fs::create_dir_all(&folder).map_err(|e| error("twin cache", e))?;
    let name = source
        .file_name()
        .ok_or_else(|| error("twin", "missing basename"))?;
    let twin = folder.join(name);
    let mut meta_name = name.to_os_string();
    meta_name.push(".sha256");
    let meta = folder.join(meta_name);
    if let Ok(saved) = fs::read_to_string(&meta) {
        if hash(&twin, deadline).is_ok_and(|h| h == saved.trim()) && verify(&twin, deadline).is_ok()
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
        if hash(&candidate, deadline)? != digest {
            return Err(error("twin", "source changed while copying"));
        }
        let ent = staging.join("ent.plist");
        fs::write(&ent, ENTITLEMENTS).map_err(|e| error("entitlements", e))?;
        command(
            Command::new("/usr/bin/codesign")
                .args(["-f", "-s", "-", "--entitlements"])
                .arg(&ent)
                .arg("--preserve-metadata=identifier,flags,runtime")
                .arg(&candidate),
            deadline,
        )?;
        verify(&candidate, deadline)?;
        let signed_hash = hash(&candidate, deadline)?;
        fs::write(staging.join("metadata"), signed_hash).map_err(|e| error("twin metadata", e))?;
        fs::rename(candidate, &twin).map_err(|e| error("twin publish", e))?;
        fs::rename(staging.join("metadata"), meta).map_err(|e| error("twin metadata", e))?;
        Ok(twin)
    })();
    let _ = fs::remove_dir_all(staging);
    result
}
