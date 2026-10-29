//! Deterministic differential test against C liblzma 5.8.4: valid streams
//! and thousands of mutated ones (bit flips, byte changes, truncation,
//! insertion) must give the same success/failure and identical output.
//! The CI fuzz jobs do the same with coverage guidance.

use liblzma::stream::{Action, CONCATENATED, Check, Filters, LzmaOptions, Status, Stream};
use std::io::Read;

const MEMLIMIT: u64 = 256 << 20;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn reference(input: &[u8], lzma_alone: bool) -> Option<Vec<u8>> {
    let s = if lzma_alone {
        Stream::new_lzma_decoder(MEMLIMIT)
    } else {
        Stream::new_stream_decoder(MEMLIMIT, CONCATENATED)
    }
    .unwrap();
    let mut out = Vec::new();
    liblzma::bufread::XzDecoder::new_stream(input, s)
        .read_to_end(&mut out)
        .ok()
        .map(|_| out)
}

fn ours(input: &[u8], lzma_alone: bool) -> Option<Vec<u8>> {
    if lzma_alone {
        xz_native::decompress_lzma_with_memlimit(input, MEMLIMIT).ok()
    } else {
        let opts = xz_native::DecoderOptions {
            memlimit: MEMLIMIT,
            ..Default::default()
        };
        xz_native::decompress_with_options(input, opts).ok()
    }
}

fn compress(data: &[u8], filters: &Filters, check: Check) -> Vec<u8> {
    let mut s = Stream::new_stream_encoder(filters, check).unwrap();
    let mut out = Vec::with_capacity(data.len() + 1024);
    loop {
        let consumed = s.total_in() as usize;
        if out.len() == out.capacity() {
            out.reserve(1 << 16);
        }
        if s.process_vec(&data[consumed..], &mut out, Action::Finish)
            .unwrap()
            == Status::StreamEnd
        {
            return out;
        }
    }
}

fn samples() -> Vec<(Vec<u8>, bool)> {
    let mut r = Rng(0xDEAD_BEEF_1234_5678);
    let text: Vec<u8> = (0..60_000)
        .map(|i| b"the quick brown fox jumps "[(i * 7 + r.below(3)) % 26])
        .collect();
    let rnd: Vec<u8> = (0..20_000).map(|_| r.next() as u8).collect();
    let mut v = Vec::new();
    for data in [&text, &rnd] {
        for preset in [0, 6] {
            let mut f = Filters::new();
            f.lzma2(&LzmaOptions::new_preset(preset).unwrap());
            v.push((compress(data, &f, Check::Crc64), false));
            let mut f = Filters::new();
            f.x86();
            f.lzma2(&LzmaOptions::new_preset(preset).unwrap());
            v.push((compress(data, &f, Check::Sha256), false));
        }
        let mut f = Filters::new();
        f.arm64();
        f.lzma2(&LzmaOptions::new_preset(3).unwrap());
        v.push((compress(data, &f, Check::Crc32), false));
        let mut out = Vec::new();
        let enc = Stream::new_lzma_encoder(&LzmaOptions::new_preset(6).unwrap()).unwrap();
        liblzma::read::XzEncoder::new_stream(&data[..], enc)
            .read_to_end(&mut out)
            .unwrap();
        v.push((out, true));
    }
    v
}

#[test]
fn mutations_agree_with_liblzma() {
    let mut r = Rng(42);
    let mut cases = 0;
    let mut ok_cases = 0;
    for (packed, alone) in samples() {
        assert_eq!(ours(&packed, alone), reference(&packed, alone));
        let iterations = if cfg!(debug_assertions) { 300 } else { 3000 };
        for _ in 0..iterations {
            let mut m = packed.clone();
            match r.below(5) {
                0 => {
                    let i = r.below(m.len());
                    m[i] ^= 1 << r.below(8);
                }
                1 => {
                    // Concentrate on headers, where most of the logic is.
                    let i = r.below(m.len().min(64));
                    m[i] = r.next() as u8;
                }
                2 => {
                    let i = m.len() - 1 - r.below(m.len().min(64));
                    m[i] = r.next() as u8;
                }
                3 => m.truncate(r.below(m.len())),
                _ => {
                    let i = r.below(m.len());
                    m.insert(i, r.next() as u8);
                }
            }
            let a = ours(&m, alone);
            let b = reference(&m, alone);
            if a != b {
                std::fs::write(concat!(env!("CARGO_TARGET_TMPDIR"), "/mismatch.bin"), &m).unwrap();
            }
            assert!(
                a == b,
                "mismatch (alone={alone}): ours ok={} liblzma ok={}",
                a.is_some(),
                b.is_some()
            );
            cases += 1;
            ok_cases += a.is_some() as usize;
        }
    }
    eprintln!("{cases} mutated inputs, {ok_cases} decoded successfully by both");
}
