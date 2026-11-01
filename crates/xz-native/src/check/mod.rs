//! Integrity checks: CRC32, CRC64 and SHA-256.

pub(crate) mod crc32;
mod crc64;
mod sha256;

#[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
mod aarch64;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
mod x86;

pub(crate) use crc32::Crc32;
pub use crc32::crc32;
pub(crate) use crc64::Crc64;
pub use crc64::crc64;
pub use sha256::Sha256;

/// Integrity check type stored in the .xz Stream Flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Check {
    /// No integrity check.
    None,
    /// CRC32 (4 bytes).
    Crc32,
    /// CRC64 (8 bytes). The xz default.
    #[default]
    Crc64,
    /// SHA-256 (32 bytes).
    Sha256,
    /// A check ID reserved by the .xz format that this implementation can't
    /// verify (2, 3, 5-9, 11-15). Like liblzma, the decoder skips it.
    ///
    /// The variant is `#[non_exhaustive]`: only [`Check::from_id`] creates
    /// it, so it never holds the ID of a supported check (there is no
    /// `Unsupported { id: 4 }` that would differ from [`Check::Crc64`]).
    /// Outside this crate, match it with `..`: `Check::Unsupported { id, .. }`.
    #[non_exhaustive]
    Unsupported {
        /// The 4-bit check ID.
        id: u8,
    },
}

impl Check {
    /// Maps a 4-bit check ID from the Stream Flags.
    pub fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0x00 => Check::None,
            0x01 => Check::Crc32,
            0x04 => Check::Crc64,
            0x0A => Check::Sha256,
            0x02..=0x0F => Check::Unsupported { id },
            _ => return None,
        })
    }

    /// The 4-bit check ID.
    pub fn id(self) -> u8 {
        match self {
            Check::None => 0x00,
            Check::Crc32 => 0x01,
            Check::Crc64 => 0x04,
            Check::Sha256 => 0x0A,
            Check::Unsupported { id } => id,
        }
    }

    /// Size of the check field in bytes.
    pub fn size(self) -> usize {
        const SIZES: [u8; 16] = [0, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64];
        SIZES[(self.id() & 0x0F) as usize] as usize
    }
}

/// Running check computation for one Block.
#[derive(Debug, Clone)]
pub(crate) enum CheckState {
    None,
    Crc32(Crc32),
    Crc64(Crc64),
    Sha256(Sha256),
}

impl CheckState {
    pub(crate) fn new(check: Check) -> Self {
        match check {
            Check::Crc32 => CheckState::Crc32(Crc32::default()),
            Check::Crc64 => CheckState::Crc64(Crc64::default()),
            Check::Sha256 => CheckState::Sha256(Sha256::new()),
            Check::None | Check::Unsupported { .. } => CheckState::None,
        }
    }

    #[inline]
    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            CheckState::None => {}
            CheckState::Crc32(c) => c.update(data),
            CheckState::Crc64(c) => c.update(data),
            CheckState::Sha256(s) => s.update(data),
        }
    }

    /// Writes the finished check value (little-endian for CRCs) and returns
    /// its length.
    pub(crate) fn finish(self, out: &mut [u8; 64]) -> usize {
        match self {
            CheckState::None => 0,
            CheckState::Crc32(c) => {
                out[..4].copy_from_slice(&c.value().to_le_bytes());
                4
            }
            CheckState::Crc64(c) => {
                out[..8].copy_from_slice(&c.value().to_le_bytes());
                8
            }
            CheckState::Sha256(s) => {
                out[..32].copy_from_slice(&s.finalize());
                32
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn data(n: usize) -> Vec<u8> {
        let mut x = 0x1234_5678_9abc_def0_u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32(b"123456789", 0), 0xCBF4_3926);
        assert_eq!(crc64(b"123456789", 0), 0x995D_C9BB_DF19_39FA);
        assert_eq!(crc32(b"", 0), 0);
        assert_eq!(crc64(b"", 0), 0);
    }

    #[test]
    fn simd_matches_generic_all_lengths_and_offsets() {
        let d = data(1200);
        let (offs, lens) = if cfg!(miri) { (3, 40) } else { (17, 300) };
        for off in 0..offs {
            for len in 0..(d.len() - off).min(lens) {
                let s = &d[off..off + len];
                assert_eq!(
                    crc32(s, 0),
                    crc32::crc32_generic(s, 0),
                    "crc32 off={off} len={len}"
                );
                assert_eq!(
                    crc64(s, 0),
                    crc64::crc64_generic(s, 0),
                    "crc64 off={off} len={len}"
                );
            }
        }
        for len in [1000, 1023, 1024, 1025, 1199] {
            let s = &d[1..len];
            assert_eq!(crc32(s, 7), crc32::crc32_generic(s, 7));
            assert_eq!(crc64(s, 7), crc64::crc64_generic(s, 7));
        }
    }

    /// Compares the CRC32/CRC64 functions `f32_`/`f64_` with the portable
    /// code for every length up to 2100 at several offsets with
    /// pseudo-random starting CRCs, and for inputs split in two at many
    /// points (incremental use).
    fn compare_with_generic(f32_: impl Fn(&[u8], u32) -> u32, f64_: impl Fn(&[u8], u64) -> u64) {
        let d = data(2200);
        let (max_len, offs): (usize, &[usize]) = if cfg!(miri) {
            (100, &[0, 5])
        } else {
            (2100, &[0, 1, 3, 8, 15, 32, 63])
        };
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for &off in offs {
            for len in 0..max_len.min(d.len() - off) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let s = &d[off..off + len];
                // Start from zero too: the common case.
                let (c32, c64) = if len % 5 == 0 {
                    (0, 0)
                } else {
                    (seed as u32, seed.rotate_left(29))
                };
                assert_eq!(
                    f32_(s, c32),
                    crc32::crc32_generic(s, c32),
                    "crc32 off={off} len={len} crc={c32:#x}"
                );
                assert_eq!(
                    f64_(s, c64),
                    crc64::crc64_generic(s, c64),
                    "crc64 off={off} len={len} crc={c64:#x}"
                );
            }
        }
        let d = &d[1..2101];
        let want32 = crc32::crc32_generic(d, 0);
        let want64 = crc64::crc64_generic(d, 0);
        for split in [
            0, 1, 15, 16, 127, 128, 129, 255, 256, 257, 300, 1024, 1500, 2099,
        ] {
            let (a, b) = d.split_at(split.min(d.len()));
            assert_eq!(f32_(b, f32_(a, 0)), want32, "crc32 split={split}");
            assert_eq!(f64_(b, f64_(a, 0)), want64, "crc64 split={split}");
        }
    }

    #[test]
    fn dispatch_matches_generic_long() {
        compare_with_generic(crc32, crc64);
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    fn clmul_matches_generic() {
        match x86::clmul_fns() {
            Some((a, b)) => compare_with_generic(a, b),
            None => require_absent("XZ_NATIVE_REQUIRE_CLMUL", "PCLMULQDQ"),
        }
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    fn vpclmul_matches_generic() {
        match x86::vpclmul_fns() {
            Some((a, b)) => compare_with_generic(a, b),
            None => require_absent("XZ_NATIVE_REQUIRE_VPCLMUL", "VPCLMULQDQ"),
        }
    }

    #[test]
    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        not(feature = "portable")
    ))]
    fn arm_crc_matches_generic() {
        if let Some((a, b)) = aarch64::crc_fns() {
            compare_with_generic(a, b);
        }
    }

    /// The SHA-256 instructions against the portable block function, called
    /// directly (runtime dispatch could silently pick the generic code).
    #[test]
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "portable")
    ))]
    fn sha256_blocks_match_generic() {
        #[cfg(target_arch = "x86_64")]
        let f = x86::sha256_fn();
        #[cfg(target_arch = "aarch64")]
        let f = aarch64::sha256_fn();
        let Some(f) = f else { return };
        let d = data(64 * 40);
        for n in [1, 2, 3, 7, 40] {
            let mut a = [
                0x6a09_e667u32,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ];
            let mut b = a;
            f(&mut a, &d[..64 * n]);
            sha256::blocks_generic(&mut b, &d[..64 * n]);
            assert_eq!(a, b, "{n} blocks");
        }
    }

    /// CI names the SIMD paths each runner has in `XZ_NATIVE_REQUIRE_SIMD`
    /// (comma-separated: clmul, vpclmul, sha, crc). If detection fails for
    /// one, the tests above would compare the generic code with itself
    /// and pass; this fails instead. The SIMD paths exist only on x86-64
    /// and AArch64 (CI also runs the tests as 32-bit x86).
    #[test]
    #[cfg(all(
        feature = "std",
        not(feature = "portable"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn required_simd_is_detected() {
        let Ok(req) = std::env::var("XZ_NATIVE_REQUIRE_SIMD") else {
            return;
        };
        for f in req.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            let found = match f {
                "clmul" => crate::cpu::has_clmul(),
                "sha" => crate::cpu::has_sha256(),
                #[cfg(target_arch = "x86_64")]
                "vpclmul" => crate::cpu::has_vpclmul(),
                #[cfg(target_arch = "aarch64")]
                "crc" => crate::cpu::has_arm_crc32(),
                _ => panic!("XZ_NATIVE_REQUIRE_SIMD: unknown feature {f}"),
            };
            assert!(found, "XZ_NATIVE_REQUIRE_SIMD: {f} not detected");
        }
    }

    /// A SIMD test was skipped because the CPU lacks the feature: fail if
    /// the environment variable `var` says the path must be tested.
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    fn require_absent(var: &str, what: &str) {
        #[cfg(feature = "std")]
        assert!(
            std::env::var_os(var).is_none(),
            "{var} is set but the CPU lacks {what}"
        );
        let _ = (var, what);
    }

    /// The CLMUL constants derived at compile time: the 128-bit ones match
    /// liblzma's crc_clmul_consts_gen.c output, and every fold constant
    /// matches `x^(bits + 63) mod P` computed independently in the normal
    /// (non-reflected) bit order.
    #[test]
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    fn clmul_constants() {
        use x86::{CRC32, CRC64};
        assert_eq!(CRC32.fold512, [0x1d95_13d7, 0x8f35_2d95]);
        assert_eq!(CRC32.fold128, [0xccaa_009e, 0xae68_9191]);
        assert_eq!(CRC32.mu_p, [0xb4e5_b025_f701_1641, 0x1_db71_0640]);
        assert_eq!(
            CRC64.fold512,
            [0x081f_6054_a784_2df4, 0x6ae3_efbb_9dd4_41f3]
        );
        assert_eq!(
            CRC64.fold128,
            [0xdabe_95af_c787_5f40, 0xe05d_d497_ca39_3ae4]
        );
        assert_eq!(CRC64.mu_p, [0x9c3e_466c_1729_63d5, 0x92d8_af2b_af0e_1e84]);

        // x^n mod (x^64 + reverse(p)), bit-reversed back. For CRC32 this is
        // modulo x^32 * P(x), the modulus scaling the CLMUL code uses.
        fn xpow_mod_reflected(p: u64, n: u32) -> u64 {
            let q = p.reverse_bits();
            let mut r = 1u64;
            for _ in 0..n {
                let carry = r >> 63;
                r = (r << 1) ^ if carry != 0 { q } else { 0 };
            }
            r.reverse_bits()
        }
        for (c, p) in [(CRC32, 0xEDB8_8320), (CRC64, 0xC96C_5795_D787_0F42)] {
            for (k, bits) in [
                (c.fold128, 128),
                (c.fold256, 256),
                (c.fold512, 512),
                (c.fold1024, 1024),
            ] {
                let want = [
                    xpow_mod_reflected(p, bits - 64 + 63),
                    xpow_mod_reflected(p, bits + 63),
                ];
                assert_eq!(k, want, "p={p:#x} bits={bits}");
            }
        }
    }

    #[test]
    fn incremental() {
        let d = data(5000);
        let (a, b) = d.split_at(1777);
        assert_eq!(crc32(b, crc32(a, 0)), crc32(&d, 0));
        assert_eq!(crc64(b, crc64(a, 0)), crc64(&d, 0));
    }

    fn sha_hex(d: &[u8]) -> alloc::string::String {
        use core::fmt::Write;
        let mut h = Sha256::new();
        h.update(d);
        let mut s = alloc::string::String::new();
        for b in h.finalize() {
            write!(s, "{b:02x}").unwrap();
        }
        s
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            sha_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_simd_matches_generic() {
        let d = data(4000);
        let lens = [0, 1, 55, 56, 63, 64, 65, 127, 128, 129, 1000, 4000];
        for len in lens.into_iter().filter(|&l| !cfg!(miri) || l <= 1000) {
            for split in [0, 1, 33, 64] {
                let split = split.min(len);
                let mut a = Sha256::new();
                a.update(&d[..split]);
                a.update(&d[split..len]);
                let mut b = Sha256::new_generic();
                b.update(&d[..len]);
                assert_eq!(a.finalize(), b.finalize(), "len={len} split={split}");
            }
        }
    }
}
