//! A failed prepared observer must return before even starting this provider.
fn main() {
    let executable = std::env::current_exe().expect("sentinel executable");
    let marker = executable.parent().unwrap().join("platform-started");
    std::fs::write(marker, b"unexpected platform connection\n").expect("sentinel marker");
    std::process::exit(86);
}
