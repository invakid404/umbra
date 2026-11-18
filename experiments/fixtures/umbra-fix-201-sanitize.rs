//! Native control for Rust's pre-main standard descriptor sanitization.
use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
fn main() {
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    for fd in 0..3 {
        assert!(unsafe { fcntl(fd, 1) } >= 0, "closed fd at Rust main");
    }
    // Inspect all three before opening anything that could reuse a closed slot.
    let slots: Vec<_> = (0..3)
        .map(|fd| unsafe { ManuallyDrop::new(File::from_raw_fd(fd)) }.metadata())
        .collect();
    let reference = File::open("/dev/null").unwrap().metadata().unwrap();
    for slot in slots {
        let metadata = slot.expect("Rust did not reopen the closed slot");
        assert!(metadata.file_type().is_char_device());
        assert_eq!(metadata.rdev(), reference.rdev());
    }
}
