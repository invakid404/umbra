//! Read-only qualification of this provider's installed spawn ABI.
use crate::{
    spawn::{baseline, Profile, ATTRIBUTE_BYTES},
    unsupported,
};
use std::ffi::{CStr, CString};
use umbra_core::Result;

unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(index: u32) -> *const libc::c_char;
    fn _dyld_get_image_header(index: u32) -> *const u8;
    fn CC_SHA256(data: *const u8, len: u32, out: *mut u8) -> *mut u8;
}

fn mismatch() -> umbra_core::UmbraError {
    unsupported("spawn ABI profile mismatch")
}

fn sysctl(name: &str) -> Result<String> {
    let name = CString::new(name).map_err(|_| mismatch())?;
    let mut bytes = [0u8; 1024];
    let mut size = bytes.len();
    // SAFETY: the output buffer and length are valid for the complete call.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size == 0 || size > bytes.len() || bytes[size - 1] != 0 {
        return Err(mismatch());
    }
    String::from_utf8(bytes[..size - 1].to_vec()).map_err(|_| mismatch())
}

fn image_uuid() -> Result<[u8; 16]> {
    // SAFETY: dyld owns the installed, resident image names and headers.
    // Reads below stay inside the Mach-O header's bounded command region.
    unsafe {
        for index in 0.._dyld_image_count() {
            let name = _dyld_get_image_name(index);
            if name.is_null()
                || CStr::from_ptr(name).to_bytes() != b"/usr/lib/system/libsystem_kernel.dylib"
            {
                continue;
            }
            let header = _dyld_get_image_header(index);
            if header.is_null() {
                return Err(mismatch());
            }
            let words = std::slice::from_raw_parts(header.cast::<u32>(), 8);
            if words[0] != 0xfeedfacf || words[1] != 0x100000c || words[5] > 1024 * 1024 {
                return Err(mismatch());
            }
            let commands = std::slice::from_raw_parts(header.add(32), words[5] as usize);
            let mut offset = 0usize;
            for _ in 0..words[4] {
                let prefix = commands.get(offset..offset + 8).ok_or_else(mismatch)?;
                let tag = u32::from_le_bytes(prefix[..4].try_into().unwrap());
                let size = u32::from_le_bytes(prefix[4..].try_into().unwrap()) as usize;
                if size < 8 {
                    return Err(mismatch());
                }
                let end = offset.checked_add(size).ok_or_else(mismatch)?;
                let command = commands.get(offset..end).ok_or_else(mismatch)?;
                if tag == 0x1b {
                    if size != 24 {
                        return Err(mismatch());
                    }
                    return Ok(command[8..24].try_into().unwrap());
                }
                offset = end;
            }
        }
    }
    Err(mismatch())
}

/// No task is resumed by qualification; an unsupported observation refuses.
pub(crate) fn qualify() -> Result<()> {
    let profile = Profile {
        architecture: if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "unsupported"
        }
        .into(),
        product_version: sysctl("kern.osproductversion")?,
        build: sysctl("kern.osversion")?,
        release: sysctl("kern.osrelease")?,
        kernel: sysctl("kern.version")?,
        image_uuid: image_uuid()?,
    };
    Profile::admit(Some(&profile))?;
    // SAFETY: dlsym returns the resident executable wrapper. The qualified
    // UUID above identifies the image; only its bounded instruction prefix is read.
    unsafe {
        let wrapper = libc::dlsym(libc::RTLD_DEFAULT, c"posix_spawn".as_ptr());
        if wrapper.is_null() {
            return Err(mismatch());
        }
        let bytes = std::slice::from_raw_parts(wrapper.cast::<u8>(), 0x198);
        let instruction =
            |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        // mov w11,#248; mov w8,#1040: measured wrapper's attribute length
        // and action stride. The image UUID also covers the surrounding packing.
        if instruction(0x98) != 0x52801f0b || instruction(0x188) != 0x52808208 {
            return Err(mismatch());
        }
        let mut attr = std::mem::zeroed();
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return Err(mismatch());
        }
        let result = (|| {
            if libc::malloc_size(attr.cast()) < ATTRIBUTE_BYTES {
                return Err(mismatch());
            }
            let original = std::slice::from_raw_parts(attr.cast::<u8>(), ATTRIBUTE_BYTES);
            if original != baseline() {
                return Err(mismatch());
            }
            let mask = 0x1234u32;
            if libc::posix_spawnattr_setflags(&mut attr, 0x400c) != 0
                || libc::posix_spawnattr_setsigdefault(&mut attr, &mask) != 0
                || libc::posix_spawnattr_setsigmask(&mut attr, &mask) != 0
                || libc::posix_spawnattr_setpgroup(&mut attr, 123) != 0
            {
                return Err(mismatch());
            }
            let mut expected = baseline();
            expected[..2].copy_from_slice(&0x400cu16.to_le_bytes());
            expected[4..8].copy_from_slice(&mask.to_le_bytes());
            expected[8..12].copy_from_slice(&mask.to_le_bytes());
            expected[12..16].copy_from_slice(&123i32.to_le_bytes());
            if std::slice::from_raw_parts(attr.cast::<u8>(), ATTRIBUTE_BYTES) != expected {
                return Err(mismatch());
            }
            Ok(())
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}

pub(crate) fn digest(bytes: &[u8]) -> Result<[u8; 32]> {
    let length = u32::try_from(bytes.len()).map_err(|_| mismatch())?;
    let mut result = [0; 32];
    // SAFETY: both buffers cover their advertised lengths; CommonCrypto writes 32 bytes.
    if unsafe { CC_SHA256(bytes.as_ptr(), length, result.as_mut_ptr()) }.is_null() {
        return Err(mismatch());
    }
    Ok(result)
}
