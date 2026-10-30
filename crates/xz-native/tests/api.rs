//! The 0.1 API surface: options setters, reader and writer accessors, and
//! the shared `Action` argument of the encoders.

use std::io::{Read, Write};
use xz_native::{
    Action, Check, DecoderOptions, EncoderOptions, Error, LzmaDecoder, LzmaEncoder, LzmaOptions,
    LzmaReader, LzmaWriter, MatchFinder, RawMethod, RawOptions, Status, XzReader, XzWriter,
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
        Err(Error::InvalidOptions)
    );
    assert_eq!(
        enc.encode(b"x", &mut out, Action::FullFlush),
        Err(Error::InvalidOptions)
    );

    let mut enc = LzmaEncoder::new(0).unwrap();
    let (used, n, status) = enc.encode(b"hello", &mut out, Action::Finish).unwrap();
    assert_eq!((used, status), (5, Status::StreamEnd));
    let mut dec = LzmaDecoder::new();
    let mut plain = [0u8; 16];
    let (_, w, status) = dec.decode(&out[..n], &mut plain, true).unwrap();
    assert_eq!((&plain[..w], status), (&b"hello"[..], Status::StreamEnd));
}
