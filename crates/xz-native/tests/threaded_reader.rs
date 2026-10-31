//! `XzReader` and `XzDecoder` with threads: the same output, the same
//! errors and at least the same data before an error as without threads, on
//! multi-Block, mixed (Blocks with and without sizes in their headers) and
//! damaged files.

mod common;

use common::{Rng, push_xz, read_xz, threads, xz};
use std::io::{BufRead, BufReader, Read};
use xz_native::{DecoderOptions, Error, Status, XzDecoder, XzReader};

fn text(n: usize, seed: u64) -> Vec<u8> {
    let words = [
        "alpha ", "beta ", "gamma ", "delta\n", "xz ", "native ", "block ",
    ];
    let mut r = Rng(seed);
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(words[r.below(words.len() as u64) as usize].as_bytes());
        if r.below(5) == 0 {
            v.push(r.next() as u8);
        }
    }
    v.truncate(n);
    v
}

fn read_vli(c: &[u8], p: &mut usize) -> u64 {
    let mut v = 0;
    for i in 0..9 {
        let b = c[*p];
        *p += 1;
        v |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}

/// The Blocks of the Stream at `start`: `(offset, header size, size with
/// padding and check)`. Every Block Header must store its Compressed Size.
fn blocks(c: &[u8], start: usize) -> Vec<(usize, usize, usize)> {
    let check = [0, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64][c[start + 7] as usize];
    let mut v = Vec::new();
    let mut pos = start + 12;
    while c[pos] != 0 {
        let size = (c[pos] as usize + 1) * 4;
        assert!(c[pos + 1] & 0x40 != 0, "no Compressed Size");
        let mut p = pos + 2;
        let comp = read_vli(c, &mut p) as usize;
        v.push((pos, size, size + comp.next_multiple_of(4) + check));
        pos += size + comp.next_multiple_of(4) + check;
    }
    v
}

fn fix_header_crc(c: &mut [u8], off: usize, size: usize) {
    let crc = xz_native::crc32(&c[off..off + size - 4], 0);
    c[off + size - 4..off + size].copy_from_slice(&crc.to_le_bytes());
}

/// Rewrites a Block Header without the sizes (as single-threaded `xz`
/// writes them), keeping its size.
fn strip_sizes(c: &mut [u8], off: usize, size: usize) {
    let flags = c[off + 1];
    let mut p = off + 2;
    if flags & 0x40 != 0 {
        read_vli(c, &mut p);
    }
    if flags & 0x80 != 0 {
        read_vli(c, &mut p);
    }
    let mut h = vec![c[off], flags & 0x3F];
    h.extend_from_slice(&c[p..off + size - 4]);
    h.resize(size - 4, 0);
    c[off..off + size - 4].copy_from_slice(&h);
    fix_header_crc(c, off, size);
}

fn multi_block(data: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let mut a = vec!["-z", "-c", "-T4"];
    a.extend_from_slice(args);
    xz(&a, data)
}

/// Every reader configuration must decode `input` to `expected`.
fn same_output(label: &str, input: &[u8], expected: &[u8]) {
    for (i, t) in [1, 2, 4, 0].into_iter().enumerate() {
        for cap in [1, 8192] {
            let (out, err) = read_xz(input, threads(t), cap, i as u64);
            assert_eq!(err, None, "{label}: threads {t}, capacity {cap}");
            assert!(
                out == expected,
                "{label}: threads {t}, capacity {cap}: data differs"
            );
        }
        let (out, err) = push_xz(input, threads(t), i as u64);
        assert_eq!(err, None, "{label}: XzDecoder, threads {t}");
        assert!(
            out == expected,
            "{label}: XzDecoder, threads {t}: data differs"
        );
    }
}

/// Threaded readers must report the error of the single-threaded reader,
/// after at least the same data. `good` bytes of output come before the
/// damage: threaded readers return all of them. `XzDecoder` with threads
/// returns the same error after the `good` bytes; how much of a damaged
/// Block it returns depends on the sizes of its calls, as without threads.
fn same_error(label: &str, input: &[u8], good: usize) {
    let mut first: Option<Vec<u8>> = None;
    let mut same_data = |what: &str, out: &[u8]| {
        // Damaged data may decode to garbage before the error is found;
        // every decoder must return the same garbage.
        let first = first.get_or_insert_with(|| out.to_vec());
        assert!(
            first.starts_with(out) || out.starts_with(first),
            "{what}: data before the error differs"
        );
    };
    for (cap, seed) in [(1, 1), (8192, 2)] {
        let (single, error) = read_xz(input, threads(1), cap, seed);
        assert!(error.is_some(), "{label}: no error");
        for t in [2, 4, 0] {
            let (out, err) = read_xz(input, threads(t), cap, seed);
            let what = format!("{label}: threads {t}, capacity {cap}");
            assert_eq!(err, error, "{what}: error");
            assert!(
                out.len() >= single.len() && out.len() >= good,
                "{what}: {} bytes before the error, single-threaded {}, good {good}",
                out.len(),
                single.len()
            );
            assert!(out.starts_with(&single), "{what}: data differs");
            same_data(&what, &out);
        }
        for t in [1, 2, 4, 0] {
            let (out, err) = push_xz(input, threads(t), seed);
            let what = format!("{label}: XzDecoder, threads {t}");
            assert_eq!(err, error, "{what}: error");
            if t != 1 {
                assert!(
                    out.len() >= good,
                    "{what}: {} bytes, good {good}",
                    out.len()
                );
            }
            same_data(&what, &out);
        }
    }
}

#[test]
fn mixed_and_concatenated() {
    let data = text(600_000, 1);
    let (Some(t1), Some(t4), Some(t1_blocks)) = (
        xz(&["-z", "-c", "-T1"], &data[..100_000]),
        multi_block(
            &data[100_000..400_000],
            &["--block-size=30000", "--check=sha256"],
        ),
        xz(
            &["-z", "-c", "-T1", "--block-size=50000", "--check=crc32"],
            &data[400_000..],
        ),
    ) else {
        return;
    };
    let mut cat = t1.clone();
    cat.extend_from_slice(&t4);
    cat.extend_from_slice(&[0; 8]);
    cat.extend_from_slice(&t1_blocks);
    same_output("T1 + T4 + T1 with Blocks", &cat, &data);

    // Some Blocks without sizes in the middle of a multi-Block Stream.
    let mut patched = t4.clone();
    for (i, &(off, size, _)) in blocks(&t4, 0).iter().enumerate() {
        if i % 3 == 1 {
            strip_sizes(&mut patched, off, size);
        }
    }
    assert_ne!(patched, t4);
    same_output("patched", &patched, &data[100_000..400_000]);
    // Only the first or only the last Block without sizes.
    for which in [0, blocks(&t4, 0).len() - 1] {
        let mut p = t4.clone();
        let (off, size, _) = blocks(&t4, 0)[which];
        strip_sizes(&mut p, off, size);
        same_output("one Block without sizes", &p, &data[100_000..400_000]);
    }

    // Empty Streams and a Stream with an empty Block.
    let (Some(e1), Some(e4)) = (xz(&["-z", "-c", "-T1"], b""), multi_block(b"", &[])) else {
        return;
    };
    let mut empties = e1.clone();
    empties.extend_from_slice(&t4);
    empties.extend_from_slice(&e4);
    same_output("empty Streams", &empties, &data[100_000..400_000]);
    same_output("empty", &e4, b"");
}

#[test]
fn not_concatenated_leaves_the_rest() {
    let data = text(200_000, 2);
    let Some(a) = multi_block(&data, &["--block-size=20000"]) else {
        return;
    };
    let mut cat = a.clone();
    cat.extend_from_slice(b"\0\0\0\0trailing");
    for t in [1, 4] {
        let opts = DecoderOptions::default().concatenated(false).threads(t);
        let mut r = XzReader::with_options(BufReader::with_capacity(100, &cat[..]), opts);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert!(out == data);
        let mut inner = r.into_inner();
        let mut rest = Vec::new();
        inner.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"\0\0\0\0trailing", "threads {t}");

        let mut dec = XzDecoder::with_options(opts);
        let (mut pos, mut out, mut buf) = (0, Vec::new(), vec![0u8; 10_000]);
        loop {
            let (used, n, status) = dec.decode(&cat[pos..], &mut buf, true).unwrap();
            pos += used;
            out.extend_from_slice(&buf[..n]);
            if status == Status::StreamEnd {
                break;
            }
        }
        assert!(out == data, "XzDecoder, threads {t}");
        assert_eq!(pos, a.len(), "XzDecoder, threads {t}");
    }
    // Trailing garbage is an error after all the data, as without threads.
    same_error("trailing garbage", &cat, data.len());
}

#[test]
fn memory_limits() {
    let data = text(3_000_000, 3);
    let Some(c) = multi_block(&data, &["-1", "--block-size=1000000"]) else {
        return;
    };
    // Blocks larger than the memory limit are decoded on the reading thread.
    for memlimit in [2 << 20, 3 << 20, 64 << 20] {
        for t in [2, 0] {
            let opts = DecoderOptions::default().memlimit(memlimit).threads(t);
            let (out, err) = read_xz(&c, opts, 8192, 5);
            assert_eq!(err, None, "memlimit {memlimit}");
            assert!(out == data, "memlimit {memlimit}");
        }
    }
    // A dictionary over the limit fails like without threads.
    for t in [1, 4] {
        let opts = DecoderOptions::default().memlimit(1 << 20).threads(t);
        let (out, err) = read_xz(&c, opts, 8192, 5);
        assert_eq!((out.len(), err), (0, Some(Error::MemLimit)), "threads {t}");
    }

    // XzDecoder returns the data before a Block over the limit, and goes on
    // after the limit is raised.
    let Some(small) = multi_block(&data[..300_000], &["-0", "--block-size=50000"]) else {
        return;
    };
    let mut cat = small;
    cat.extend_from_slice(&c);
    let mut expected = data[..300_000].to_vec();
    expected.extend_from_slice(&data);
    for t in [1, 4] {
        let opts = DecoderOptions::default().memlimit(1 << 20).threads(t);
        let mut dec = XzDecoder::with_options(opts);
        let (mut pos, mut out, mut buf) = (0, Vec::new(), vec![0u8; 1 << 16]);
        let mut hit = None;
        loop {
            match dec.decode(&cat[pos..], &mut buf, true) {
                Ok((used, n, status)) => {
                    pos += used;
                    out.extend_from_slice(&buf[..n]);
                    if status == Status::StreamEnd {
                        break;
                    }
                }
                Err(Error::MemLimit) => {
                    assert_eq!(hit, None, "threads {t}: second MemLimit");
                    assert!(dec.memusage() > 1 << 20);
                    // Calling again with the same limit fails again.
                    assert_eq!(
                        dec.decode(&cat[pos..], &mut buf, true).err(),
                        Some(Error::MemLimit)
                    );
                    hit = Some(out.len());
                    dec.set_memlimit(u64::MAX);
                }
                Err(e) => panic!("threads {t}: {e}"),
            }
        }
        assert_eq!(hit, Some(300_000), "threads {t}");
        assert!(out == expected, "threads {t}");
    }
}

#[test]
fn checks_and_options() {
    let data = text(300_000, 4);
    let Some(c) = multi_block(&data, &["--block-size=30000", "--check=crc64"]) else {
        return;
    };
    let bl = blocks(&c, 0);
    let (off, _, total) = bl[bl.len() / 2];
    let mut bad = c.clone();
    bad[off + total - 1] ^= 1;
    for t in [1, 2, 4] {
        let opts = DecoderOptions::default().ignore_check(true).threads(t);
        let (out, err) = read_xz(&bad, opts, 8192, 6);
        assert_eq!(err, None, "ignore_check, threads {t}");
        assert!(out == data);
        let (_, err) = read_xz(&bad, threads(t), 8192, 6);
        assert_eq!(err, Some(Error::CheckMismatch), "threads {t}");
    }
}

/// Damages a multi-Block file in many ways; threaded readers must behave
/// like the single-threaded one.
fn damage(label: &str, c: &[u8], bl: &[(usize, usize, usize)], data: &[u8], block_size: usize) {
    let mid = bl.len() / 2;
    let (off, size, total) = bl[mid];
    let good = mid * block_size;
    let has_sizes = c[off + 1] & 0xC0 == 0xC0;

    let mut cases: Vec<(String, Vec<u8>, usize)> = Vec::new();
    let mut add = |name: &str, f: &dyn Fn(&mut Vec<u8>), good: usize| {
        let mut m = c.to_vec();
        f(&mut m);
        cases.push((format!("{label}: {name}"), m, good));
    };
    add("data start", &|m| m[off + size + 3] ^= 0x10, good);
    add(
        "data middle",
        &|m| m[off + size + (total - size) / 2] ^= 0x04,
        good,
    );
    add("data end", &|m| m[off + total - 12] ^= 0x80, good);
    add("check", &|m| m[off + total - 1] ^= 1, good);
    add("header CRC", &|m| m[off + size - 1] ^= 1, good);
    add("header size byte", &|m| m[off] = 0, good);
    add("first Block", &|m| m[bl[0].0 + bl[0].1 + 8] ^= 0x20, 0);
    let last = bl[bl.len() - 1];
    add(
        "last Block check",
        &|m| m[last.0 + last.2 - 1] ^= 0x20,
        (bl.len() - 1) * block_size,
    );
    if has_sizes {
        // Compressed Size and Uncompressed Size off by one, valid CRC.
        add(
            "compressed size",
            &|m| {
                m[off + 2] ^= 1;
                fix_header_crc(m, off, size);
            },
            good,
        );
        add(
            "uncompressed size",
            &|m| {
                let mut p = off + 2;
                read_vli(m, &mut p);
                m[p] ^= 1;
                fix_header_crc(m, off, size);
            },
            good,
        );
    }
    // The Index, the Stream Footer and the Stream Header.
    let index = bl.last().map_or(12, |b| b.0 + b.2);
    for i in (index..c.len()).step_by(3) {
        add(
            &format!("byte {i} of the Index/Footer"),
            &|m| m[i] ^= 0x41,
            data.len(),
        );
    }
    for i in [0, 6, 7, 8, 11] {
        add(&format!("Stream Header byte {i}"), &|m| m[i] ^= 2, 0);
    }
    // Truncation all over, and at Block boundaries.
    let mut cuts: Vec<usize> = (0..c.len()).step_by(c.len() / 30 + 1).collect();
    for &(o, s, t) in bl {
        cuts.extend([o - 1, o + 1, o + s, o + t - 1]);
    }
    cuts.extend([index, index + 1, c.len() - 12, c.len() - 1]);
    for cut in cuts {
        let good = bl.iter().take_while(|b| b.0 + b.2 <= cut).count() * block_size;
        add(&format!("cut at {cut}"), &|m| m.truncate(cut), good);
    }
    drop(add);
    for (name, m, good) in cases {
        same_error(&name, &m, good.min(data.len()));
    }
}

#[test]
fn damaged_multi_block() {
    let data = text(200_000, 7);
    let Some(c) = multi_block(&data, &["--block-size=20000"]) else {
        return;
    };
    damage("T4", &c, &blocks(&c, 0), &data, 20_000);
    let Some(c) = multi_block(
        &data,
        &["--block-size=20000", "--check=sha256", "--x86", "--lzma2"],
    ) else {
        return;
    };
    damage("T4 x86 SHA-256", &c, &blocks(&c, 0), &data, 20_000);
    // Every second Block without sizes, so both paths fail.
    let mut mixed = c.clone();
    for (i, &(off, size, _)) in blocks(&c, 0).iter().enumerate() {
        if i % 2 == 1 {
            strip_sizes(&mut mixed, off, size);
        }
    }
    damage("mixed", &mixed, &blocks(&c, 0), &data, 20_000);
}

/// A reader that counts how much was read, to see the reader stop early.
struct Counting<'a>(&'a [u8], usize);

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1 += n;
        Ok(n)
    }
}

impl BufRead for Counting<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        Ok(&self.0[..self.0.len().min(4096)])
    }
    fn consume(&mut self, n: usize) {
        self.0 = &self.0[n..];
        self.1 += n;
    }
}

#[test]
fn drop_mid_stream() {
    let data = text(8_000_000, 8);
    let Some(c) = multi_block(&data, &["-6", "--block-size=100000"]) else {
        return;
    };
    for t in [2, 4, 0] {
        for read in [0usize, 1, 100_000, 1_000_000, 7_000_000] {
            let start = std::time::Instant::now();
            let mut r = XzReader::with_options(Counting(&c, 0), threads(t));
            let mut buf = vec![0u8; read];
            r.read_exact(&mut buf).unwrap();
            assert!(buf == data[..read]);
            // `into_inner` drops the decoder, which joins its threads.
            let consumed = r.into_inner().1;
            assert!(start.elapsed().as_secs() < 30, "drop took too long");
            // Read-ahead is bounded: 2 * threads Blocks of 100 kB.
            if t != 0 && read <= 1_000_000 {
                assert!(
                    consumed < c.len() / 2,
                    "threads {t}: read {consumed} bytes ahead"
                );
            }
        }
    }
    // Dropping after an error.
    let mut bad = c.clone();
    let mid = c.len() / 2;
    bad[mid] ^= 1;
    let mut r = XzReader::with_options(&bad[..], threads(4));
    let mut out = Vec::new();
    assert!(r.read_to_end(&mut out).is_err());
    assert!(r.read(&mut [0; 10]).is_err(), "the error repeats");
    drop(r);
}

#[test]
fn reader_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<XzReader<&[u8]>>();
    check::<XzDecoder>();
}

/// `XzDecoder` with threads reads Blocks ahead of its output, at most
/// `2 * threads`, and dropping it mid-stream joins its threads.
#[test]
fn push_decoder_reads_ahead() {
    let data = text(2_000_000, 10);
    let Some(c) = multi_block(&data, &["--block-size=100000"]) else {
        return;
    };
    let bl = blocks(&c, 0);
    for t in [1, 2] {
        let mut dec = XzDecoder::with_options(threads(t));
        let mut buf = [0u8; 1000];
        let (used, n, _) = dec.decode(&c, &mut buf, true).unwrap();
        assert!(n > 0 && buf[..n] == data[..n], "threads {t}");
        let ahead = bl.iter().filter(|b| b.0 + b.2 <= used).count();
        let expected = if t == 1 { 0 } else { 2 * t };
        assert_eq!(ahead, expected, "threads {t}: Blocks read");
        drop(dec);
    }
}

/// Input that fails once at each of some positions.
struct Flaky<'a> {
    data: &'a [u8],
    pos: usize,
    fail_at: Vec<usize>,
}

impl Read for Flaky<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.fill_buf()?.read(buf)?;
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Flaky<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let next = self.fail_at.first().copied().unwrap_or(usize::MAX);
        if next == self.pos {
            self.fail_at.remove(0);
            return Err(std::io::Error::other("flaky"));
        }
        Ok(&self.data[self.pos..next.min(self.data.len()).min(self.pos + 1000)])
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

#[test]
fn io_errors_can_be_retried() {
    let data = text(300_000, 9);
    let Some(t4) = multi_block(&data[..200_000], &["--block-size=20000"]) else {
        return;
    };
    let Some(t1) = xz(&["-z", "-c", "-T1"], &data[200_000..]) else {
        return;
    };
    let mut c = t4;
    c.extend_from_slice(&t1);
    for t in [1, 2, 4] {
        let fail_at = (0..c.len()).step_by(997).chain([c.len()]).collect();
        let flaky = Flaky {
            data: &c,
            pos: 0,
            fail_at,
        };
        let mut r = XzReader::with_options(flaky, threads(t));
        let (mut out, mut buf, mut errors) = (Vec::new(), [0u8; 3000], 0);
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::Other, "threads {t}");
                    errors += 1;
                }
            }
        }
        assert!(out == data, "threads {t}");
        assert!(errors > 0);
    }
}

/// More threads than liblzma's LZMA_THREADS_MAX is an options error, as in
/// lzma_stream_decoder_mt.
#[test]
fn too_many_threads() {
    let packed = xz_native::compress(b"thread limit", 1).unwrap();
    let opts = xz_native::DecoderOptions::default().threads(16385);
    assert_eq!(
        xz_native::decompress_with_options(&packed, opts).err(),
        Some(xz_native::Error::Options)
    );
    let mut out = Vec::new();
    let e = xz_native::XzReader::with_options(&packed[..], opts)
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    // The limit itself is fine.
    let opts = xz_native::DecoderOptions::default().threads(16384);
    assert_eq!(
        xz_native::decompress_with_options(&packed, opts).unwrap(),
        b"thread limit"
    );
}

/// A damaged magic in a later Stream Header is a data error, not a format
/// error (only the first Stream decides whether the input is .xz at all),
/// with threads too.
#[test]
fn bad_magic_in_a_later_stream() {
    let data = text(100_000, 12);
    let Some(one) = multi_block(&data, &["--block-size=20000"]) else {
        return;
    };
    let mut cat = one.clone();
    cat.extend_from_slice(&one);
    let second = one.len();
    cat[second + 1] ^= 0x20;
    same_error("second Stream Header magic", &cat, data.len());
    for t in [1, 4] {
        let (_, err) = read_xz(&cat, threads(t), 8192, 1);
        assert_eq!(err, Some(Error::Data), "threads {t}");
    }
}

/// `memlimit_threading` bounds the Blocks in flight: at 0, every Block is
/// decoded on the reading thread (no read-ahead); the output is the same
/// either way, and the one-shot decoder falls back to one thread.
#[test]
fn memlimit_threading_limits_read_ahead() {
    let data = text(2_000_000, 14);
    let Some(c) = multi_block(&data, &["--block-size=100000"]) else {
        return;
    };
    let bl = blocks(&c, 0);
    for (limit, expected) in [(0, 0), (u64::MAX, 4)] {
        let opts = threads(2).memlimit_threading(limit);
        let mut dec = XzDecoder::with_options(opts);
        let mut buf = [0u8; 1000];
        let (used, n, _) = dec.decode(&c, &mut buf, true).unwrap();
        assert!(n > 0 && buf[..n] == data[..n]);
        let ahead = bl.iter().filter(|b| b.0 + b.2 <= used).count();
        assert_eq!(ahead, expected, "memlimit_threading {limit}");
        let (out, err) = read_xz(&c, opts, 8192, 3);
        assert_eq!(err, None);
        assert!(out == data, "memlimit_threading {limit}");
        assert!(xz_native::decompress_with_options(&c, opts.threads(4)).unwrap() == data);
    }
    // The stop limit still fails.
    let opts = threads(2).memlimit(1 << 10).memlimit_threading(u64::MAX);
    assert_eq!(read_xz(&c, opts, 8192, 3).1, Some(Error::MemLimit));
}
