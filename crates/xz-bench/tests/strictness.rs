//! Decoder strictness cases ported from Lzma.Net (issue #11). Each input is
//! built from liblzma output with only one field made wrong (every other
//! size and CRC is fixed up). Both xz-native decoders (one-shot, streaming
//! and streaming through 1-byte buffers) must fail with the same error as
//! C liblzma 5.8.4.

use liblzma::stream::{Action, CONCATENATED, Check, Filters, LzmaOptions, Status, Stream};
use xz_native::{DecoderOptions, Error, XzDecoder, crc32};

/// Size of the integrity check for each Check ID.
const CHECK_SIZES: [usize; 16] = [0, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64];

fn compress(data: &[u8], preset: u32, check: Check) -> Vec<u8> {
    let mut f = Filters::new();
    f.lzma2(&LzmaOptions::new_preset(preset).unwrap());
    let mut s = Stream::new_stream_encoder(&f, check).unwrap();
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

/// Decodes with C liblzma, mapping `lzma_ret` to [`Error`]. `LZMA_BUF_ERROR`
/// with output space left means the input was truncated.
fn reference(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut s = Stream::new_stream_decoder(u64::MAX, CONCATENATED).unwrap();
    let mut out = Vec::with_capacity(1 << 16);
    loop {
        if out.len() == out.capacity() {
            out.reserve(1 << 16);
        }
        let pos = s.total_in() as usize;
        match s.process_vec(&input[pos..], &mut out, Action::Finish) {
            Ok(Status::StreamEnd) => return Ok(out),
            Ok(Status::MemNeeded) if out.len() < out.capacity() => return Err(Error::Truncated),
            Ok(_) => {}
            Err(liblzma::stream::Error::Data) => return Err(Error::Data),
            Err(liblzma::stream::Error::Options) => return Err(Error::Options),
            Err(liblzma::stream::Error::Format) => return Err(Error::Format),
            Err(liblzma::stream::Error::MemLimit) => return Err(Error::MemLimit),
            Err(e) => panic!("unexpected liblzma error {e:?}"),
        }
    }
}

/// Streaming decoder fed through 1-byte input and output buffers.
fn decode_tiny(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut d = XzDecoder::new();
    let mut out = Vec::new();
    let mut ip = 0;
    let mut idle = 0;
    loop {
        let end = (ip + 1).min(input.len());
        let mut b = [0u8; 1];
        let (u, w, s) = d.decode(&input[ip..end], &mut b, end == input.len())?;
        ip += u;
        out.extend_from_slice(&b[..w]);
        if s == xz_native::Status::StreamEnd {
            return if ip == input.len() {
                Ok(out)
            } else {
                Err(Error::Data)
            };
        }
        idle = if u + w == 0 { idle + 1 } else { 0 };
        assert!(idle < 4, "decoder makes no progress at {ip}");
    }
}

/// Asserts that liblzma and all xz-native paths fail with `expected`.
fn assert_error(name: &str, input: &[u8], expected: Error) {
    let as_lzma_ret = |e: Error| {
        if e == Error::CheckMismatch {
            Error::Data
        } else {
            e
        }
    };
    let lib = reference(input).map(|_| ());
    assert_eq!(lib, Err(as_lzma_ret(expected)), "{name}: liblzma");
    let opts = DecoderOptions::default();
    let flat = xz_native::decompress_with_options(input, opts).map(|_| ());
    assert_eq!(flat, Err(expected), "{name}: one-shot");
    let streaming = xz_native::decompress_streaming(input, opts).map(|_| ());
    assert_eq!(streaming, Err(expected), "{name}: streaming");
    assert_eq!(
        decode_tiny(input).map(|_| ()),
        Err(expected),
        "{name}: 1-byte"
    );
}

/// Asserts that liblzma and all xz-native paths decode `input` to `data`.
fn assert_decodes(name: &str, input: &[u8], data: &[u8]) {
    assert!(reference(input).unwrap() == data, "{name}: liblzma");
    let opts = DecoderOptions::default();
    assert!(
        xz_native::decompress_with_options(input, opts).unwrap() == data,
        "{name}: one-shot"
    );
    assert!(
        xz_native::decompress_streaming(input, opts).unwrap() == data,
        "{name}: streaming"
    );
    assert!(decode_tiny(input).unwrap() == data, "{name}: 1-byte");
}

fn read_vli(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0;
    for shift in (0..).step_by(7) {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return v;
        }
    }
    unreachable!()
}

fn write_vli(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn push_crc32(out: &mut Vec<u8>, from: usize) {
    let crc = crc32(&out[from..], 0);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// A Block taken apart so that its fields can be changed and re-encoded.
#[derive(Clone)]
struct Block {
    /// Filter count - 1 (Block Flags bits 0-1).
    filter_bits: u8,
    /// Raw Filter Flags.
    filters: Vec<u8>,
    /// Compressed Data (LZMA2 chunks).
    data: Vec<u8>,
    check: Vec<u8>,
    uncompressed: u64,
}

impl Block {
    fn header(&self, compressed: Option<u64>, uncompressed: Option<u64>) -> Vec<u8> {
        let mut h = vec![0, self.filter_bits];
        if let Some(c) = compressed {
            h[1] |= 0x40;
            write_vli(&mut h, c);
        }
        if let Some(u) = uncompressed {
            h[1] |= 0x80;
            write_vli(&mut h, u);
        }
        h.extend_from_slice(&self.filters);
        while (h.len() + 4) % 4 != 0 {
            h.push(0);
        }
        h[0] = ((h.len() + 4) / 4 - 1) as u8;
        push_crc32(&mut h, 0);
        h
    }

    /// Block Header (optionally with the sizes), data, padding and check.
    /// Returns the bytes and the Unpadded Size.
    fn encode(&self, compressed: Option<u64>, uncompressed: Option<u64>) -> (Vec<u8>, u64) {
        let mut b = self.header(compressed, uncompressed);
        let unpadded = (b.len() + self.data.len() + self.check.len()) as u64;
        b.extend_from_slice(&self.data);
        b.resize(b.len().next_multiple_of(4), 0);
        b.extend_from_slice(&self.check);
        (b, unpadded)
    }
}

/// A single .xz Stream taken apart.
#[derive(Clone)]
struct Xz {
    check: u8,
    blocks: Vec<Block>,
}

impl Xz {
    fn parse(xz: &[u8]) -> Self {
        let check = xz[7];
        let check_size = CHECK_SIZES[check as usize];
        let f = &xz[xz.len() - 12..];
        let backward = (u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize + 1) * 4;
        let index = &xz[xz.len() - 12 - backward..];
        let mut ip = 1;
        let count = read_vli(index, &mut ip);
        let mut blocks = Vec::new();
        let mut pos = 12;
        for _ in 0..count {
            let unpadded = read_vli(index, &mut ip) as usize;
            let uncompressed = read_vli(index, &mut ip);
            let header_size = (xz[pos] as usize + 1) * 4;
            let h = &xz[pos..pos + header_size];
            let mut p = 2;
            if h[1] & 0x40 != 0 {
                read_vli(h, &mut p);
            }
            if h[1] & 0x80 != 0 {
                read_vli(h, &mut p);
            }
            let start = p;
            for _ in 0..=(h[1] & 3) {
                read_vli(h, &mut p);
                let props = read_vli(h, &mut p) as usize;
                p += props;
            }
            let comp = unpadded - header_size - check_size;
            let data_start = pos + header_size;
            let check_start = data_start + comp.next_multiple_of(4);
            blocks.push(Block {
                filter_bits: h[1] & 3,
                filters: h[start..p].to_vec(),
                data: xz[data_start..data_start + comp].to_vec(),
                check: xz[check_start..check_start + check_size].to_vec(),
                uncompressed,
            });
            pos = check_start + check_size;
        }
        assert_eq!(pos, xz.len() - 12 - backward, "parse error");
        Xz { check, blocks }
    }

    /// Re-encodes the Stream. With `sizes`, Block Headers store the
    /// Compressed and Uncompressed Size. `index` may rewrite the Index
    /// fields (Number of Records and the Records) before the CRC32.
    fn build(&self, sizes: bool, index: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut out = stream_header(self.check);
        let mut records = Vec::new();
        for b in &self.blocks {
            let (bytes, unpadded) = if sizes {
                b.encode(Some(b.data.len() as u64), Some(b.uncompressed))
            } else {
                b.encode(None, None)
            };
            out.extend_from_slice(&bytes);
            records.push((unpadded, b.uncompressed));
        }
        let mut fields = Vec::new();
        write_vli(&mut fields, records.len() as u64);
        for (u, c) in records {
            write_vli(&mut fields, u);
            write_vli(&mut fields, c);
        }
        index(&mut fields);
        finish_stream(&mut out, self.check, &fields);
        out
    }
}

fn stream_header(check: u8) -> Vec<u8> {
    let mut h = vec![0xFD, b'7', b'z', b'X', b'Z', 0, 0, check];
    let crc = crc32(&h[6..8], 0);
    h.extend_from_slice(&crc.to_le_bytes());
    h
}

/// Appends the Index (from its fields after the Indicator) and the Footer.
fn finish_stream(out: &mut Vec<u8>, check: u8, index_fields: &[u8]) {
    let start = out.len();
    out.push(0);
    out.extend_from_slice(index_fields);
    out.resize(start + (out.len() - start).next_multiple_of(4), 0);
    push_crc32(out, start);
    let backward = ((out.len() - start) / 4 - 1) as u32;
    let f = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&backward.to_le_bytes());
    out.extend_from_slice(&[0, check]);
    let crc = crc32(&out[f + 4..f + 10], 0);
    out[f..f + 4].copy_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(b"YZ");
}

/// Compressible data that liblzma -0 encodes in one LZMA2 chunk.
fn one_chunk_data(seed: u64, period: usize) -> Vec<u8> {
    let mut x = seed;
    let mut data: Vec<u8> = (0..4096)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    for i in period..data.len() {
        data[i] = data[i - period];
    }
    data
}

/// Stored compressed size (minus one) of the first LZMA2 chunk, which must
/// reset the dictionary and set new properties.
fn first_chunk(data: &[u8]) -> u16 {
    assert!(
        data[0] >= 0xE0,
        "expected an LZMA chunk with a dictionary reset"
    );
    u16::from_be_bytes([data[3], data[4]])
}

/// Makes the first LZMA2 chunk `extra` bytes longer (0xFF filler after the
/// coded data) and its stored compressed size `extra` larger.
fn overstate_first_chunk(xz: &Xz, extra: u16) -> Xz {
    let mut xz = xz.clone();
    let d = &mut xz.blocks[0].data;
    let comp = first_chunk(d);
    let end = 6 + comp as usize + 1;
    d.splice(end..end, std::iter::repeat_n(0xFF, extra as usize));
    d[3..5].copy_from_slice(&(comp + extra).to_be_bytes());
    xz
}

#[test]
fn lzma2_chunk_compressed_size_overstated_by_one() {
    let data = one_chunk_data(1234, 1024);
    let packed = compress(&data, 0, Check::Crc64);
    let xz = Xz::parse(&packed);
    assert_eq!(
        xz.blocks[0].data[first_chunk(&xz.blocks[0].data) as usize + 7],
        0
    );
    for sizes in [false, true] {
        assert_decodes("rebuilt", &xz.build(sizes, |_| {}), &data);
        let bad = overstate_first_chunk(&xz, 1).build(sizes, |_| {});
        assert_error(&format!("overstated, sizes={sizes}"), &bad, Error::Data);
    }
}

#[test]
fn lzma2_chunk_compressed_size_understated_by_one() {
    let data = one_chunk_data(4321, 512);
    let xz = Xz::parse(&compress(&data, 0, Check::Crc64));
    let mut bad = xz.clone();
    let d = &mut bad.blocks[0].data;
    let comp = first_chunk(d);
    d[3..5].copy_from_slice(&(comp - 1).to_be_bytes());
    for sizes in [false, true] {
        let name = format!("understated, sizes={sizes}");
        assert_error(&name, &bad.build(sizes, |_| {}), Error::Data);
    }
}

#[test]
fn first_chunk_with_four_extra_bytes() {
    // Several chunks: only the first one is changed.
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 7) as u8 ^ (i / 1000) as u8)
        .collect();
    for check in [Check::None, Check::Crc32, Check::Sha256] {
        let xz = Xz::parse(&compress(&data, 0, check));
        for sizes in [false, true] {
            let bad = overstate_first_chunk(&xz, 4).build(sizes, |_| {});
            assert_error(&format!("4 extra, sizes={sizes}"), &bad, Error::Data);
        }
    }
}

#[test]
fn index_record_count_too_large() {
    let data = b"index record count guard";
    let xz = Xz::parse(&compress(data, 6, Check::Crc64));
    assert_decodes("rebuilt", &xz.build(false, |_| {}), data);
    for (count, name) in [(0x7F, "127 records"), (2, "2 records"), (0, "0 records")] {
        // Number of Records is one byte both before and after.
        let bad = xz.build(false, |f| f[0] = count);
        assert_error(name, &bad, Error::Data);
    }
    // An empty Stream claiming records.
    let mut empty = stream_header(1);
    finish_stream(&mut empty, 1, &[0x7F]);
    assert_error("empty, 127 records", &empty, Error::Data);
}

#[test]
fn index_non_canonical_vli() {
    let mut empty = stream_header(1);
    finish_stream(&mut empty, 1, &[0x00]);
    assert_decodes("empty", &empty, b"");
    let mut bad = stream_header(1);
    finish_stream(&mut bad, 1, &[0x80, 0x00]);
    assert_error("count 0x80 0x00", &bad, Error::Data);

    let data = b"non-canonical VLI in the Index";
    let xz = Xz::parse(&compress(data, 6, Check::Crc32));
    // Each Record field written with a redundant 0x80 0x00 tail.
    for field in 0..3 {
        let bad = xz.build(false, |f| {
            let mut pos = 0;
            for _ in 0..field {
                read_vli(f, &mut pos);
            }
            let start = pos;
            let v = read_vli(f, &mut pos);
            let mut enc = Vec::new();
            write_vli(&mut enc, v);
            *enc.last_mut().unwrap() |= 0x80;
            enc.push(0);
            f.splice(start..pos, enc);
        });
        assert_error(&format!("field {field}"), &bad, Error::Data);
    }
}

/// A Block with a single LZMA2 filter and no check holding `data`.
fn stored_block(data: &[u8], sizes: Option<(u64, u64)>) -> (Vec<u8>, u64) {
    let block = Block {
        filter_bits: 0,
        filters: vec![0x21, 1, 0],
        data: data.to_vec(),
        check: Vec::new(),
        uncompressed: 1,
    };
    block.encode(sizes.map(|s| s.0), sizes.map(|s| s.1))
}

#[test]
fn truncated_uncompressed_chunk() {
    let chunk = [0x01, 0x00, 0x00, 0x41];
    // The input ends right after the chunk.
    for sizes in [None, Some((4, 1)), Some((5, 1))] {
        let mut input = stream_header(0);
        let (block, _) = stored_block(&chunk, sizes);
        input.extend_from_slice(&block);
        let expected = if sizes.is_some_and(|s| s.0 == 4) {
            // All of the declared Compressed Size is there.
            Error::Data
        } else {
            Error::Truncated
        };
        assert_error(&format!("cut, sizes={sizes:?}"), &input, expected);
    }
    // The chunk has no end marker but the Stream goes on: the Index follows.
    for sizes in [None, Some((4, 1))] {
        let mut input = stream_header(0);
        let (block, unpadded) = stored_block(&chunk, sizes);
        input.extend_from_slice(&block);
        let mut fields = vec![1];
        write_vli(&mut fields, unpadded);
        write_vli(&mut fields, 1);
        finish_stream(&mut input, 0, &fields);
        assert_error(&format!("no end, sizes={sizes:?}"), &input, Error::Data);
    }
    // liblzma decodes up to the end of the input before it reports
    // truncation: a corrupt first chunk is a data error even when the
    // input is cut in a later chunk.
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 251) as u8 ^ (i >> 12) as u8)
        .collect();
    let mut xz = Xz::parse(&compress(&data, 0, Check::Crc32));
    // The range coder's first byte must be zero.
    assert_eq!(xz.blocks[0].data[6], 0);
    xz.blocks[0].data[6] = 1;
    for sizes in [false, true] {
        let input = xz.build(sizes, |_| {});
        let cut = &input[..input.len() / 2];
        assert_error(
            &format!("corrupt, then cut, sizes={sizes}"),
            cut,
            Error::Data,
        );
    }
    // Sanity: with the end marker it decodes.
    let mut input = stream_header(0);
    let (block, unpadded) = stored_block(&[0x01, 0x00, 0x00, 0x41, 0x00], None);
    input.extend_from_slice(&block);
    let mut fields = vec![1];
    write_vli(&mut fields, unpadded);
    write_vli(&mut fields, 1);
    finish_stream(&mut input, 0, &fields);
    assert_decodes("complete", &input, b"A");
}

#[test]
fn reserved_stream_flags() {
    let data = b"reserved stream flags";
    let packed = compress(data, 6, Check::Crc64);
    let fix_header = |m: &mut [u8]| {
        let crc = crc32(&m[6..8], 0);
        m[8..12].copy_from_slice(&crc.to_le_bytes());
    };
    let fix_footer = |m: &mut [u8]| {
        let f = m.len() - 12;
        let crc = crc32(&m[f + 4..f + 10], 0);
        m[f..f + 4].copy_from_slice(&crc.to_le_bytes());
    };
    for (byte, bits) in [(0, 0x01), (0, 0x80), (1, 0x10), (1, 0x80)] {
        // Stream Header of the first Stream.
        let mut m = packed.clone();
        m[6 + byte] |= bits;
        fix_header(&mut m);
        assert_error(&format!("header {byte}:{bits:#x}"), &m, Error::Options);

        // Stream Footer.
        let mut m = packed.clone();
        let f = m.len() - 12;
        m[f + 8 + byte] |= bits;
        fix_footer(&mut m);
        assert_error(&format!("footer {byte}:{bits:#x}"), &m, Error::Options);

        // Stream Header of a second, concatenated Stream.
        let mut second = packed.clone();
        second[6 + byte] |= bits;
        fix_header(&mut second);
        let mut m = packed.clone();
        m.extend_from_slice(&second);
        assert_error(&format!("second {byte}:{bits:#x}"), &m, Error::Options);
    }
}

#[test]
fn block_header_uncompressed_size_off_by_one() {
    let data = one_chunk_data(99, 300);
    let xz = Xz::parse(&compress(&data, 6, Check::Crc32));
    let b = &xz.blocks[0];
    let build = |compressed: Option<u64>, uncompressed: u64| {
        let mut out = stream_header(xz.check);
        let (block, unpadded) = b.encode(compressed, Some(uncompressed));
        out.extend_from_slice(&block);
        let mut fields = vec![1];
        write_vli(&mut fields, unpadded);
        // The Index states the real size: only the Block Header is wrong.
        write_vli(&mut fields, b.uncompressed);
        finish_stream(&mut out, xz.check, &fields);
        out
    };
    for compressed in [Some(b.data.len() as u64), None] {
        let name = |what| format!("{what}, compressed size {compressed:?}");
        assert_decodes(&name("exact"), &build(compressed, b.uncompressed), &data);
        let small = build(compressed, b.uncompressed - 1);
        assert_error(&name("too small"), &small, Error::Data);
        let large = build(compressed, b.uncompressed + 1);
        assert_error(&name("too large"), &large, Error::Data);
    }
}

/// A reserved Filter ID (2^62 and up) in a Block Header is corrupt data,
/// not an unsupported filter (filter_flags_decoder.c).
#[test]
fn reserved_filter_id() {
    let data = b"reserved filter IDs are data errors";
    let mut xz = Xz::parse(&compress(data, 6, Check::Crc64));
    let props = xz.blocks[0].filters[2];
    let mut f = Vec::new();
    write_vli(&mut f, (1 << 62) | 0x21);
    f.extend_from_slice(&[1, props]);
    xz.blocks[0].filters = f;
    assert_error("reserved ID", &xz.build(false, |_| {}), Error::Data);
}

/// When the input ends inside the Index, a record that doesn't match the
/// Blocks, or a wrong CRC32 byte already read, is a data error: liblzma
/// checks the records before the CRC32 and the CRC32 byte by byte.
#[test]
fn index_mismatch_before_truncation() {
    let data = b"index records are checked before the CRC32".repeat(3);
    let xz = Xz::parse(&compress(&data, 6, Check::Crc64));
    let good = xz.build(false, |_| {});
    assert_decodes("rebuilt", &good, &data);
    // Uncompressed Size + 1 (same VLI length), cut before the CRC32.
    let bad = xz.build(false, |f| *f.last_mut().unwrap() += 1);
    assert_error("record, cut at CRC32", &bad[..bad.len() - 16], Error::Data);
    // Unpadded Size + 4 (the records exceed the Blocks), cut right after
    // the record.
    let bad = xz.build(false, |f| f[1] += 4);
    let backward = u32::from_le_bytes(bad[bad.len() - 8..][..4].try_into().unwrap());
    let index = bad.len() - 12 - (backward as usize + 1) * 4;
    let mut p = index + 1;
    read_vli(&bad, &mut p);
    read_vli(&bad, &mut p);
    read_vli(&bad, &mut p);
    assert_error("sum, cut after record", &bad[..p], Error::Data);
    // A wrong first CRC32 byte, cut after it.
    let mut bad = good.clone();
    let crc = bad.len() - 16;
    bad[crc] ^= 1;
    assert_error("CRC32 byte, cut after it", &bad[..crc + 1], Error::Data);
    // A correct first CRC32 byte, cut after it: truncated.
    assert_error("cut in CRC32", &good[..crc + 1], Error::Truncated);
}
