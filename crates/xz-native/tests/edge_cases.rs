//! Fast tests for boundaries found by mutation testing (#22): exact memory
//! limits, streaming with odd buffer sizes and `finish`, BCJ buffering, and
//! BCJ in later Blocks of one-shot decoding.

mod common;

use common::{Rng, xz};
use std::path::PathBuf;
use xz_native::{
    DecoderOptions, Error, LzipDecoder, LzmaDecoder, Result, Status, XzDecoder, decompress,
    decompress_lzip, decompress_lzip_with_options, decompress_lzma, decompress_lzma_with_options,
    decompress_streaming, decompress_with_options,
};

/// Memory the LZMA1 decoders (.lzma, .lz) count for a dictionary, as
/// liblzma (64-bit) does: the dictionary plus the decoder state and
/// `LZMA_MEMUSAGE_BASE`, plus 32 bytes on x86 for its SSE2 dictionary copy.
fn dict_mem(dict: u32) -> u64 {
    dict as u64 + 65_984 + LZ_DICT_EXTRA
}

/// [`dict_mem`] for an .xz Block with only LZMA2.
fn lzma2_mem(dict: u32) -> u64 {
    dict as u64 + 66_168 + LZ_DICT_EXTRA
}

const LZ_DICT_EXTRA: u64 = if cfg!(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse2")
)) {
    32
} else {
    0
};

fn test_file(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Binary-ish data with x86 call opcodes, so BCJ changes something.
fn code_like(n: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u32;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match i % 9 {
                0 => 0xE8,
                5 => {
                    if x >> 31 == 0 {
                        0x00
                    } else {
                        0xFF
                    }
                }
                _ => (x >> 24) as u8 & 0x3F,
            }
        })
        .collect()
}

/// Streams `input` with the given chunk sizes (cycled). `finish` is set only
/// for the call that receives the last input byte, unless `finish_always`.
fn stream(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    finish_always: bool,
    mut step: impl FnMut(&[u8], &mut [u8], bool) -> Result<(usize, usize, Status)>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; *out_sizes.iter().max().unwrap()];
    let (mut ip, mut k) = (0, 0);
    loop {
        let n = in_sizes[k % in_sizes.len()].min(input.len() - ip);
        let o = out_sizes[k % out_sizes.len()];
        k += 1;
        let finish = finish_always || ip + n == input.len();
        let (u, w, s) = step(&input[ip..ip + n], &mut buf[..o], finish)?;
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return Ok(out);
        }
        assert!(k < 50_000_000, "no progress");
    }
}

#[test]
fn xz_memlimit_is_exact() {
    let data = b"memory limit boundary ".repeat(3000);
    let Some(packed) = xz(&["-zc", "--lzma2=preset=1,dict=1MiB"], &data) else {
        return;
    };
    let need = lzma2_mem(1 << 20);
    for (limit, ok) in [(need, true), (need - 1, false)] {
        let opts = DecoderOptions::default().memlimit(limit);
        let r = decompress_with_options(&packed, opts);
        assert_eq!(r.is_ok(), ok, "one-shot limit={limit}");
        if !ok {
            assert_eq!(r, Err(Error::MemLimit));
        }
        let r = decompress_streaming(&packed, opts);
        assert_eq!(
            r.map(|v| v == data),
            if ok { Ok(true) } else { Err(Error::MemLimit) }
        );
    }
}

#[test]
fn lzma_memlimit_is_exact() {
    let data = b"memory limit boundary ".repeat(3000);
    let Some(packed) = xz(
        &["-zc", "--format=lzma", "--lzma1=preset=1,dict=1MiB"],
        &data,
    ) else {
        return;
    };
    let need = dict_mem(1 << 20);
    assert_eq!(
        decompress_lzma_with_options(&packed, xz_native::DecoderOptions::new().memlimit(need))
            .unwrap(),
        data
    );
    assert_eq!(
        decompress_lzma_with_options(&packed, xz_native::DecoderOptions::new().memlimit(need - 1)),
        Err(Error::MemLimit)
    );
}

#[test]
fn lzip_memlimit_is_exact() {
    for name in ["good-1-v0.lz", "good-1-v1.lz"] {
        let packed = test_file(name);
        // lzip dictionary size: 2^(low 5 bits) minus (high 3 bits)/16 of it.
        let b = packed[5];
        let base = 1u32 << (b & 0x1F);
        let dict = base - (b as u32 >> 5) * (base / 16);
        let need = dict_mem(dict);
        let expected = decompress_lzip(&packed).unwrap();
        assert_eq!(
            decompress_lzip_with_options(&packed, xz_native::DecoderOptions::new().memlimit(need))
                .unwrap(),
            expected,
            "{name}"
        );
        assert_eq!(
            decompress_lzip_with_options(
                &packed,
                xz_native::DecoderOptions::new().memlimit(need - 1)
            ),
            Err(Error::MemLimit),
            "{name}"
        );
    }
}

#[test]
fn lzma_streaming_edges() {
    for name in [
        "good-known_size-with_eopm.lzma",
        "good-known_size-without_eopm.lzma",
        "good-unknown_size-with_eopm.lzma",
    ] {
        let packed = test_file(name);
        let expected = decompress_lzma(&packed).unwrap();
        for (ins, outs, always) in [
            (&[1usize][..], &[4096usize][..], false),
            (&[5, 20, 3][..], &[7, 1][..], false),
            (&[packed.len()][..], &[1][..], true),
            (&[2][..], &[1][..], false),
        ] {
            let mut d = LzmaDecoder::new();
            let out = stream(&packed, ins, outs, always, |i, o, f| d.decode(i, o, f)).unwrap();
            assert_eq!(out, expected, "{name} {ins:?} {outs:?} {always}");
        }
        // Truncated input with `finish` is an error, whatever the buffers.
        for cut in [1, 12, 13, 14, packed.len() - 1] {
            let mut d = LzmaDecoder::new();
            let r = stream(&packed[..cut], &[3], &[64], false, |i, o, f| {
                d.decode(i, o, f)
            });
            assert!(r.is_err(), "{name} cut={cut}");
        }
    }
}

#[test]
fn lzip_streaming_edges() {
    for name in [
        "good-1-v0.lz",
        "good-1-v1.lz",
        "good-2-v0-v1.lz",
        "good-2-v1-v1.lz",
    ] {
        let packed = test_file(name);
        let expected = decompress_lzip(&packed).unwrap();
        for (ins, outs, always) in [
            (&[1usize][..], &[4096usize][..], false),
            (&[3, 17][..], &[1, 9][..], false),
            (&[packed.len()][..], &[1][..], true),
        ] {
            let mut d = LzipDecoder::new();
            let out = stream(&packed, ins, outs, always, |i, o, f| d.decode(i, o, f)).unwrap();
            assert_eq!(out, expected, "{name} {ins:?} {outs:?} {always}");
        }
        for cut in [3, 6, packed.len() - 21, packed.len() - 1] {
            let mut d = LzipDecoder::new();
            let r = stream(&packed[..cut], &[1], &[64], false, |i, o, f| {
                d.decode(i, o, f)
            });
            assert!(r.is_err(), "{name} cut={cut}");
        }
    }
}

#[test]
fn bcj_streaming_buffers() {
    let data = code_like(70_000);
    let mut files = vec![
        test_file("good-1-arm64-lzma2-1.xz"),
        test_file("good-1-arm64-lzma2-2.xz"),
    ];
    for f in [
        &["--x86", "--lzma2=preset=1"][..],
        &["--x86=start=16", "--delta=dist=3", "--lzma2=preset=1"][..],
    ] {
        let mut args = vec!["-zc"];
        args.extend_from_slice(f);
        if let Some(p) = xz(&args, &data) {
            files.push(p);
        }
    }
    for packed in &files {
        let expected = decompress(packed).unwrap();
        for (ins, outs) in [
            (&[1usize][..], &[1usize][..]),
            (&[7, 64][..], &[3, 5, 13, 4096][..]),
            (&[4096][..], &[2, 9][..]),
        ] {
            let mut d = XzDecoder::new();
            let out = stream(packed, ins, outs, false, |i, o, f| d.decode(i, o, f)).unwrap();
            assert!(out == expected, "{ins:?} {outs:?}");
        }
        // Random sizes per call, so the filter's 32-byte buffer is left at
        // every fill level.
        let mut r = Rng(7);
        for _ in 0..4 {
            let ins: Vec<usize> = (0..97).map(|_| 1 + r.below(50) as usize).collect();
            let outs: Vec<usize> = (0..89).map(|_| 1 + r.below(40) as usize).collect();
            let mut d = XzDecoder::new();
            let out = stream(packed, &ins, &outs, false, |i, o, f| d.decode(i, o, f)).unwrap();
            assert!(out == expected, "random sizes");
        }
    }
}

#[test]
fn bcj_in_later_blocks_one_shot() {
    let data = code_like(200_000);
    for f in [
        &["--x86", "--lzma2=preset=1"][..],
        &["--x86=start=4096", "--lzma2=preset=1"][..],
    ] {
        let mut args = vec!["-zc", "--block-size=30000"];
        args.extend_from_slice(f);
        let Some(packed) = xz(&args, &data) else {
            return;
        };
        assert!(decompress(&packed).unwrap() == data, "{f:?}");
        let opts = DecoderOptions::default().threads(4);
        assert!(
            decompress_with_options(&packed, opts).unwrap() == data,
            "{f:?} threaded"
        );
    }
}

/// x86 code with call/jump opcodes at every distance from 1 to 9, so the
/// BCJ x86 mask logic sees calls exactly 5 bytes apart and closer.
fn dense_x86(n: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9u32;
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let gap = 1 + (x >> 28) as usize % 9;
        v.push(if x & 0x100 == 0 { 0xE8 } else { 0xE9 });
        for k in 1..gap {
            v.push(match k {
                4 => {
                    if x & 0x200 == 0 {
                        0x00
                    } else {
                        0xFF
                    }
                }
                _ => (x >> (k * 3)) as u8,
            });
        }
    }
    v.truncate(n);
    v
}

#[test]
fn every_bcj_filter_in_xz() {
    let data = dense_x86(40_000);
    for f in [
        "--x86",
        "--powerpc",
        "--ia64",
        "--arm",
        "--armthumb",
        "--arm64",
        "--sparc",
        "--riscv",
    ] {
        let Some(packed) = xz(&["-zc", f, "--lzma2=preset=0"], &data) else {
            return;
        };
        assert!(decompress(&packed).unwrap() == data, "{f}");
        assert!(
            decompress_streaming(&packed, DecoderOptions::default()).unwrap() == data,
            "{f}"
        );
    }
}

#[test]
fn bcj_filter_ids() {
    use xz_native::BcjKind::*;
    let ids: Vec<u64> = [X86, PowerPc, Ia64, Arm, ArmThumb, Sparc, Arm64, RiscV]
        .iter()
        .map(|k| k.filter_id())
        .collect();
    assert_eq!(ids, [4, 5, 6, 7, 8, 9, 10, 11]);
}

/// Rewrites the 4-byte BCJ start offset in the first Block Header of an
/// .xz made with `--<filter>=start=<old>` and fixes the header CRC.
fn patch_bcj_start(packed: &[u8], id: u8, old: u32, new: u32) -> Vec<u8> {
    let mut v = packed.to_vec();
    let size = (v[12] as usize + 1) * 4;
    let mut pat = vec![id, 4];
    pat.extend_from_slice(&old.to_le_bytes());
    let at = v[12..12 + size]
        .windows(6)
        .position(|w| w == pat)
        .expect("BCJ props")
        + 12
        + 2;
    v[at..at + 4].copy_from_slice(&new.to_le_bytes());
    let crc = xz_native::crc32(&v[12..12 + size - 4], 0);
    v[12 + size - 4..12 + size].copy_from_slice(&crc.to_le_bytes());
    v
}

#[test]
fn bcj_start_offset_alignment() {
    let data = dense_x86(10_000);
    // (filter, ID, alignment)
    for (f, id, align) in [
        ("x86", 4u8, 1u32),
        ("powerpc", 5, 4),
        ("ia64", 6, 16),
        ("arm", 7, 4),
        ("armthumb", 8, 2),
        ("sparc", 9, 4),
        ("arm64", 10, 4),
        ("riscv", 11, 2),
    ] {
        // No check: a different start offset changes the output.
        let opt = format!("--{f}=start=64");
        let Some(packed) = xz(&["-zc", "--check=none", &opt, "--lzma2=preset=0"], &data) else {
            return;
        };
        // Aligned start offsets decode; unaligned ones are unsupported options.
        let ok = patch_bcj_start(&packed, id, 64, 64 + align);
        assert!(decompress(&ok).is_ok(), "{f} aligned");
        if align > 1 {
            let bad = patch_bcj_start(&packed, id, 64, 64 + align / 2);
            assert_eq!(decompress(&bad), Err(Error::Options), "{f} misaligned");
            assert_eq!(
                decompress_streaming(&bad, DecoderOptions::default()),
                Err(Error::Options),
                "{f}"
            );
        }
    }
}

mod seekable {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use xz_native::XzSeekableReader;

    fn open(v: &[u8]) -> std::io::Result<XzSeekableReader<Cursor<Vec<u8>>>> {
        XzSeekableReader::new(Cursor::new(v.to_vec()))
    }

    fn kind(r: std::io::Result<impl Sized>) -> Option<Error> {
        r.err()
            .map(|e| *e.get_ref().unwrap().downcast_ref::<Error>().unwrap())
    }

    fn read_all_and_seek(v: &[u8], data: &[u8]) {
        let mut r = open(v).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert!(out == data);
        let mut x = 7u64;
        for _ in 0..40 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let pos = (x >> 33) % (data.len() as u64 + 1);
            r.seek(SeekFrom::Start(pos)).unwrap();
            let mut b = vec![0u8; ((x >> 20) % 5000) as usize];
            let n = r.read(&mut b).unwrap();
            assert!(b[..n] == data[pos as usize..pos as usize + n], "pos={pos}");
        }
    }

    #[test]
    fn file_size_checks() {
        let Some(empty) = xz(&["-zc"], b"") else {
            return;
        };
        // A Stream Header alone: too short for a Stream, a data error.
        assert_eq!(kind(open(&empty[..12])), Some(Error::Data));
        // Shorter than a Stream Header: not .xz at all.
        assert_eq!(kind(open(&empty[..8])), Some(Error::Format));
        // Size not a multiple of four.
        let mut v = empty.clone();
        v.extend_from_slice(&[0, 0]);
        assert_eq!(kind(open(&v)), Some(Error::Data));
        v.extend_from_slice(&[0, 0]);
        assert!(open(&v).is_ok());
    }

    #[test]
    fn long_padding_and_multiple_streams() {
        let a = b"first stream ".repeat(5000);
        let b = b"second".repeat(3000);
        let (Some(pa), Some(pb)) = (xz(&["-zc"], &a), xz(&["-zc", "--check=sha256"], &b)) else {
            return;
        };
        // Padding longer than the 64 KiB read buffer, between and after Streams.
        let mut v = pa.clone();
        v.extend(std::iter::repeat_n(0u8, 70_000));
        v.extend_from_slice(&pb);
        v.extend(std::iter::repeat_n(0u8, 140_000));
        let mut data = a.clone();
        data.extend_from_slice(&b);
        read_all_and_seek(&v, &data);
        assert_eq!(open(&v).unwrap().stream_count(), 2);
        // Padding that isn't a multiple of four.
        let mut bad = pa.clone();
        bad.extend(std::iter::repeat_n(0u8, 70_002));
        bad.extend_from_slice(&pb);
        assert_eq!(kind(open(&bad)), Some(Error::Data));
        // A second Stream Header that doesn't fit in front of its Blocks.
        let mut short = pa[..8].to_vec();
        short.extend_from_slice(&pb);
        assert!(open(&short).is_err());
    }

    #[test]
    fn index_larger_than_buffer() {
        // Many tiny Blocks give an Index of more than 64 KiB.
        let data: Vec<u8> = (0..120_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let Some(v) = xz(&["-zc", "-0", "--block-size=4"], &data) else {
            return;
        };
        let r = open(&v).unwrap();
        assert_eq!(r.block_count(), 30_000);
        drop(r);
        read_all_and_seek(&v, &data);
    }

    #[test]
    fn blocks_larger_than_buffer() {
        // Incompressible Blocks of 300 KiB need several buffer refills.
        let data: Vec<u8> = (0..900_000u32)
            .map(|i| (i.wrapping_mul(2_246_822_519) >> 13) as u8)
            .collect();
        let Some(v) = xz(&["-zc", "-0", "--block-size=300000"], &data) else {
            return;
        };
        read_all_and_seek(&v, &data);
    }
}

/// Corruptions of a multi-Block file give the same result with threads as
/// without, for one-shot decoding and for `XzReader`.
#[test]
fn threaded_errors_match_single_threaded() {
    use std::io::Read;
    let data = code_like(300_000);
    let Some(packed) = xz(&["-zc", "-T4", "-1", "--block-size=40000"], &data) else {
        return;
    };
    let single = |v: &[u8]| decompress(v);
    let threaded = |v: &[u8]| decompress_with_options(v, DecoderOptions::default().threads(4));
    let reader = |v: &[u8], threads| {
        let opts = DecoderOptions::default().threads(threads);
        let mut out = Vec::new();
        let r = xz_native::XzReader::with_options(std::io::BufReader::with_capacity(4096, v), opts)
            .read_to_end(&mut out);
        r.map(|_| out)
            .map_err(|e| *e.get_ref().unwrap().downcast_ref::<Error>().unwrap())
    };
    let mut x = 0x1234_5678_9ABC_DEF1u64;
    for i in 0..400 {
        let mut v = packed.clone();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        match i % 4 {
            0 => {
                let p = (x as usize) % v.len();
                v[p] ^= 1 << ((x >> 40) % 8);
            }
            1 => {
                // Damage near the end: Index, Footer, last Blocks.
                let p = v.len() - 1 - (x as usize) % 200.min(v.len());
                v[p] = (x >> 32) as u8;
            }
            2 => v.truncate((x as usize) % v.len()),
            _ => {
                let p = 12 + (x as usize) % 64;
                v[p] ^= 0x80;
            }
        }
        let a = single(&v);
        assert_eq!(threaded(&v), a, "case {i}");
        let r1 = reader(&v, 1);
        let r4 = reader(&v, 4);
        match (&r1, &r4) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "case {i}"),
            (Err(x), Err(y)) => assert_eq!(x, y, "case {i}"),
            _ => panic!(
                "case {i}: XzReader single {:?} vs threaded {:?}",
                r1.is_ok(),
                r4.is_ok()
            ),
        }
        assert_eq!(a.is_ok(), r1.is_ok(), "case {i}: one-shot vs reader");
    }
}

#[test]
fn check_type_after_stream_header() {
    let packed = xz(&["-zc", "--check=sha256"], b"check").unwrap_or_else(|| {
        // Without xz: the header bytes from our own encoder are the same.
        let opts = xz_native::EncoderOptions::default().check(xz_native::Check::Sha256);
        xz_native::compress_with_options(b"check", &opts).unwrap()
    });
    let mut d = XzDecoder::new();
    assert_eq!(d.check(), None);
    let mut out = [0u8; 16];
    d.decode(&packed[..11], &mut out, false).unwrap();
    assert_eq!(d.check(), None);
    d.decode(&packed[11..12], &mut out, false).unwrap();
    assert_eq!(d.check(), Some(xz_native::Check::Sha256));
}

/// Reads the VLI at `v[*p..]`.
fn read_vli(v: &[u8], p: &mut usize) -> u64 {
    let mut x = 0u64;
    for i in 0.. {
        let b = v[*p];
        *p += 1;
        x |= ((b & 0x7F) as u64) << (7 * i);
        if b < 0x80 {
            break;
        }
    }
    x
}

#[test]
fn compressed_size_shorter_than_data() {
    // Running out of the declared Compressed Size is a data error even
    // though the input itself is complete.
    let data = code_like(100_000);
    let opts = xz_native::EncoderOptions::default()
        .threads(2)
        .block_size(40_000);
    let packed = xz_native::compress_with_options(&data, &opts).unwrap();
    let size = (packed[12] as usize + 1) * 4;
    assert_eq!(packed[13] & 0x40, 0x40, "Compressed Size present");
    let mut p = 14;
    let comp = read_vli(&packed, &mut p);
    let len = p - 14;
    let mut v = packed.clone();
    let mut x = comp - 8;
    for i in 0..len {
        v[14 + i] = (x & 0x7F) as u8 | if i + 1 < len { 0x80 } else { 0 };
        x >>= 7;
    }
    assert_eq!(x, 0);
    let crc = xz_native::crc32(&v[12..12 + size - 4], 0);
    v[12 + size - 4..12 + size].copy_from_slice(&crc.to_le_bytes());

    assert_eq!(decompress(&v), Err(Error::Data));
    assert_eq!(
        decompress_streaming(&v, DecoderOptions::default()),
        Err(Error::Data)
    );
    let threaded = DecoderOptions::default().threads(4);
    assert_eq!(decompress_with_options(&v, threaded), Err(Error::Data));
}

/// Dictionary sizes from untrusted headers that can't be allocated (on
/// 32-bit targets, anything near 4 GiB) give an error, not a panic.
#[test]
fn huge_lzma_dictionary_is_an_error_on_32_bit() {
    if usize::BITS != 32 {
        return;
    }
    for dict in [0x8000_0000u32, 0xFFFF_FF00, u32::MAX] {
        let mut lzma = vec![0x5D];
        lzma.extend_from_slice(&dict.to_le_bytes());
        lzma.extend_from_slice(&[0xFF; 8]);
        lzma.extend_from_slice(&[0; 16]);
        assert_eq!(
            xz_native::decompress_lzma(&lzma).err(),
            Some(xz_native::Error::MemLimit),
            "dict {dict:#x}"
        );
    }
}

/// The .lz dictionary size byte: 2^n minus 0/16 to 7/16 of it.
#[test]
fn lzip_dictionary_size_fractions() {
    for (b2log, frac) in [(20u32, 0u32), (20, 1), (20, 7), (29, 3), (13, 5)] {
        let dict = (1 << b2log) - (frac << (b2log - 4));
        let header = [b'L', b'Z', b'I', b'P', 1, (frac << 5 | b2log) as u8];
        let mut dec = LzipDecoder::new();
        dec.decode(&header, &mut [0; 16], false).unwrap();
        assert_eq!(dec.memusage(), dict_mem(dict), "2^{b2log} - {frac}/16");
    }
}

/// .lz decoding one byte of input at a time without `finish`, and a call
/// without output space at the end of the input: neither is truncation.
#[test]
fn lzip_steps_are_not_truncation() {
    let packed = test_file("good-2-v1-v1.lz");
    let expected = decompress_lzip(&packed).unwrap();
    let mut d = LzipDecoder::new();
    let out = stream(&packed, &[1], &[4096], false, |i, o, f| d.decode(i, o, f)).unwrap();
    assert!(out == expected);

    let mut d = LzipDecoder::new();
    let mut buf = vec![0u8; expected.len() + 1];
    let (u, w, _) = d.decode(&packed[..20], &mut buf, false).unwrap();
    assert_eq!(u, 20);
    assert_eq!(d.decode(&[], &mut [], true), Ok((0, 0, Status::Ok)));
    let (_, w2, s) = d.decode(&packed[20..], &mut buf[w..], true).unwrap();
    assert_eq!(s, Status::StreamEnd);
    assert!(buf[..w + w2] == expected);
}

/// The one-shot decoder (`decompress`, which decodes each Block straight
/// into the output) and the streaming decoder must agree on damaged files:
/// the same data, or the same error (`Data` for corruption before a cut,
/// `Truncated` for a clean cut, as liblzma decides). Multi-Block files with
/// and without sizes in their Block Headers, cut and corrupted all over.
#[test]
fn one_shot_and_streaming_agree_on_damage() {
    let data = code_like(60_000);
    let mut files = Vec::new();
    for args in [
        &["-zc", "-T2", "--block-size=20000", "--lzma2=preset=1"][..],
        &[
            "-zc",
            "-T1",
            "--block-size=20000",
            "--x86",
            "--lzma2=preset=1",
        ][..],
    ] {
        if let Some(f) = xz(args, &data) {
            files.push(f);
        }
    }
    for f in files {
        let opts = DecoderOptions::default();
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for cut in (1..f.len()).step_by(61) {
            cases.push(f[..cut].to_vec());
        }
        for i in (12..f.len()).step_by(97) {
            for bit in [0x01, 0x40] {
                let mut m = f.clone();
                m[i] ^= bit;
                cases.push(m.clone());
                // Corrupt and cut short after the damage.
                m.truncate((i + 50).min(m.len()));
                cases.push(m);
            }
        }
        for c in &cases {
            let one_shot = decompress_with_options(c, opts);
            let streaming = decompress_streaming(c, opts);
            assert!(
                one_shot == streaming,
                "{} bytes: one-shot {:?}, streaming {:?}",
                c.len(),
                one_shot.as_ref().map(|v| v.len()),
                streaming.as_ref().map(|v| v.len())
            );
        }
    }
}
