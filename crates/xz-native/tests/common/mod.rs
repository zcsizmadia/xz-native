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

/// Version of the system `xz` as (major, minor), if installed.
pub fn xz_version() -> Option<(u32, u32)> {
    let out = Command::new("xz").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "xz (XZ Utils) 5.8.4"
    let ver = text.lines().next()?.rsplit(' ').next()?;
    let mut it = ver.split('.').map(|p| p.parse().ok());
    Some((it.next()??, it.next()??))
}

/// Whether the system `xz` supports the options (ARM64 needs xz 5.4,
/// RISC-V 5.6).
pub fn xz_supports(args: &[&str]) -> bool {
    let needed = if args.iter().any(|a| a.starts_with("--riscv")) {
        (5, 6)
    } else if args.iter().any(|a| a.starts_with("--arm64")) {
        (5, 4)
    } else {
        (5, 0)
    };
    xz_version().is_some_and(|v| v >= needed)
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

/// `xz` option sets for interop tests.
pub const XZ_OPTIONS: &[&[&str]] = &[
    &["-0"],
    &["-1"],
    &["-6"],
    &["-9e"],
    &["--check=none"],
    &["--check=crc32"],
    &["--check=sha256"],
    &["--lzma2=preset=6,lc=4,lp=0,pb=0"],
    &["--lzma2=preset=1,lc=0,lp=4,pb=4"],
    &["--lzma2=preset=6,lc=1,lp=3,pb=1,mf=hc4"],
    &["--block-size=50000"],
    &["-T4", "--block-size=300000"],
    &["--x86", "--lzma2"],
    &["--powerpc", "--lzma2"],
    &["--ia64", "--lzma2"],
    &["--arm", "--lzma2"],
    &["--armthumb", "--lzma2"],
    &["--arm64", "--lzma2"],
    &["--sparc", "--lzma2"],
    &["--riscv", "--lzma2"],
    &["--x86=start=4096", "--lzma2"],
    &["--delta=dist=1", "--lzma2"],
    &["--delta=dist=4", "--x86", "--lzma2"],
    &["-T1", "--block-size=100000"],
    &["-T4", "--block-size=65536", "--check=sha256"],
    &["-T4", "--block-size=40000", "--check=none", "-1"],
    &[
        "-T4",
        "--block-size=100000",
        "--check=crc32",
        "--x86",
        "--lzma2",
    ],
    &[
        "-T4",
        "--block-size=30000",
        "--delta=dist=2",
        "--lzma2=preset=0",
    ],
    &["-T4", "--block-size=200000", "--arm64", "--lzma2"],
];

/// Reads .xz data with [`xz_native::XzReader`] through a `BufReader` of
/// `capacity`, with read sizes from `seed`. Returns the data read before an
/// error, and the error.
pub fn read_xz(
    input: &[u8],
    opts: xz_native::DecoderOptions,
    capacity: usize,
    seed: u64,
) -> (Vec<u8>, Option<xz_native::Error>) {
    use std::io::Read;
    let inner = std::io::BufReader::with_capacity(capacity, input);
    let mut reader = xz_native::XzReader::with_options(inner, opts);
    let mut r = Rng(seed | 1);
    let sizes = [1usize, 7, 100, 4096, 65536, 1 << 20];
    let mut buf = vec![0u8; 1 << 20];
    let mut out = Vec::new();
    loop {
        let n = sizes[r.below(sizes.len() as u64) as usize];
        match reader.read(&mut buf[..n]) {
            Ok(0) => return (out, None),
            Ok(k) => out.extend_from_slice(&buf[..k]),
            Err(e) => {
                let e = e
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<xz_native::Error>());
                return (out, Some(*e.expect("not an xz_native::Error")));
            }
        }
    }
}

/// [`xz_native::DecoderOptions`] with `threads`.
pub fn threads(threads: usize) -> xz_native::DecoderOptions {
    xz_native::DecoderOptions {
        threads,
        ..Default::default()
    }
}
