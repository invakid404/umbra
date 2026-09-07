//! Byte-preserving mount table and Darwin negotiated-option validation.
use crate::native::{error, io};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use umbra_core::{ErrorKind, Result};

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|s| s == needle)
}
fn unescape(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = ((bytes[i + 1] - b'0') as u16) * 64
                + ((bytes[i + 2] - b'0') as u16) * 8
                + (bytes[i + 3] - b'0') as u16;
            if value <= 255 {
                result.push(value as u8);
                i += 4;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    result
}
fn version(options: &[u8]) -> Option<bool> {
    options
        .split(|b| b.is_ascii_whitespace() || *b == b',' || *b == b'(' || *b == b')')
        .find_map(|part| {
            part.strip_prefix(b"vers=")
                .or_else(|| part.strip_prefix(b"nfsvers="))
                .map(|v| v == b"4" || v.starts_with(b"4."))
        })
}
pub(crate) fn validate_output(root: &[u8], mounts: &[u8], details: &[u8]) -> Result<()> {
    let mut matched = None;
    for line in mounts.split(|b| *b == b'\n') {
        let Some(start) = find(line, b" on ") else {
            continue;
        };
        let rest = &line[start + 4..];
        let Some(end) = find(rest, b" (") else {
            continue;
        };
        let location = &rest[..end];
        let (path, fs) = if let Some(i) = find(location, b" type ") {
            (&location[..i], &location[i + 6..])
        } else {
            (
                location,
                rest[end + 2..]
                    .split(|b| *b == b',' || *b == b')')
                    .next()
                    .unwrap_or_default(),
            )
        };
        if unescape(path) == root {
            matched = Some((fs, &rest[end + 2..]));
        }
    }
    let (fs, options) = matched.ok_or_else(|| {
        error(
            ErrorKind::StorageUnavailable,
            "mount",
            "configured root is not an exact mounted filesystem",
        )
    })?;
    if fs != b"nfs" && fs != b"nfs4" {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "mount",
            "mount is not NFS",
        ));
    }
    let mut negotiated = version(options);
    // Darwin mount(8) omits NFS version. Trust only Current parameters for the
    // exact mount, never another export or merely the originally requested vers.
    let mut in_mount = false;
    let mut current = false;
    for line in details.split(|b| *b == b'\n') {
        if let Some(i) = find(line, b" from ") {
            in_mount = unescape(&line[..i]) == root;
            current = false;
        }
        if in_mount && find(line, b"-- Current mount parameters:").is_some() {
            current = true;
        }
        if in_mount && current {
            if let Some(v) = version(line) {
                negotiated = Some(v);
            }
        }
    }
    match negotiated {
        Some(true) => Ok(()),
        _ => Err(error(
            ErrorKind::UnsupportedCapability,
            "mount",
            "NFSv4 (vers=4) was not verified",
        )),
    }
}
pub(crate) fn validate(root: &Path) -> Result<()> {
    let output = Command::new("/sbin/mount")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| io("mount", e))?;
    if !output.status.success() {
        return Err(error(
            ErrorKind::StorageUnavailable,
            "mount",
            "mount command failed",
        ));
    }
    #[cfg(target_os = "macos")]
    let details = {
        let output = Command::new("/usr/bin/nfsstat")
            .arg("-m")
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| io("nfsstat", e))?;
        if output.status.success() {
            output.stdout
        } else {
            Vec::new()
        }
    };
    #[cfg(not(target_os = "macos"))]
    let details = Vec::new();
    validate_output(root.as_os_str().as_bytes(), &output.stdout, &details)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_mount_and_protocol() {
        assert!(validate_output(b"/nfs", b"host:/x on /nfs type nfs (rw,vers=4.1)\n", b"").is_ok());
        for line in [
            b"host:/x on /nfs (nfs, vers=3)".as_slice(),
            b"disk on /nfs (apfs, vers=4)",
            b"host:/x on /nfs-other (nfs, vers=4)",
            b"host:/x on /nfs (nfs)",
        ] {
            assert!(validate_output(b"/nfs", line, b"").is_err());
        }
        assert!(validate_output(
            b"/nfs space/\xff",
            b"host:/x on /nfs\\040space/\xff (nfs, vers=4)",
            b""
        )
        .is_ok());
    }
    #[test]
    fn darwin_current_options_are_scoped_to_mount() {
        let mounts = b"host:/x on /nfs (nfs, nodev)";
        let details = b"/other from h:/o\n -- Current mount parameters:\n vers=4.0\n/nfs from h:/x\n -- Original mount options:\n vers=4.0\n -- Current mount parameters:\n vers=3,tcp\n";
        assert!(validate_output(b"/nfs", mounts, details).is_err());
        assert!(validate_output(
            b"/nfs",
            mounts,
            b"/nfs from h:/x\n -- Current mount parameters:\n NFS parameters: vers=4.0,tcp\n"
        )
        .is_ok());
    }
}
