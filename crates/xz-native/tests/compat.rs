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
