//! Build script for the `transport-raw` (`raw_rpc`) libnfs FFI.
//!
//! Three jobs, all of them scope-lock enforcement rather than convenience:
//!
//! 1. **Pin the dependency.** `libnfs.pin` carries one commit SHA. The libnfs
//!    checkout is rejected unless it is at exactly that commit, so the generated
//!    ABI cannot drift under the transport without a visible diff to the pin.
//! 2. **Allowlist the ABI.** Only the public raw-RPC/XDR and task primitives are
//!    generated. The managed lifecycle API (`nfs_context` and every `nfs_*`
//!    entry point) is parsed — `libnfs-raw.h` needs `libnfs.h` for `rpc_cb` —
//!    but never emitted.
//! 3. **Prove the allowlist held.** The generated bindings are scanned after the
//!    fact and the build fails if a managed symbol appears. An allowlist that is
//!    only asserted in a builder call is a comment; this makes it a gate.
//!
//! Nothing here runs unless the `transport-raw` feature is enabled.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Public libnfs headers the binding is allowed to read.
///
/// `libnfs.h` is included only because `libnfs-raw.h` uses its `rpc_cb`
/// typedef; nothing from its managed surface is emitted (see `BLOCKED_*`).
const HEADERS: &[&str] = &[
    "include/nfsc/libnfs-zdr.h",
    "include/nfsc/libnfs.h",
    "include/nfsc/libnfs-raw.h",
    "nfs4/libnfs-raw-nfs4.h",
];

/// Raw RPC context, connection and event-pump primitives.
const ALLOWED_RPC_FUNCTIONS: &[&str] = &[
    "rpc_init_context",
    "rpc_destroy_context",
    "rpc_connect_async",
    "rpc_disconnect",
    "rpc_get_fd",
    "rpc_which_events",
    "rpc_service",
    "rpc_queue_length",
    "rpc_get_error",
    "rpc_set_auth",
    "rpc_cancel_pdu",
];

/// Task primitives that queue one NFSv4 COMPOUND PDU.
const ALLOWED_TASK_FUNCTIONS: &[&str] = &[
    "rpc_nfs4_compound_task",
    "rpc_nfs4_read_task",
    "rpc_nfs4_write_task",
];

/// AUTH_SYS credential construction from the public ZDR surface.
const ALLOWED_AUTH_FUNCTIONS: &[&str] = &["libnfs_authunix_create", "libnfs_auth_destroy"];

/// XDR argument/result roots. bindgen pulls their transitive NFSv4 types in.
const ALLOWED_TYPES: &[&str] = &[
    "rpc_context",
    "rpc_pdu",
    "AUTH",
    "rpc_cb",
    "COMPOUND4args",
    "COMPOUND4res",
    "nfs_argop4",
    "nfs_resop4",
    "nfs_opnum4",
    "nfsstat4",
];

/// RPC status constants used to classify a completion.
const ALLOWED_VARS: &[&str] = &["RPC_STATUS_.*"];

/// A generated `pub fn` may never start with these. Every managed-lifecycle
/// entry point in libnfs is `nfs_*` or `nfs4_*`; the raw surface is `rpc_*`,
/// and the two ZDR auth helpers are `libnfs_*`.
const BLOCKED_FUNCTION_PREFIXES: &[&str] = &["nfs_", "nfs4_", "nfs3_", "nfs2_", "mount_", "nlm_"];

/// Managed-lifecycle types that must never reach the binding. `nfs_argop4` and
/// friends are XDR payload types and are deliberately not in this list.
const BLOCKED_TYPES: &[&str] = &["nfs_context", "nfsfh", "nfs_url", "nfs_stat_64", "nfsdir"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=libnfs.pin");
    println!("cargo:rerun-if-env-changed=UMBRA_LIBNFS_SRC");

    if std::env::var_os("CARGO_FEATURE_TRANSPORT_RAW").is_none() {
        // Default build: no FFI, no libnfs, nothing to generate.
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let source = libnfs_source(&manifest);

    verify_pin(&manifest, &source);
    let build = build_libnfs(&source, &out);
    generate_bindings(&source, &build, &out);
}

/// Where the pinned libnfs checkout lives. `UMBRA_LIBNFS_SRC` overrides the
/// in-repo vendored path so a packager can supply its own checkout — the pin
/// check below applies to it just the same.
fn libnfs_source(manifest: &Path) -> PathBuf {
    if let Some(explicit) = std::env::var_os("UMBRA_LIBNFS_SRC") {
        return PathBuf::from(explicit);
    }
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crate lives at <root>/crates/<name>")
        .join("third_party/libnfs")
}

/// Refuse to build against anything but the pinned commit.
fn verify_pin(manifest: &Path, source: &Path) {
    let pin_file = manifest.join("libnfs.pin");
    let pinned = std::fs::read_to_string(&pin_file)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", pin_file.display()))
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("libnfs.pin carries one commit SHA")
        .to_owned();

    if !source.join("include/nfsc/libnfs-raw.h").is_file() {
        panic!(
            "libnfs source not found at {}.\n\
             Check it out at the pinned commit:\n  \
             git clone https://github.com/sahlberg/libnfs {0}\n  \
             git -C {0} checkout {pinned}\n\
             or point UMBRA_LIBNFS_SRC at an existing checkout of that commit.",
            source.display()
        );
    }

    let head = Command::new("git")
        .args(["-C", &source.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|error| panic!("cannot run git in {}: {error}", source.display()));
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();

    assert_eq!(
        head,
        pinned,
        "libnfs at {} is commit {head}, but libnfs.pin requires {pinned}.\n\
         The raw-RPC ABI is pinned deliberately: change libnfs.pin in the same \
         commit that revalidates the binding, never silently.",
        source.display()
    );
    println!("cargo:rustc-env=UMBRA_LIBNFS_PIN={pinned}");
}

/// Build libnfs as a static archive with its managed extras switched off.
fn build_libnfs(source: &Path, out: &Path) -> PathBuf {
    let build = out.join("libnfs-build");
    run(
        Command::new("cmake")
            .arg("-S")
            .arg(source)
            .arg("-B")
            .arg(&build)
            .args([
                "-DCMAKE_BUILD_TYPE=Release",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
                "-DENABLE_TESTS=OFF",
                "-DENABLE_UTILS=OFF",
                "-DENABLE_EXAMPLES=OFF",
                "-DENABLE_DOCUMENTATION=OFF",
            ]),
        "cmake configure",
    );
    run(
        Command::new("cmake")
            .arg("--build")
            .arg(&build)
            .args(["--parallel", "4"]),
        "cmake build",
    );

    println!(
        "cargo:rustc-link-search=native={}",
        build.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=nfs");
    build
}

/// Generate the binding, then prove the allowlist actually held.
fn generate_bindings(source: &Path, build: &Path, out: &Path) {
    let wrapper = out.join("wrapper.h");
    let mut contents = String::from("/* generated by build.rs; allowlisted raw surface only */\n");
    for header in HEADERS {
        contents.push_str(&format!("#include \"{}\"\n", source.join(header).display()));
    }
    std::fs::write(&wrapper, contents).expect("write wrapper header");

    let mut builder = bindgen::Builder::default()
        .header(wrapper.to_string_lossy())
        // `config.h` lives in the build tree; the headers expect it on the path.
        .clang_arg(format!("-I{}", build.display()))
        .clang_arg(format!("-I{}", source.join("include").display()))
        .clang_arg(format!("-I{}", source.join("nfs4").display()))
        .clang_arg("-DHAVE_CONFIG_H")
        .use_core()
        .ctypes_prefix("::core::ffi")
        .derive_default(true)
        .derive_debug(false)
        .generate_comments(false)
        .layout_tests(false)
        .allowlist_recursively(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for function in ALLOWED_RPC_FUNCTIONS
        .iter()
        .chain(ALLOWED_TASK_FUNCTIONS)
        .chain(ALLOWED_AUTH_FUNCTIONS)
    {
        builder = builder.allowlist_function(function);
    }
    for kind in ALLOWED_TYPES {
        builder = builder.allowlist_type(kind);
    }
    for var in ALLOWED_VARS {
        builder = builder.allowlist_var(var);
    }
    for blocked in BLOCKED_TYPES {
        builder = builder.blocklist_type(blocked);
    }

    let bindings = builder
        .generate()
        .expect("bindgen generated the raw surface");
    let path = out.join("libnfs_raw.rs");
    bindings.write_to_file(&path).expect("write bindings");

    enforce_allowlist(&path);
}

/// Fail the build if a managed-lifecycle symbol reached the binding.
///
/// The allowlist is inclusive, so this should be unreachable. It exists because
/// "we passed an allowlist to bindgen" is a claim about intent, and a scope-lock
/// wants a claim about the artefact.
fn enforce_allowlist(path: &Path) {
    let generated = std::fs::read_to_string(path).expect("read generated bindings");
    let mut leaked = BTreeSet::new();

    for line in generated.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pub fn ") {
            let name = rest
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            if BLOCKED_FUNCTION_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                leaked.insert(format!("fn {name}"));
            }
        }
        for blocked in BLOCKED_TYPES {
            if line.starts_with(&format!("pub struct {blocked}"))
                || line.starts_with(&format!("pub type {blocked}"))
            {
                leaked.insert(format!("type {blocked}"));
            }
        }
    }

    assert!(
        leaked.is_empty(),
        "managed-lifecycle libnfs symbols leaked into the generated binding: {leaked:?}.\n\
         `raw_rpc` is scoped to the public raw RPC/XDR and task surface only."
    );

    let emitted = generated
        .lines()
        .filter(|line| line.trim_start().starts_with("pub fn "))
        .count();
    println!("cargo:warning=libnfs raw binding: {emitted} functions emitted");
}

fn run(command: &mut Command, what: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("{what} failed to start: {error}"));
    assert!(status.success(), "{what} failed with {status}");
}
