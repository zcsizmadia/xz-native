//! CRC32 (IEEE 802.3, reflected polynomial 0xEDB88320), as used by .xz
//! headers and the CRC32 integrity check.

const POLY: u32 = 0xEDB8_8320;

/// Slicing-by-8 tables, generated at compile time.
pub(crate) static TABLE: [[u32; 256]; 8] = make_table();

const fn make_table() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut r = i as u32;
        let mut j = 0;
        while j < 8 {
            r = if r & 1 != 0 { (r >> 1) ^ POLY } else { r >> 1 };
            j += 1;
        }
        t[0][i] = r;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

/// Portable slicing-by-8 implementation. `crc` is the running (non-inverted)
/// value, as in `lzma_crc32`.
pub(crate) fn crc32_generic(data: &[u8], crc: u32) -> u32 {
    let mut crc = !crc;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let one = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let two = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = TABLE[7][(one & 0xFF) as usize]
            ^ TABLE[6][((one >> 8) & 0xFF) as usize]
            ^ TABLE[5][((one >> 16) & 0xFF) as usize]
            ^ TABLE[4][(one >> 24) as usize]
            ^ TABLE[3][(two & 0xFF) as usize]
            ^ TABLE[2][((two >> 8) & 0xFF) as usize]
            ^ TABLE[1][((two >> 16) & 0xFF) as usize]
            ^ TABLE[0][(two >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = TABLE[0][((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Computes or continues a CRC32. Start with `crc = 0`; pass the previous
/// result to continue over more data. Compatible with `lzma_crc32`.
#[inline]
pub fn crc32(data: &[u8], crc: u32) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if data.len() >= super::x86::VPCLMUL_MIN_LEN && crate::cpu::has_vpclmul() {
        // SAFETY: the required CPU features were detected above.
        #[allow(unsafe_code)]
        return unsafe { super::x86::crc32_vpclmul(data, crc) };
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if crate::cpu::has_clmul() {
        // SAFETY: the required CPU features were detected above.
        #[allow(unsafe_code)]
        return unsafe { super::x86::crc32_clmul(data, crc) };
    }
    #[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
    if crate::cpu::has_arm_crc32() {
        // SAFETY: the required CPU feature was detected above.
        #[allow(unsafe_code)]
        return unsafe { super::aarch64::crc32_arm(data, crc) };
    }
    crc32_generic(data, crc)
}

/// Incremental CRC32 state.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Crc32(u32);

impl Crc32 {
    #[inline]
    pub(crate) fn update(&mut self, data: &[u8]) {
        self.0 = crc32(data, self.0);
    }

    #[inline]
    pub(crate) fn value(&self) -> u32 {
        self.0
    }
}
