//! Branch/call/jump (BCJ) converters for executable code, ported from
//! liblzma `simple/*.c`. Each function converts as much of `buf` as it can
//! and returns how many bytes were processed; the rest must be passed again
//! with more data appended.

/// Instruction set of a BCJ filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BcjKind {
    /// x86 (32-bit and 64-bit).
    X86,
    /// PowerPC (big endian only).
    PowerPc,
    /// IA-64 (Itanium).
    Ia64,
    /// ARM (32-bit, little endian).
    Arm,
    /// ARM Thumb (little endian).
    ArmThumb,
    /// SPARC.
    Sparc,
    /// ARM64.
    Arm64,
    /// RISC-V.
    RiscV,
}

impl BcjKind {
    /// The .xz filter ID.
    pub fn filter_id(self) -> u64 {
        match self {
            BcjKind::X86 => 0x04,
            BcjKind::PowerPc => 0x05,
            BcjKind::Ia64 => 0x06,
            BcjKind::Arm => 0x07,
            BcjKind::ArmThumb => 0x08,
            BcjKind::Sparc => 0x09,
            BcjKind::Arm64 => 0x0A,
            BcjKind::RiscV => 0x0B,
        }
    }

    /// Converts `buf` in place for compression, like liblzma's one-shot
    /// `lzma_bcj_x86_encode()`, `lzma_bcj_arm64_encode()` and
    /// `lzma_bcj_riscv_encode()` (here for every instruction set).
    /// `start_offset` is the position of `buf[0]` in the executable.
    /// Returns the bytes converted: the last few may be left as they are,
    /// because an instruction there could continue past the end.
    ///
    /// ```
    /// use xz_native::BcjKind;
    ///
    /// // An x86 relative call to offset 0x1000, at offset 0x100.
    /// let mut code = [0xE8, 0xFB, 0x0E, 0x00, 0x00, 0x90, 0x90, 0x90, 0x90, 0x90];
    /// let n = BcjKind::X86.encode(0x100, &mut code);
    /// assert_eq!(code[..5], [0xE8, 0x00, 0x10, 0x00, 0x00]); // absolute
    /// assert_eq!(BcjKind::X86.decode(0x100, &mut code), n);
    /// assert_eq!(code[..5], [0xE8, 0xFB, 0x0E, 0x00, 0x00]);
    /// ```
    pub fn encode(self, start_offset: u32, buf: &mut [u8]) -> usize {
        Bcj::new(self, true).code(start_offset, buf)
    }

    /// Undoes [`encode`](Self::encode), like liblzma's
    /// `lzma_bcj_x86_decode()` and the others.
    pub fn decode(self, start_offset: u32, buf: &mut [u8]) -> usize {
        Bcj::new(self, false).code(start_offset, buf)
    }

    pub(crate) fn from_filter_id(id: u64) -> Option<Self> {
        Some(match id {
            0x04 => BcjKind::X86,
            0x05 => BcjKind::PowerPc,
            0x06 => BcjKind::Ia64,
            0x07 => BcjKind::Arm,
            0x08 => BcjKind::ArmThumb,
            0x09 => BcjKind::Sparc,
            0x0A => BcjKind::Arm64,
            0x0B => BcjKind::RiscV,
            _ => return None,
        })
    }

    /// Required alignment of the start offset.
    pub(crate) fn alignment(self) -> u32 {
        match self {
            BcjKind::X86 => 1,
            BcjKind::ArmThumb | BcjKind::RiscV => 2,
            BcjKind::Ia64 => 16,
            _ => 4,
        }
    }

    /// Most bytes a call may leave unprocessed.
    pub(crate) fn unfiltered_max(self) -> usize {
        match self {
            BcjKind::X86 => 5,
            BcjKind::Ia64 => 16,
            BcjKind::RiscV => 8,
            _ => 4,
        }
    }
}

/// State of a BCJ converter (only x86 has any).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bcj {
    pub(crate) kind: BcjKind,
    pub(crate) encoder: bool,
    prev_mask: u32,
    prev_pos: u32,
}

impl Bcj {
    pub(crate) fn new(kind: BcjKind, encoder: bool) -> Self {
        Self {
            kind,
            encoder,
            prev_mask: 0,
            prev_pos: 0u32.wrapping_sub(5),
        }
    }

    /// Converts `buf` whose first byte is at stream position `now_pos`.
    pub(crate) fn code(&mut self, now_pos: u32, buf: &mut [u8]) -> usize {
        let e = self.encoder;
        match self.kind {
            BcjKind::X86 => self.x86(now_pos, buf),
            BcjKind::PowerPc => powerpc(now_pos, e, buf),
            BcjKind::Ia64 => ia64(now_pos, e, buf),
            BcjKind::Arm => arm(now_pos, e, buf),
            BcjKind::ArmThumb => armthumb(now_pos, e, buf),
            BcjKind::Sparc => sparc(now_pos, e, buf),
            BcjKind::Arm64 => arm64(now_pos, e, buf),
            BcjKind::RiscV if e => riscv_encode(now_pos, buf),
            BcjKind::RiscV => riscv_decode(now_pos, buf),
        }
    }

    fn x86(&mut self, now_pos: u32, buf: &mut [u8]) -> usize {
        const MASK_TO_BIT_NUMBER: [u32; 5] = [0, 1, 2, 2, 3];
        #[inline(always)]
        fn test_ms_byte(b: u8) -> bool {
            b == 0 || b == 0xFF
        }
        let size = buf.len();
        if size < 5 {
            return 0;
        }
        let mut prev_mask = self.prev_mask;
        let mut prev_pos = self.prev_pos;
        if now_pos.wrapping_sub(prev_pos) > 5 {
            prev_pos = now_pos.wrapping_sub(5);
        }
        let limit = size - 5;
        let mut i = 0;
        while i <= limit {
            let b = buf[i];
            if b != 0xE8 && b != 0xE9 {
                i += 1;
                continue;
            }
            let offset = now_pos.wrapping_add(i as u32).wrapping_sub(prev_pos);
            prev_pos = now_pos.wrapping_add(i as u32);
            if offset > 5 {
                prev_mask = 0;
            } else {
                for _ in 0..offset {
                    prev_mask &= 0x77;
                    prev_mask <<= 1;
                }
            }
            let b = buf[i + 4];
            if test_ms_byte(b) && (prev_mask >> 1) <= 4 && (prev_mask >> 1) != 3 {
                let mut src = u32::from_le_bytes([buf[i + 1], buf[i + 2], buf[i + 3], b]);
                let mut dest;
                loop {
                    let pc = now_pos.wrapping_add(i as u32).wrapping_add(5);
                    dest = if self.encoder {
                        src.wrapping_add(pc)
                    } else {
                        src.wrapping_sub(pc)
                    };
                    if prev_mask == 0 {
                        break;
                    }
                    let idx = MASK_TO_BIT_NUMBER[(prev_mask >> 1) as usize];
                    let b = (dest >> (24 - idx * 8)) as u8;
                    if !test_ms_byte(b) {
                        break;
                    }
                    src = dest ^ ((1u32 << (32 - idx * 8)) - 1);
                }
                buf[i + 4] = !(((dest >> 24) & 1).wrapping_sub(1)) as u8;
                buf[i + 3] = (dest >> 16) as u8;
                buf[i + 2] = (dest >> 8) as u8;
                buf[i + 1] = dest as u8;
                i += 5;
                prev_mask = 0;
            } else {
                i += 1;
                prev_mask |= 1;
                if test_ms_byte(b) {
                    prev_mask |= 0x10;
                }
            }
        }
        self.prev_mask = prev_mask;
        self.prev_pos = prev_pos;
        i
    }
}

fn arm(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    for (i, c) in buf[..size].chunks_exact_mut(4).enumerate() {
        if c[3] == 0xEB {
            let src = u32::from_le_bytes([c[0], c[1], c[2], 0]) << 2;
            let pc = now_pos.wrapping_add((i * 4) as u32).wrapping_add(8);
            let dest = if enc {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 2;
            c[2] = (dest >> 16) as u8;
            c[1] = (dest >> 8) as u8;
            c[0] = dest as u8;
        }
    }
    size
}

fn armthumb(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    if buf.len() < 4 {
        return 0;
    }
    let size = buf.len() - 4;
    let mut i = 0;
    while i <= size {
        if (buf[i + 1] & 0xF8) == 0xF0 && (buf[i + 3] & 0xF8) == 0xF8 {
            let src = ((((buf[i + 1] as u32) & 7) << 19)
                | ((buf[i] as u32) << 11)
                | (((buf[i + 3] as u32) & 7) << 8)
                | buf[i + 2] as u32)
                << 1;
            let pc = now_pos.wrapping_add(i as u32).wrapping_add(4);
            let dest = if enc {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 1;
            buf[i + 1] = 0xF0 | ((dest >> 19) & 7) as u8;
            buf[i] = (dest >> 11) as u8;
            buf[i + 3] = 0xF8 | ((dest >> 8) & 7) as u8;
            buf[i + 2] = dest as u8;
            i += 2;
        }
        i += 2;
    }
    i
}

fn arm64(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    for (i, c) in buf[..size].chunks_exact_mut(4).enumerate() {
        let pc = now_pos.wrapping_add((i * 4) as u32);
        let mut instr = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        if (instr >> 26) == 0x25 {
            let src = instr;
            let mut pc = pc >> 2;
            if !enc {
                pc = 0u32.wrapping_sub(pc);
            }
            instr = 0x9400_0000 | (src.wrapping_add(pc) & 0x03FF_FFFF);
            c.copy_from_slice(&instr.to_le_bytes());
        } else if (instr & 0x9F00_0000) == 0x9000_0000 {
            let src = ((instr >> 29) & 3) | ((instr >> 3) & 0x001F_FFFC);
            if (src.wrapping_add(0x0002_0000) & 0x001C_0000) != 0 {
                continue;
            }
            instr &= 0x9000_001F;
            let mut pc = pc >> 12;
            if !enc {
                pc = 0u32.wrapping_sub(pc);
            }
            let dest = src.wrapping_add(pc);
            instr |= (dest & 3) << 29;
            instr |= (dest & 0x0003_FFFC) << 3;
            instr |= (0u32.wrapping_sub(dest & 0x0002_0000)) & 0x00E0_0000;
            c.copy_from_slice(&instr.to_le_bytes());
        }
    }
    size
}

fn powerpc(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    for (i, c) in buf[..size].chunks_exact_mut(4).enumerate() {
        if (c[0] >> 2) == 0x12 && (c[3] & 3) == 1 {
            let src = (((c[0] as u32) & 3) << 24)
                | ((c[1] as u32) << 16)
                | ((c[2] as u32) << 8)
                | ((c[3] as u32) & !3);
            let pc = now_pos.wrapping_add((i * 4) as u32);
            let dest = if enc {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            };
            c[0] = 0x48 | ((dest >> 24) & 0x03) as u8;
            c[1] = (dest >> 16) as u8;
            c[2] = (dest >> 8) as u8;
            c[3] = (c[3] & 0x03) | dest as u8;
        }
    }
    size
}

fn sparc(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    for (i, c) in buf[..size].chunks_exact_mut(4).enumerate() {
        if (c[0] == 0x40 && (c[1] & 0xC0) == 0x00) || (c[0] == 0x7F && (c[1] & 0xC0) == 0xC0) {
            let src = u32::from_be_bytes([c[0], c[1], c[2], c[3]]) << 2;
            let pc = now_pos.wrapping_add((i * 4) as u32);
            let dest = if enc {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 2;
            let dest = ((0u32.wrapping_sub((dest >> 22) & 1) << 22) & 0x3FFF_FFFF)
                | (dest & 0x3F_FFFF)
                | 0x4000_0000;
            c.copy_from_slice(&dest.to_be_bytes());
        }
    }
    size
}

fn ia64(now_pos: u32, enc: bool, buf: &mut [u8]) -> usize {
    const BRANCH_TABLE: [u32; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4,
        0, 0,
    ];
    let size = buf.len() & !15;
    let mut i = 0;
    while i < size {
        let mask = BRANCH_TABLE[(buf[i] & 0x1F) as usize];
        let mut bit_pos = 5u32;
        for slot in 0..3 {
            if (mask >> slot) & 1 != 0 {
                let byte_pos = (bit_pos >> 3) as usize;
                let bit_res = bit_pos & 7;
                let mut instruction = 0u64;
                for j in 0..6 {
                    instruction |= (buf[i + j + byte_pos] as u64) << (8 * j);
                }
                let mut inst_norm = instruction >> bit_res;
                if ((inst_norm >> 37) & 0xF) == 0x5 && ((inst_norm >> 9) & 0x7) == 0 {
                    let mut src = ((inst_norm >> 13) & 0xFFFFF) as u32;
                    src |= (((inst_norm >> 36) & 1) as u32) << 20;
                    src <<= 4;
                    let pc = now_pos.wrapping_add(i as u32);
                    let dest = if enc {
                        pc.wrapping_add(src)
                    } else {
                        src.wrapping_sub(pc)
                    } >> 4;
                    inst_norm &= !(0x8F_FFFFu64 << 13);
                    inst_norm |= ((dest & 0xFFFFF) as u64) << 13;
                    inst_norm |= ((dest & 0x100000) as u64) << (36 - 20);
                    instruction &= (1u64 << bit_res) - 1;
                    instruction |= inst_norm << bit_res;
                    for j in 0..6 {
                        buf[i + j + byte_pos] = (instruction >> (8 * j)) as u8;
                    }
                }
            }
            bit_pos += 41;
        }
        i += 16;
    }
    i
}

#[inline(always)]
fn not_auipc_pair(auipc: u32, inst2: u32) -> bool {
    ((auipc << 8) ^ inst2.wrapping_sub(3)) & 0xF8003 != 0
}

#[inline(always)]
fn not_special_auipc(auipc: u32, inst2_rs1: u32) -> bool {
    (auipc.wrapping_sub(0x3117) << 18) >= (inst2_rs1 & 0x1D)
}

fn rd32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn riscv_encode(now_pos: u32, buf: &mut [u8]) -> usize {
    if buf.len() < 8 {
        return 0;
    }
    let size = buf.len() - 8;
    let mut i = 0;
    while i <= size {
        let mut inst = buf[i] as u32;
        if inst == 0xEF {
            let b1 = buf[i + 1] as u32;
            if (b1 & 0x0D) != 0 {
                i += 2;
                continue;
            }
            let b2 = buf[i + 2] as u32;
            let b3 = buf[i + 3] as u32;
            let pc = now_pos.wrapping_add(i as u32);
            let mut addr = ((b1 & 0xF0) << 8)
                | ((b2 & 0x0F) << 16)
                | ((b2 & 0x10) << 7)
                | ((b2 & 0xE0) >> 4)
                | ((b3 & 0x7F) << 4)
                | ((b3 & 0x80) << 13);
            addr = addr.wrapping_add(pc);
            buf[i + 1] = ((b1 & 0x0F) | ((addr >> 13) & 0xF0)) as u8;
            buf[i + 2] = (addr >> 9) as u8;
            buf[i + 3] = (addr >> 1) as u8;
            i += 4;
        } else if (inst & 0x7F) == 0x17 {
            inst |= (buf[i + 1] as u32) << 8;
            inst |= (buf[i + 2] as u32) << 16;
            inst |= (buf[i + 3] as u32) << 24;
            if inst & 0xE80 != 0 {
                let inst2 = rd32le(buf, i + 4);
                if not_auipc_pair(inst, inst2) {
                    i += 6;
                    continue;
                }
                let mut addr = inst & 0xFFFF_F000;
                addr = addr.wrapping_add((inst2 >> 20).wrapping_sub((inst2 >> 19) & 0x1000));
                addr = addr.wrapping_add(now_pos.wrapping_add(i as u32));
                let inst = 0x17 | (2 << 7) | (inst2 << 12);
                buf[i..i + 4].copy_from_slice(&inst.to_le_bytes());
                buf[i + 4..i + 8].copy_from_slice(&addr.to_be_bytes());
            } else {
                let fake_rs1 = inst >> 27;
                if not_special_auipc(inst, fake_rs1) {
                    i += 4;
                    continue;
                }
                let fake_addr = rd32le(buf, i + 4);
                let fake_inst2 = (inst >> 12) | (fake_addr << 20);
                let inst = 0x17 | (fake_rs1 << 7) | (fake_addr & 0xFFFF_F000);
                buf[i..i + 4].copy_from_slice(&inst.to_le_bytes());
                buf[i + 4..i + 8].copy_from_slice(&fake_inst2.to_le_bytes());
            }
            i += 8;
        } else {
            i += 2;
        }
    }
    i
}

fn riscv_decode(now_pos: u32, buf: &mut [u8]) -> usize {
    if buf.len() < 8 {
        return 0;
    }
    let size = buf.len() - 8;
    let mut i = 0;
    while i <= size {
        let mut inst = buf[i] as u32;
        if inst == 0xEF {
            let b1 = buf[i + 1] as u32;
            if (b1 & 0x0D) != 0 {
                i += 2;
                continue;
            }
            let b2 = buf[i + 2] as u32;
            let b3 = buf[i + 3] as u32;
            let pc = now_pos.wrapping_add(i as u32);
            let addr = (((b1 & 0xF0) << 13) | (b2 << 9) | (b3 << 1)).wrapping_sub(pc);
            buf[i + 1] = ((b1 & 0x0F) | ((addr >> 8) & 0xF0)) as u8;
            buf[i + 2] =
                (((addr >> 16) & 0x0F) | ((addr >> 7) & 0x10) | ((addr << 4) & 0xE0)) as u8;
            buf[i + 3] = (((addr >> 4) & 0x7F) | ((addr >> 13) & 0x80)) as u8;
            i += 4;
        } else if (inst & 0x7F) == 0x17 {
            inst |= (buf[i + 1] as u32) << 8;
            inst |= (buf[i + 2] as u32) << 16;
            inst |= (buf[i + 3] as u32) << 24;
            let inst2;
            if inst & 0xE80 != 0 {
                let i2 = rd32le(buf, i + 4);
                if not_auipc_pair(inst, i2) {
                    i += 6;
                    continue;
                }
                let addr = (inst & 0xFFFF_F000).wrapping_add(i2 >> 20);
                inst = 0x17 | (2 << 7) | (i2 << 12);
                inst2 = addr;
            } else {
                let inst2_rs1 = inst >> 27;
                if not_special_auipc(inst, inst2_rs1) {
                    i += 4;
                    continue;
                }
                let addr = u32::from_be_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]])
                    .wrapping_sub(now_pos.wrapping_add(i as u32));
                inst2 = (inst >> 12) | (addr << 20);
                inst = 0x17 | (inst2_rs1 << 7) | (addr.wrapping_add(0x800) & 0xFFFF_F000);
            }
            buf[i..i + 4].copy_from_slice(&inst.to_le_bytes());
            buf[i + 4..i + 8].copy_from_slice(&inst2.to_le_bytes());
            i += 8;
        } else {
            i += 2;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn roundtrip_all_kinds() {
        let mut x = 0x9E37_79B9_u32;
        let data: Vec<u8> = (0..if cfg!(miri) { 2000 } else { 20000 })
            .map(|i| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Sprinkle opcode bytes that trigger conversions.
                match i % 7 {
                    0 => 0xE8,
                    1 => 0xEB,
                    2 => 0x17,
                    3 => 0x94,
                    _ => (x >> 24) as u8,
                }
            })
            .collect();
        for kind in [
            BcjKind::X86,
            BcjKind::PowerPc,
            BcjKind::Ia64,
            BcjKind::Arm,
            BcjKind::ArmThumb,
            BcjKind::Sparc,
            BcjKind::Arm64,
            BcjKind::RiscV,
        ] {
            let mut buf = data.clone();
            let mut enc = Bcj::new(kind, true);
            let n = enc.code(0, &mut buf);
            assert!(data.len() - n <= kind.unfiltered_max(), "{kind:?}");
            let mut dec = Bcj::new(kind, false);
            let m = dec.code(0, &mut buf);
            assert_eq!(m, n, "{kind:?}");
            assert_eq!(buf, data, "{kind:?}");
        }
    }
}
