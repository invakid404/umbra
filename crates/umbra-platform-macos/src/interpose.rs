//! Publishing the embedded userspace-routing interposer for dyld to load.
//!
//! The library itself is `interpose/umbra_interpose.c`, compiled by `build.rs`
//! and embedded here by `include_bytes!`. dyld can only load a dylib from a
//! path, so the bytes have to reach the filesystem before a launch; this module
//! is the only place that happens, and it is deliberately shaped like
//! [`crate::cache`]: a content-addressed directory under the twin cache, mode
//! `0700`, written to a staging name and renamed into place, verified before it
//! is handed out.
//!
//! # Why embedded rather than installed beside the executable
//!
//! Because the alternative is a swappable path. `DYLD_INSERT_LIBRARIES` names a
//! file, and dyld loads whatever is at that name into a process umbra is about to
//! stop trusting with host writes. Embedding means the bytes come from the same
//! build as the backend that interprets their trap, so there is no version skew
//! to reason about and no file for anything else to occupy first. It is the same
//! argument `umbra-supervisor/src/sandbox.rs` makes for embedding the one
//! Seatbelt template.
//!
//! Publishing it back out to a cache path does not undo that: the path is derived
//! from the bytes, and [`publish`] re-reads and compares the whole file -- not
//! its length, not its mtime -- before reusing one, so a tampered or truncated
//! copy is replaced rather than loaded.
//!
//! # Not a privileged step and not new setup
//!
//! `~/Library/Caches/umbra` already exists for the resigned twins every
//! supervised launch creates, with the same mode and the same owner. This adds a
//! subdirectory to it.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use umbra_core::Result;

use crate::{cache, error, Options};

/// The interposer, as built by `build.rs` from `interpose/umbra_interpose.c`.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const DYLIB: &[u8] = include_bytes!(env!("UMBRA_INTERPOSE_DYLIB"));

/// The descriptor fence a supervised interposed launch applies.
///
/// `RLIMIT_NOFILE` soft *and* hard for the tracee and every descendant, and the
/// floor of the interposer's virtual descriptor range. See
/// [`umbra_core::LaunchPolicy::descriptor_limit`] for why fencing the kernel's
/// range is what makes a virtual descriptor safe across the whole lifetime
/// rather than only at the instant it is allocated.
///
/// 4096 is chosen rather than derived: it is far above what a supervised command
/// needs, far below the measured default soft limit of 1048576, and reported
/// honestly through `getrlimit`, so a program wanting more concurrent
/// descriptors is refused `EMFILE` rather than silently handed a number in
/// umbra's range.
pub const DESCRIPTOR_LIMIT: u32 = 4096;

/// The value umbra writes into the interposer's `__DATA,__umbra_arm` block to
/// switch it on. `b"UMBRARM1"` read big-endian.
///
/// **A wire format**, mirrored by `UMBRA_ARM_MAGIC` in
/// `interpose/umbra_interpose.c`; `the_arming_magic_matches_the_interposer`
/// asserts the two agree by reading the C source. Not a checksum and not a
/// secret: it is a value a zeroed or half-written block cannot hold by accident,
/// so "armed" is a state umbra put the library into rather than one it drifted
/// into.
///
/// Nothing in the environment can produce it. The interposer used to activate
/// itself from two environment variables, which made "the library is live" a
/// property of the *launch* rather than of umbra's readiness -- and it went live
/// during the library-initializer window, before its traps were breakpointed.
/// See `Session::arm_interposer`.
pub const ARM_MAGIC: u64 = 0x554d_4252_4152_4d31;

/// The dyld variable that loads it.
pub const INSERT_VARIABLE: &str = "DYLD_INSERT_LIBRARIES";

/// Publish the embedded interposer and return the path dyld should load.
///
/// Idempotent across runs and across concurrent launches: the directory name is
/// derived from the bytes, the write goes to a unique staging name first, and a
/// published file that already matches byte for byte is reused.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn publish(options: &Options, deadline: Instant) -> Result<PathBuf> {
    let root = cache_root(options)?.join("interpose");
    make_directory(&root)?;
    let folder = root.join(digest(DYLIB));
    make_directory(&folder)?;
    let published = folder.join("libumbra_interpose.dylib");

    if matches(&published, DYLIB) && verify(&published, deadline).is_ok() {
        return Ok(published);
    }

    // A real nonce, not a clock reading. This used to be
    // `Instant::now().elapsed()`, which is ~0 by construction, so every staging
    // name inside one provider process was `.stage-<pid>-0`: two launches racing
    // here could have one `rename` a file the other was mid-`write` on, handing
    // dyld a truncated dylib. The counter is per process and the process id
    // separates processes, which together is what the module doc claims.
    static STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = folder.join(format!(
        ".stage-{}-{}",
        std::process::id(),
        STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        fs::write(&staging, DYLIB).map_err(|e| error("interposer write", e))?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o600))
            .map_err(|e| error("interposer mode", e))?;
        // Verify the staged copy, not the published one: a signature that does
        // not validate means dyld will refuse to map it, and refusing here names
        // the cause instead of leaving a tracee that dies during image loading.
        verify(&staging, deadline)?;
        fs::rename(&staging, &published).map_err(|e| error("interposer publish", e))?;
        Ok(published.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

/// A host this backend cannot trace loads nothing.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn publish(_options: &Options, _deadline: Instant) -> Result<PathBuf> {
    Err(crate::unsupported(
        "the userspace-routing interposer requires macOS on arm64",
    ))
}

/// Where published copies live. The twin cache root, so one setting moves both.
fn cache_root(options: &Options) -> Result<PathBuf> {
    match &options.twin_cache {
        Some(path) => Ok(path.clone()),
        None => Ok(PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| error("twin cache", "HOME unset"))?,
        )
        .join("Library/Caches/umbra")),
    }
}

fn make_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| error("interposer cache", e))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|e| error("interposer cache", e))
}

/// Whether the published file is byte-identical to what is embedded here.
///
/// The whole file, not its length or its mtime. The published path is derived
/// from the bytes, so a file there that differs is either a truncated write or
/// something else's, and reusing it would load code this build never produced.
fn matches(path: &Path, expected: &[u8]) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut found = Vec::with_capacity(expected.len());
    // One byte past the expectation, so a longer file is rejected rather than
    // compared only over its prefix.
    if file
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut found)
        .is_err()
    {
        return false;
    }
    found == expected
}

/// Refuse a copy dyld would not map.
fn verify(path: &Path, deadline: Instant) -> Result<()> {
    cache::command(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(path),
        deadline,
    )
    .map(|_| ())
}

/// A stable, collision-resistant-enough name for one exact byte string.
///
/// FNV-1a over the bytes, not a cryptographic digest, and that is the right
/// strength for what this name does: it distinguishes *our own* builds from each
/// other so two coexisting umbra versions do not fight over one path. It is not
/// an integrity check and is not relied on as one -- [`matches`] compares every
/// byte before a published file is reused, so a collision costs a rewrite rather
/// than loading the wrong library. Using `/usr/bin/shasum` as [`cache`] does
/// would mean spawning a process to hash bytes already in memory.
fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:016x}-{}", hash, bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_copy_is_reused_only_when_every_byte_matches() {
        let directory = std::env::temp_dir().join(format!(
            "umbra-interpose-match-{}-{}",
            std::process::id(),
            digest(b"seed")
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("copy");

        fs::write(&path, b"abcd").unwrap();
        assert!(matches(&path, b"abcd"));
        // Shorter, longer and different all have to be refused: a prefix compare
        // would accept the longer case and a length compare the different one.
        assert!(!matches(&path, b"abc"));
        assert!(!matches(&path, b"abcde"));
        assert!(!matches(&path, b"abcz"));
        assert!(!matches(&directory.join("absent"), b"abcd"));

        fs::remove_dir_all(&directory).unwrap();
    }

    /// The arming magic is a wire format shared with the C source; a mismatch
    /// would leave the interposer inert with no diagnosis at all, because an
    /// unarmed library is silently a passthrough. Read the C rather than
    /// restating it.
    #[test]
    fn the_arming_magic_matches_the_interposer() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("interpose/umbra_interpose.c"),
        )
        .expect("the interposer source ships beside this crate");
        let literal = source
            .lines()
            .find_map(|line| line.trim().strip_prefix("#define UMBRA_ARM_MAGIC "))
            .expect("the C source defines UMBRA_ARM_MAGIC");
        let literal = literal
            .trim()
            .trim_end_matches("ULL")
            .trim_start_matches("0x");
        assert_eq!(
            u64::from_str_radix(literal, 16).expect("a hex literal"),
            ARM_MAGIC,
            "the Rust and C arming magics disagree, so arming would silently do nothing"
        );
        assert_eq!(ARM_MAGIC.to_be_bytes(), *b"UMBRARM1");
    }

    /// Republishing is idempotent, byte-exact, and leaves no staging residue --
    /// both when the published copy is already there and when it is not.
    ///
    /// **Asserted against `publish`, not against a counter.** An earlier version
    /// declared its own `AtomicU64` and checked four `fetch_add`s differed,
    /// which cannot fail and would have passed unchanged while `publish` still
    /// used `Instant::now().elapsed()` -- the very defect it was added for. This
    /// drives the real function.
    ///
    /// **What it does not pin, stated so the name cannot be read as more than it
    /// is: staging-name *uniqueness*.** Round 3 found the second call taking the
    /// content-addressed early return, so it never reached the staging path at
    /// all; the third call below was added to fix that, and it does exercise a
    /// real write-then-rename. But uniqueness is a property of *concurrent*
    /// publishes, and two sequential ones cannot collide however the name is
    /// derived -- the first staging file is renamed away before the second is
    /// created. So this case would still pass with the old clock-reading nonce.
    /// Uniqueness is **argued** at the `STAGE` counter in `publish` (a per-process
    /// counter plus the pid, which is what the module doc claims) and is not
    /// tested here: a test for it would have to race two launches and would be
    /// flaky rather than convincing.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn republishing_is_idempotent_and_leaves_no_staging_residue() {
        let cache = std::env::temp_dir().join(format!(
            "umbra-interpose-publish-{}-{}",
            std::process::id(),
            digest(DYLIB)
        ));
        let _ = fs::remove_dir_all(&cache);
        let options = Options {
            twin_cache: Some(cache.clone()),
            ..Options::default()
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        // First publish: nothing is there, so this takes the staging path.
        let first = publish(&options, deadline).expect("first publish");
        // Second: the published copy already matches, so this takes the
        // content-addressed early return and writes nothing.
        let second = publish(&options, deadline).expect("second publish");
        assert_eq!(first, second, "the published path is content-addressed");
        assert!(
            matches(&first, DYLIB),
            "the published copy is not the bytes"
        );
        assert!(residue(&first).is_empty(), "residue after the early return");

        // Third: remove the published file and publish again, so the staging
        // write-then-rename runs a second time in this process -- which is the
        // half the two calls above do not reach, and the half that can leave a
        // `.stage-*` file behind or hand back the wrong bytes.
        fs::remove_file(&first).unwrap();
        let third = publish(&options, deadline).expect("republish after removal");
        assert_eq!(
            third, first,
            "the path is derived from the bytes, not the run"
        );
        assert!(
            matches(&third, DYLIB),
            "the republished copy is not the bytes"
        );
        assert!(
            residue(&third).is_empty(),
            "staging residue left behind: {:?}",
            residue(&third)
        );

        fs::remove_dir_all(&cache).unwrap();
    }

    /// The `.stage-*` files left in a published dylib's directory, if any.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn residue(published: &std::path::Path) -> Vec<std::ffi::OsString> {
        fs::read_dir(published.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".stage-"))
            .collect()
    }

    #[test]
    fn the_digest_separates_different_byte_strings_and_lengths() {
        assert_ne!(digest(b"a"), digest(b"b"));
        assert_ne!(digest(b"a"), digest(b"aa"));
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert!(digest(b"abc").ends_with("-3"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn the_embedded_interposer_carries_both_slices_dyld_can_ask_for() {
        // `build.rs` asserts this at build time; asserting it here too means a
        // stale `OUT_DIR` artifact cannot survive into a test run. The magic is
        // `FAT_CIGAM_64`/`FAT_CIGAM`: a universal header, big-endian on disk.
        assert!(DYLIB.len() > 4, "the interposer is empty");
        let magic = u32::from_be_bytes(DYLIB[..4].try_into().unwrap());
        assert!(
            magic == 0xcafe_babe || magic == 0xcafe_babf,
            "the interposer is not a universal binary (magic {magic:#x}); dyld refuses an \
             inserted dylib with no slice for the running image, and the supervised launch \
             runs an arm64e installer before a thin arm64 target"
        );
        let count = u32::from_be_bytes(DYLIB[4..8].try_into().unwrap());
        assert!(count >= 2, "expected at least two slices, found {count}");
    }
}
