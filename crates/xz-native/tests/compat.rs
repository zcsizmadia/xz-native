//! The `xz2`-compatible module: round trips through every reader/writer
//! pairing, stream boundaries, totals and errors.

mod common;

use std::io::{BufReader, Read, Write};
use xz_native::compat::{bufread, read, write};

fn data() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..60_000u32 {
        v.extend_from_slice(
            format!("line {} {}\n", i, i.wrapping_mul(2_654_435_761) % 977).as_bytes(),
        );
    }
    v
}

#[test]
fn round_trips() {
    let data = data();

    // write::XzEncoder in odd-sized writes.
    let mut enc = write::XzEncoder::new(Vec::new(), 6);
    for chunk in data.chunks(7777) {
        enc.write_all(chunk).unwrap();
    }
    assert_eq!(enc.total_in(), data.len() as u64);
    let packed = enc.finish().unwrap();
    assert_eq!(xz_native::decompress(&packed).unwrap(), data);
    if let Some(out) = common::xz(&["-dc"], &packed) {
        assert_eq!(out, data, "xz CLI");
    }

    // read::XzEncoder.
    let mut packed2 = Vec::new();
    let mut r = read::XzEncoder::new(&data[..], 1);
    r.read_to_end(&mut packed2).unwrap();
    assert_eq!(r.total_out(), packed2.len() as u64);
    assert_eq!(r.total_in(), data.len() as u64);

    // Every decoder on both outputs.
    for p in [&packed, &packed2] {
        let mut out = Vec::new();
        let mut d = read::XzDecoder::new(&p[..]);
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(
            (d.total_in(), d.total_out()),
            (p.len() as u64, data.len() as u64)
        );

        let mut out = Vec::new();
        bufread::XzDecoder::new(BufReader::with_capacity(13, &p[..]))
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);

        let mut w = write::XzDecoder::new(Vec::new());
        for chunk in p.chunks(333) {
            w.write_all(chunk).unwrap();
        }
        assert_eq!(w.finish().unwrap(), data);
    }

    // bufread::XzEncoder with a tiny buffer.
    let mut packed3 = Vec::new();
    bufread::XzEncoder::new(BufReader::with_capacity(5, &data[..]), 0)
        .read_to_end(&mut packed3)
        .unwrap();
    assert_eq!(xz_native::decompress(&packed3).unwrap(), data);
}

#[test]
fn single_and_multi_stream() {
    let a = xz_native::compress(b"first", 6).unwrap();
    let b = xz_native::compress(b"second", 6).unwrap();
    let mut cat = a.clone();
    cat.extend_from_slice(&b);

    let mut out = Vec::new();
    let mut d = read::XzDecoder::new(&cat[..]);
    d.read_to_end(&mut out).unwrap();
    assert_eq!(out, b"first");

    let mut out = Vec::new();
    read::XzDecoder::new_multi_decoder(&cat[..])
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, b"firstsecond");

    // A single-stream writer doesn't consume what follows the stream.
    let mut w = write::XzDecoder::new(Vec::new());
    let n = w.write(&cat).unwrap();
    assert_eq!(n, a.len());
    assert_eq!(w.write(&cat[n..]).unwrap(), 0);
    assert_eq!(w.finish().unwrap(), b"first");
}

#[test]
fn errors() {
    let packed = xz_native::compress(&data(), 6).unwrap();
    let mut w = write::XzDecoder::new(Vec::new());
    w.write_all(&packed[..packed.len() / 2]).unwrap();
    assert!(w.finish().is_err(), "truncated stream must fail on finish");

    let mut out = Vec::new();
    assert!(
        read::XzDecoder::new(&packed[..100])
            .read_to_end(&mut out)
            .is_err()
    );

    let mut enc = write::XzEncoder::new(Vec::new(), 6);
    enc.try_finish().unwrap();
    assert!(enc.write(b"late").is_err());
}

#[test]
fn flush_makes_data_decodable() {
    let mut enc = write::XzEncoder::new(Vec::new(), 6);
    enc.write_all(b"before flush ").unwrap();
    enc.flush().unwrap();
    let partial = enc.get_ref().clone();
    let mut dec = xz_native::XzDecoder::new();
    let mut out = vec![0u8; 64];
    let (_, n, _) = dec.decode(&partial, &mut out, false).unwrap();
    assert_eq!(&out[..n], b"before flush ");
    enc.write_all(b"after").unwrap();
    let packed = enc.finish().unwrap();
    assert_eq!(
        xz_native::decompress(&packed).unwrap(),
        b"before flush after"
    );
}

#[test]
#[should_panic(expected = "invalid xz preset")]
fn invalid_preset_panics() {
    let _ = write::XzEncoder::new(Vec::new(), 10);
}

/// A writer that fails every third write with `WouldBlock` (consuming
/// nothing) and takes at most 1000 bytes per write.
struct Flaky {
    out: Vec<u8>,
    calls: usize,
}

impl Write for Flaky {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.calls.is_multiple_of(3) {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let n = buf.len().min(1000);
        self.out.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Retries `f` while it fails with `WouldBlock`.
fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> T {
    loop {
        match f() {
            Ok(v) => return v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }
    }
}

/// An error from the underlying writer consumes nothing: retrying gives a
/// correct stream, with nothing lost or duplicated.
#[test]
fn writers_keep_output_on_inner_errors() {
    let data = data();
    let flaky = Flaky {
        out: Vec::new(),
        calls: 0,
    };
    let mut enc = write::XzEncoder::new(flaky, 6);
    let mut pos = 0;
    while pos < data.len() {
        let end = (pos + 70_000).min(data.len());
        pos += retry(|| enc.write(&data[pos..end]));
        if pos.is_multiple_of(3) {
            retry(|| enc.flush());
        }
    }
    retry(|| enc.try_finish());
    let packed = std::mem::take(&mut enc.get_mut().out);
    assert!(xz_native::decompress(&packed).unwrap() == data);

    let flaky = Flaky {
        out: Vec::new(),
        calls: 0,
    };
    let mut dec = write::XzDecoder::new(flaky);
    let mut pos = 0;
    while pos < packed.len() {
        let end = (pos + 5_000).min(packed.len());
        pos += retry(|| dec.write(&packed[pos..end]));
    }
    retry(|| dec.try_finish());
    assert!(dec.get_ref().out == data);
}

/// A source that reports the end of the input once and then has more:
/// the encoder finishes with what it had instead of spinning.
#[test]
fn bufread_encoder_finishes_once() {
    struct Gappy(usize);
    impl Read for Gappy {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0 += 1;
            if self.0 == 2 {
                return Ok(0);
            }
            buf[0] = b'x';
            Ok(1)
        }
    }
    let mut enc = bufread::XzEncoder::new(BufReader::with_capacity(1, Gappy(0)), 0);
    let mut packed = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match enc.read(&mut b).unwrap() {
            0 => break,
            n => packed.extend_from_slice(&b[..n]),
        }
    }
    assert_eq!(xz_native::decompress(&packed).unwrap(), b"x");
}

/// Like `xz2`, the readers' `new_multi_decoder` also reads .lzma and .lz.
#[test]
fn multi_decoder_detects_the_format() {
    let data = data();
    let lzma = xz_native::compress_lzma(&data, 6).unwrap();
    let mut out = Vec::new();
    read::XzDecoder::new_multi_decoder(&lzma[..])
        .read_to_end(&mut out)
        .unwrap();
    assert!(out == data);

    let lz = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../third_party/xz/tests/files/good-1-v1.lz"
    ))
    .unwrap();
    let want = xz_native::decompress_lzip(&lz).unwrap();
    let mut out = Vec::new();
    bufread::XzDecoder::new_multi_decoder(&lz[..])
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, want);

    // `new` stays .xz only.
    assert!(
        read::XzDecoder::new(&lzma[..])
            .read_to_end(&mut out)
            .is_err()
    );
    // Empty input is truncated.
    assert!(
        read::XzDecoder::new_multi_decoder(&[][..])
            .read_to_end(&mut out)
            .is_err()
    );
}

/// `flush` is a full flush, so it also works with the multithreaded
/// encoder, which has no sync flush.
#[test]
fn flush_with_threaded_encoder() {
    let opts = xz_native::EncoderOptions::preset(1).unwrap().threads(2);
    let enc = xz_native::XzEncoder::with_options(opts).unwrap();
    let mut w = write::XzEncoder::new_encoder(Vec::new(), enc);
    w.write_all(b"first ").unwrap();
    w.flush().unwrap();
    assert_eq!(
        xz_native::__private::decompress_streaming(
            w.get_ref(),
            xz_native::DecoderOptions::new().concatenated(false)
        )
        .err(),
        Some(xz_native::Error::Truncated)
    );
    w.write_all(b"second").unwrap();
    let packed = w.finish().unwrap();
    assert_eq!(xz_native::decompress(&packed).unwrap(), b"first second");
}

/// A writer dropped by a panic doesn't end the stream, so the interrupted
/// data can't pass for complete.
#[test]
fn writer_dropped_by_panic_stays_incomplete() {
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for kind in 0..3 {
        let sink = Shared(Arc::new(Mutex::new(Vec::new())));
        let s = sink.clone();
        let r = std::panic::catch_unwind(move || {
            let data = data();
            match kind {
                0 => {
                    let mut w = xz_native::XzWriter::new(s, 6).unwrap();
                    w.write_all(&data).unwrap();
                    panic!("interrupted");
                }
                1 => {
                    let mut w = xz_native::LzmaWriter::new(s, 6).unwrap();
                    w.write_all(&data).unwrap();
                    panic!("interrupted");
                }
                _ => {
                    let mut w = write::XzEncoder::new(s, 6);
                    w.write_all(&data).unwrap();
                    panic!("interrupted");
                }
            }
        });
        assert!(r.is_err());
        let out = sink.0.lock().unwrap().clone();
        let ok = if kind == 1 {
            xz_native::decompress_lzma(&out).is_ok()
        } else {
            xz_native::decompress(&out).is_ok()
        };
        assert!(!ok, "kind {kind}: an interrupted stream decodes");
    }
}
