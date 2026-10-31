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
    let opts = DecoderOptions::default().concatenated(false);
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

/// A Block whose matches reach further back than the dictionary size in
/// its header is corrupt, also when the decoder has that much output at
/// hand (the one-shot decoder decodes into the output). liblzma: DATA_ERROR.
#[test]
fn match_beyond_declared_dictionary_is_data_error() {
    let mut r = Rng(0xD1C7);
    let part: Vec<u8> = (0..50_000).map(|_| r.next() as u8).collect();
    let data = [&part[..], &part[..]].concat();
    let lzma = xz_native::LzmaOptions::preset(6)
        .unwrap()
        .dict_size(64 << 10);
    let opts = xz_native::EncoderOptions::preset(6).unwrap().lzma(lzma);
    let mut packed = xz_native::compress_with_options(&data, &opts).unwrap();
    assert_eq!(decompress(&packed).unwrap(), data);

    // Set the LZMA2 dictionary size property to 4 KiB.
    let size = (packed[12] as usize + 1) * 4;
    let header = &mut packed[12..12 + size];
    let at = header.windows(2).position(|w| w == [0x21, 1]).unwrap() + 2;
    header[at] = 0;
    let crc = crc32(&header[..size - 4], 0);
    header[size - 4..].copy_from_slice(&crc.to_le_bytes());

    assert_eq!(decompress(&packed).err(), Some(xz_native::Error::Data));
    let opts = DecoderOptions::default();
    assert_eq!(
        xz_native::decompress_streaming(&packed, opts).err(),
        Some(xz_native::Error::Data)
    );
    let mut out = Vec::new();
    assert!(XzReader::new(&packed[..]).read_to_end(&mut out).is_err());
    let mt = DecoderOptions::default().threads(4);
    assert!(
        XzReader::with_options(&packed[..], mt)
            .read_to_end(&mut out)
            .is_err()
    );
}

/// Runs `input` through the one-shot, streaming and reader paths.
fn all_paths(input: &[u8], opts: DecoderOptions) -> [Result<Vec<u8>, xz_native::Error>; 3] {
    let mut out = Vec::new();
    let reader = XzReader::with_options(input, opts)
        .read_to_end(&mut out)
        .map(|_| out)
        .map_err(|e| {
            *e.get_ref()
                .and_then(|e| e.downcast_ref::<xz_native::Error>())
                .unwrap()
        });
    [
        xz_native::decompress_with_options(input, opts),
        xz_native::decompress_streaming(input, opts),
        reader,
    ]
}

/// Stream boundaries: later Streams, Stream Padding and unsupported checks
/// give the same result on every path.
#[test]
fn stream_boundaries_on_all_paths() {
    use xz_native::Error;
    let data = b"stream boundaries".repeat(100);
    let s = xz_native::compress(&data, 1).unwrap();
    let d = DecoderOptions::default();
    let cat = |parts: &[&[u8]]| parts.concat();

    // A later Stream with a bad magic: Data, not Format.
    let mut bad = s.clone();
    bad[1] = b'8';
    for r in all_paths(&cat(&[&s, &[0; 4], &bad]), d) {
        assert_eq!(r.err(), Some(Error::Data));
    }
    // Stream Padding that isn't a multiple of 4.
    for r in all_paths(&cat(&[&s, &[0; 3], &s]), d) {
        assert_eq!(r.err(), Some(Error::Data));
    }
    for r in all_paths(&cat(&[&s, &[0; 5]]), d) {
        assert_eq!(r.err(), Some(Error::Data));
    }
    // Valid padding and a second Stream.
    for r in all_paths(&cat(&[&s, &[0; 8], &s]), d) {
        assert_eq!(r.unwrap(), cat(&[&data, &data]));
    }
    // Check ID 2 is reserved (unsupported): decoded without the check, or
    // an error with fail_on_unsupported_check.
    let mut odd = s.clone();
    odd[7] = 2;
    let crc = crc32(&odd[6..8], 0);
    odd[8..12].copy_from_slice(&crc.to_le_bytes());
    let n = odd.len();
    odd[n - 3] = 2;
    let crc = crc32(&odd[n - 8..n - 2], 0);
    odd[n - 12..n - 8].copy_from_slice(&crc.to_le_bytes());
    let strict = DecoderOptions::default().fail_on_unsupported_check(true);
    for r in all_paths(&odd, strict) {
        assert_eq!(r.err(), Some(Error::Options));
    }
}

/// With `concatenated(false)`, data after the first Stream is ignored,
/// like `xz --single-stream`; by default it is an error.
#[test]
fn single_stream_ignores_trailing_data() {
    let data = b"single stream".repeat(50);
    let s = xz_native::compress(&data, 1).unwrap();
    let single = DecoderOptions::default().concatenated(false);
    for tail in [&[0u8; 4][..], b"junk", &s] {
        let input = [&s[..], tail].concat();
        for r in all_paths(&input, single) {
            assert_eq!(r.unwrap(), data);
        }
        // By default, padding and a second Stream are fine, junk isn't.
        assert_eq!(decompress(&input).is_err(), tail == b"junk");
    }
}
