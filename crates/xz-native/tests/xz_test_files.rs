//! Runs the test files shipped with xz 5.8.4 (third_party/xz/tests/files).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use xz_native::{Error, LzmaDecoder, Status, decompress, decompress_lzip, decompress_lzma};

fn files_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/xz/tests/files");
    assert!(
        dir.is_dir(),
        "missing {}: run `git submodule update --init`",
        dir.display()
    );
    dir
}

fn files(ext: &str) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(files_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        // Under Miri, only the small files (all but the delta-coded TIFF).
        .filter(|p| !cfg!(miri) || fs::metadata(p).unwrap().len() <= 4096)
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&p).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Output of the reference `xz -dc`, if an xz binary is available.
fn reference(name: &str) -> Option<Vec<u8>> {
    if cfg!(miri) {
        return None;
    }
    let out = Command::new("xz")
        .arg("-dc")
        .arg(files_dir().join(name))
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

#[test]
fn xz_files() {
    let mut checked = 0;
    for (name, data) in files("xz") {
        let r = decompress(&data);
        if name.starts_with("good-") || name == "unsupported-check.xz" {
            let out = r.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            if let Some(expected) = reference(&name) {
                assert_eq!(out, expected, "{name}: output differs from xz");
                checked += 1;
            }
        } else if name.starts_with("bad-") {
            assert!(
                r.is_err(),
                "{name}: expected an error, got {} bytes",
                r.unwrap().len()
            );
        } else if name.starts_with("unsupported-") {
            assert_eq!(r, Err(Error::Options), "{name}");
        }
    }
    eprintln!("compared {checked} files with xz -dc");
}

#[test]
fn lzma_files() {
    for (name, data) in files("lzma") {
        let r = decompress_lzma(&data);
        if name.starts_with("good-") {
            let out = r.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            if let Some(expected) = reference(&name) {
                assert_eq!(out, expected, "{name}");
            }
        } else {
            assert!(r.is_err(), "{name}: expected an error");
        }
    }
}

#[test]
fn lzip_files() {
    let mut checked = 0;
    for (name, data) in files("lz") {
        let r = decompress_lzip(&data);
        if name.starts_with("good-") {
            let out = r.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            if let Some(expected) = reference(&name) {
                assert_eq!(out, expected, "{name}: output differs from xz");
                checked += 1;
            }
        } else if name.starts_with("bad-") {
            assert!(
                r.is_err(),
                "{name}: expected an error, got {} bytes",
                r.unwrap().len()
            );
        } else if name.starts_with("unsupported-") {
            assert_eq!(r, Err(Error::Options), "{name}");
        }
    }
    eprintln!("compared {checked} files with xz -dc");
}

#[test]
fn lzip_trailing_data() {
    use std::io::{BufRead, Read};
    use xz_native::{DecoderOptions, LzipReader};
    let dir = files_dir();
    // Like `( xz -dc ; cat ) < file` in tests/files/README: the reader stops
    // at the trailing data but consumes a leading partial "LZIP" magic,
    // unless only one member is decoded.
    for (name, concatenated, rest) in [
        ("good-1-v1-trailing-1.lz", true, &b"Trailing garbage\n"[..]),
        ("good-1-v1-trailing-2.lz", true, b"Trailing garbage\n"),
        ("good-1-v1-trailing-2.lz", false, b"LZITrailing garbage\n"),
    ] {
        let data = fs::read(dir.join(name)).unwrap();
        let opts = DecoderOptions::default().concatenated(concatenated);
        let mut r = LzipReader::with_options(&data[..], opts);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"Hello\nWorld!\n", "{name}");
        assert_eq!(r.into_inner().fill_buf().unwrap(), rest, "{name}");
    }
    let bad = fs::read(dir.join("bad-1-v1-magic-1.lz")).unwrap();
    assert_eq!(decompress_lzip(&bad), Err(Error::Format));
    let bad = fs::read(dir.join("bad-1-v1-trailing-magic.lz")).unwrap();
    assert_eq!(decompress_lzip(&bad), Err(Error::Truncated));
}

/// Decodes .lzma with `LzmaDecoder`, the input in pieces ending at `cuts`.
fn decode_lzma_split(data: &[u8], cuts: &[usize]) -> Result<Vec<u8>, Error> {
    let mut dec = LzmaDecoder::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut start = 0;
    for &end in cuts.iter().chain([&data.len()]) {
        let finish = end == data.len();
        let mut input = &data[start..end];
        loop {
            let (used, written, status) = dec.decode(input, &mut buf, finish)?;
            input = &input[used..];
            out.extend_from_slice(&buf[..written]);
            if status == Status::StreamEnd {
                return Ok(out);
            }
            if input.is_empty() && !finish {
                break;
            }
        }
        start = end;
    }
    Err(Error::Truncated)
}

/// A known size with an end marker, the input split at every offset and
/// fed a byte at a time. liblzma 5.8.4 fails this when an input ends inside
/// the end marker (tukaani-project/xz#260); the end-marker state must
/// survive between calls.
#[test]
fn lzma_known_size_with_eopm_split() {
    let data = fs::read(files_dir().join("good-known_size-with_eopm.lzma")).unwrap();
    let want = decompress_lzma(&data).unwrap();
    for split in 1..data.len() {
        assert_eq!(
            decode_lzma_split(&data, &[split]),
            Ok(want.clone()),
            "split at {split}"
        );
    }
    let every: Vec<usize> = (1..data.len()).collect();
    assert_eq!(decode_lzma_split(&data, &every), Ok(want));
}

/// `bad-1-powerpc-1.xz` from xz after 5.8.4 (tukaani-project/xz b355a311):
/// the PowerPC filter's start offset (21) isn't a multiple of 4. xz 5.8.4
/// rejects it as unsupported options.
#[test]
fn bad_powerpc_start_offset() {
    const FILE: [u8; 60] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x01, 0x69, 0x22, 0xde, 0x36, 0x03, 0x01, 0x05,
        0x04, 0x15, 0x00, 0x00, 0x00, 0x21, 0x01, 0x08, 0x00, 0x3c, 0x46, 0xce, 0x93, 0x01, 0x00,
        0x03, 0x48, 0x55, 0x55, 0x15, 0x00, 0x12, 0xda, 0x58, 0xa5, 0x00, 0x01, 0x1c, 0x04, 0x6f,
        0x2c, 0x9c, 0xc1, 0x90, 0x42, 0x99, 0x0d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x59, 0x5a,
    ];
    assert_eq!(decompress(&FILE), Err(Error::Options));
    let mut dec = xz_native::XzDecoder::new();
    let r = dec.decode(&FILE, &mut [0u8; 64], true);
    assert_eq!(r.map(|_| ()), Err(Error::Options));
}
