//! The 0.1 API surface: options setters, reader and writer accessors, and
//! the shared `Action` argument of the encoders.

use std::io::{Read, Write};
use xz_native::{
    Action, Check, DecoderOptions, EncoderOptions, Error, LzmaDecoder, LzmaEncoder, LzmaOptions,
    LzmaReader, LzmaWriter, MatchFinder, RawMethod, RawOptions, Status, XzDecoder, XzReader,
    XzWriter,
};

#[test]
fn setters_match_field_assignment() {
    let a = DecoderOptions::new()
        .memlimit(1 << 20)
        .concatenated(false)
        .ignore_check(true)
        .fail_on_unsupported_check(true)
        .threads(3);
    let mut b = DecoderOptions::default();
    b.memlimit = 1 << 20;
    b.concatenated = false;
    b.ignore_check = true;
    b.fail_on_unsupported_check = true;
    b.threads = 3;
    assert_eq!(format!("{a:?}"), format!("{b:?}"));

    let e = EncoderOptions::preset(1)
        .unwrap()
        .check(Check::Sha256)
        .block_size(1 << 16)
        .threads(2);
    assert_eq!(
        (e.check, e.block_size, e.threads),
        (Check::Sha256, 1 << 16, 2)
    );
    assert_eq!(e.lzma, LzmaOptions::preset(1).unwrap());

    let l = LzmaOptions::preset(6)
        .unwrap()
        .dict_size(1 << 16)
        .mf(MatchFinder::Hc4)
        .nice_len(32);
    assert_eq!(
        (l.dict_size, l.mf, l.nice_len),
        (1 << 16, MatchFinder::Hc4, 32)
    );

    let r = RawOptions::preset(RawMethod::Lzma2, 0)
        .unwrap()
        .method(RawMethod::Lzma1);
    assert_eq!(r.method, RawMethod::Lzma1);
}

#[test]
fn writer_accessors_and_try_finish() {
    let data = b"accessor test data ".repeat(500);
    let mut w = XzWriter::new(Vec::new(), 1).unwrap();
    w.write_all(&data).unwrap();
    assert_eq!(w.total_in(), data.len() as u64);
    w.try_finish().unwrap();
    let out_len = w.get_ref().len() as u64;
    assert_eq!(w.total_out(), out_len);
    // Writing after try_finish fails; finishing again is a no-op.
    assert!(w.write(b"more").is_err());
    w.try_finish().unwrap();
    // The inner writer is reachable mutably, e.g. to append a trailer.
    w.get_mut().extend_from_slice(b"TRAILER");
    let out = w.finish().unwrap();
    assert_eq!(&out[out.len() - 7..], b"TRAILER");
    assert_eq!(
        xz_native::decompress(&out[..out_len as usize]).unwrap(),
        data
    );

    let mut lw = LzmaWriter::with_options(Vec::new(), LzmaOptions::preset(0).unwrap()).unwrap();
    lw.write_all(&data).unwrap();
    assert_eq!(lw.total_in(), data.len() as u64);
    let out = lw.finish().unwrap();
    assert_eq!(xz_native::decompress_lzma(&out).unwrap(), data);
}

#[test]
fn reader_accessors_and_lzma_memlimit() {
    let data = b"reader accessor data ".repeat(300);
    let packed = xz_native::compress(&data, 0).unwrap();
    let mut r = XzReader::new(&packed[..]);
    assert_eq!(r.get_ref().len(), packed.len());
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    assert_eq!(out, data);
    assert!(r.get_mut().is_empty());

    // .lzma honors DecoderOptions::memlimit through the reader and the
    // one-shot function alike.
    let lzma = xz_native::compress_lzma(&data, 6).unwrap();
    let tiny = DecoderOptions::new().memlimit(1 << 16);
    let mut out = Vec::new();
    let err = LzmaReader::with_options(&lzma[..], tiny)
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(
        err.into_inner()
            .unwrap()
            .downcast::<Error>()
            .map(|e| *e)
            .ok(),
        Some(Error::MemLimit)
    );
    assert_eq!(
        xz_native::decompress_lzma_with_options(&lzma, tiny),
        Err(Error::MemLimit)
    );
    assert_eq!(
        xz_native::decompress_lzma_with_options(&lzma, DecoderOptions::new()).unwrap(),
        data
    );
}

#[test]
fn lzma_encoder_takes_action() {
    let mut enc = LzmaEncoder::new(0).unwrap();
    let mut out = vec![0u8; 4096];
    // .lzma has no flushing, as in liblzma.
    assert_eq!(
        enc.encode(b"x", &mut out, Action::SyncFlush),
        Err(Error::Usage)
    );
    assert_eq!(
        enc.encode(b"x", &mut out, Action::FullFlush),
        Err(Error::Usage)
    );

    let mut enc = LzmaEncoder::new(0).unwrap();
    let (used, n, status) = enc.encode(b"hello", &mut out, Action::Finish).unwrap();
    assert_eq!((used, status), (5, Status::StreamEnd));
    let mut dec = LzmaDecoder::new();
    let mut plain = [0u8; 16];
    let (_, w, status) = dec.decode(&out[..n], &mut plain, true).unwrap();
    assert_eq!((&plain[..w], status), (&b"hello"[..], Status::StreamEnd));
}

/// Feeds all of `input`, raising the memory limit to what the decoder needs
/// whenever it reports `Error::MemLimit`. Returns the output and the limits
/// it needed.
fn decode_raising<D>(
    dec: &mut D,
    input: &[u8],
    mut decode: impl FnMut(&mut D, &[u8], &mut [u8]) -> Result<(usize, usize, Status), Error>,
    memusage: impl Fn(&D) -> u64,
    mut raise: impl FnMut(&mut D, u64),
) -> (Vec<u8>, Vec<u64>) {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut ip = 0;
    let mut needed = Vec::new();
    loop {
        match decode(dec, &input[ip..], &mut buf) {
            Ok((u, w, status)) => {
                ip += u;
                out.extend_from_slice(&buf[..w]);
                if status == Status::StreamEnd {
                    return (out, needed);
                }
            }
            Err(Error::MemLimit) => {
                // Repeating the call fails the same way: nothing was used.
                assert_eq!(decode(dec, &input[ip..], &mut buf), Err(Error::MemLimit));
                needed.push(memusage(dec));
                raise(dec, memusage(dec));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn memlimit_raise_and_resume() {
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    // Two streams with different dictionaries (1 and 8 MiB in the Block
    // Headers): the second limit is hit after the first stream's output.
    let mut xz = xz_native::compress(&data, 1).unwrap();
    xz.extend_from_slice(&xz_native::compress(&data, 6).unwrap());
    let mut dec = XzDecoder::with_options(DecoderOptions::new().memlimit(1 << 16));
    assert_eq!((dec.memusage(), dec.memlimit()), (0, 1 << 16));
    let (out, needed) = decode_raising(
        &mut dec,
        &xz,
        |d, i, o| d.decode(i, o, true),
        XzDecoder::memusage,
        XzDecoder::set_memlimit,
    );
    assert_eq!(out.len(), 2 * data.len());
    assert!(out[..data.len()] == data[..] && out[data.len()..] == data[..]);
    assert_eq!(needed.len(), 2);
    assert!(1 << 20 < needed[0] && needed[0] < needed[1], "{needed:?}");

    // The readers report the same figure after the error.
    for threads in [1, 2] {
        let opts = DecoderOptions::new().memlimit(1 << 16).threads(threads);
        let mut r = XzReader::with_options(&xz[..], opts);
        let err = r.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory);
        assert_eq!(r.memusage(), needed[0], "threads {threads}");
    }

    let lzma = xz_native::compress_lzma(&data, 1).unwrap();
    let mut dec = LzmaDecoder::with_options(DecoderOptions::new().memlimit(1 << 16));
    let (out, needed) = decode_raising(
        &mut dec,
        &lzma,
        |d, i, o| d.decode(i, o, true),
        LzmaDecoder::memusage,
        LzmaDecoder::set_memlimit,
    );
    assert!(out == data);
    assert_eq!(needed.len(), 1);
    let mut r = LzmaReader::with_options(&lzma[..], DecoderOptions::new().memlimit(1 << 16));
    assert!(r.read_to_end(&mut Vec::new()).is_err());
    assert_eq!(r.memusage(), needed[0]);
}

#[test]
fn usage_error() {
    let e = std::io::Error::from(Error::Usage);
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(Error::Usage.to_string(), "unsupported use of the API");
}
