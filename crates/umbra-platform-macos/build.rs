//! Build the userspace-routing interposer this backend loads into a tracee.
//!
//! `interpose/umbra_interpose.c` is compiled here and embedded in the backend
//! binary by `include_bytes!`. Three properties are the reason it is built and
//! embedded rather than shipped as a file beside the executable, and each one is
//! enforced here rather than trusted:
//!
//! 1. **The loaded library cannot be swapped.** The bytes travel inside the
//!    provider binary, so there is no path on disk for anything else to occupy.
//!    This is the same reasoning `umbra-supervisor/src/sandbox.rs` gives for
//!    embedding the one Seatbelt template: a runtime file read, a stale copy or a
//!    missing directory must not be able to change what gets installed.
//! 2. **It matches the backend that loads it.** The trap number and operand
//!    layout are a wire format shared with `src/abi.rs`. Building from source in
//!    the same `cargo build` makes a mismatch impossible rather than unlikely.
//! 3. **It is universal.** The supervised launch execs `/usr/bin/sandbox-exec`
//!    first, and on Apple silicon that binary is **arm64e** while the target is
//!    ordinarily thin arm64. dyld loads an inserted dylib into *both* processes,
//!    and refuses fatally -- terminating the launch -- if a slice for the running
//!    architecture is missing. Measured: a thin arm64 dylib kills the launch at
//!    the installer with "incompatible architecture (have 'arm64', need
//!    'arm64e')". So both slices are required, and their presence is checked
//!    below rather than assumed from the compiler flags.
//!
//! Nothing here runs on a host this backend does not support: the tracer itself
//! is `cfg`-gated to macOS arm64, and so is this.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The one source file. A second would be a second library.
const SOURCE: &str = "interpose/umbra_interpose.c";

/// Slices dyld may be asked for across the installer/target exec chain.
const ARCHITECTURES: &[&str] = &["arm64", "arm64e"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={SOURCE}");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "macos" || target_arch != "aarch64" {
        // The backend compiles to `unsupported::MacosTraceBackend` here and can
        // load nothing, so there is nothing to build. `src/native.rs` is the only
        // consumer and is gated on the same pair.
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dylib = out.join("libumbra_interpose.dylib");

    let mut clang = Command::new("clang");
    for architecture in ARCHITECTURES {
        clang.args(["-arch", architecture]);
    }
    clang
        .args([
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            // The interposer is loaded into a process umbra does not own the
            // source of, so it must not pull in anything beyond libSystem.
            "-dynamiclib",
        ])
        .arg("-o")
        .arg(&dylib)
        .arg(manifest.join(SOURCE));
    run(&mut clang, "compiling the interposer");

    verify_slices(&dylib);
    verify_signature(&dylib);

    println!("cargo:rustc-env=UMBRA_INTERPOSE_DYLIB={}", dylib.display());
}

/// Refuse a build whose dylib is missing a slice dyld will ask for.
///
/// Checked rather than inferred from the `-arch` flags: a toolchain that silently
/// dropped `arm64e` would otherwise produce a backend that fails at the
/// installer exec, at run time, on a host that had built cleanly.
fn verify_slices(dylib: &Path) {
    let output = Command::new("/usr/bin/lipo")
        .arg("-archs")
        .arg(dylib)
        .output()
        .unwrap_or_else(|error| panic!("cannot run lipo: {error}"));
    assert!(
        output.status.success(),
        "lipo -archs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let archs = String::from_utf8_lossy(&output.stdout);
    let present: Vec<&str> = archs.split_whitespace().collect();
    for architecture in ARCHITECTURES {
        assert!(
            present.contains(architecture),
            "the interposer has no {architecture} slice (found {present:?}); dyld refuses \
             an inserted dylib with no slice for the running image and the launch dies \
             at the sandbox installer"
        );
    }
}

/// Refuse an unsigned dylib.
///
/// Every arm64 image must carry at least an ad-hoc signature to be mapped, and
/// the linker supplies one. Asserting it here turns "dyld could not load the
/// interposer" -- which surfaces as a dead tracee -- into a build failure naming
/// the cause.
fn verify_signature(dylib: &Path) {
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(dylib)
        .output()
        .unwrap_or_else(|error| panic!("cannot run codesign: {error}"));
    assert!(
        output.status.success(),
        "the interposer is not validly signed, so dyld cannot map it: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run(command: &mut Command, what: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{what}: cannot run: {error}"));
    assert!(
        output.status.success(),
        "{what} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
