//! Transparent huge page hint for large buffers on Linux (the library's only
//! OS call).
//!
//! [`hint`] asks the kernel to back the 2 MiB-aligned part of a buffer with
//! transparent huge pages (`madvise(MADV_HUGEPAGE)`). Where the kernel's
//! THP mode is `madvise` (Ubuntu's default) nothing gets huge pages without
//! it; with `always` it changes nothing, with `never` the call is ignored.
//! It is only a hint: the result is ignored and the data is the same either
//! way.
//!
//! - The binary tree match finder is bound by memory latency, and with
//!   4 KiB pages most of its steps also miss the TLB: on Zen 3 at -6 the
//!   hint cuts TLB misses by 90% and compression time by 10-13% (#94).
//! - Decoding writes fresh memory (the output of one-shot decoding, the
//!   dictionary of a new streaming decoder). With 4 KiB pages the kernel
//!   takes a page fault for every 4 KiB of it, which costs as much as the
//!   decoding at many threads; huge pages fault once per 2 MiB.
//!
//! Call it before the first write, so the pages are huge from the start; on
//! memory that is already in use, the kernel may merge it into huge pages
//! later in the background.

/// Asks for huge pages on the 2 MiB-aligned part of `region` (see the
/// module documentation).
#[cfg(all(
    feature = "std",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    not(feature = "portable"),
    not(miri)
))]
pub(crate) fn hint<T>(region: &[T]) {
    const HUGE: usize = 2 << 20;
    /// The same value on x86-64 and AArch64.
    const MADV_HUGEPAGE: i32 = 14;
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: i32) -> i32;
    }
    let p = region.as_ptr() as usize;
    let start = p.next_multiple_of(HUGE);
    let end = (p + core::mem::size_of_val(region)) & !(HUGE - 1);
    if end > start {
        // SAFETY: an advice on whole pages inside `region`'s allocation; it
        // changes how the kernel backs them, never their contents.
        #[allow(unsafe_code)]
        unsafe {
            madvise(start as *mut _, end - start, MADV_HUGEPAGE);
        }
    }
}

#[cfg(not(all(
    feature = "std",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    not(feature = "portable"),
    not(miri)
)))]
pub(crate) fn hint<T>(_region: &[T]) {}

/// [`hint`] on the spare capacity of `v`, the part a decoder is about to
/// write for the first time.
#[inline]
pub(crate) fn hint_spare(v: &mut alloc::vec::Vec<u8>) {
    hint(v.spare_capacity_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// The hint takes any region (empty, smaller than a huge page,
    /// unaligned, uninitialized spare capacity) and never changes its
    /// contents. Not under Miri: the hint is compiled out there, and
    /// filling the 20 MiB table took most of an hour.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn hint_any_region() {
        hint::<u32>(&[]);
        hint(&[1u8, 2, 3]);
        let mut big: Vec<u32> = (0..(5u32 << 20)).collect();
        hint(&big);
        hint(&big[3..]);
        assert!(big.iter().enumerate().all(|(i, &v)| v == i as u32));
        big[4 << 20] = 7;
        assert_eq!(big[4 << 20], 7);

        let mut spare = Vec::with_capacity(9 << 20);
        spare.extend_from_slice(b"xz");
        hint_spare(&mut spare);
        assert_eq!(spare, b"xz");
        spare.resize(9 << 20, 5);
        assert!(spare[2..].iter().all(|&b| b == 5));
    }
}
