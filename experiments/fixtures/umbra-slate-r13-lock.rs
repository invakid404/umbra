// C13-01: the Rust API leaf; the C companion owns only instrumentation.
use std::fs::File;
use std::os::fd::FromRawFd;
use std::mem::ManuallyDrop;

#[no_mangle]
pub extern "C" fn c13_rust_lock(fd: i32, error: &mut i32, unlocked: &mut i32) -> i32 {
    // Borrow the C-owned descriptor without closing it on return.
    let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    match file.try_lock() {
        Ok(()) => {
            *unlocked = match file.unlock() { Ok(()) => 0, Err(_) => -1 };
            0
        }
        Err(std::fs::TryLockError::Error(e)) => { *error = e.raw_os_error().unwrap_or(-1); -1 }
        Err(std::fs::TryLockError::WouldBlock) => { *error = -2; -1 }
    }
}
unsafe extern "C" { fn c13_main(argc: i32, argv: *const *const i8) -> i32; }
fn main() {
    let args: Vec<std::ffi::CString> = std::env::args().map(|a| std::ffi::CString::new(a).unwrap()).collect();
    let ptrs: Vec<_> = args.iter().map(|a| a.as_ptr()).collect();
    std::process::exit(unsafe { c13_main(ptrs.len() as i32, ptrs.as_ptr()) });
}
