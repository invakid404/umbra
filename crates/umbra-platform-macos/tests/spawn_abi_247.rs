#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::{
    ffi::CString,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};

struct Mapping(*mut libc::c_void, usize);
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.0, self.1) };
    }
}

/// A positive PID is our unreaped direct child, so it cannot be reused before
/// waitpid. Install this owner before checking any syscall result.
struct Child(libc::pid_t);
impl Child {
    fn reap(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.0, &mut status, libc::WNOHANG) };
            if result == self.0 {
                self.0 = 0;
                return status;
            }
            assert!(
                result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR),
                "waitpid failed: {}",
                std::io::Error::last_os_error()
            );
            assert!(Instant::now() < deadline, "recorded child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if self.0 <= 0 {
            return;
        }
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            loop {
                let result = libc::waitpid(self.0, std::ptr::null_mut(), 0);
                if result == self.0 {
                    break;
                }
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    panic!("could not reap recorded child {}", self.0);
                }
            }
        }
    }
}

#[test]
fn f247_native_attribute_prefix_boundary() {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP: native spawn ABI qualification requires provisioned host");
        return;
    }
    let evidence = std::env::var_os("UMBRA_F247_EVIDENCE")
        .expect("required native reading needs UMBRA_F247_EVIDENCE");
    let mut records = Vec::new();
    // Same valid descriptor/path/argv and advertised length in every leg.
    // The middle leg must fail specifically at the fixed-prefix copy.
    for readable in [256usize, 199, 200] {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert!(page > 256);
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(raw, libc::MAP_FAILED);
        let mapping = Mapping(raw, page * 2);
        let boundary = unsafe { raw.cast::<u8>().add(page) };
        assert_eq!(
            unsafe { libc::mprotect(boundary.cast(), page, libc::PROT_NONE) },
            0
        );
        let attributes = unsafe { boundary.sub(readable) };
        let mut defaults = [0u8; 256];
        defaults[56] = 1;
        defaults[68..80].fill(0xff);
        defaults[112..128].fill(0xff);
        unsafe { std::ptr::copy_nonoverlapping(defaults.as_ptr(), attributes, readable) };
        let mut descriptor = [0u8; 144];
        descriptor[..8].copy_from_slice(&1u64.to_le_bytes());
        descriptor[8..16].copy_from_slice(&(attributes as u64).to_le_bytes());
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let nonce = format!("f247-{}-{readable}", std::process::id());
        let script = CString::new(format!(
            "printf %s \"$1\" > /dev/fd/{}",
            write_fd.as_raw_fd()
        ))
        .unwrap();
        let argument = CString::new(nonce.as_str()).unwrap();
        let argv = [
            c"/bin/sh".as_ptr(),
            c"-c".as_ptr(),
            script.as_ptr(),
            c"f247".as_ptr(),
            argument.as_ptr(),
            std::ptr::null(),
        ];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];
        let mut pid = -1;
        let rc = unsafe {
            libc::syscall(
                244,
                &mut pid as *mut libc::pid_t,
                c"/bin/sh".as_ptr(),
                descriptor.as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        let errno = std::io::Error::last_os_error().raw_os_error();
        let mut child = Child(pid);
        drop(write_fd);
        let mut birth = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let birth_size = if pid > 0 {
            unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut birth as *mut libc::proc_bsdinfo).cast(),
                    std::mem::size_of_val(&birth) as i32,
                )
            }
        } else {
            0
        };
        // Reap even an unexpected successful negative leg before asserting.
        let status = (pid > 0).then(|| child.reap());
        let mut poll = libc::pollfd {
            fd: read_fd.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1);
        let mut pipe = std::fs::File::from(read_fd);
        let mut bytes = [0u8; 128];
        let count = pipe.read(&mut bytes).unwrap();
        // A child that retained another writer would make this bounded check fail.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1);
        let mut tail = [0u8; 1];
        assert_eq!(pipe.read(&mut tail).unwrap(), 0, "expected EOF");
        records.push(serde_json::json!({
            "readable": readable, "advertised": 1, "rc": rc, "errno": errno,
            "pid": pid, "status": status, "nonce": String::from_utf8_lossy(&bytes[..count]),
            "birth_size": birth_size, "birth": [birth.pbi_start_tvsec, birth.pbi_start_tvusec],
            "attribute_address": attributes as usize, "guard_address": boundary as usize,
            "reaped": pid > 0, "eof": true,
        }));
        std::fs::write(&evidence, serde_json::to_vec_pretty(&records).unwrap()).unwrap();
        if readable == 199 {
            assert_eq!(rc, -1);
            assert_eq!(errno, Some(libc::EFAULT));
            assert_eq!(pid, -1);
            assert_eq!(count, 0);
        } else {
            assert_eq!(rc, 0, "native positive control: errno={errno:?}");
            assert!(pid > 0);
            assert_eq!(birth_size as usize, std::mem::size_of_val(&birth));
            let status = status.unwrap();
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            assert_eq!(&bytes[..count], nonce.as_bytes());
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        drop(mapping);
    }
}
