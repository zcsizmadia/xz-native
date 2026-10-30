//! Encoder fuzzing: the first bytes of the input pick the options (preset,
//! mode, match finder, nice_len, depth, lc/lp/pb, dictionary size, check,
//! filters, Block size, .xz or .lzma); the rest is the data. xz-native's
//! output must decode back to the data and be byte-identical to C liblzma
//! 5.8.4's for the same options.
#![no_main]

use libfuzzer_sys::fuzz_target;
use liblzma::stream::{Action, Check, Filters, LzmaOptions, MatchFinder, Mode, Status, Stream};
use xz_native::{BcjKind, EncoderOptions, Filter};

const HEADER: usize = 10;

fn options(h: &[u8; HEADER]) -> (EncoderOptions, bool) {
    let preset = (h[0] % 10) as u32
        | if h[0] & 0x80 != 0 {
            xz_native::PRESET_EXTREME
        } else {
            0
        };
    let mut o = EncoderOptions::preset(preset).unwrap();
    // Small dictionaries keep the fuzzer fast and make the window slide.
    o.lzma.dict_size = 4096 + ((h[1] as u32) << 10);
    if h[2] & 1 != 0 {
        o.lzma.mode = if h[2] & 2 != 0 {
            xz_native::Mode::Fast
        } else {
            xz_native::Mode::Normal
        };
        o.lzma.mf = match (h[2] >> 2) % 5 {
            0 => xz_native::MatchFinder::Hc3,
            1 => xz_native::MatchFinder::Hc4,
            2 => xz_native::MatchFinder::Bt2,
            3 => xz_native::MatchFinder::Bt3,
            _ => xz_native::MatchFinder::Bt4,
        };
        o.lzma.nice_len = 2 + (h[3] as u32 * 271) / 255;
        o.lzma.depth = if h[4] < 128 {
            0
        } else {
            (h[4] as u32 - 127) * 4
        };
    }
    if h[5] & 1 != 0 {
        o.lzma.lc = (h[5] >> 1) as u32 % 5;
        o.lzma.lp = (h[5] >> 4) as u32 % (5 - o.lzma.lc);
        o.lzma.pb = (h[6] >> 5) as u32 % 5;
    }
    o.check = match h[6] & 3 {
        0 => xz_native::Check::None,
        1 => xz_native::Check::Crc32,
        2 => xz_native::Check::Crc64,
        _ => xz_native::Check::Sha256,
    };
    let kinds = [
        BcjKind::X86,
        BcjKind::PowerPc,
        BcjKind::Ia64,
        BcjKind::Arm,
        BcjKind::ArmThumb,
        BcjKind::Sparc,
        BcjKind::Arm64,
        BcjKind::RiscV,
    ];
    for i in 0..(h[7] % 4) as usize {
        let b = h[7].rotate_left(2 + 2 * i as u32);
        o.filters.push(if b & 1 != 0 {
            Filter::Delta {
                distance: 1 + (b >> 1) as u16 % 16,
            }
        } else {
            Filter::Bcj {
                kind: kinds[(b >> 1) as usize % 8],
                start_offset: 0,
            }
        });
    }
    o.block_size = if h[8] < 200 {
        0
    } else {
        1 + ((h[8] as u64 - 200) << 9)
    };
    (o, h[9] & 1 != 0)
}

fn lzma_opts_c(o: &xz_native::LzmaOptions) -> LzmaOptions {
    let mut c = LzmaOptions::new_preset(6).unwrap();
    c.dict_size(o.dict_size)
        .literal_context_bits(o.lc)
        .literal_position_bits(o.lp)
        .position_bits(o.pb)
        .mode(match o.mode {
            xz_native::Mode::Fast => Mode::Fast,
            _ => Mode::Normal,
        })
        .nice_len(o.nice_len)
        .match_finder(match o.mf {
            xz_native::MatchFinder::Hc3 => MatchFinder::HashChain3,
            xz_native::MatchFinder::Hc4 => MatchFinder::HashChain4,
            xz_native::MatchFinder::Bt2 => MatchFinder::BinaryTree2,
            xz_native::MatchFinder::Bt3 => MatchFinder::BinaryTree3,
            _ => MatchFinder::BinaryTree4,
        })
        .depth(o.depth);
    c
}

fn liblzma(o: &EncoderOptions, alone: bool, data: &[u8]) -> Vec<u8> {
    let mut s = if alone {
        Stream::new_lzma_encoder(&lzma_opts_c(&o.lzma)).unwrap()
    } else {
        let mut f = Filters::new();
        for filter in &o.filters {
            match *filter {
                Filter::Delta { distance } => {
                    f.delta_properties(&[(distance - 1) as u8]).unwrap();
                }
                Filter::Bcj { kind, .. } => {
                    match kind {
                        BcjKind::X86 => f.x86(),
                        BcjKind::PowerPc => f.powerpc(),
                        BcjKind::Ia64 => f.ia64(),
                        BcjKind::Arm => f.arm(),
                        BcjKind::ArmThumb => f.arm_thumb(),
                        BcjKind::Sparc => f.sparc(),
                        BcjKind::Arm64 => f.arm64(),
                        BcjKind::RiscV => f.riscv(),
                    };
                }
                _ => unreachable!(),
            }
        }
        f.lzma2(&lzma_opts_c(&o.lzma));
        let check = match o.check {
            xz_native::Check::None => Check::None,
            xz_native::Check::Crc32 => Check::Crc32,
            xz_native::Check::Sha256 => Check::Sha256,
            _ => Check::Crc64,
        };
        Stream::new_stream_encoder(&f, check).unwrap()
    };
    // Single-threaded `xz --block-size`: a full flush after each Block.
    let mut chunks: Vec<(&[u8], Action)> = if o.block_size != 0 && !alone {
        data.chunks(o.block_size as usize)
            .map(|c| (c, Action::FullFlush))
            .collect()
    } else {
        vec![]
    };
    chunks.push((if chunks.is_empty() { data } else { &[] }, Action::Finish));
    let mut out = Vec::with_capacity(data.len() + 4096);
    for (chunk, action) in chunks {
        let start = s.total_in();
        loop {
            if out.capacity() - out.len() < 4096 {
                out.reserve(out.capacity());
            }
            let used = (s.total_in() - start) as usize;
            if let Status::StreamEnd = s.process_vec(&chunk[used..], &mut out, action).unwrap() {
                break;
            }
        }
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.len() < HEADER {
        return;
    }
    let (h, input) = data.split_at(HEADER);
    let (o, alone) = options(h.try_into().unwrap());
    let ours = if alone {
        xz_native::compress_lzma_with_options(input, &o.lzma).unwrap()
    } else {
        xz_native::compress_with_options(input, &o).unwrap()
    };
    let back = if alone {
        xz_native::decompress_lzma(&ours).unwrap()
    } else {
        xz_native::decompress(&ours).unwrap()
    };
    assert!(back == input, "round trip failed: {o:?}");
    assert!(
        ours == liblzma(&o, alone, input),
        "differs from liblzma: {o:?} alone={alone}"
    );
});
