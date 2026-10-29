#![allow(dead_code)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Deterministic xorshift generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Test inputs covering literals, long matches, overlapping copies and
/// incompressible data.
pub fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    let mut v = vec![
        ("empty".into(), vec![]),
        ("one".into(), vec![42]),
        ("zeros-1M".into(), vec![0; 1 << 20]),
        (
            "random-300k".into(),
            (0..300_000).map(|_| r.next() as u8).collect(),
        ),
    ];
    let words = [
        "lorem", "ipsum", "dolor", "sit", "amet", "xz", "native", "rust", "\n", "  ", "lzma2",
    ];
    let mut text = Vec::new();
    while text.len() < 2_000_000 {
        text.extend_from_slice(words[r.below(words.len() as u64) as usize].as_bytes());
        text.push(b' ');
    }
    v.push(("text-2M".into(), text));
    for period in [1usize, 2, 3, 4, 7, 8, 13, 37, 255, 256, 300] {
        let pat: Vec<u8> = (0..period).map(|_| r.next() as u8).collect();
        v.push((
            format!("periodic-{period}"),
            pat.iter().cycle().take(70_000).copied().collect(),
        ));
    }
    // Binary-ish data with x86 call/jump opcodes for BCJ.
    let bin: Vec<u8> = (0..500_000)
        .map(|i| match r.below(10) {
            0 => 0xE8,
            1 => 0xE9,
            2 => (i % 251) as u8,
            _ => (r.next() % 16) as u8,
        })
        .collect();
    v.push(("binary-500k".into(), bin));
    for n in [
        2usize, 3, 15, 16, 17, 100, 4095, 4096, 4097, 65535, 65536, 65537,
    ] {
        v.push((
            format!("small-{n}"),
            (0..n)
                .map(|i| (i * 7 % 13) as u8 ^ (r.next() as u8 & 1))
                .collect(),
        ));
    }
    v
}

/// Runs the system `xz` with `args`, feeding `input` on stdin.
/// Returns None if xz isn't installed (unless XZ_NATIVE_REQUIRE_XZ is set).
pub fn xz(args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let child = Command::new("xz")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            assert!(
                std::env::var_os("XZ_NATIVE_REQUIRE_XZ").is_none(),
                "xz not found: {e}"
            );
            return None;
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let t = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().unwrap();
    t.join().unwrap().unwrap();
    assert!(out.status.success(), "xz {args:?} failed");
    Some(out.stdout)
}
