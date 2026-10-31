//! Deterministic differential test against C liblzma 5.8.4: valid streams
//! and thousands of mutated ones (bit flips, byte changes, truncation,
//! insertion, trailing data) must give the same success/failure and
//! identical output. The CI fuzz jobs do the same with coverage guidance.

use liblzma::stream::{Action, CONCATENATED, Check, Filters, LzmaOptions, Status, Stream};
use std::io::Read;

const MEMLIMIT: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Xz,
    Lzma,
    Lzip,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn reference(input: &[u8], format: Format) -> Option<Vec<u8>> {
    let s = match format {
        Format::Xz => Stream::new_stream_decoder(MEMLIMIT, CONCATENATED),
        Format::Lzma => Stream::new_lzma_decoder(MEMLIMIT),
        Format::Lzip => Stream::new_lzip_decoder(MEMLIMIT, CONCATENATED),
    }
    .unwrap();
    let mut out = Vec::new();
    liblzma::bufread::XzDecoder::new_stream(input, s)
        .read_to_end(&mut out)
        .ok()
        .map(|_| out)
}

fn ours(input: &[u8], format: Format) -> Option<Vec<u8>> {
    match format {
        Format::Lzma => xz_native::decompress_lzma_with_options(
            input,
            xz_native::DecoderOptions::new().memlimit(MEMLIMIT),
        )
        .ok(),
        // Like liblzma, data after the last member is ignored.
        Format::Lzip => xz_native::decompress_lzip_with_options(
            input,
            xz_native::DecoderOptions::new().memlimit(MEMLIMIT),
        )
        .ok(),
        Format::Xz => {
            let opts = xz_native::DecoderOptions::default().memlimit(MEMLIMIT);
            let flat = xz_native::decompress_with_options(input, opts).ok();
            let streaming = xz_native::decompress_streaming(input, opts).ok();
            assert!(
                flat == streaming,
                "one-shot and streaming decoders disagree"
            );
            let threads = opts.threads(4);
            let threaded = xz_native::decompress_with_options(input, threads).ok();
            assert!(
                threaded == flat,
                "threaded and single-threaded decoders disagree"
            );
            // XzReader with threads: the same error as without, after at
            // least the same data.
            let (single, single_err) = read(input, opts);
            let (mt, mt_err) = read(input, threads);
            assert_eq!(
                mt_err, single_err,
                "threaded and streaming readers disagree"
            );
            assert!(
                mt.starts_with(&single),
                "threaded reader returned {} bytes before the error, streaming {}",
                mt.len(),
                single.len()
            );
            assert!(
                mt_err.is_none() == flat.is_some() && (mt_err.is_some() || flat == Some(mt)),
                "threaded reader and one-shot decoder disagree"
            );
            flat
        }
    }
}

/// Decodes with [`xz_native::XzReader`]; returns the data read and the error.
fn read(input: &[u8], opts: xz_native::DecoderOptions) -> (Vec<u8>, Option<xz_native::Error>) {
    // Small reads through a small buffer, so that the threaded reader's
    // read-ahead and the streaming decoder's chunking both matter.
    let inner = std::io::BufReader::with_capacity(97, input);
    let mut r = xz_native::XzReader::with_options(inner, opts);
    let mut buf = [0u8; 1000];
    let mut out = Vec::new();
    loop {
        match r.read(&mut buf) {
            Ok(0) => return (out, None),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                let e = e
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<xz_native::Error>());
                return (out, Some(*e.expect("not an xz_native::Error")));
            }
        }
    }
}

fn encode(mut s: Stream, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1024);
    loop {
        let consumed = s.total_in() as usize;
        if out.len() == out.capacity() {
            out.reserve(1 << 16);
        }
        if s.process_vec(&data[consumed..], &mut out, Action::Finish)
            .unwrap()
            == Status::StreamEnd
        {
            return out;
        }
    }
}

fn compress(data: &[u8], filters: &Filters, check: Check) -> Vec<u8> {
    encode(Stream::new_stream_encoder(filters, check).unwrap(), data)
}

/// A .lz member: raw LZMA1 (lc=3, lp=0, pb=2, ending with an EOPM) with
/// the lzip header and footer. `dict` is the dictionary size byte.
fn lzip_member(data: &[u8], preset: u32, version: u8, dict: u8) -> Vec<u8> {
    let b2log = dict as u32 & 0x1F;
    let dict_size = (1 << b2log) - ((dict as u32 >> 5) << (b2log - 4));
    let mut opts = LzmaOptions::new_preset(preset).unwrap();
    opts.dict_size(dict_size)
        .literal_context_bits(3)
        .literal_position_bits(0)
        .position_bits(2);
    let mut f = Filters::new();
    f.lzma1(&opts);
    let raw = encode(Stream::new_raw_encoder(&f).unwrap(), data);
    let mut m = b"LZIP".to_vec();
    m.extend_from_slice(&[version, dict]);
    m.extend_from_slice(&raw);
    m.extend_from_slice(&xz_native::crc32(data, 0).to_le_bytes());
    m.extend_from_slice(&(data.len() as u64).to_le_bytes());
    if version == 1 {
        let size = m.len() as u64 + 8;
        m.extend_from_slice(&size.to_le_bytes());
    }
    m
}

fn samples() -> Vec<(Vec<u8>, Format)> {
    let mut r = Rng(0xDEAD_BEEF_1234_5678);
    let text: Vec<u8> = (0..60_000)
        .map(|i| b"the quick brown fox jumps "[(i * 7 + r.below(3)) % 26])
        .collect();
    let rnd: Vec<u8> = (0..20_000).map(|_| r.next() as u8).collect();
    let mut v = Vec::new();
    for data in [&text, &rnd] {
        for preset in [0, 6] {
            let mut f = Filters::new();
            f.lzma2(&LzmaOptions::new_preset(preset).unwrap());
            v.push((compress(data, &f, Check::Crc64), Format::Xz));
            let mut f = Filters::new();
            f.x86();
            f.lzma2(&LzmaOptions::new_preset(preset).unwrap());
            v.push((compress(data, &f, Check::Sha256), Format::Xz));
        }
        let mut f = Filters::new();
        f.arm64();
        f.lzma2(&LzmaOptions::new_preset(3).unwrap());
        v.push((compress(data, &f, Check::Crc32), Format::Xz));
        // Multi-Block, for the threaded decoder.
        let mut mt = Vec::new();
        let enc = liblzma::stream::MtStreamBuilder::new()
            .threads(2)
            .block_size(4096)
            .preset(1)
            .check(Check::Crc64)
            .encoder()
            .unwrap();
        liblzma::read::XzEncoder::new_stream(&data[..], enc)
            .read_to_end(&mut mt)
            .unwrap();
        v.push((mt, Format::Xz));
        let mut out = Vec::new();
        let enc = Stream::new_lzma_encoder(&LzmaOptions::new_preset(6).unwrap()).unwrap();
        liblzma::read::XzEncoder::new_stream(&data[..], enc)
            .read_to_end(&mut out)
            .unwrap();
        v.push((out, Format::Lzma));
        // One version 1 member with an 8 MiB dictionary, and two members
        // (version 0 with 64 KiB - 3/16, version 1 with 4 KiB).
        v.push((lzip_member(data, 6, 1, 0x17), Format::Lzip));
        let third = data.len() / 3;
        let mut two = lzip_member(&data[..third], 1, 0, 0x70);
        two.extend_from_slice(&lzip_member(&data[third..], 0, 1, 0x0C));
        v.push((two, Format::Lzip));
    }
    v
}

#[test]
fn mutations_agree_with_liblzma() {
    let mut r = Rng(42);
    let mut cases = 0;
    let mut ok_cases = 0;
    for (packed, format) in samples() {
        assert_eq!(ours(&packed, format), reference(&packed, format));
        let iterations = if cfg!(debug_assertions) { 300 } else { 3000 };
        // .lz allows trailing data, so it gets one more kind of mutation.
        let kinds = if format == Format::Lzip { 6 } else { 5 };
        for _ in 0..iterations {
            let mut m = packed.clone();
            match r.below(kinds) {
                0 => {
                    let i = r.below(m.len());
                    m[i] ^= 1 << r.below(8);
                }
                1 => {
                    // Concentrate on headers, where most of the logic is.
                    let i = r.below(m.len().min(64));
                    m[i] = r.next() as u8;
                }
                2 => {
                    let i = m.len() - 1 - r.below(m.len().min(64));
                    m[i] = r.next() as u8;
                }
                3 => m.truncate(r.below(m.len())),
                4 => {
                    let i = r.below(m.len());
                    m.insert(i, r.next() as u8);
                }
                _ => {
                    // Trailing data, often starting like a new member.
                    m.extend(b"LZIP\x01\x0C".iter().take(r.below(7)));
                    if r.below(2) == 0 {
                        m.push(r.next() as u8);
                    }
                }
            }
            let a = ours(&m, format);
            let b = reference(&m, format);
            if a != b {
                std::fs::write(concat!(env!("CARGO_TARGET_TMPDIR"), "/mismatch.bin"), &m).unwrap();
            }
            assert!(
                a == b,
                "mismatch ({format:?}): ours ok={} liblzma ok={}",
                a.is_some(),
                b.is_some()
            );
            cases += 1;
            ok_cases += a.is_some() as usize;
        }
    }
    eprintln!("{cases} mutated inputs, {ok_cases} decoded successfully by both");
}

/// `memusage()` matches liblzma's `lzma_memusage` once the headers are read
/// (on 64-bit platforms): liblzma decodes with that memory limit and fails
/// with one byte less, so limits accept and reject the same files.
#[test]
#[cfg(target_pointer_width = "64")]
fn memusage_matches_liblzma() {
    use xz_native::{BcjKind, EncoderOptions, Filter, LzmaDecoder, XzDecoder};
    let data = b"memory usage".repeat(100);
    let decodes = |s: Result<Stream, liblzma::stream::Error>, input: &[u8]| {
        let mut s = s.unwrap();
        let mut out = Vec::with_capacity(data.len() + 100);
        match s.process_vec(input, &mut out, Action::Finish) {
            Ok(_) => true,
            Err(liblzma::stream::Error::MemLimit) => false,
            Err(e) => panic!("{e:?}"),
        }
    };
    for dict in [4096u32, 5000, 1 << 20, (3 << 20) + 1] {
        for filters in [
            vec![],
            vec![Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            }],
            vec![Filter::Delta { distance: 4 }],
        ] {
            let mut opts = EncoderOptions::preset(1).unwrap().filters(filters.clone());
            opts.lzma.dict_size = dict;
            let xz = xz_native::compress_with_options(&data, &opts).unwrap();
            let mut d = XzDecoder::new();
            d.decode(&xz, &mut [0u8; 1], false).unwrap();
            let m = d.memusage();
            let label = format!("xz, dict {dict}, {filters:?}");
            assert!(decodes(Stream::new_stream_decoder(m, 0), &xz), "{label}");
            assert!(
                !decodes(Stream::new_stream_decoder(m - 1, 0), &xz),
                "{label}"
            );
        }
        let lzma_opts = xz_native::LzmaOptions::preset(1).unwrap().dict_size(dict);
        let lzma = xz_native::compress_lzma_with_options(&data, &lzma_opts).unwrap();
        let mut d = LzmaDecoder::new();
        d.decode(&lzma, &mut [0u8; 1], false).unwrap();
        let m = d.memusage();
        assert!(
            decodes(Stream::new_lzma_decoder(m), &lzma),
            "lzma, dict {dict}"
        );
        assert!(
            !decodes(Stream::new_lzma_decoder(m - 1), &lzma),
            "lzma, dict {dict}"
        );
    }
}

/// liblzma's `lzma_auto_decoder` with `LZMA_CONCATENATED`.
fn reference_auto(input: &[u8]) -> Option<Vec<u8>> {
    let s = Stream::new_auto_decoder(MEMLIMIT, CONCATENATED).unwrap();
    let mut out = Vec::new();
    liblzma::bufread::XzDecoder::new_stream(input, s)
        .read_to_end(&mut out)
        .ok()
        .map(|_| out)
}

/// `decompress_auto` and `AutoReader` (through a small buffer) must agree;
/// returns their result.
fn ours_auto(input: &[u8]) -> Option<Vec<u8>> {
    let opts = xz_native::DecoderOptions::default().memlimit(MEMLIMIT);
    let one_shot = xz_native::decompress_auto_with_options(input, opts).ok();
    let mut r =
        xz_native::AutoReader::with_options(std::io::BufReader::with_capacity(97, input), opts);
    let mut out = Vec::new();
    let streamed = r.read_to_end(&mut out).ok().map(|_| out);
    assert!(
        one_shot == streamed,
        "decompress_auto and AutoReader disagree"
    );
    one_shot
}

#[test]
fn auto_decoder_agrees_with_liblzma() {
    let lzip = reference_auto_has_lzip();
    let mut r = Rng(7);
    let mut cases = 0;
    for (packed, format) in samples() {
        if format == Format::Lzip && !lzip {
            continue;
        }
        assert_eq!(ours_auto(&packed), reference_auto(&packed));
        let iterations = if cfg!(debug_assertions) { 100 } else { 1000 };
        for _ in 0..iterations {
            let mut m = packed.clone();
            match r.below(5) {
                0 => {
                    let i = r.below(m.len().min(32));
                    m[i] = r.next() as u8;
                }
                1 => m.truncate(r.below(m.len())),
                2 => m.extend((0..1 + r.below(8)).map(|_| r.next() as u8)),
                3 => m.extend_from_slice(&[0; 4]),
                _ => {
                    // Another file after it, of any format.
                    let (other, _) = &samples()[r.below(samples().len())];
                    m.extend_from_slice(&other[..other.len().min(200)]);
                }
            }
            let a = ours_auto(&m);
            let b = reference_auto(&m);
            assert!(
                a == b,
                "auto mismatch: ours ok={} liblzma ok={}",
                a.is_some(),
                b.is_some()
            );
            cases += 1;
        }
    }
    eprintln!("{cases} mutated inputs through both auto decoders");
}

/// Every xz test file (.xz, .lzma, .lz), and short garbage.
#[test]
fn auto_decoder_test_files() {
    let lzip = reference_auto_has_lzip();
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        let Some(ext) = path.extension().and_then(|x| x.to_str()) else {
            continue;
        };
        if !matches!(ext, "xz" | "lzma" | "lz") {
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        if data.first() == Some(&b'L') && !lzip {
            continue;
        }
        assert!(
            ours_auto(&data) == reference_auto(&data),
            "{}",
            path.display()
        );
        n += 1;
    }
    assert!(n > 90, "{n} files");
    for garbage in [
        &b"hello, world"[..],
        b"L",
        b"LZIP",
        b"\xFD7zX",
        b"\x5d\0\0\x80\0",
        &[0; 13],
    ] {
        assert!(ours_auto(garbage) == reference_auto(garbage), "{garbage:?}");
    }
}

/// Whether the C build's auto decoder detects .lz (`HAVE_LZIP_DECODER`,
/// which `.cargo/config.toml` sets and plain `liblzma-sys` leaves out).
fn reference_auto_has_lzip() -> bool {
    let lz = lzip_member(b"probe", 0, 1, 0x0C);
    let ok = reference_auto(&lz).is_some();
    if !ok {
        eprintln!("C liblzma without HAVE_LZIP_DECODER: skipping .lz in the auto decoder tests");
    }
    ok
}
