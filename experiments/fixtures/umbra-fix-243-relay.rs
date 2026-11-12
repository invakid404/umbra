//! Drive the production guardian from a terminal qualification run.
#[cfg(target_os = "macos")]
fn main() {
    let exe = std::path::PathBuf::from(
        std::env::var_os("UMBRA_FIX_243_RELAY_EXE").expect("real CLI executable"),
    );
    let dir = std::path::PathBuf::from(
        std::env::var_os("UMBRA_FIX_243_RELAY_RECORDS").expect("guardian records"),
    );
    let args = std::iter::once(exe.clone().into_os_string())
        .chain(std::env::args_os().skip(1))
        .collect();
    std::fs::create_dir_all(&dir).expect("create guardian record directory");
    umbra_isolation::__relay_for_tests(exe, args, dir)
}

#[cfg(not(target_os = "macos"))]
fn main() {
    panic!("the guardian qualification fixture requires macOS");
}
