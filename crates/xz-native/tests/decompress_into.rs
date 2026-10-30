//! `decompress_into`: appends, reuses capacity, restores the length on error,
//! and gives the same output as `decompress` with and without threads.

mod common;

use common::{Rng, corpus, xz};
use xz_native::{DecoderOptions, decompress, decompress_into, decompress_with_options_into};

#[test]
fn appends_and_matches_decompress() {
    for (name, data) in corpus().iter().filter(|(n, _)| !n.starts_with("small-")) {
        for args in [
            &["-zc", "-6"][..],
            &["-zc", "-T4", "--block-size=65536"][..],
        ] {
            let Some(packed) = xz(args, data) else { return };
            let mut out = b"prefix".to_vec();
            decompress_into(&packed, &mut out).unwrap();
            assert_eq!(&out[..6], b"prefix", "{name} {args:?}");
            assert!(out[6..] == *data, "{name} {args:?}");
            assert_eq!(out[6..], decompress(&packed).unwrap()[..]);

            // Reuse the buffer, threaded.
            out.clear();
            let opts = DecoderOptions::default().threads(4);
            decompress_with_options_into(&packed, opts, &mut out).unwrap();
            assert!(out == *data, "{name} {args:?} threaded");
        }
    }
}

#[test]
fn error_restores_length() {
    let data = b"some data for an xz stream ".repeat(5000);
    let Some(packed) = xz(&["-zc", "-T2", "--block-size=4096"], &data) else {
        return;
    };
    for cut in [packed.len() / 2, packed.len() - 1] {
        for threads in [1, 4] {
            let mut out = b"keep".to_vec();
            let opts = DecoderOptions::default().threads(threads);
            assert!(decompress_with_options_into(&packed[..cut], opts, &mut out).is_err());
            assert_eq!(out, b"keep", "cut={cut} threads={threads}");
        }
    }
}

/// One Block alternating stored chunks (random data) and LZMA chunks (text),
/// in runs shorter and longer than a chunk: the one-shot decoder appends
/// stored chunks and zero-fills only for LZMA, so both orders must work at
/// any position, including across its 256 KiB steps.
#[test]
fn mixed_stored_and_lzma_chunks() {
    let mut r = Rng(0x5EED);
    let mut data = Vec::new();
    for run in [3_000, 70_000, 200_000, 1_000, 300_000, 65_536, 5_000] {
        data.extend((0..run).map(|_| r.next() as u8));
        data.extend(
            b"text compresses into LZMA chunks. "
                .iter()
                .cycle()
                .take(run),
        );
    }
    for preset in [0, 6] {
        let packed = xz_native::compress(&data, preset).unwrap();
        let stream = xz_native::decompress_streaming(&packed, DecoderOptions::default()).unwrap();
        assert!(stream == data, "preset {preset}: streaming");
        assert!(decompress(&packed).unwrap() == data, "preset {preset}");
        let mut out = b"prefix".to_vec();
        decompress_into(&packed, &mut out).unwrap();
        assert!(out[6..] == data, "preset {preset}: into");
    }
}

/// Corrupt LZMA data (not a truncation) fails inside the Block decoder,
/// after the output buffer was handed to it. The buffer still comes back.
#[test]
fn corrupt_block_restores_buffer() {
    let data = b"some data for an xz stream ".repeat(5000);
    let mut packed = xz_native::compress(&data, 6).unwrap();
    // Stream Header, Block Header, LZMA2 control byte and two size bytes
    // each: the next byte starts the range coder and must be 0.
    let hdr = (packed[12] as usize + 1) * 4;
    let first = 12 + hdr + 6;
    assert_eq!(packed[first], 0);
    packed[first] = 1;
    for threads in [1, 4] {
        let mut out = b"prefix".to_vec();
        out.reserve(1 << 20);
        let opts = DecoderOptions::default().threads(threads);
        assert!(decompress_with_options_into(&packed, opts, &mut out).is_err());
        assert_eq!(out, b"prefix", "threads={threads}");
        assert!(
            out.capacity() >= 1 << 20,
            "threads={threads}: capacity kept"
        );
    }
}

fn push_vli(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Replaces the Index and Stream Footer of a single-Stream file, giving
/// each record the Uncompressed Size `claim(i, real)`.
fn rewrite_index(packed: &[u8], claim: impl Fn(usize, u64) -> u64) -> Vec<u8> {
    let r = xz_native::XzSeekableReader::new(std::io::Cursor::new(packed)).unwrap();
    let blocks = r.blocks().to_vec();
    let last = blocks.last().unwrap();
    let index = (last.compressed_offset + last.total_size()) as usize;
    let mut out = packed[..index].to_vec();
    out.push(0);
    push_vli(&mut out, blocks.len() as u64);
    for (i, b) in blocks.iter().enumerate() {
        push_vli(&mut out, b.unpadded_size);
        push_vli(&mut out, claim(i, b.uncompressed_size));
    }
    out.resize(index + (out.len() - index).next_multiple_of(4), 0);
    let crc = xz_native::crc32(&out[index..], 0);
    out.extend_from_slice(&crc.to_le_bytes());
    let backward = ((out.len() - index) / 4 - 1) as u32;
    let mut footer = backward.to_le_bytes().to_vec();
    footer.extend_from_slice(&packed[packed.len() - 4..packed.len() - 2]);
    out.extend_from_slice(&xz_native::crc32(&footer, 0).to_le_bytes());
    out.extend_from_slice(&footer);
    out.extend_from_slice(b"YZ");
    out
}

/// An Index that claims far more output than its Blocks can hold must not
/// make the threaded decoder allocate that much up front: it gives the
/// same error as the single-threaded decoder.
#[test]
fn lying_index_is_not_allocated() {
    let data = b"index sizes are checked against the Blocks ".repeat(4000);
    let opts = xz_native::EncoderOptions::preset(1)
        .unwrap()
        .block_size(16 << 10);
    let packed = xz_native::compress_with_options(&data, &opts).unwrap();
    // Sanity check: an unchanged rewrite decodes.
    assert_eq!(decompress(&rewrite_index(&packed, |_, u| u)).unwrap(), data);
    for claim in [1u64 << 61, 1 << 40, 1 << 33] {
        let bad = rewrite_index(&packed, |i, u| if i == 1 { claim } else { u });
        for threads in [1, 4] {
            let mut out = b"keep".to_vec();
            let opts = DecoderOptions::default().threads(threads);
            assert_eq!(
                decompress_with_options_into(&bad, opts, &mut out),
                Err(xz_native::Error::Data),
                "claim={claim} threads={threads}"
            );
            assert_eq!(out, b"keep");
        }
    }
}
