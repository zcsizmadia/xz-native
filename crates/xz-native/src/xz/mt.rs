//! Multithreaded .xz decoding for [`crate::XzReader`] and [`XzDecoder`]
//! (liblzma `stream_decoder_mt.c`).
//!
//! With [`XzDecoder`], the input is what each `decode` call passes; its end
//! pauses parsing until the next call, unless it is the end of the data.
//!
//! The reading thread parses the input in order and checks the Stream
//! Headers, Block Headers, Index, Stream Footers and Stream Padding exactly
//! like [`XzDecoder`]. A Block whose header stores both sizes (multithreaded
//! `xz` and liblzma always write them) is read into memory and decoded by a
//! worker with the one-shot Block decoder. Other Blocks, and Blocks too large
//! for the memory budget, are decoded on the reading thread with the
//! streaming decoder once the Blocks before them have been read, as liblzma
//! does. Output is returned strictly in Block order.
//!
//! Errors are those of the single-threaded reader, at the same place in the
//! output: a Block that fails in a worker, or that the end of the input cuts
//! short, is decoded again with the streaming decoder, and the data before
//! the error is returned before the error. An error found by the reading
//! thread waits in the queue behind the Blocks before it.
//!
//! Memory: at most `2 * threads` Blocks are in flight (read but not yet
//! returned), holding at most `min(memlimit, memlimit_threading)` bytes of
//! compressed input plus decoded output, like liblzma's `memlimit_threading`.
//!
//! [`XzDecoder`]: super::stream::XzDecoder

use super::flat::decode_block;
use super::format::*;
use super::stream::{BlockDecoder, DecoderOptions, IndexDecoder, RecordHash, Status};
use crate::check::Check;
use crate::error::Error;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use std::io::{self, BufRead};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Input and output per decoder call when a failed Block is decoded again.
const REPLAY_STEP: usize = 64 << 10;

/// Resolves [`DecoderOptions::threads`] (0 = one per CPU).
/// Most threads liblzma accepts (`LZMA_THREADS_MAX`).
pub(crate) const THREADS_MAX: usize = 16384;

pub(crate) fn thread_count(threads: usize) -> usize {
    match threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
}

/// A Block for a worker: its data after the header, through the check.
struct Job {
    id: u64,
    hdr: BlockHeader,
    check: Check,
    input: Vec<u8>,
    /// Reused output buffer.
    out: Vec<u8>,
    /// The input ends inside the Block.
    cut: bool,
}

struct Done {
    id: u64,
    input: Vec<u8>,
    /// The Block, or the data decoded before `error`.
    out: Vec<u8>,
    error: Option<Error>,
    panic: Option<Box<dyn Any + Send>>,
}

/// Decodes a Block with the streaming decoder into `out`, which keeps the
/// data decoded before an error.
fn replay(job: &Job, opts: &DecoderOptions, out: &mut Vec<u8>) -> core::result::Result<(), Error> {
    // The first pass finds the decoder call that fails. The second repeats
    // the calls before it and then decodes one byte in and at most one byte
    // out per call, so everything decoded before the error is kept: at least
    // as much as the single-threaded reader returns, whatever its buffers.
    replay_pass(job, opts, out, usize::MAX).or_else(|(_, call)| {
        out.clear();
        replay_pass(job, opts, out, call).map_err(|(e, _)| e)
    })
}

/// One replay pass; byte by byte from call `fine_from`. Returns the error
/// and the call that failed.
fn replay_pass(
    job: &Job,
    opts: &DecoderOptions,
    out: &mut Vec<u8>,
    fine_from: usize,
) -> core::result::Result<(), (Error, usize)> {
    let mut dec = BlockDecoder::new(&job.hdr, job.check, opts).map_err(|e| (e, 0))?;
    let input = &job.input[..];
    let mut ip = 0;
    let mut len = 0;
    let mut call = 0;
    let result = loop {
        let (step, space) = if call < fine_from {
            (REPLAY_STEP, REPLAY_STEP)
        } else {
            (1, 1)
        };
        if out.len() < len + space {
            out.resize(len + space, 0);
        }
        let end = input.len().min(ip + step);
        let (mut u, mut w) = (0, 0);
        let r = dec.decode_partial(&input[ip..end], &mut out[len..len + space], &mut u, &mut w);
        ip += u;
        len += w;
        match r {
            Err(e) => break Err((e, call)),
            Ok(true) => break Ok(()),
            // Out of input: the end of the file for a cut Block, which the
            // streaming decoder reports as truncation. A whole Block can't
            // get here; the guard only rules out a loop that never ends.
            Ok(false) if (ip == input.len() && w < space) || (u == 0 && w == 0) => {
                break Err((
                    if job.cut {
                        Error::Truncated
                    } else {
                        Error::Data
                    },
                    call,
                ));
            }
            Ok(false) => {}
        }
        call += 1;
    };
    out.truncate(len);
    result
}

/// Decodes a Block into `job.out`: one-shot, and on failure again with the
/// streaming decoder for the exact error and the data before it.
fn decode_job(job: &mut Job, opts: &DecoderOptions) -> Option<Error> {
    let mut out = core::mem::take(&mut job.out);
    out.clear();
    let mut error = None;
    if job.cut
        || decode_block(
            &job.input,
            &mut out,
            &job.hdr,
            job.check,
            !opts.ignore_check,
        )
        .is_err()
    {
        out.clear();
        // A cut Block always fails; the fallback only guards that.
        error = match replay(job, opts, &mut out) {
            Ok(()) if job.cut => Some(Error::Truncated),
            Ok(()) => None,
            Err(e) => Some(e),
        };
    }
    job.out = out;
    error
}

fn worker(
    jobs: &Mutex<Receiver<Job>>,
    done: &Sender<Done>,
    stop: &AtomicU64,
    opts: DecoderOptions,
) {
    loop {
        let Ok(rx) = jobs.lock() else { return };
        let Ok(mut job) = rx.recv() else { return };
        drop(rx);
        if job.id >= stop.load(Ordering::Relaxed) {
            continue;
        }
        let (error, panic) = match catch_unwind(AssertUnwindSafe(|| decode_job(&mut job, &opts))) {
            Ok(e) => (e, None),
            Err(p) => (None, Some(p)),
        };
        if error.is_some() {
            // The Blocks after this one won't be needed.
            stop.fetch_min(job.id + 1, Ordering::Relaxed);
        }
        let done_job = Done {
            id: job.id,
            input: job.input,
            out: job.out,
            error,
            panic,
        };
        if done.send(done_job).is_err() {
            return;
        }
    }
}

/// Worker threads, started as Blocks arrive. Dropping it skips the queued
/// Blocks and joins the threads.
struct Pool {
    jobs: Option<Sender<Job>>,
    jobs_rx: Arc<Mutex<Receiver<Job>>>,
    done_tx: Sender<Done>,
    done: Mutex<Receiver<Done>>,
    /// Jobs with this id or higher are skipped.
    stop: Arc<AtomicU64>,
    opts: DecoderOptions,
    handles: Vec<JoinHandle<()>>,
}

impl Pool {
    fn new(opts: DecoderOptions) -> Self {
        let (jobs, jobs_rx) = channel();
        let (done_tx, done) = channel();
        Self {
            jobs: Some(jobs),
            jobs_rx: Arc::new(Mutex::new(jobs_rx)),
            done_tx,
            done: Mutex::new(done),
            stop: Arc::new(AtomicU64::new(u64::MAX)),
            opts,
            handles: Vec::new(),
        }
    }

    /// Starts a worker; false if the OS refuses a new thread.
    fn spawn(&mut self) -> bool {
        let (jobs, done, stop) = (
            self.jobs_rx.clone(),
            self.done_tx.clone(),
            self.stop.clone(),
        );
        let opts = self.opts;
        let spawned = std::thread::Builder::new()
            .name("xz-native-decoder".into())
            .spawn(move || worker(&jobs, &done, &stop, opts));
        match spawned {
            Ok(handle) => {
                self.handles.push(handle);
                true
            }
            Err(_) => false,
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop.store(0, Ordering::Relaxed);
        // Workers exit once the queue is empty and the sender is gone.
        drop(self.jobs.take());
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// Reusable buffers, up to a byte limit.
#[derive(Default)]
struct Buffers {
    free: Vec<Vec<u8>>,
    bytes: u64,
}

impl Buffers {
    fn take(&mut self) -> Vec<u8> {
        let mut v = self.free.pop().unwrap_or_default();
        self.bytes -= v.capacity() as u64;
        v.clear();
        v
    }

    fn give(&mut self, v: Vec<u8>, room: u64) {
        let size = v.capacity() as u64;
        if size > 0 && size <= room {
            self.bytes += size;
            self.free.push(v);
        }
    }
}

/// A reader that counts the bytes consumed.
struct Counting<'a, R> {
    inner: &'a mut R,
    consumed: u64,
}

impl<R: BufRead> io::Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.consumed += n as u64;
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Counting<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        self.inner.consume(n);
        self.consumed += n as u64;
    }
}

/// Output queue entry, in stream order.
enum Item {
    /// A Block given to a worker: `(data, error after it)` once decoded.
    Block {
        id: u64,
        cost: u64,
        data: Option<(Vec<u8>, Option<Error>)>,
        pos: usize,
    },
    /// A Block decoded on the reading thread, from the inner reader.
    Direct(Box<BlockDecoder>),
    Fail(Error),
    /// A Block Header, of this size, over the memory limit: the error, and
    /// once the limit is raised, decoding goes on from that header.
    MemLimit(usize),
    /// A Stream Header with a check type that can't be verified, with
    /// [`DecoderOptions::tell_unsupported_check`](crate::DecoderOptions::tell_unsupported_check).
    UnsupportedCheck,
    End,
}

/// What the parser expects next.
enum Parse {
    StreamHeader,
    /// A Block Header or the Index.
    BlockStart,
    /// A Block Header of this size.
    BlockHeader(usize),
    /// A Block waiting for room, with its cost.
    Held(BlockHeader, u64),
    /// Reading a Block's data.
    BlockData(BlockHeader, Vec<u8>, u64),
    Index(IndexDecoder),
    /// The Stream Footer, after an Index of this size.
    Footer(u64),
    /// Stream Padding bytes so far.
    Padding(u64),
    /// Waiting for a [`Item::Direct`] Block.
    Blocked,
    Done,
}

/// Why parsing stopped.
enum Stop {
    Io(io::Error),
    Xz(Error),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Io(e)
    }
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Stop::Xz(e)
    }
}

/// Reads into `buf[*pos..]`; returns false if the input ends first.
fn fill<R: BufRead>(inner: &mut R, buf: &mut [u8], pos: &mut usize) -> io::Result<bool> {
    while *pos < buf.len() {
        let avail = inner.fill_buf()?;
        if avail.is_empty() {
            return Ok(false);
        }
        let n = avail.len().min(buf.len() - *pos);
        buf[*pos..*pos + n].copy_from_slice(&avail[..n]);
        inner.consume(n);
        *pos += n;
    }
    Ok(true)
}

/// Appends to `buf` until it holds `need` bytes; returns false if the input
/// ends first.
fn fill_vec<R: BufRead>(inner: &mut R, buf: &mut Vec<u8>, need: usize) -> io::Result<bool> {
    while buf.len() < need {
        let avail = inner.fill_buf()?;
        if avail.is_empty() {
            return Ok(false);
        }
        let n = avail.len().min(need - buf.len());
        buf.extend_from_slice(&avail[..n]);
        inner.consume(n);
    }
    Ok(true)
}

/// The input of [`MtDecoder::decode`]: until `finish`, its end is
/// [`io::ErrorKind::WouldBlock`] instead of the end of the data.
struct PushInput<'a> {
    data: &'a [u8],
    pos: usize,
    finish: bool,
}

impl io::Read for PushInput<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for PushInput<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.data.len() && !self.finish {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(&self.data[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

/// The multithreaded engine of [`crate::XzReader`] and [`XzDecoder`] with
/// threads.
///
/// [`XzDecoder`]: super::stream::XzDecoder
pub(crate) struct MtDecoder {
    opts: DecoderOptions,
    threads: usize,
    /// Most Blocks in flight.
    max_blocks: usize,
    /// Most bytes (compressed + uncompressed) of the Blocks in flight.
    budget: u64,
    pool: Option<Pool>,
    queue: VecDeque<Item>,
    in_flight: usize,
    in_flight_bytes: u64,
    /// Jobs given to workers and not yet returned.
    pending: usize,
    next_id: u64,
    inputs: Buffers,
    outputs: Buffers,
    parse: Parse,
    flags: Option<StreamFlags>,
    first_stream: bool,
    records: RecordHash,
    hbuf: Box<[u8; 1024]>,
    hpos: usize,
    failed: Option<Error>,
    /// Memory the last parsed Block needs (`XzReader::memusage`).
    memusage: u64,
    /// Input consumed and output decoded so far, and the input of the
    /// Blocks still with workers (`progress_in`, `progress_out`).
    consumed: u64,
    decoded: u64,
    with_workers: u64,
}

impl fmt::Debug for MtDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MtDecoder")
            .field("threads", &self.threads)
            .field("in_flight", &self.in_flight)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl MtDecoder {
    pub(crate) fn new(opts: DecoderOptions, threads: usize) -> Self {
        Self {
            opts,
            threads,
            max_blocks: threads.saturating_mul(2),
            budget: opts.memlimit.min(opts.memlimit_threading),
            pool: None,
            queue: VecDeque::new(),
            in_flight: 0,
            in_flight_bytes: 0,
            pending: 0,
            next_id: 0,
            inputs: Buffers::default(),
            outputs: Buffers::default(),
            parse: Parse::StreamHeader,
            flags: None,
            first_stream: true,
            records: RecordHash::default(),
            hbuf: Box::new([0; 1024]),
            hpos: 0,
            // Like lzma_stream_decoder_mt, too many threads is an error.
            failed: (threads > THREADS_MAX).then_some(Error::Options),
            memusage: 0,
            consumed: 0,
            decoded: 0,
            with_workers: 0,
        }
    }

    /// Memory the last parsed Block needs; after [`Error::MemLimit`], the
    /// Block that failed (parsing stops there).
    pub(crate) fn memusage(&self) -> u64 {
        self.memusage
    }

    pub(crate) fn memlimit(&self) -> u64 {
        self.opts.memlimit
    }

    /// Changes the memory limit, and with it the budget for Blocks in
    /// flight; it applies from the next Block Header.
    pub(crate) fn set_memlimit(&mut self, memlimit: u64) {
        self.opts.memlimit = memlimit;
        self.budget = memlimit.min(self.opts.memlimit_threading);
    }

    /// Check type of the Stream being parsed.
    pub(crate) fn check(&self) -> Option<Check> {
        self.flags.map(|f| f.check)
    }

    /// [`XzDecoder::decode`] with threads: like [`read`](Self::read) on the
    /// input given so far. Without `finish`, the end of `input` only pauses
    /// parsing; the Blocks already read are still returned, and when none is
    /// ready, the call waits for a worker to finish one.
    ///
    /// [`XzDecoder::decode`]: super::stream::XzDecoder::decode
    pub(crate) fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
        ip: &mut usize,
        op: &mut usize,
    ) -> core::result::Result<Status, Error> {
        let mut src = PushInput {
            data: input,
            pos: 0,
            finish,
        };
        let r = if output.is_empty() {
            self.pump(&mut src).map(|()| 0)
        } else {
            self.read_from(&mut src, output)
        };
        *ip = src.pos;
        self.consumed += src.pos as u64;
        match r {
            Ok(n) => {
                *op = n;
                Ok(if matches!(self.queue.front(), Some(Item::End)) {
                    Status::StreamEnd
                } else {
                    Status::Ok
                })
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Status::Ok),
            // `PushInput` returns no other I/O errors.
            Err(e) => Err(*e
                .get_ref()
                .and_then(|e| e.downcast_ref::<Error>())
                .expect("an xz_native::Error")),
        }
    }

    /// Room in the budget for unused buffers.
    fn room(&self) -> u64 {
        let held = self
            .in_flight_bytes
            .saturating_add(self.inputs.bytes)
            .saturating_add(self.outputs.bytes);
        self.budget.saturating_sub(held)
    }

    /// Parses ahead and hands out Blocks while there is room.
    fn pump<R: BufRead>(&mut self, inner: &mut R) -> io::Result<()> {
        loop {
            match self.step(inner) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(Stop::Io(e)) => return Err(e),
                Err(Stop::Xz(e)) => {
                    self.queue.push_back(Item::Fail(e));
                    self.parse = Parse::Done;
                    return Ok(());
                }
            }
        }
    }

    /// One parsing step; false when parsing can't continue for now. Mirrors
    /// `XzDecoder::decode`. Partial reads are kept, so an I/O error can be
    /// retried.
    fn step<R: BufRead>(&mut self, inner: &mut R) -> core::result::Result<bool, Stop> {
        let check = self.flags.map_or(Check::None, |f| f.check);
        match &mut self.parse {
            Parse::Blocked | Parse::Done => return Ok(false),
            Parse::StreamHeader => {
                if !fill(inner, &mut self.hbuf[..STREAM_HEADER_SIZE], &mut self.hpos)? {
                    return Err(Error::Truncated.into());
                }
                self.hpos = 0;
                let h: &[u8; 12] = self.hbuf[..12].try_into().unwrap();
                let flags = match decode_stream_header(h) {
                    Err(Error::Format) if !self.first_stream => return Err(Error::Data.into()),
                    r => r?,
                };
                if self.opts.fail_on_unsupported_check
                    && matches!(flags.check, Check::Unsupported(_))
                {
                    return Err(Error::Options.into());
                }
                self.flags = Some(flags);
                self.records = RecordHash::default();
                self.parse = Parse::BlockStart;
                if self.opts.tell_unsupported_check && matches!(flags.check, Check::Unsupported(_))
                {
                    self.queue.push_back(Item::UnsupportedCheck);
                }
            }
            Parse::BlockStart => {
                let b = *inner.fill_buf()?.first().ok_or(Error::Truncated)?;
                self.parse = if b == 0 {
                    Parse::Index(IndexDecoder::new())
                } else {
                    Parse::BlockHeader((b as usize + 1) * 4)
                };
            }
            Parse::BlockHeader(size) => {
                let size = *size;
                if !fill(inner, &mut self.hbuf[..size], &mut self.hpos)? {
                    return Err(Error::Truncated.into());
                }
                let hdr = decode_block_header(&self.hbuf[..size], check)?;
                self.memusage = BlockDecoder::mem_usage(&hdr);
                if self.memusage > self.opts.memlimit {
                    // The header stays read (`hpos`), as in XzDecoder.
                    self.queue.push_back(Item::MemLimit(size));
                    self.parse = Parse::Blocked;
                    return Ok(false);
                }
                self.hpos = 0;
                let cost = match (hdr.compressed_size, hdr.uncompressed_size) {
                    (Some(c), Some(u)) => c
                        .next_multiple_of(4)
                        .saturating_add(check.size() as u64)
                        .saturating_add(u),
                    _ => u64::MAX,
                };
                if cost <= self.budget {
                    self.parse = Parse::Held(hdr, cost);
                } else {
                    // Sizes unknown or too large: decode it here once the
                    // Blocks before it are out.
                    let dec = BlockDecoder::new(&hdr, check, &self.opts)?;
                    self.queue.push_back(Item::Direct(Box::new(dec)));
                    self.parse = Parse::Blocked;
                }
            }
            Parse::Held(_, cost) => {
                let fits = self
                    .in_flight_bytes
                    .checked_add(*cost)
                    .is_some_and(|n| n <= self.budget);
                if self.in_flight == 0 && !fits {
                    // `set_memlimit` lowered the budget below the Block: with
                    // nothing in flight it would never fit, so decode it here.
                    let Parse::Held(hdr, _) = core::mem::replace(&mut self.parse, Parse::Blocked)
                    else {
                        unreachable!()
                    };
                    let dec = BlockDecoder::new(&hdr, check, &self.opts)?;
                    self.queue.push_back(Item::Direct(Box::new(dec)));
                    return Ok(true);
                }
                if self.in_flight >= self.max_blocks || !fits {
                    return Ok(false);
                }
                let Parse::Held(hdr, cost) = core::mem::replace(&mut self.parse, Parse::Done)
                else {
                    unreachable!()
                };
                self.in_flight += 1;
                self.in_flight_bytes += cost;
                self.parse = Parse::BlockData(hdr, self.inputs.take(), cost);
            }
            Parse::BlockData(hdr, buf, _) => {
                // Data, padding and check; the cost bounds it.
                let need = hdr.compressed_size.unwrap().next_multiple_of(4) as usize + check.size();
                let complete = fill_vec(inner, buf, need)?;
                let Parse::BlockData(hdr, buf, cost) =
                    core::mem::replace(&mut self.parse, Parse::Done)
                else {
                    unreachable!()
                };
                let unpadded =
                    (hdr.header_size + check.size()) as u64 + hdr.compressed_size.unwrap();
                let uncompressed = hdr.uncompressed_size.unwrap();
                self.dispatch(hdr, check, buf, cost, !complete);
                if !complete {
                    // The Block's error ends the output.
                    return Ok(false);
                }
                // A worker verifies the sizes; the record holds if it succeeds.
                self.records.add(unpadded, uncompressed)?;
                self.parse = Parse::BlockStart;
            }
            Parse::Index(idx) => {
                let avail = inner.fill_buf()?;
                if avail.is_empty() {
                    return Err(Error::Truncated.into());
                }
                let (n, done) = idx.feed(avail, Some(&self.records))?;
                inner.consume(n);
                if done {
                    let size = idx.total_size();
                    self.parse = Parse::Footer(size);
                }
            }
            Parse::Footer(index_size) => {
                let index_size = *index_size;
                if !fill(inner, &mut self.hbuf[..STREAM_HEADER_SIZE], &mut self.hpos)? {
                    return Err(Error::Truncated.into());
                }
                self.hpos = 0;
                let f: &[u8; 12] = self.hbuf[..12].try_into().unwrap();
                let (flags, backward) = decode_stream_footer(f)?;
                if Some(flags) != self.flags || backward != index_size {
                    return Err(Error::Data.into());
                }
                self.first_stream = false;
                if !self.opts.concatenated {
                    self.queue.push_back(Item::End);
                    self.parse = Parse::Done;
                    return Ok(false);
                }
                self.parse = Parse::Padding(0);
            }
            Parse::Padding(n) => {
                let avail = inner.fill_buf()?;
                let end = avail.is_empty();
                let zeros = avail.iter().take_while(|&&b| b == 0).count();
                let more = zeros < avail.len();
                inner.consume(zeros);
                *n += zeros as u64;
                if end || more {
                    if !n.is_multiple_of(4) {
                        return Err(Error::Data.into());
                    }
                    if end {
                        self.queue.push_back(Item::End);
                        self.parse = Parse::Done;
                        return Ok(false);
                    }
                    self.parse = Parse::StreamHeader;
                }
            }
        }
        Ok(true)
    }

    fn dispatch(&mut self, hdr: BlockHeader, check: Check, input: Vec<u8>, cost: u64, cut: bool) {
        // The limit is checked here, at the Block Header, and can change.
        let worker_opts = self.opts.memlimit(u64::MAX);
        let pool = self.pool.get_or_insert_with(|| Pool::new(worker_opts));
        // Start a worker when all the others are busy. If the OS refuses,
        // the workers there are carry on; without any, the Block is decoded
        // on this thread.
        if pool.handles.len() < self.threads.min(self.pending + 1) {
            pool.spawn();
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut job = Job {
            id,
            hdr,
            check,
            input,
            out: self.outputs.take(),
            cut,
        };
        if pool.handles.is_empty() {
            let error = decode_job(&mut job, &self.opts);
            self.decoded += job.out.len() as u64;
            self.queue.push_back(Item::Block {
                id,
                cost,
                data: Some((job.out, error)),
                pos: 0,
            });
            return;
        }
        self.pending += 1;
        self.with_workers += job.input.len() as u64;
        // If the workers are gone, `collect` reports it.
        let _ = pool.jobs.as_ref().unwrap().send(job);
        self.queue.push_back(Item::Block {
            id,
            cost,
            data: None,
            pos: 0,
        });
    }

    /// Stores finished Blocks; with `wait`, waits for at least one.
    fn collect(&mut self, mut wait: bool) {
        let Some(pool) = &self.pool else { return };
        let rx = pool.done.lock().unwrap();
        loop {
            let d = if wait {
                wait = false;
                rx.recv().expect("xz-native decoder threads exited")
            } else {
                match rx.try_recv() {
                    Ok(d) => d,
                    Err(_) => return,
                }
            };
            if let Some(p) = d.panic {
                drop(rx);
                // A bug in the decoder. If the caller catches the panic,
                // later reads fail instead of waiting for the lost Block.
                self.failed = Some(Error::Usage);
                self.parse = Parse::Done;
                self.queue.clear();
                pool.stop.store(0, Ordering::Relaxed);
                resume_unwind(p);
            }
            self.pending -= 1;
            self.with_workers -= d.input.len() as u64;
            self.decoded += d.out.len() as u64;
            let room = self.room();
            self.inputs.give(d.input, room);
            let item = self
                .queue
                .iter_mut()
                .find(|i| matches!(i, Item::Block { id, .. } if *id == d.id));
            match item {
                Some(Item::Block { data, .. }) => *data = Some((d.out, d.error)),
                _ => self.outputs.give(d.out, room),
            }
        }
    }

    /// Ends the output with `e`, after the `written` bytes already copied.
    /// The first error found so far in the queue, by the parser or a
    /// worker, with output before it still waiting.
    fn first_error(&self) -> Option<Error> {
        self.queue.iter().find_map(|item| match item {
            Item::Fail(e) => Some(*e),
            Item::Block {
                data: Some((_, e)), ..
            } => *e,
            _ => None,
        })
    }

    fn fail(&mut self, e: Error, written: usize) -> io::Result<usize> {
        self.failed = Some(e);
        self.parse = Parse::Done;
        self.queue.clear();
        if let Some(pool) = &self.pool {
            pool.stop.store(0, Ordering::Relaxed);
        }
        if written > 0 {
            Ok(written)
        } else {
            Err(e.into())
        }
    }

    /// [`std::io::Read::read`] of the decoded data.
    pub(crate) fn read<R: BufRead>(&mut self, inner: &mut R, out: &mut [u8]) -> io::Result<usize> {
        let mut counted = Counting { inner, consumed: 0 };
        let r = self.read_from(&mut counted, out);
        self.consumed += counted.consumed;
        r
    }

    /// [`crate::XzDecoder::progress`] with threads (`get_progress()`):
    /// the input parsed and the Blocks decoded, by whole Blocks (workers
    /// decode a Block in one go).
    pub(crate) fn progress(&self) -> (u64, u64) {
        let reading = match &self.parse {
            Parse::BlockData(_, buf, _) => buf.len() as u64,
            _ => 0,
        };
        (self.consumed - self.with_workers - reading, self.decoded)
    }

    fn read_from<R: BufRead>(&mut self, inner: &mut R, out: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = self.failed {
            return Err(e.into());
        }
        let mut written = 0;
        while written < out.len() {
            // Without more input for now, the Blocks already read are still
            // returned.
            let paused = match self.pump(inner) {
                Ok(()) => false,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => true,
                Err(e) => return if written > 0 { Ok(written) } else { Err(e) },
            };
            self.collect(false);
            // `LZMA_FAIL_FAST`: an error anywhere ahead ends decoding now.
            if self.opts.fail_fast
                && let Some(e) = self.first_error()
            {
                return self.fail(e, written);
            }
            let Some(front) = self.queue.front_mut() else {
                // Parsing stops only at an item in the queue or for input.
                assert!(paused);
                return if written > 0 {
                    Ok(written)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                };
            };
            match front {
                Item::Block {
                    data: Some((buf, error)),
                    pos,
                    ..
                } => {
                    let n = (buf.len() - *pos).min(out.len() - written);
                    out[written..written + n].copy_from_slice(&buf[*pos..*pos + n]);
                    *pos += n;
                    written += n;
                    if *pos == buf.len() {
                        if let Some(e) = *error {
                            return self.fail(e, written);
                        }
                        let Some(Item::Block {
                            cost,
                            data: Some((buf, _)),
                            ..
                        }) = self.queue.pop_front()
                        else {
                            unreachable!()
                        };
                        self.in_flight -= 1;
                        self.in_flight_bytes -= cost;
                        let room = self.room();
                        self.outputs.give(buf, room);
                    }
                }
                Item::Block { data: None, .. } => {
                    if written > 0 {
                        break;
                    }
                    self.collect(true);
                }
                Item::Direct(dec) => {
                    // As `XzReader` without threads: one call per input chunk.
                    let input = match inner.fill_buf() {
                        Ok(i) => i,
                        Err(_) if written > 0 => break,
                        Err(e) => return Err(e),
                    };
                    let eof = input.is_empty();
                    let space = out.len() - written;
                    let (mut ip, mut op) = (0, 0);
                    let r = dec.decode_partial(input, &mut out[written..], &mut ip, &mut op);
                    inner.consume(ip);
                    written += op;
                    self.decoded += op as u64;
                    match r {
                        Err(e) => return self.fail(e, written),
                        Ok(true) => {
                            let (unpadded, uncompressed) =
                                (dec.unpadded_size(), dec.uncompressed());
                            self.queue.pop_front();
                            self.parse = Parse::BlockStart;
                            if let Err(e) = self.records.add(unpadded, uncompressed) {
                                return self.fail(e, written);
                            }
                        }
                        Ok(false) if eof && op < space => {
                            return self.fail(Error::Truncated, written);
                        }
                        Ok(false) if written > 0 => break,
                        Ok(false) => {}
                    }
                }
                Item::Fail(e) => {
                    let e = *e;
                    return self.fail(e, written);
                }
                Item::MemLimit(size) => {
                    if written > 0 {
                        break;
                    }
                    self.parse = Parse::BlockHeader(*size);
                    self.queue.pop_front();
                    return Err(Error::MemLimit.into());
                }
                Item::UnsupportedCheck => {
                    if written > 0 {
                        break;
                    }
                    self.queue.pop_front();
                    return Err(Error::UnsupportedCheck.into());
                }
                Item::End => break,
            }
        }
        Ok(written)
    }
}
