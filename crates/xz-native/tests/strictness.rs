//! Reader strictness and round-trip cases ported from Lzma.Net (issue #11).
//! The cases comparing errors with C liblzma are in
//! crates/xz-bench/tests/strictness.rs.

mod common;

use common::{Rng, xz};
use std::io::{self, BufRead, BufReader, Read};
use xz_native::{DecoderOptions, Status, XzDecoder, XzReader, crc32, decompress};

/// A reader that can't seek and returns at most `max` bytes per read.
struct NonSeekable<'a> {
    data: &'a [u8],
    max: usize,
}

impl Read for NonSeekable<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.max).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// Streaming decoder fed through 1-byte input and output buffers.
fn decode_tiny(input: &[u8], opts: DecoderOptions) -> (Vec<u8>, usize) {
    let mut d = XzDecoder::with_options(opts);
    let mut out = Vec::new();
    let mut ip = 0;
    loop {
        let end = (ip + 1).min(input.len());
        let mut b = [0u8; 1];
        let (u, w, s) = d
            .decode(&input[ip..end], &mut b, end == input.len())
            .unwrap();
        ip += u;
        out.extend_from_slice(&b[..w]);
        if s == Status::StreamEnd {
            return (out, ip);
        }
    }
}

/// Reads all of `r` with read sizes cycling through small values.
fn read_small(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let sizes = [1usize, 2, 3, 7, 64, 1000];
    let mut out = Vec::new();
    let mut buf = [0u8; 1000];
    for i in 0.. {
        let n = r.read(&mut buf[..sizes[i % sizes.len()]])?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn push_crc32(out: &mut Vec<u8>, from: usize) {
    let crc = crc32(&out[from..], 0);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// One Stream with a Block whose header doesn't store the Compressed or
/// Uncompressed Size: the end of the Block is found only by decoding it.
/// The data is stored in uncompressed LZMA2 chunks.
fn unsized_stream(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFD, b'7', b'z', b'X', b'Z', 0, 0, 1];
    push_crc32(&mut out, 6);
    let block = out.len();
    // Header Size, Block Flags (one filter, no sizes), LZMA2, 1 property
    // byte, 4 KiB dictionary and Header Padding.
    out.extend_from_slice(&[2, 0, 0x21, 1, 0, 0, 0, 0]);
    push_crc32(&mut out, block);
    for (i, chunk) in data.chunks(1000).enumerate() {
        out.push(if i == 0 { 1 } else { 2 });
        out.extend_from_slice(&(chunk.len() as u16 - 1).to_be_bytes());
        out.extend_from_slice(chunk);
    }
    out.push(0);
    let unpadded = (out.len() - block) + 4;
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(&crc32(data, 0).to_le_bytes());

    // Index: one Record (both sizes fit in two-byte VLIs here).
    let index = out.len();
    out.push(0);
    out.push(1);
    for v in [unpadded as u64, data.len() as u64] {
        let mut v = v;
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    out.resize(index + (out.len() - index).next_multiple_of(4), 0);
    push_crc32(&mut out, index);
    let backward = ((out.len() - index) / 4 - 1) as u32;
    let footer = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&backward.to_le_bytes());
    out.extend_from_slice(&[0, 1]);
    let crc = crc32(&out[footer + 4..footer + 10], 0);
    out[footer..footer + 4].copy_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(b"YZ");
    out
}

#[test]
fn unsized_block_on_non_seekable_reader_stops_at_stream_end() {
    let data = b"unsized block followed by more container data ".repeat(60);
    let mut streams = vec![("stored".to_string(), unsized_stream(&data))];
    // `xz -T1` writes Block Headers without sizes.
    if let Some(packed) = xz(&["-zc", "-T1"], &data) {
        streams.push(("xz -T1".into(), packed));
    }
    let opts = DecoderOptions {
        concatenated: false,
        ..Default::default()
    };
    for (name, packed) in &streams {
        let flags = packed[13];
        assert_eq!(flags & 0xC0, 0, "{name}: Block Header stores sizes");
        assert_eq!(decompress(packed).unwrap(), data, "{name}");
        // What follows the Stream: Stream Padding (not consumed without
        // `concatenated`), another Stream and other data.
        let mut trailer = vec![0u8; 4];
        trailer.extend_from_slice(packed);
        trailer.extend_from_slice(b"\x7Bnot xz");
        let mut input = packed.clone();
        input.extend_from_slice(&trailer);

        let (out, used) = decode_tiny(&input, opts);
        assert_eq!(out, data, "{name}: 1-byte");
        assert_eq!(used, packed.len(), "{name}: 1-byte decoder overran");

        for (max, cap) in [(1, 1), (3, 5), (7, 64), (4096, 8192)] {
            let inner = BufReader::with_capacity(cap, NonSeekable { data: &input, max });
            let mut r = XzReader::with_options(inner, opts);
            assert_eq!(read_small(&mut r).unwrap(), data, "{name}: {max}/{cap}");
            let mut inner = r.into_inner();
            let mut rest = Vec::new();
            inner.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, trailer, "{name}: {max}/{cap}: bytes after the Stream");
        }
    }
}

/// Periodic data (overlapping match copies at short distances) and runs of
/// one byte (RLE: distance 1) of many lengths.
fn periodic_inputs() -> Vec<(String, Vec<u8>)> {
    let mut r = Rng(0x1234_5678_9ABC_DEF1);
    let mut v = Vec::new();
    for period in [1usize, 2, 3, 4, 7, 8, 13, 37, 255, 256] {
        let pat: Vec<u8> = (0..period).map(|_| r.next() as u8).collect();
        v.push((
            format!("periodic-{period}"),
            pat.iter().cycle().take(50_000).copied().collect(),
        ));
    }
    // Runs from 1 byte to well over the longest match (273), with literals
    // and short repeats in between.
    let mut runs = Vec::new();
    for len in (1..300).chain([511, 512, 513, 4096, 70_000]) {
        runs.extend(std::iter::repeat_n(r.next() as u8, len));
        runs.push(r.next() as u8);
    }
    v.push(("rle-runs".into(), runs));
    let mut mixed = Vec::new();
    while mixed.len() < 200_000 {
        let byte = r.next() as u8;
        let len = r.below(600) as usize + 1;
        mixed.extend(std::iter::repeat_n(byte, len));
        let period = r.below(300) as usize + 1;
        let start = mixed.len().saturating_sub(period);
        for i in 0..r.below(400) as usize {
            mixed.push(mixed[start + i % period.min(mixed.len() - start)]);
        }
    }
    v.push(("rle-mixed".into(), mixed));
    v
}

#[test]
fn periodic_and_rle_round_trip() {
    for (name, data) in periodic_inputs() {
        for opts in [
            &["-0"][..],
            &["-6"],
            &["-9e"],
            &["--lzma2=preset=6,nice=273"],
        ] {
            let mut args = vec!["-zc", "-T1"];
            args.extend_from_slice(opts);
            let Some(packed) = xz(&args, &data) else {
                eprintln!("xz not available; skipping");
                return;
            };
            let what = format!("{name} {opts:?}");
            assert!(decompress(&packed).unwrap() == data, "{what}: one-shot");
            let d = DecoderOptions::default();
            let streamed = xz_native::decompress_streaming(&packed, d).unwrap();
            assert!(streamed == data, "{what}: streaming");
            let (tiny, used) = decode_tiny(&packed, d);
            assert!(tiny == data && used == packed.len(), "{what}: 1-byte");
            let inner = BufReader::with_capacity(
                5,
                NonSeekable {
                    data: &packed,
                    max: 3,
                },
            );
            let mut r = XzReader::new(inner);
            assert!(read_small(&mut r).unwrap() == data, "{what}: XzReader");
            assert!(r.into_inner().fill_buf().unwrap().is_empty());
        }
    }
}
