//! `progress()` like liblzma's `lzma_get_progress()` (#126): the totals
//! without threads; with threads, never going down, within what was
//! passed in and out, and the totals at the end.

use std::io::{BufReader, Read, Write};
use xz_native::{
    Action, AutoReader, DecoderOptions, EncoderOptions, Status, XzDecoder, XzEncoder, XzReader,
    XzWriter,
};

fn sample(n: usize) -> Vec<u8> {
    let mut x = 1u32;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if i % 3 == 0 {
                b"abcdefgh"[(x >> 28) as usize & 7]
            } else {
                (x >> 24) as u8 & 0x0F
            }
        })
        .collect()
}

fn size() -> usize {
    if cfg!(miri) { 20_000 } else { 3 << 20 }
}

/// Progress never goes down.
#[derive(Default)]
struct Monotonic((u64, u64));

impl Monotonic {
    #[track_caller]
    fn check(&mut self, p: (u64, u64)) {
        assert!(
            p.0 >= self.0.0 && p.1 >= self.0.1,
            "{p:?} after {:?}",
            self.0
        );
        self.0 = p;
    }
}

/// Encodes `data` in chunks, checking progress after each call.
fn encode(opts: EncoderOptions, data: &[u8]) -> Vec<u8> {
    let threaded = opts.threads != 1;
    let mut enc = XzEncoder::with_options(opts).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 50_000];
    let mut m = Monotonic::default();
    let mut ip = 0;
    loop {
        let end = (ip + 70_000).min(data.len());
        let action = if end == data.len() {
            Action::Finish
        } else {
            Action::Run
        };
        let (used, n, st) = enc.encode(&data[ip..end], &mut buf, action).unwrap();
        ip += used;
        out.extend_from_slice(&buf[..n]);
        assert_eq!(
            (enc.total_in(), enc.total_out()),
            (ip as u64, out.len() as u64)
        );
        let p = enc.progress();
        m.check(p);
        if threaded {
            // Encoded input was taken; output may still be inside.
            assert!(p.0 <= enc.total_in(), "{p:?}");
        } else {
            assert_eq!(p, (enc.total_in(), enc.total_out()));
        }
        if action == Action::Finish && st == Status::StreamEnd {
            break;
        }
    }
    assert_eq!(enc.progress(), (data.len() as u64, out.len() as u64));
    out
}

#[test]
fn encoder() {
    let data = sample(size());
    let single = encode(EncoderOptions::preset(1).unwrap(), &data);
    let mt = encode(
        EncoderOptions::preset(1)
            .unwrap()
            .threads(4)
            .block_size(200_000),
        &data,
    );
    assert_eq!(xz_native::decompress(&single).unwrap(), data);
    assert_eq!(xz_native::decompress(&mt).unwrap(), data);
}

#[test]
fn writer() {
    let data = sample(size() / 2);
    let opts = EncoderOptions::preset(0)
        .unwrap()
        .threads(3)
        .block_size(100_000);
    let mut w = XzWriter::with_options(Vec::new(), opts).unwrap();
    let mut m = Monotonic::default();
    for chunk in data.chunks(64 << 10) {
        w.write_all(chunk).unwrap();
        m.check(w.progress());
    }
    w.try_finish().unwrap();
    let p = w.progress();
    m.check(p);
    assert_eq!(p, (data.len() as u64, w.total_out()));
}

/// Decodes `xz` with the push decoder in chunks, checking progress.
fn decode(xz: &[u8], threads: usize, data: &[u8]) {
    let mut dec = XzDecoder::with_options(DecoderOptions::default().threads(threads));
    let mut out = Vec::new();
    let mut buf = vec![0u8; 40_000];
    let mut m = Monotonic::default();
    let mut ip = 0;
    loop {
        let end = (ip + 30_000).min(xz.len());
        let (used, n, st) = dec.decode(&xz[ip..end], &mut buf, end == xz.len()).unwrap();
        ip += used;
        out.extend_from_slice(&buf[..n]);
        assert_eq!(
            (dec.total_in(), dec.total_out()),
            (ip as u64, out.len() as u64)
        );
        let p = dec.progress();
        m.check(p);
        if threads == 1 {
            assert_eq!(p, (ip as u64, out.len() as u64));
        } else {
            assert!(p.0 <= ip as u64 && p.1 >= out.len() as u64, "{p:?}");
        }
        if st == Status::StreamEnd {
            break;
        }
    }
    assert_eq!(out, data);
    assert_eq!(dec.progress(), (xz.len() as u64, data.len() as u64));
}

#[test]
fn decoder() {
    let data = sample(size());
    let xz = xz_native::compress_with_options(
        &data,
        &EncoderOptions::preset(1)
            .unwrap()
            .threads(2)
            .block_size(150_000),
    )
    .unwrap();
    decode(&xz, 1, &data);
    decode(&xz, 4, &data);
}

/// Reads through `r`, checking progress against what was returned.
fn read_all<R: Read>(mut r: R, progress: impl Fn(&R) -> (u64, u64)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 33_333];
    let mut m = Monotonic::default();
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        let p = progress(&r);
        m.check(p);
        assert!(p.1 >= out.len() as u64, "{p:?}");
    }
    m.check(progress(&r));
    out
}

#[test]
fn readers() {
    let data = sample(size());
    let xz = xz_native::compress_with_options(
        &data,
        &EncoderOptions::preset(1)
            .unwrap()
            .threads(2)
            .block_size(150_000),
    )
    .unwrap();
    for threads in [1, 3] {
        let opts = DecoderOptions::default().threads(threads);
        let r = XzReader::with_options(BufReader::new(&xz[..]), opts);
        let out = read_all(r, |r| r.progress());
        assert_eq!(out, data);
        let r = AutoReader::with_options(BufReader::new(&xz[..]), opts);
        let out = read_all(r, |r| r.progress());
        assert_eq!(out, data);
    }
    // At the end, progress is the totals.
    let mut r = XzReader::with_options(
        BufReader::new(&xz[..]),
        DecoderOptions::default().threads(3),
    );
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    assert_eq!(r.progress(), (xz.len() as u64, data.len() as u64));
    // .lzma: the totals.
    let lzma = xz_native::compress_lzma(&data, 0).unwrap();
    let mut r = AutoReader::new(BufReader::new(&lzma[..]));
    r.read_to_end(&mut Vec::new()).unwrap();
    assert_eq!(r.progress(), (lzma.len() as u64, data.len() as u64));
}
