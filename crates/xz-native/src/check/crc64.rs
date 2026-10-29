//! CRC64 (ECMA-182, reflected polynomial 0xC96C5795D7870F42), the default
//! .xz integrity check.

const POLY: u64 = 0xC96C_5795_D787_0F42;

static TABLE: [[u64; 256]; 8] = make_table();

const fn make_table() -> [[u64; 256]; 8] {
    let mut t = [[0u64; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut r = i as u64;
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

/// Portable slicing-by-8 implementation.
pub(crate) fn crc64_generic(data: &[u8], crc: u64) -> u64 {
    let mut crc = !crc;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let v = u64::from_le_bytes(c.try_into().unwrap()) ^ crc;
        crc = TABLE[7][(v & 0xFF) as usize]
            ^ TABLE[6][((v >> 8) & 0xFF) as usize]
            ^ TABLE[5][((v >> 16) & 0xFF) as usize]
            ^ TABLE[4][((v >> 24) & 0xFF) as usize]
            ^ TABLE[3][((v >> 32) & 0xFF) as usize]
            ^ TABLE[2][((v >> 40) & 0xFF) as usize]
            ^ TABLE[1][((v >> 48) & 0xFF) as usize]
            ^ TABLE[0][(v >> 56) as usize];
    }
    for &b in chunks.remainder() {
        crc = TABLE[0][((crc ^ b as u64) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Computes or continues a CRC64. Start with `crc = 0`; pass the previous
/// result to continue over more data. Compatible with `lzma_crc64`.
#[inline]
pub fn crc64(data: &[u8], crc: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if data.len() >= super::x86::VPCLMUL_MIN_LEN && crate::cpu::has_vpclmul() {
        // SAFETY: the required CPU features were detected above.
        #[allow(unsafe_code)]
        return unsafe { super::x86::crc64_vpclmul(data, crc) };
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if crate::cpu::has_clmul() {
        // SAFETY: the required CPU features were detected above.
        #[allow(unsafe_code)]
        return unsafe { super::x86::crc64_clmul(data, crc) };
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        not(feature = "portable")
    ))]
    if crate::cpu::has_clmul() {
        // SAFETY: the required CPU features were detected above.
        #[allow(unsafe_code)]
        return unsafe { super::aarch64::crc64_pmull(data, crc) };
    }
    crc64_generic(data, crc)
}

/// Incremental CRC64 state.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Crc64(u64);

impl Crc64 {
    #[inline]
    pub(crate) fn update(&mut self, data: &[u8]) {
        self.0 = crc64(data, self.0);
    }

    #[inline]
    pub(crate) fn value(&self) -> u64 {
        self.0
    }
}
