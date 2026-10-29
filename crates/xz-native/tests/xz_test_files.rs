//! Runs the test files shipped with xz 5.8.4 (third_party/xz/tests/files).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use xz_native::{Error, decompress, decompress_lzip, decompress_lzma};

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
        let opts = DecoderOptions {
            concatenated,
            ..DecoderOptions::default()
        };
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
