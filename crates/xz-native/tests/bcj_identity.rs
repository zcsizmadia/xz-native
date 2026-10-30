//! BCJ encoders and decoders on data dense in each architecture's branch
//! instructions: our .xz output must equal `xz`'s byte for byte, and both
//! directions must round-trip. Fast enough for mutation testing (#22).

mod common;

use common::xz;
use xz_native::{BcjKind, EncoderOptions, Filter, LzmaOptions, compress_with_options, decompress};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
}

/// Half random bytes, half instructions that the filter converts.
fn arch_data(kind: BcjKind, n: usize) -> Vec<u8> {
    let mut r = Rng(0x5DEE_CE66_D1CE_4E5B ^ kind.filter_id());
    let mut v = Vec::with_capacity(n + 16);
    while v.len() < n {
        let x = r.next();
        if x & 1 == 0 {
            v.extend_from_slice(&r.next().to_le_bytes());
            continue;
        }
        let imm = r.next();
        match kind {
            BcjKind::X86 => {
                v.push(if x & 2 == 0 { 0xE8 } else { 0xE9 });
                let msb = if x & 4 == 0 { 0x00 } else { 0xFF };
                v.extend_from_slice(&[imm as u8, (imm >> 8) as u8, (imm >> 16) as u8, msb]);
            }
            BcjKind::PowerPc => {
                let w = 0x4800_0001 | (imm & 0x03FF_FFFC);
                v.extend_from_slice(&w.to_be_bytes());
            }
            BcjKind::Ia64 => {
                // Bundle templates with branch slots, opcode 5 in slot 2.
                let mut b = [0u8; 16];
                for (i, byte) in b.iter_mut().enumerate() {
                    *byte = (r.next() >> (i % 4)) as u8;
                }
                b[0] = (b[0] & 0xE0) | [0x10, 0x11, 0x16, 0x17, 0x18, 0x1C][imm as usize % 6];
                let bit_pos = 5 + 2 * 41;
                let mut inst = u64::from_le_bytes(b[8..16].try_into().unwrap()) >> (bit_pos - 64);
                inst = (inst & !(0xF << 37) & !(0x7 << 9)) | (0x5 << 37);
                let base = u64::from_le_bytes(b[8..16].try_into().unwrap());
                let mask = (1u64 << (bit_pos - 64)) - 1;
                b[8..16].copy_from_slice(&((base & mask) | (inst << (bit_pos - 64))).to_le_bytes());
                v.extend_from_slice(&b);
            }
            BcjKind::Arm => {
                v.extend_from_slice(&[imm as u8, (imm >> 8) as u8, (imm >> 16) as u8, 0xEB])
            }
            BcjKind::ArmThumb => v.extend_from_slice(&[
                imm as u8,
                0xF0 | (imm >> 8) as u8 & 7,
                (imm >> 16) as u8,
                0xF8 | (imm >> 24) as u8 & 7,
            ]),
            BcjKind::Sparc => {
                let w = if x & 2 == 0 {
                    0x4000_0000 | (imm & 0x003F_FFFF)
                } else {
                    0x7FC0_0000 | (imm & 0x003F_FFFF)
                };
                v.extend_from_slice(&w.to_be_bytes());
            }
            BcjKind::Arm64 => {
                let w = if x & 2 == 0 {
                    0x9400_0000 | (imm & 0x03FF_FFFF)
                } else {
                    // ADRP x0..x31 with a small page offset (in range).
                    let page = imm & 0x3FFFF;
                    0x9000_0000 | ((page & 3) << 29) | ((page >> 2) << 5) | (imm >> 27)
                };
                v.extend_from_slice(&w.to_le_bytes());
            }
            BcjKind::RiscV => {
                let rd = 1 + (imm >> 27) % 31;
                match x % 6 {
                    // JAL with a random target.
                    0 | 1 => {
                        v.extend_from_slice(&(0x6F | (rd << 7) | (imm & 0xFFFF_F000)).to_le_bytes())
                    }
                    // AUIPC + ADDI/LW pair on the same register.
                    2 | 3 => {
                        let auipc = 0x17 | (rd << 7) | (imm & 0xFFFF_F000);
                        let opcode = if x & 8 == 0 { 0x13 } else { 0x03 | (2 << 12) };
                        let inst2 = opcode | (rd << 7) | (rd << 15) | ((imm & 0xFFF) << 20);
                        v.extend_from_slice(&auipc.to_le_bytes());
                        v.extend_from_slice(&inst2.to_le_bytes());
                    }
                    // AUIPC followed by something else.
                    _ => {
                        v.extend_from_slice(
                            &(0x17 | (rd << 7) | (imm & 0xFFFF_F000)).to_le_bytes(),
                        );
                        v.extend_from_slice(&r.next().to_le_bytes());
                    }
                }
            }
            _ => unreachable!("a BcjKind this test doesn't know"),
        }
    }
    v.truncate(n);
    v
}

const KINDS: [(BcjKind, &str, u32); 8] = [
    (BcjKind::X86, "x86", 1),
    (BcjKind::PowerPc, "powerpc", 4),
    (BcjKind::Ia64, "ia64", 16),
    (BcjKind::Arm, "arm", 4),
    (BcjKind::ArmThumb, "armthumb", 2),
    (BcjKind::Sparc, "sparc", 4),
    (BcjKind::Arm64, "arm64", 4),
    (BcjKind::RiscV, "riscv", 2),
];

#[test]
fn bcj_output_matches_xz() {
    for (kind, name, align) in KINDS {
        for size in [3usize, 17, 4099, 60_000] {
            let data = arch_data(kind, size);
            for start in [0u32, 4096 + align] {
                let opts = EncoderOptions::default()
                    .lzma(LzmaOptions::preset(0).unwrap())
                    .filters(vec![Filter::Bcj {
                        kind,
                        start_offset: start,
                    }]);
                let ours = compress_with_options(&data, &opts).unwrap();
                assert_eq!(
                    decompress(&ours).unwrap(),
                    data,
                    "{name} size={size} start={start}"
                );
                let filter = if start == 0 {
                    format!("--{name}")
                } else {
                    format!("--{name}=start={start}")
                };
                let Some(theirs) = xz(&["-zc", "-T1", &filter, "--lzma2=preset=0"], &data) else {
                    return;
                };
                assert!(
                    ours == theirs,
                    "{name} size={size} start={start}: differs from xz"
                );
            }
        }
    }
}

#[test]
fn bcj_data_triggers_conversions() {
    // The generator must produce data each filter actually changes, or the
    // tests above would prove little. Encoding as raw BCJ+LZMA2 and decoding
    // with LZMA2 alone gives the BCJ-converted bytes.
    for (kind, name, _) in KINDS {
        let data = arch_data(kind, 60_000);
        let filter = format!("--{name}");
        let Some(raw) = xz(&["-zc", "--format=raw", &filter, "--lzma2=preset=0"], &data) else {
            return;
        };
        let converted = xz(&["-dc", "--format=raw", "--lzma2=preset=0"], &raw).unwrap();
        let changed = converted.iter().zip(&data).filter(|(a, b)| a != b).count();
        assert!(changed > 100, "{name}: only {changed} bytes converted");
    }
}
