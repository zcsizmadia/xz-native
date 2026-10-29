//! Runs the test files shipped with xz 5.8.4 (third_party/xz/tests/files).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use xz_native::{Error, decompress, decompress_lzma};

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
