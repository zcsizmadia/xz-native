//! Integrity checks: CRC32, CRC64 and SHA-256.

mod crc32;
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
    Unsupported(u8),
}

impl Check {
    /// Maps a 4-bit check ID from the Stream Flags.
    pub fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0x00 => Check::None,
            0x01 => Check::Crc32,
            0x04 => Check::Crc64,
            0x0A => Check::Sha256,
            0x02..=0x0F => Check::Unsupported(id),
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
            Check::Unsupported(id) => id,
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
            Check::None | Check::Unsupported(_) => CheckState::None,
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
        for len in [0, 1, 55, 56, 63, 64, 65, 127, 128, 129, 1000, 4000] {
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
