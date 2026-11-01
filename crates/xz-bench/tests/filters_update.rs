//! Changing filters mid-stream (#97): `XzEncoder::update_filters` against
//! liblzma's `lzma_filters_update()`, single-threaded and multithreaded.
//! Both encoders get the same calls with the same output buffer sizes; the
//! results of every call and the output bytes must be the same.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use std::ffi::c_void;
use xz_native::{
    Action, BcjKind, Check, EncoderOptions, Error, Filter, LzmaOptions, Status, XzEncoder,
};

/// `lzma_options_delta`, which `liblzma-sys` doesn't declare.
#[repr(C)]
struct DeltaOptions {
    kind: u32,
    dist: u32,
    reserved_int: [u32; 4],
    reserved_ptr: [*mut c_void; 2],
}

/// The options behind a liblzma filter chain, kept alive while it is used.
enum COptions {
    Lzma(sys::lzma_options_lzma),
    Bcj(sys::lzma_options_bcj),
    Delta(DeltaOptions),
}

fn lzma_c(o: &LzmaOptions) -> sys::lzma_options_lzma {
    // SAFETY: an all-zero lzma_options_lzma is valid; the preset fills it.
    let mut c: sys::lzma_options_lzma = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is valid and writable.
    assert_eq!(unsafe { sys::lzma_lzma_preset(&mut c, 6) }, 0);
    c.dict_size = o.dict_size;
    c.lc = o.lc;
    c.lp = o.lp;
    c.pb = o.pb;
    c.mode = match o.mode {
        xz_native::Mode::Fast => sys::LZMA_MODE_FAST,
        _ => sys::LZMA_MODE_NORMAL,
    };
    c.nice_len = o.nice_len;
    c.mf = match o.mf {
        xz_native::MatchFinder::Hc3 => sys::LZMA_MF_HC3,
        xz_native::MatchFinder::Hc4 => sys::LZMA_MF_HC4,
        xz_native::MatchFinder::Bt2 => sys::LZMA_MF_BT2,
        xz_native::MatchFinder::Bt3 => sys::LZMA_MF_BT3,
        _ => sys::LZMA_MF_BT4,
    };
    c.depth = o.depth;
    c
}

/// A liblzma chain: `filters`, then LZMA2 with `lzma`, and the options it
/// points to (on the heap, so they stay put when the pair moves).
fn chain_c(filters: &[Filter], lzma: &LzmaOptions) -> (Vec<sys::lzma_filter>, Vec<COptions>) {
    let mut opts: Vec<COptions> = filters
        .iter()
        .map(|f| match *f {
            Filter::Bcj { start_offset, .. } => {
                COptions::Bcj(sys::lzma_options_bcj { start_offset })
            }
            Filter::Delta { distance, .. } => COptions::Delta(DeltaOptions {
                kind: 0,
                dist: u32::from(distance),
                reserved_int: [0; 4],
                reserved_ptr: [std::ptr::null_mut(); 2],
            }),
            Filter::Lzma2 { dict_size, .. } => {
                let mut c = lzma_c(lzma);
                c.dict_size = dict_size;
                COptions::Lzma(c)
            }
            _ => unreachable!(),
        })
        .collect();
    opts.push(COptions::Lzma(lzma_c(lzma)));
    let ids = filters
        .iter()
        .map(Filter::id)
        .chain([sys::LZMA_FILTER_LZMA2]);
    let mut chain: Vec<sys::lzma_filter> = ids
        .zip(opts.iter_mut())
        .map(|(id, o)| {
            let options = match o {
                COptions::Lzma(x) => x as *mut _ as *mut c_void,
                COptions::Bcj(x) => x as *mut _ as *mut c_void,
                COptions::Delta(x) => x as *mut _ as *mut c_void,
            };
            sys::lzma_filter { id, options }
        })
        .collect();
    chain.push(sys::lzma_filter {
        id: sys::LZMA_VLI_UNKNOWN,
        options: std::ptr::null_mut(),
    });
    (chain, opts)
}

fn check_c(c: Check) -> sys::lzma_check {
    match c {
        Check::None => sys::LZMA_CHECK_NONE,
        Check::Crc32 => sys::LZMA_CHECK_CRC32,
        Check::Sha256 => sys::LZMA_CHECK_SHA256,
        _ => sys::LZMA_CHECK_CRC64,
    }
}

fn ret_to_result(r: sys::lzma_ret) -> Result<(), Error> {
    match r {
        sys::LZMA_OK => Ok(()),
        sys::LZMA_PROG_ERROR => Err(Error::Usage),
        sys::LZMA_OPTIONS_ERROR => Err(Error::InvalidOptions),
        r => panic!("unexpected lzma_ret {r}"),
    }
}

/// A liblzma .xz encoder.
struct CEncoder {
    strm: Box<sys::lzma_stream>,
}

impl CEncoder {
    fn new(o: &EncoderOptions) -> Self {
        // SAFETY: an all-zero lzma_stream is LZMA_STREAM_INIT.
        let mut strm: Box<sys::lzma_stream> = Box::new(unsafe { std::mem::zeroed() });
        let (chain, _keep) = chain_c(&o.filters, &o.lzma);
        let r = if o.threads == 1 {
            // SAFETY: `strm` is initialized; `chain` is a valid chain alive
            // for the call (liblzma copies it).
            unsafe { sys::lzma_stream_encoder(&mut *strm, chain.as_ptr(), check_c(o.check)) }
        } else {
            // SAFETY: an all-zero lzma_mt is valid; the fields are set below.
            let mut mt: sys::lzma_mt = unsafe { std::mem::zeroed() };
            mt.threads = o.threads as u32;
            mt.block_size = o.block_size;
            mt.filters = chain.as_ptr();
            mt.check = check_c(o.check);
            // SAFETY: as above, and `mt` is valid for the call.
            unsafe { sys::lzma_stream_encoder_mt(&mut *strm, &mt) }
        };
        assert_eq!(r, sys::LZMA_OK);
        Self { strm }
    }

    fn update(&mut self, filters: &[Filter], lzma: &LzmaOptions) -> Result<(), Error> {
        let (chain, _keep) = chain_c(filters, lzma);
        // SAFETY: the stream is an initialized encoder; `chain` is valid
        // for the call (liblzma copies it).
        ret_to_result(unsafe { sys::lzma_filters_update(&mut *self.strm, chain.as_ptr()) })
    }

    fn code(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, bool), Error> {
        let a = match action {
            Action::Run => sys::LZMA_RUN,
            Action::SyncFlush => sys::LZMA_SYNC_FLUSH,
            Action::FullFlush => sys::LZMA_FULL_FLUSH,
            Action::FullBarrier => sys::LZMA_FULL_BARRIER,
            _ => sys::LZMA_FINISH,
        };
        let s = &mut *self.strm;
        s.next_in = input.as_ptr();
        s.avail_in = input.len();
        s.next_out = out.as_mut_ptr();
        s.avail_out = out.len();
        // SAFETY: the buffers are valid for the lengths given.
        let r = unsafe { sys::lzma_code(s, a) };
        if r != sys::LZMA_STREAM_END {
            ret_to_result(r)?;
        }
        Ok((
            input.len() - s.avail_in,
            out.len() - s.avail_out,
            r == sys::LZMA_STREAM_END,
        ))
    }
}

impl Drop for CEncoder {
    fn drop(&mut self) {
        // SAFETY: the stream was initialized and isn't used again.
        unsafe { sys::lzma_end(&mut *self.strm) }
    }
}

enum Step<'a> {
    /// Input with its action; `Run` until it is used, the others until
    /// `StreamEnd`.
    Code(&'a [u8], Action),
    Update(Vec<Filter>, LzmaOptions),
}

/// Runs `steps` on both encoders with output buffers of `out_size` bytes:
/// single-threaded in lockstep, comparing every call.
/// Returns the stream and the results of the updates.
#[track_caller]
fn run(
    o: &EncoderOptions,
    steps: &[Step<'_>],
    out_size: usize,
) -> (Vec<u8>, Vec<Result<(), Error>>, bool) {
    let mut ours = XzEncoder::with_options(o.clone()).unwrap();
    let mut theirs = CEncoder::new(o);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let mut results = Vec::new();
    let mut buf = vec![0u8; out_size];
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Update(filters, lzma) => {
                let r = ours.update_filters(filters, *lzma);
                assert_eq!(r, theirs.update(filters, lzma), "step {i}: update result");
                results.push(r);
            }
            &Step::Code(data, action) if o.threads != 1 => {
                // How much each call does depends on the workers; only the
                // output after a full flush or the end is fixed (a barrier,
                // like Run, doesn't wait for the output).
                let done = |ip: usize, end: bool| {
                    if action == Action::Run {
                        ip == data.len()
                    } else {
                        end
                    }
                };
                let mut ip = 0;
                loop {
                    let (used, n, st) = ours
                        .encode(&data[ip..], &mut buf, action)
                        .unwrap_or_else(|e| panic!("step {i}: {e:?}"));
                    a.extend_from_slice(&buf[..n]);
                    ip += used;
                    if done(ip, st == Status::StreamEnd) {
                        break;
                    }
                }
                ip = 0;
                loop {
                    let (used, n, end) = theirs
                        .code(&data[ip..], &mut buf, action)
                        .unwrap_or_else(|e| panic!("step {i}: liblzma error {e:?}"));
                    b.extend_from_slice(&buf[..n]);
                    ip += used;
                    if done(ip, end) {
                        break;
                    }
                }
                if !matches!(action, Action::Run | Action::FullBarrier) {
                    assert_eq!(a, b, "step {i}: output");
                }
            }
            &Step::Code(data, action) => {
                let mut ip = 0;
                loop {
                    let r = ours.encode(&data[ip..], &mut buf, action);
                    let (used, n, st) = match r {
                        Ok(r) => r,
                        Err(e) => {
                            // Both reject the call (a BCJ filter can't
                            // sync-flush), and the stream ends there.
                            let c = theirs.code(&data[ip..], &mut buf, action);
                            assert_eq!(c.err(), Some(e), "step {i}: encode error");
                            results.push(Err(e));
                            return (a, results, false);
                        }
                    };
                    a.extend_from_slice(&buf[..n]);
                    let (used_c, n_c, end_c) = theirs
                        .code(&data[ip..], &mut buf, action)
                        .unwrap_or_else(|e| panic!("step {i}: liblzma error {e:?}"));
                    b.extend_from_slice(&buf[..n_c]);
                    assert_eq!((used, n), (used_c, n_c), "step {i}: progress");
                    let end = st == Status::StreamEnd;
                    assert_eq!(end, end_c, "step {i}: end");
                    ip += used;
                    if if action == Action::Run {
                        ip == data.len()
                    } else {
                        end
                    } {
                        break;
                    }
                }
                assert_eq!(a, b, "step {i}: output");
            }
        }
    }
    (a, results, true)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Text-like data with some x86-like call instructions.
fn sample(n: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"filter ",
        b"chain ",
        b"block ",
        b"lzma2 ",
        b"\xe8\x10\x20\0\0",
        b"update ",
        b"\n",
        b"x86 ",
    ];
    let mut r = Rng(seed | 1);
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(WORDS[(r.next() % 8) as usize]);
        if r.next().is_multiple_of(5) {
            v.push(r.next() as u8);
        }
    }
    v.truncate(n);
    v
}

fn lzma(preset: u32, dict_size: u32) -> LzmaOptions {
    let mut o = LzmaOptions::preset(preset).unwrap();
    o.dict_size = dict_size;
    o
}

fn opts(threads: usize, block_size: u64) -> EncoderOptions {
    let mut o = EncoderOptions::preset(6).unwrap();
    o.lzma.dict_size = 1 << 18;
    o.threads = threads;
    // liblzma's single-threaded encoder has no Block size.
    o.block_size = if threads == 1 { 0 } else { block_size };
    o
}

fn x86() -> Vec<Filter> {
    vec![Filter::bcj(BcjKind::X86)]
}

fn decodes_to(xz: &[u8], parts: &[&[u8]]) {
    assert_eq!(xz_native::decompress(xz).unwrap(), parts.concat());
}

#[test]
fn between_blocks() {
    let d = sample(300_000, 1);
    let (p1, p2, p3) = (&d[..100_000], &d[100_000..200_000], &d[200_000..]);
    let mut lc0 = lzma(9, 1 << 17);
    (lc0.lc, lc0.lp, lc0.pb) = (0, 2, 0);
    let delta = vec![
        Filter::delta(4).unwrap(),
        Filter::bcj_with_start_offset(BcjKind::Arm64, 64).unwrap(),
    ];
    for threads in [1, 3] {
        let steps = [
            // Before any input: the first Block uses the new chain.
            Step::Update(x86(), lzma(1, 1 << 16)),
            Step::Code(p1, Action::FullFlush),
            Step::Update(delta.clone(), lc0),
            Step::Code(p2, Action::FullBarrier),
            Step::Update(Vec::new(), lzma(6 | xz_native::PRESET_EXTREME, 1 << 18)),
            Step::Code(p3, Action::Finish),
            // After the end.
            Step::Update(Vec::new(), lzma(6, 1 << 18)),
        ];
        let (xz, results, _) = run(&opts(threads, 40_000), &steps, 1 << 16);
        assert_eq!(results, [Ok(()), Ok(()), Ok(()), Err(Error::Usage)]);
        decodes_to(&xz, &[p1, p2, p3]);
    }
}

#[test]
fn invalid_options() {
    let d = sample(50_000, 2);
    let mut bad_lclp = lzma(6, 1 << 18);
    (bad_lclp.lc, bad_lclp.lp) = (3, 2);
    let four = vec![Filter::delta(1).unwrap(); 4];
    let lzma2_inside = vec![Filter::lzma2(1 << 20)];
    for threads in [1, 2] {
        let steps = [
            Step::Code(&d[..20_000], Action::FullFlush),
            Step::Update(Vec::new(), bad_lclp),
            Step::Update(four.clone(), lzma(6, 1 << 18)),
            Step::Update(lzma2_inside.clone(), lzma(6, 1 << 18)),
            Step::Update(Vec::new(), lzma(6, 1000)),
            // The encoder carries on with the old chain.
            Step::Code(&d[20_000..], Action::Finish),
        ];
        let (xz, results, _) = run(&opts(threads, 0), &steps, 4096);
        assert!(
            results.iter().all(|r| *r == Err(Error::InvalidOptions)),
            "{results:?}"
        );
        decodes_to(&xz, &[&d]);
    }
}

#[test]
fn within_a_block_single_threaded() {
    let d = sample(400_000, 3);
    let mut lc0 = lzma(6, 1 << 18);
    (lc0.lc, lc0.lp, lc0.pb) = (0, 2, 0);
    let mut lc4 = lzma(6, 1 << 18);
    (lc4.lc, lc4.lp, lc4.pb) = (4, 0, 4);
    let steps = [
        Step::Code(&d[..1000], Action::Run),
        // Other filter kinds within a Block.
        Step::Update(x86(), lzma(6, 1 << 18)),
        // After a sync flush the next chunk takes lc/lp/pb.
        Step::Code(&d[1000..100_000], Action::SyncFlush),
        Step::Update(Vec::new(), lc0),
        Step::Code(&d[100_000..200_000], Action::SyncFlush),
        // A new dictionary size and lc/lp/pb: lc/lp/pb now, the rest with
        // the next Block.
        Step::Update(Vec::new(), {
            let mut o = lc4;
            o.dict_size = 1 << 16;
            o
        }),
        Step::Code(&d[200_000..300_000], Action::FullFlush),
        Step::Code(&d[300_000..350_000], Action::SyncFlush),
        // Other filter kinds after a sync flush: an error, but liblzma
        // has taken the new lc/lp/pb for the rest of the Block.
        Step::Update(x86(), lc0),
        Step::Code(&d[350_000..], Action::Finish),
    ];
    let (xz, results, _) = run(&opts(1, 0), &steps, 1 << 16);
    assert_eq!(
        results,
        [Err(Error::Usage), Ok(()), Ok(()), Err(Error::Usage)]
    );
    decodes_to(&xz, &[&d]);
}

#[test]
fn within_a_block_multithreaded() {
    let d = sample(200_000, 4);
    let steps = [
        Step::Code(&d[..50_000], Action::Run),
        // A Block is being filled.
        Step::Update(x86(), lzma(6, 1 << 18)),
        Step::Update(Vec::new(), lzma(5, 1 << 18)),
        // Exactly two Blocks: none is being filled.
        Step::Code(&d[50_000..80_000], Action::Run),
        Step::Update(x86(), lzma(3, 1 << 17)),
        Step::Code(&d[80_000..], Action::Finish),
    ];
    let (xz, results, _) = run(&opts(2, 40_000), &steps, 1 << 16);
    assert_eq!(results, [Err(Error::Usage), Err(Error::Usage), Ok(())]);
    decodes_to(&xz, &[&d]);
}

/// Random calls, updates and output buffer sizes.
#[test]
fn random_sequences() {
    let d = sample(1 << 20, 5);
    let filter_sets = [
        Vec::new(),
        x86(),
        vec![Filter::delta(2).unwrap()],
        vec![Filter::delta(1).unwrap(), Filter::bcj(BcjKind::Arm)],
    ];
    let seeds = if cfg!(debug_assertions) { 10 } else { 200 };
    for seed in 0..seeds {
        let mut r = Rng(seed * 0x9e37_79b9 + 17);
        let threads = [1, 1, 2, 3][(r.next() % 4) as usize];
        let o = opts(threads, [0, 30_000, 70_000][(r.next() % 3) as usize]);
        let mut steps = Vec::new();
        let mut pos = 0;
        while pos < d.len() && steps.len() < 24 {
            if r.next().is_multiple_of(3) {
                let mut l = lzma(
                    [0, 1, 4, 6, 9][(r.next() % 5) as usize],
                    4096 << (r.next() % 8),
                );
                if r.next().is_multiple_of(2) {
                    l.lc = (r.next() % 5) as u32;
                    l.lp = (r.next() % 5) as u32 % (5 - l.lc);
                    l.pb = (r.next() % 5) as u32;
                }
                steps.push(Step::Update(
                    filter_sets[(r.next() % 4) as usize].clone(),
                    l,
                ));
            } else {
                let n = ((r.next() % 60_000) as usize + 1).min(d.len() - pos);
                let action = match r.next() % 5 {
                    0 if threads == 1 => Action::SyncFlush,
                    1 => Action::FullFlush,
                    2 => Action::FullBarrier,
                    _ => Action::Run,
                };
                steps.push(Step::Code(&d[pos..pos + n], action));
                pos += n;
            }
        }
        steps.push(Step::Code(&[], Action::Finish));
        let out_size = [1, 13, 4096, 1 << 16][(r.next() % 4) as usize];
        let (xz, _, completed) = std::panic::catch_unwind(|| run(&o, &steps, out_size))
            .unwrap_or_else(|_| {
                panic!(
                    "seed {seed} threads {threads} block {} out {out_size}",
                    o.block_size
                )
            });
        if completed {
            decodes_to(&xz, &[&d[..pos]]);
        }
    }
}
