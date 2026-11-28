#[cfg(target_os = "macos")]
#[path = "endpoint_inventory.rs"]
mod fixture;

fn main() {
    #[cfg(target_os = "macos")]
    if let Err(error) = fixture::run() {
        eprintln!("endpoint inventory: {error}");
        std::process::exit(1);
    }
}
