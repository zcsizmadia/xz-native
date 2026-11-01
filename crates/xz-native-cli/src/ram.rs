//! Total physical RAM (`lzma_physmem()`) and the resource limits, for the
//! default memory budget of the threads (a quarter of the RAM, below the
//! limits, like xz).
//!
//! Windows and macOS need one OS call each for the RAM, and Unix one for
//! the limits.

/// Total RAM in bytes, or None if unknown.
pub fn total() -> Option<u64> {
    imp::total().filter(|&n| n > 0)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    pub fn total() -> Option<u64> {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = info.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        kib.checked_mul(1024)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    /// `MEMORYSTATUSEX`.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub fn total() -> Option<u64> {
        let mut m = MemoryStatusEx {
            length: size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `m` is a valid, writable MEMORYSTATUSEX with its length
        // set, as the call requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
        (ok != 0).then_some(m.total_phys)
    }
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
mod imp {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    pub fn total() -> Option<u64> {
        let mut mem: u64 = 0;
        let mut len = size_of::<u64>();
        // SAFETY: the name is NUL-terminated, and `mem` and `len` are valid
        // and writable; hw.memsize is a 64-bit integer.
        let r = unsafe {
            sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut mem).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && len == size_of::<u64>()).then_some(mem)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_vendor = "apple"
)))]
mod imp {
    pub fn total() -> Option<u64> {
        None
    }
}

/// The lowest soft limit on the data segment and the address space
/// (`getrlimit()`), or None if there is none.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn rlimit() -> Option<u64> {
    let mut resources = vec![libc::RLIMIT_DATA];
    // OpenBSD has no RLIMIT_AS.
    #[cfg(not(target_os = "openbsd"))]
    resources.push(libc::RLIMIT_AS);
    resources
        .into_iter()
        .filter_map(|r| {
            let mut rl = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `rl` is valid and writable.
            let ok = unsafe { libc::getrlimit(r, &mut rl) } == 0;
            // `rlim_t` is signed on FreeBSD.
            #[allow(clippy::unnecessary_cast)]
            (ok && rl.rlim_cur != libc::RLIM_INFINITY).then_some(rl.rlim_cur as u64)
        })
        .min()
}

#[cfg(not(unix))]
pub fn rlimit() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(any(target_os = "linux", windows, target_vendor = "apple"))]
    fn known_on_tier_1_targets() {
        let ram = super::total().expect("RAM size");
        assert!(ram >= 256 << 20, "{ram}");
    }
}
