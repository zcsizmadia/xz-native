//! Runtime CPU feature detection.
//!
//! With `std`, features are detected at run time (std caches the result).
//! Without `std`, only features enabled at compile time (`-C target-feature`
//! or `-C target-cpu`) are used. The `portable` feature turns every SIMD path
//! off so the fallbacks can be tested on any machine.

#![cfg_attr(
    any(
        feature = "portable",
        not(any(target_arch = "x86_64", target_arch = "aarch64"))
    ),
    allow(dead_code, reason = "no SIMD paths on this target")
)]

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
macro_rules! x86_has {
    ($($f:tt),+) => {{
        #[cfg(feature = "std")]
        { true $(&& std::arch::is_x86_feature_detected!($f))+ }
        #[cfg(not(feature = "std"))]
        { true $(&& cfg!(target_feature = $f))+ }
    }};
}

#[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
macro_rules! arm_has {
    ($($f:tt),+) => {{
        #[cfg(feature = "std")]
        { true $(&& std::arch::is_aarch64_feature_detected!($f))+ }
        #[cfg(not(feature = "std"))]
        { true $(&& cfg!(target_feature = $f))+ }
    }};
}

/// Carry-less multiplication usable for CRC folding
/// (x86-64: PCLMULQDQ + SSSE3 + SSE4.1, AArch64: PMULL).
#[inline]
pub(crate) fn has_clmul() -> bool {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    {
        x86_has!("pclmulqdq", "ssse3", "sse4.1")
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        not(feature = "portable")
    ))]
    {
        arm_has!("aes", "neon")
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", not(feature = "portable")),
        all(
            target_arch = "aarch64",
            target_endian = "little",
            not(feature = "portable")
        )
    )))]
    {
        false
    }
}

/// 256-bit carry-less multiplication for wide CRC folding (x86-64:
/// VPCLMULQDQ + AVX2, plus everything [`has_clmul`] needs for the tail).
#[inline]
#[cfg(target_arch = "x86_64")]
pub(crate) fn has_vpclmul() -> bool {
    #[cfg(not(feature = "portable"))]
    {
        x86_has!("vpclmulqdq", "avx2", "pclmulqdq", "sse4.1", "ssse3")
    }
    #[cfg(feature = "portable")]
    {
        false
    }
}

/// ARMv8 CRC32 instructions.
#[inline]
#[cfg(target_arch = "aarch64")]
pub(crate) fn has_arm_crc32() -> bool {
    #[cfg(not(feature = "portable"))]
    {
        arm_has!("crc")
    }
    #[cfg(feature = "portable")]
    {
        false
    }
}

/// SHA-256 instructions (x86-64: SHA-NI, AArch64: SHA2).
#[inline]
pub(crate) fn has_sha256() -> bool {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    {
        x86_has!("sha", "sse2", "ssse3", "sse4.1")
    }
    #[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
    {
        arm_has!("sha2", "neon")
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", not(feature = "portable")),
        all(target_arch = "aarch64", not(feature = "portable"))
    )))]
    {
        false
    }
}
