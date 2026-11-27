use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=tests/support/socket_inventory.c");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || env::var_os("CARGO_FEATURE_MANAGED_ENDPOINT_PROBE").is_none()
    {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported inspection architecture: {other}"),
    };
    assert!(Command::new("clang")
        .args([
            "-arch",
            arch,
            "-Wall",
            "-Wextra",
            "-Werror",
            "-c",
            "tests/support/socket_inventory.c",
            "-o"
        ])
        .arg(out.join("socket_inventory.o"))
        .status()
        .unwrap()
        .success());
    assert!(Command::new("ar")
        .arg("crs")
        .arg(out.join("libumbra_socket_inventory.a"))
        .arg(out.join("socket_inventory.o"))
        .status()
        .unwrap()
        .success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=umbra_socket_inventory");
    println!("cargo:rustc-link-lib=proc");
}
