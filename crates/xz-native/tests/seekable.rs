//! `XzSeekableReader`: random access compared with the original data, the xz
//! test files, and corrupt or truncated files.

mod common;

use common::{Rng, corpus, xz};
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;
use xz_native::{BcjKind, Check, DecoderOptions, Error, Filter, XzSeekableReader, decompress};

type Reader<'a> = XzSeekableReader<Cursor<&'a [u8]>>;

/// The `xz_native::Error` inside an `io::Error`.
fn xz_error(e: io::Error) -> Error {
    *e.get_ref()
        .and_then(|e| e.downcast_ref::<Error>())
        .unwrap_or_else(|| panic!("not an xz error: {e}"))
}

fn open(data: &[u8]) -> Result<Reader<'_>, Error> {
    XzSeekableReader::new(Cursor::new(data)).map_err(xz_error)
}

fn read_all<R: Read>(r: &mut R) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    r.read_to_end(&mut out).map_err(xz_error)?;
    Ok(out)
}

/// Reads up to `n` bytes (less only at the end).
fn read_n<R: Read>(r: &mut R, n: usize) -> Vec<u8> {
    let mut out = vec![0; n];
    let mut len = 0;
    while len < n {
        match r.read(&mut out[len..]).unwrap() {
            0 => break,
            k => len += k,
        }
    }
    out.truncate(len);
    out
}

/// Sequential and random reads must match `original`.
fn check_random_access(label: &str, compressed: &[u8], original: &[u8], seed: u64) {
    let mut r = open(compressed).unwrap_or_else(|e| panic!("{label}: {e:?}"));
    assert_eq!(r.uncompressed_size(), original.len() as u64, "{label}");
    assert!(read_all(&mut r).unwrap() == original, "{label}: sequential");
    assert_eq!(r.stream_position().unwrap(), original.len() as u64);

    let len = original.len() as u64;
    let mut rng = Rng(seed);
    let mut pos = len;
    for i in 0..150 {
        let target = rng.below(len + 100);
        let from = match i % 3 {
            0 => SeekFrom::Start(target),
            1 => SeekFrom::End(target as i64 - len as i64),
            _ => SeekFrom::Current(target as i64 - pos as i64),
        };
        pos = r.seek(from).unwrap();
        assert_eq!(pos, target, "{label}");
        let n = match rng.below(4) {
            0 => rng.below(10),
            1 => rng.below(1000),
            _ => rng.below(100_000),
        } as usize;
        let got = read_n(&mut r, n);
        let start = (pos as usize).min(original.len());
        let end = (start + n).min(original.len());
        assert!(
            got == original[start..end],
            "{label}: read {n} at {pos} (seed {seed})"
        );
        pos += got.len() as u64;
        // A sequential read continues where the previous one stopped.
        if rng.below(2) == 0 {
            let more = read_n(&mut r, 5000);
            let end2 = (end + 5000).min(original.len());
            assert!(
                more == original[end..end2],
                "{label}: sequential after seek"
            );
            pos += more.len() as u64;
        }
    }
    assert!(r.seek(SeekFrom::Current(-(pos as i64) - 1)).is_err());
    assert!(r.seek(SeekFrom::End(-(len as i64) - 1)).is_err());
}

fn text(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    let words = ["alpha", "beta", "gamma", "delta", "\n", "xz", "seek", "  "];
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(words[r.below(words.len() as u64) as usize].as_bytes());
        if r.below(3) == 0 {
            v.push(r.next() as u8);
        }
    }
    v.truncate(n);
    v
}

const OPTIONS: &[&[&str]] = &[
    &[],
    &["--block-size=10000"],
    &["--block-size=65536"],
    &["-T4", "--block-size=100000"],
    &["--check=none", "--block-size=30000"],
    &["--check=crc32", "--block-size=30000"],
    &["--check=crc64", "--block-size=30000"],
    &["--check=sha256", "--block-size=30000"],
    &["--x86", "--lzma2", "--block-size=40000"],
    &["--arm64", "--lzma2", "--block-size=40000"],
    &["--delta=dist=4", "--lzma2", "--block-size=40000"],
    &[
        "--delta=dist=1",
        "--x86",
        "--lzma2=preset=1",
        "--block-size=25000",
    ],
];

#[test]
fn xz_cli_files() {
    let data = text(700_000, 1);
    let binary: Vec<u8> = corpus()
        .into_iter()
        .find(|(n, _)| n == "binary-500k")
        .unwrap()
        .1;
    for (i, opts) in OPTIONS.iter().enumerate() {
        let mut args = vec!["-c"];
        args.extend_from_slice(opts);
        let input = if opts.iter().any(|o| o.starts_with("--x86")) {
            &binary
        } else {
            &data
        };
        let Some(c) = xz(&args, input) else { return };
        let label = format!("{opts:?}");
        let r = open(&c).unwrap();
        assert_eq!(r.stream_count(), 1, "{label}");
        let expected_blocks = opts
            .iter()
            .find_map(|o| o.strip_prefix("--block-size="))
            .map_or(1, |s| input.len().div_ceil(s.parse().unwrap()));
        assert_eq!(r.block_count(), expected_blocks, "{label}");
        let check = opts
            .iter()
            .find_map(|o| o.strip_prefix("--check="))
            .unwrap_or("crc64");
        let check = match check {
            "none" => Check::None,
            "crc32" => Check::Crc32,
            "crc64" => Check::Crc64,
            _ => Check::Sha256,
        };
        assert_eq!(r.checks(), &[check], "{label}");
        check_random_access(&label, &c, input, 100 + i as u64);
    }
}

#[test]
fn corpus_files() {
    for (name, data) in corpus() {
        let Some(c) = xz(&["-c", "-1", "--block-size=4096"], &data) else {
            return;
        };
        check_random_access(&name, &c, &data, 7);
        let r = open(&c).unwrap();
        assert_eq!(r.block_count(), data.len().div_ceil(4096), "{name}");
    }
}

/// Builds concatenated Streams with Stream Padding, including empty Streams.
#[test]
fn concatenated_streams() {
    let parts = [text(50_000, 2), vec![], text(123_457, 3), vec![], vec![]];
    let mut file = Vec::new();
    let mut original = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let check = ["--check=crc32", "--check=none", "--check=sha256"][i % 3];
        let Some(c) = xz(&["-c", check, "--block-size=20000"], part) else {
            return;
        };
        file.extend_from_slice(&c);
        file.extend(std::iter::repeat_n(0, 4 * i));
        original.extend_from_slice(part);
    }
    assert_eq!(decompress(&file).unwrap(), original);
    let r = open(&file).unwrap();
    assert_eq!(r.stream_count(), 5);
    assert_eq!(r.block_count(), 3 + 7);
    assert_eq!(
        r.checks(),
        &[
            Check::Crc32,
            Check::None,
            Check::Sha256,
            Check::Crc32,
            Check::None
        ]
    );
    check_random_access("concatenated", &file, &original, 11);

    // Only empty Streams.
    let empty = xz(&["-c"], b"").unwrap();
    let mut file = empty.clone();
    file.extend_from_slice(&[0; 8]);
    file.extend_from_slice(&empty);
    let mut r = open(&file).unwrap();
    assert_eq!((r.stream_count(), r.block_count()), (2, 0));
    assert_eq!(r.uncompressed_size(), 0);
    assert_eq!(read_all(&mut r).unwrap(), b"");
    r.seek(SeekFrom::Start(10)).unwrap();
    assert_eq!(read_all(&mut r).unwrap(), b"");
}

/// Reading a single-Block file sequentially in small pieces must not decode
/// the Block again for every read.
#[test]
fn sequential_reads_reuse_the_decoder() {
    struct Counting<'a>(Cursor<&'a [u8]>, u64);
    impl Read for Counting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1 += n as u64;
            Ok(n)
        }
    }
    impl Seek for Counting<'_> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }
    let data = text(1_000_000, 4);
    let Some(c) = xz(&["-c"], &data) else { return };
    let mut r = XzSeekableReader::new(Counting(Cursor::new(&c), 0)).unwrap();
    let opened = r.get_ref().1;
    let mut out = Vec::new();
    let mut buf = [0u8; 999];
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert!(out == data);
    assert!(r.get_ref().1 - opened <= c.len() as u64);
    // Seeking forward within the Block doesn't restart it either.
    let mut r = XzSeekableReader::new(Counting(Cursor::new(&c), 0)).unwrap();
    let opened = r.get_ref().1;
    for pos in (0..data.len()).step_by(100_000) {
        r.seek(SeekFrom::Start(pos as u64)).unwrap();
        assert!(read_n(&mut r, 10) == data[pos..pos + 10]);
    }
    assert!(r.get_ref().1 - opened <= c.len() as u64);
}

#[test]
fn memlimit_and_options() {
    let data = text(200_000, 5);
    let Some(c) = xz(&["-c", "-6", "--block-size=50000"], &data) else {
        return;
    };
    let with = |opts: DecoderOptions| XzSeekableReader::with_options(Cursor::new(&c), opts);
    // Too small for the Block table.
    let e = with(DecoderOptions::default().memlimit(100));
    assert_eq!(e.map_err(xz_error).err(), Some(Error::MemLimit));
    // Enough for the table, too small for an 8 MiB dictionary.
    let mut r = with(DecoderOptions::default().memlimit(1 << 20)).unwrap();
    assert_eq!(read_all(&mut r), Err(Error::MemLimit));

    // A corrupt check is reported at the end of the Block, unless ignored.
    let mut bad = c.clone();
    let r0 = open(&c).unwrap();
    assert_eq!(r0.block_count(), 4);
    // The CRC64 of the first Block ends where the second Block starts.
    let second = find_block(&c, 1);
    bad[second - 1] ^= 1;
    let mut r = open(&bad).unwrap();
    let first = read_n(&mut r, 49_999);
    assert!(first == data[..49_999]);
    let mut b = [0u8; 1];
    assert_eq!(r.read(&mut b).map_err(xz_error), Err(Error::CheckMismatch));
    // Other Blocks are still readable.
    r.seek(SeekFrom::Start(150_000)).unwrap();
    assert!(read_n(&mut r, 1000) == data[150_000..151_000]);
    let mut r = with_bad(&bad);
    assert!(read_all(&mut r).unwrap() == data);
}

fn with_bad(bad: &[u8]) -> Reader<'_> {
    XzSeekableReader::with_options(
        Cursor::new(bad),
        DecoderOptions::default().ignore_check(true),
    )
    .unwrap()
}

/// File offset of Block `n` in a single-Stream file, from its Index.
fn find_block(c: &[u8], n: usize) -> usize {
    let backward =
        (u32::from_le_bytes(c[c.len() - 8..c.len() - 4].try_into().unwrap()) as usize + 1) * 4;
    let mut p = c.len() - 12 - backward + 1;
    let mut vli = || {
        let mut v = 0usize;
        let mut shift = 0;
        loop {
            let b = c[p];
            p += 1;
            v |= ((b & 0x7F) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                return v;
            }
        }
    };
    let count = vli();
    assert!(n <= count);
    let mut off = 12;
    for _ in 0..n {
        off += vli().next_multiple_of(4);
        vli();
    }
    off
}

fn files_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/xz/tests/files")
}

/// Error from the streaming decoder and from the seekable reader (at open
/// or while reading everything).
fn both(data: &[u8]) -> (Result<Vec<u8>, Error>, Result<Vec<u8>, Error>) {
    let seekable = open(data).and_then(|mut r| read_all(&mut r));
    (decompress(data), seekable)
}

#[test]
fn xz_test_files() {
    let mut names: Vec<_> = fs::read_dir(files_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".xz"))
        .collect();
    names.sort();
    assert!(names.len() > 50);
    for name in names {
        let data = fs::read(files_dir().join(&name)).unwrap();
        let (streaming, seekable) = both(&data);
        if name.starts_with("good-") || name == "unsupported-check.xz" {
            let expected = streaming.unwrap();
            assert_eq!(seekable.as_ref(), Ok(&expected), "{name}");
            check_random_access(&name, &data, &expected, 3);
        } else {
            let e = seekable.expect_err(&name);
            let expected = streaming.expect_err(&name);
            if let Some(&kind) = KIND_EXCEPTIONS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, k)| k)
            {
                assert_eq!(e, kind, "{name}");
            } else {
                assert_eq!(e, expected, "{name}");
            }
        }
    }
}

/// Bad files where the seekable reader reports a different error than the
/// streaming decoder. The streaming decoder runs out of input (truncated
/// file, or Block data cut short), while the seekable reader knows the file
/// size and the Block sizes from the Index, so the same damage is an
/// inconsistency: `LZMA_DATA_ERROR`, as in liblzma's `file_info.c` and its
/// Block decoder with a known Compressed Size.
const KIND_EXCEPTIONS: &[(&str, Error)] = &[
    ("bad-0-empty-truncated.xz", Error::Data),
    ("bad-1-lzma2-9.xz", Error::Data),
    ("bad-1-lzma2-10.xz", Error::Data),
    ("bad-1-lzma2-11.xz", Error::Data),
];

#[test]
fn fail_on_unsupported_check() {
    let data = fs::read(files_dir().join("unsupported-check.xz")).unwrap();
    let r = XzSeekableReader::with_options(
        Cursor::new(&data),
        DecoderOptions::default().fail_on_unsupported_check(true),
    );
    assert_eq!(r.map_err(xz_error).err(), Some(Error::Options));
}

/// Damage to the Stream Footer, Index, Stream Header or Stream Padding is
/// found when opening.
#[test]
fn corrupt_and_truncated() {
    let part = text(30_000, 6);
    let Some(one) = xz(&["-c", "--block-size=7000"], &part) else {
        return;
    };
    let mut file = one.clone();
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&one);
    file.extend_from_slice(&[0; 8]);
    let mut original = part.clone();
    original.extend_from_slice(&part);
    check_random_access("two streams", &file, &original, 9);

    let second = one.len() + 4;
    // Any truncation.
    let valid = [one.len(), second, file.len() - 8, file.len() - 4];
    for n in (1..file.len()).filter(|n| !valid.contains(n)) {
        assert!(open(&file[..n]).is_err(), "truncated to {n}");
    }
    assert_eq!(open(&file[..11]).err(), Some(Error::Format));
    assert_eq!(open(&[]).err(), Some(Error::Format));

    // Every byte of the Index, Stream Footer and Header of the second Stream,
    // and its padding.
    let index_start = find_index(&one) + second;
    let mut damaged = Vec::new();
    damaged.extend(index_start..file.len());
    damaged.extend(second..second + 12);
    damaged.extend(one.len()..second);
    for i in damaged {
        for bit in [0x01, 0x80] {
            let mut bad = file.clone();
            bad[i] ^= bit;
            assert!(open(&bad).is_err(), "byte {i} ^ {bit:#x}");
        }
    }

    // Padding that isn't a multiple of four.
    for pad in [1, 2, 3, 5] {
        let mut bad = one.clone();
        bad.extend(std::iter::repeat_n(0, pad));
        assert_eq!(open(&bad).err(), Some(Error::Data), "padding {pad}");
        let mut bad = one.clone();
        bad.extend(std::iter::repeat_n(0, pad));
        bad.extend_from_slice(&one);
        assert_eq!(open(&bad).err(), Some(Error::Data), "padding {pad}");
    }
    // Padding only, or garbage before a Stream.
    assert_eq!(open(&[0; 64]).err(), Some(Error::Format));
    let mut bad = vec![0u8; 4];
    bad.extend_from_slice(&one);
    assert_eq!(open(&bad).err(), Some(Error::Format));
    let mut bad = one.clone();
    bad.extend_from_slice(b"abcd");
    bad.extend_from_slice(&one);
    assert_eq!(open(&bad).err(), Some(Error::Data));
}

/// File offset of the Index in a single-Stream file.
fn find_index(c: &[u8]) -> usize {
    let backward =
        (u32::from_le_bytes(c[c.len() - 8..c.len() - 4].try_into().unwrap()) as usize + 1) * 4;
    c.len() - 12 - backward
}

/// Damaged Block data is found when reading, like the streaming decoder.
#[test]
fn corrupt_blocks() {
    let data = text(40_000, 8);
    let Some(c) = xz(&["-c", "--block-size=10000"], &data) else {
        return;
    };
    let index = find_index(&c);
    let mut r = Rng(12);
    for _ in 0..300 {
        let i = 12 + r.below((index - 12) as u64) as usize;
        let mut bad = c.clone();
        bad[i] ^= 1 << r.below(8);
        let (streaming, seekable) = both(&bad);
        match (streaming, seekable) {
            (Ok(a), Ok(b)) => assert!(a == b, "byte {i}"),
            (Err(_), Err(_)) => {}
            (a, b) => panic!("byte {i}: streaming {:?}, seekable {:?}", a.err(), b.err()),
        }
    }
}

/// How `xz --list` names a check.
fn check_name(c: Check) -> String {
    match c {
        Check::None => "None".into(),
        Check::Crc32 => "CRC32".into(),
        Check::Crc64 => "CRC64".into(),
        Check::Sha256 => "SHA-256".into(),
        other => format!("{other:?}"),
    }
}

/// How `xz --list -vv` writes a filter chain (only the filters used below).
fn filter_chain(filters: &[Filter]) -> String {
    let size = |n: u32| {
        if n.is_multiple_of(1 << 20) {
            format!("{}MiB", n >> 20)
        } else if n.is_multiple_of(1 << 10) {
            format!("{}KiB", n >> 10)
        } else {
            format!("{n}B")
        }
    };
    let names: Vec<String> = filters
        .iter()
        .map(|f| match *f {
            Filter::Lzma2 { dict_size } => format!("--lzma2=dict={}", size(dict_size)),
            Filter::Delta { distance } => format!("--delta=dist={distance}"),
            Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            } => "--x86".into(),
            other => format!("{other:?}"),
        })
        .collect();
    names.join(" ")
}

/// `streams()`, `blocks()` and `block_details()` against the columns of
/// `xz --robot --list -vv`, on a file with three Streams (several Blocks
/// with sizes in their headers, BCJ and Delta filters, every check type)
/// and Stream Padding. Memory usage is xz-native's own figure, so only its
/// relation to the dictionary size is checked.
#[test]
fn metadata_matches_xz_list() {
    let mut rng = Rng(0x5EED_1157);
    let mut data: Vec<u8> = (0..150_000).map(|_| rng.next() as u8).collect();
    data.extend(std::iter::repeat_n(b"metadata ".as_slice(), 30_000).flatten());
    let parts = [
        &["-T2", "--block-size=60000", "--check=sha256", "-6"][..],
        &["--x86", "--lzma2=preset=1", "--check=crc32"],
        &["--delta=dist=4", "--lzma2=preset=0", "--check=none"],
        &["-0", "--check=crc64"],
    ];
    let mut file = Vec::new();
    for (k, args) in parts.iter().enumerate() {
        let mut a = vec!["-c"];
        a.extend_from_slice(args);
        let Some(s) = xz(&a, &data[..data.len() / (k + 1)]) else {
            return;
        };
        file.extend_from_slice(&s);
        file.extend_from_slice(&[0; 4][..4 * (k % 2)]);
    }
    let path = std::env::temp_dir().join(format!("xz-native-list-{}.xz", std::process::id()));
    fs::write(&path, &file).unwrap();
    let out = std::process::Command::new("xz")
        .args(["--robot", "--list", "-vv"])
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(&path).unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();

    let mut r = open(&file).unwrap();
    let (mut streams, mut blocks) = (0, 0);
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "stream" => {
                let s = r.streams()[streams];
                streams += 1;
                let want = [
                    streams.to_string(),
                    s.block_count.to_string(),
                    s.compressed_offset.to_string(),
                    s.uncompressed_offset.to_string(),
                    s.compressed_size.to_string(),
                    s.uncompressed_size.to_string(),
                ];
                assert_eq!(f[1..7], want, "{line}");
                assert_eq!(f[8], check_name(s.check), "{line}");
                assert_eq!(f[9], s.padding.to_string(), "{line}");
            }
            "block" => {
                let b = r.blocks()[blocks];
                blocks += 1;
                let s = r.streams()[b.stream];
                let d = r.block_details(blocks - 1).unwrap();
                let flags = format!(
                    "{}{}",
                    if d.compressed_size.is_some() {
                        "c"
                    } else {
                        "-"
                    },
                    if d.uncompressed_size.is_some() {
                        "u"
                    } else {
                        "-"
                    },
                );
                // xz prints CRC32 and CRC64 as integers; they are stored little-endian.
                let mut stored = d.check_value.clone();
                if matches!(b.check, Check::Crc32 | Check::Crc64) {
                    stored.reverse();
                }
                let mut hex: String = stored.iter().map(|b| format!("{b:02x}")).collect();
                if hex.is_empty() {
                    hex = "---".into();
                }
                let data_size = b.unpadded_size - d.header_size as u64 - b.check.size() as u64;
                let want = [
                    (b.stream + 1).to_string(),
                    (blocks - s.first_block).to_string(),
                    blocks.to_string(),
                    b.compressed_offset.to_string(),
                    b.uncompressed_offset.to_string(),
                    b.total_size().to_string(),
                    b.uncompressed_size.to_string(),
                ];
                assert_eq!(f[1..8], want, "{line}");
                assert_eq!(f[9], check_name(b.check), "{line}");
                assert_eq!(f[10], hex, "{line}");
                assert_eq!(f[11], d.header_size.to_string(), "{line}");
                assert_eq!(f[12], flags, "{line}");
                assert_eq!(f[13], data_size.to_string(), "{line}");
                assert_eq!(f[15], filter_chain(&d.filters), "{line}");
                let Some(&Filter::Lzma2 { dict_size }) = d.filters.last() else {
                    panic!("{line}")
                };
                assert!(d.memusage > u64::from(dict_size), "{line}");
            }
            _ => {}
        }
    }
    assert_eq!((streams, blocks), (r.streams().len(), r.blocks().len()));
    assert_eq!(streams, 4);
    assert!(blocks > 4);

    // block_details doesn't move the read position.
    let mut head = vec![0; 1000];
    r.read_exact(&mut head).unwrap();
    r.block_details(blocks - 1).unwrap();
    let rest = read_all(&mut r).unwrap();
    head.extend_from_slice(&rest);
    let want: Vec<u8> = (0..parts.len())
        .flat_map(|k| data[..data.len() / (k + 1)].to_vec())
        .collect();
    assert!(head == want);
}
