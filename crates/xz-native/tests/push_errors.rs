//! Error handling of the push API, like liblzma's `lzma_code`: errors are
//! final, output produced before an error is returned first, and encoders
//! check the sequence of actions.

use std::io::Read;
use xz_native::{
    Action, BcjKind, EncoderOptions, Error, Filter, LzmaDecoder, LzmaEncoder, RawDecoder,
    RawEncoder, RawOptions, Status, XzDecoder, XzEncoder, XzReader, compress, crc32,
};

fn sample() -> Vec<u8> {
    b"push decoders and encoders handle errors like liblzma. ".repeat(2000)
}

/// Feeds `input` in `step`-byte pieces until StreamEnd or an error.
fn feed(dec: &mut XzDecoder, input: &[u8], step: usize) -> (Result<Status, Error>, usize) {
    let mut ip = 0;
    let mut out = vec![0u8; 4096];
    loop {
        let end = (ip + step).min(input.len());
        match dec.decode(&input[ip..end], &mut out, end == input.len()) {
            Ok((u, _, Status::StreamEnd)) => return (Ok(Status::StreamEnd), ip + u),
            Ok((u, _, _)) => ip += u,
            Err(e) => return (Err(e), ip),
        }
    }
}

/// A Stream Footer whose Backward Size doesn't match the Index (with a
/// valid footer CRC32): the error repeats on every later call, it doesn't
/// panic.
#[test]
fn footer_error_is_final() {
    let mut packed = compress(&sample(), 6).unwrap();
    let f = packed.len() - 12;
    packed[f + 4] = packed[f + 4].wrapping_add(1);
    let crc = crc32(&packed[f + 4..f + 10], 0);
    packed[f..f + 4].copy_from_slice(&crc.to_le_bytes());

    let mut r = XzReader::new(&packed[..]);
    let mut out = Vec::new();
    assert!(r.read_to_end(&mut out).is_err());
    let mut buf = [0u8; 100];
    assert!(r.read(&mut buf).is_err());
    assert!(r.read(&mut buf).is_err());

    let mut dec = XzDecoder::new();
    let (r, used) = feed(&mut dec, &packed, packed.len());
    assert_eq!(r, Err(Error::Data));
    let mut out = [0u8; 16];
    assert_eq!(
        dec.decode(&packed[used..], &mut out, true),
        Err(Error::Data)
    );
}

/// After a check mismatch, feeding the rest of the file must not reach
/// the end of the stream.
#[test]
fn check_mismatch_is_final() {
    let mut packed = compress(&sample(), 6).unwrap();
    // The last byte of the CRC64, just before the Index.
    let index = {
        let backward = u32::from_le_bytes(packed[packed.len() - 8..][..4].try_into().unwrap());
        packed.len() - 12 - (backward as usize + 1) * 4
    };
    packed[index - 1] ^= 1;
    for step in [1, 7, 4096, packed.len()] {
        let mut dec = XzDecoder::new();
        let (r, used) = feed(&mut dec, &packed, step);
        assert_eq!(r, Err(Error::CheckMismatch), "step {step}");
        // The rest of the input changes nothing.
        let (r, _) = feed(&mut dec, &packed[used..], step);
        assert_eq!(r, Err(Error::CheckMismatch), "step {step}");
    }
}

/// With truncated input and `finish`, the output decoded before the cut is
/// returned first; the next call reports the truncation.
#[test]
fn truncated_input_returns_output_first() {
    let data = sample();
    let packed = compress(&data, 6).unwrap();
    let cut = &packed[..packed.len() - 30];
    let mut dec = XzDecoder::new();
    let mut out = vec![0u8; data.len() * 2];
    let (used, written, status) = dec.decode(cut, &mut out, true).unwrap();
    assert_eq!(status, Status::Ok);
    assert_eq!(written, data.len());
    assert_eq!(out[..written], data[..]);
    assert_eq!(
        dec.decode(&cut[used..], &mut out, true),
        Err(Error::Truncated)
    );

    // The same for .lzma.
    let lzma = xz_native::compress_lzma(&data, 6).unwrap();
    let cut = &lzma[..lzma.len() - 3];
    let mut dec = LzmaDecoder::new();
    let (used, written, status) = dec.decode(cut, &mut out, true).unwrap();
    assert_eq!(status, Status::Ok);
    assert!(written > 0 && out[..written] == data[..written]);
    assert_eq!(
        dec.decode(&cut[used..], &mut out, true),
        Err(Error::Truncated)
    );
}

/// Corrupt raw LZMA2 data: the error repeats.
#[test]
fn raw_decoder_error_is_final() {
    let opts = RawOptions::default();
    let mut raw = xz_native::compress_raw(&sample(), &opts).unwrap();
    raw[0] = 3; // an invalid LZMA2 control byte
    let mut dec = RawDecoder::with_options(&opts).unwrap();
    let mut out = [0u8; 64];
    assert_eq!(dec.decode(&raw, &mut out, true), Err(Error::Data));
    raw[0] = 1;
    assert_eq!(dec.decode(&raw, &mut out, true), Err(Error::Data));
}

/// Once Finish has started, the encoder accepts only Finish with the rest
/// of the same input. Run after Finish used to end the raw LZMA2 stream
/// and then write more chunks after its end marker.
#[test]
fn finish_then_run_is_usage_error() {
    let data = sample();
    let mut big = vec![0u8; data.len() + 1024];

    let mut e = RawEncoder::with_options(&RawOptions::default()).unwrap();
    let (u, _, s) = e.encode(&data, &mut [0u8; 1], Action::Finish).unwrap();
    assert_eq!(s, Status::Ok);
    let left = &data[u..];
    assert_eq!(e.encode(b"more", &mut big, Action::Run), Err(Error::Usage));
    assert_eq!(
        e.encode(left, &mut big, Action::SyncFlush),
        Err(Error::Usage)
    );
    if !left.is_empty() {
        assert_eq!(
            e.encode(&left[1..], &mut big, Action::Finish),
            Err(Error::Usage)
        );
    }
    // A Usage error from the sequence check doesn't end the stream:
    // finishing correctly still works.
    let mut packed = vec![0u8; 0];
    let mut first = [0u8; 1];
    let mut e = RawEncoder::with_options(&RawOptions::default()).unwrap();
    let (u, w, _) = e.encode(&data, &mut first, Action::Finish).unwrap();
    packed.extend_from_slice(&first[..w]);
    assert_eq!(e.encode(b"x", &mut big, Action::Run), Err(Error::Usage));
    let mut ip = u;
    loop {
        let (u, w, s) = e.encode(&data[ip..], &mut big, Action::Finish).unwrap();
        ip += u;
        packed.extend_from_slice(&big[..w]);
        if s == Status::StreamEnd {
            break;
        }
    }
    assert_eq!(ip, data.len());
    assert_eq!(
        xz_native::decompress_raw(&packed, &RawOptions::default()).unwrap(),
        data
    );
    // After the end, calls return StreamEnd.
    assert_eq!(
        e.encode(&[], &mut big, Action::Finish).unwrap(),
        (0, 0, Status::StreamEnd)
    );

    // The same for XzEncoder.
    let mut e = XzEncoder::new(6).unwrap();
    let (_, _, s) = e.encode(&data, &mut [0u8; 8], Action::Finish).unwrap();
    assert_eq!(s, Status::Ok);
    assert_eq!(e.encode(b"more", &mut big, Action::Run), Err(Error::Usage));
}

/// A flush must also be repeated with the input that is left until it
/// returns StreamEnd.
#[test]
fn flush_sequence_is_checked() {
    let data = sample();
    let mut e = XzEncoder::new(6).unwrap();
    let mut small = [0u8; 16];
    let (u, _, s) = e.encode(&data, &mut small, Action::SyncFlush).unwrap();
    assert_eq!(s, Status::Ok);
    assert_eq!(
        e.encode(&data[u..], &mut small, Action::Run),
        Err(Error::Usage)
    );
    assert_eq!(
        e.encode(&data[u..], &mut small, Action::Finish),
        Err(Error::Usage)
    );
    let mut ip = u;
    loop {
        let (u, _, s) = e
            .encode(&data[ip..], &mut small, Action::SyncFlush)
            .unwrap();
        ip += u;
        if s == Status::StreamEnd {
            break;
        }
    }
    assert_eq!(ip, data.len());
}

/// An encoder error ends the stream: the retry can't produce a stream that
/// misses the headers written before the error.
#[test]
fn encoder_error_is_final() {
    let opts = EncoderOptions::default().filters(vec![Filter::Bcj {
        kind: BcjKind::X86,
        start_offset: 0,
    }]);
    let mut e = XzEncoder::with_options(opts).unwrap();
    let mut out = [0u8; 4096];
    assert_eq!(
        e.encode(b"data", &mut out, Action::SyncFlush),
        Err(Error::InvalidOptions)
    );
    assert_eq!(
        e.encode(b"data", &mut out, Action::Finish),
        Err(Error::InvalidOptions)
    );
}

/// .lzma has no flushes: the action is rejected before the header is
/// written, and the encoder is still usable.
#[test]
fn lzma_encoder_rejects_flush_before_writing() {
    let mut e = LzmaEncoder::new(6).unwrap();
    let mut out = vec![0u8; 4096];
    for a in [Action::SyncFlush, Action::FullFlush, Action::FullBarrier] {
        assert_eq!(e.encode(b"abc", &mut out, a), Err(Error::Usage));
    }
    let (u, w, s) = e.encode(b"abc", &mut out, Action::Finish).unwrap();
    assert_eq!((u, s), (3, Status::StreamEnd));
    assert_eq!(xz_native::decompress_lzma(&out[..w]).unwrap(), b"abc");
}

/// .lzma decoding one byte of input at a time without `finish`, and calls
/// without output space: neither is truncation.
#[test]
fn lzma_decoder_steps_are_not_truncation() {
    let data = sample();
    let lzma = xz_native::compress_lzma(&data, 6).unwrap();
    let mut dec = LzmaDecoder::new();
    let (mut ip, mut got, mut buf) = (0, Vec::new(), [0u8; 4096]);
    for _ in 0..10 * lzma.len() {
        let end = (ip + 1).min(lzma.len());
        let (u, w, s) = dec
            .decode(&lzma[ip..end], &mut buf, end == lzma.len())
            .unwrap();
        ip += u;
        got.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            break;
        }
    }
    assert_eq!(ip, lzma.len());
    assert!(got == data);
    let mut out = vec![0u8; data.len()];

    // No output space, even with `finish` and no input left.
    let mut dec = LzmaDecoder::new();
    assert_eq!(
        dec.decode(&lzma[..13], &mut [], false),
        Ok((13, 0, Status::Ok))
    );
    assert_eq!(dec.decode(&[], &mut [], true), Ok((0, 0, Status::Ok)));
    let (u, w, _) = dec.decode(&lzma[13..], &mut out, true).unwrap();
    assert!(w == data.len() && out == data);
    // The end marker, read once there is output space again.
    let rest = &lzma[13 + u..];
    assert_eq!(
        dec.decode(rest, &mut [0; 1], true),
        Ok((rest.len(), 0, Status::StreamEnd))
    );
}

#[test]
fn lzma_decoder_memlimit() {
    let opts = xz_native::DecoderOptions::new().memlimit(123_456);
    let mut dec = LzmaDecoder::with_options(opts);
    assert_eq!(dec.memlimit(), 123_456);
    dec.set_memlimit(7);
    assert_eq!(dec.memlimit(), 7);
}

/// The .lzma header stores the dictionary size rounded up to 2^n or
/// 2^n + 2^(n-1), as liblzma's .lzma encoder writes it.
#[test]
fn lzma_header_dictionary_size_is_rounded() {
    for (dict, stored) in [
        (4096, 4096),
        (4097, 6144),
        (6144, 6144),
        (6145, 8192),
        (1 << 20, 1 << 20),
        (1_500_000, 1_572_864),
        ((3 << 20) + 1, 4 << 20),
    ] {
        let opts = xz_native::LzmaOptions::preset(0).unwrap().dict_size(dict);
        let lzma = xz_native::compress_lzma_with_options(b"x", &opts).unwrap();
        assert_eq!(lzma[1..5], u32::to_le_bytes(stored), "dict {dict}");
    }
}

/// A call without output space isn't truncation, even with `finish` and no
/// input left; and a file with a check this crate can't verify decodes
/// through the push API (with its check skipped) unless
/// `UnsupportedCheck::Fail` is set.
#[test]
fn xz_decoder_no_space_and_unsupported_check() {
    let data = sample();
    let packed = compress(&data, 1).unwrap();
    let mut dec = XzDecoder::new();
    let (u, _, _) = dec.decode(&packed[..30], &mut [], false).unwrap();
    assert_eq!(dec.decode(&[], &mut [], true), Ok((0, 0, Status::Ok)));
    let mut out = vec![0u8; data.len()];
    let (_, w, _) = dec.decode(&packed[u..], &mut out, true).unwrap();
    assert!(w == data.len() && out == data);

    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files/unsupported-check.xz");
    let file = std::fs::read(p).unwrap();
    let mut out = vec![0u8; 1 << 16];
    let mut dec = XzDecoder::new();
    let (_, _, s) = dec.decode(&file, &mut out, true).unwrap();
    assert_eq!(s, Status::StreamEnd);
    let opts =
        xz_native::DecoderOptions::new().unsupported_check(xz_native::UnsupportedCheck::Fail);
    let mut dec = XzDecoder::with_options(opts);
    // The Stream Header is consumed first, then the error comes.
    assert_eq!(dec.decode(&file, &mut out, true), Ok((12, 0, Status::Ok)));
    assert_eq!(dec.decode(&file[12..], &mut out, true), Err(Error::Options));
}

/// `UnsupportedCheck::Report`: an `Error::UnsupportedCheck` event per
/// Stream, after which decoding goes on, in the push decoder and the
/// readers, with and without threads.
#[test]
fn report_unsupported_check() {
    use xz_native::{AutoReader, DecoderOptions, UnsupportedCheck};
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files/unsupported-check.xz");
    let file = std::fs::read(p).unwrap();
    let plain = xz_native::decompress(&file).unwrap();
    let two = [file.clone(), file].concat();
    let want = [plain.clone(), plain].concat();
    for threads in [1, 2] {
        let opts = DecoderOptions::new()
            .unsupported_check(UnsupportedCheck::Report)
            .threads(threads);
        for step in [1, two.len()] {
            let mut dec = XzDecoder::with_options(opts);
            let (mut ip, mut out, mut events) = (0, Vec::new(), 0);
            let mut buf = [0u8; 4096];
            loop {
                let end = (ip + step).min(two.len());
                match dec.decode(&two[ip..end], &mut buf, end == two.len()) {
                    Ok((u, w, s)) => {
                        ip += u;
                        out.extend_from_slice(&buf[..w]);
                        if s == Status::StreamEnd {
                            break;
                        }
                    }
                    Err(Error::UnsupportedCheck) => events += 1,
                    Err(e) => panic!("threads {threads} step {step}: {e:?}"),
                }
            }
            assert_eq!(events, 2, "threads {threads} step {step}");
            assert!(out == want, "threads {threads} step {step}");
        }
        for auto in [false, true] {
            let mut r: Box<dyn Read> = if auto {
                Box::new(AutoReader::with_options(&two[..], opts))
            } else {
                Box::new(XzReader::with_options(&two[..], opts))
            };
            let (mut out, mut events) = (Vec::new(), 0);
            let mut buf = [0u8; 4096];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e)
                        if e.get_ref().and_then(|e| e.downcast_ref::<Error>())
                            == Some(&Error::UnsupportedCheck) =>
                    {
                        events += 1
                    }
                    Err(e) => panic!("threads {threads} auto {auto}: {e}"),
                }
            }
            assert_eq!(events, 2, "threads {threads} auto {auto}");
            assert!(out == want, "threads {threads} auto {auto}");
        }
    }
}
