//! Multithreaded .xz encoding (liblzma `stream_encoder_mt.c`,
//! `block_buffer_encoder.c` and `lzma_mt_block_size()`), std only.
//!
//! The input is cut into Blocks of `block_size` bytes; a flush also ends a
//! Block. Each Block is encoded on its own with a fresh LZMA2 encoder, so
//! Blocks can be encoded in any order on any thread and the output depends
//! only on the options and the flushes. As in liblzma:
//!
//! - The Block Header is sized for the largest possible sizes (the output
//!   bound of a full Block and `block_size`) and then filled with the real
//!   ones; the bytes they leave free are Header Padding.
//! - A Block whose header, LZMA2 data, padding and check don't fit in that
//!   output bound is stored instead: LZMA2 uncompressed chunks, with a header
//!   that names only LZMA2 with a 4 KiB dictionary
//!   (`lzma_block_uncomp_encode()`).
//! - Empty Blocks are never written.
//!
//! Splitting a Block's input between `Run` calls doesn't change the LZMA2
//! output, so a Block is encoded from one slice (one-shot) or in steps that
//! can be cancelled (streaming), with the same bytes as liblzma's workers.
//!
//! One-shot ([`compress`]): workers claim Blocks in order, encode them into
//! reused buffers and append them to the output in order, at most
//! `2 * threads` Blocks ahead of the next one to append. Streaming
//! ([`MtEncoder`]): the calling thread copies input into Blocks and hands
//! full ones to workers; at most `threads` Blocks are being filled or
//! encoded and at most `2 * threads` wait for their output to be copied
//! (liblzma's output queue). Dropping the encoder stops the workers within
//! one step of input and joins them.

use super::encoder::{
    Action, block_header_size, encode_index, encode_stream_footer, encode_stream_header,
    filter_flags, write_block_header,
};
use super::mt::thread_count;
use super::stream::Status;
use crate::check::CheckState;
use crate::error::{Error, Result};
use crate::filter::FILTER_LZMA2;
use crate::lzma::encoder::LZMA2_CHUNK_MAX;
use crate::lzma::lz_encoder::MfAction;
use crate::lzma::lzma2_encoder::{LzEncoder, Lzma2Encoder};
use crate::options::EncoderOptions;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::mt::THREADS_MAX;
/// Largest Block size (`BLOCK_SIZE_MAX`).
const BLOCK_SIZE_MAX: u64 = u64::MAX / THREADS_MAX as u64;
/// Bound of a Block Header plus padding and check (`HEADERS_BOUND`).
const HEADERS_BOUND: u64 = (1 + 1 + 2 * 9 + 3 + 4 + 64 + 3) & !3;
/// Input per encoder call in the streaming workers, between checks for
/// cancellation.
const STEP: usize = 64 << 10;

/// Size of `n` bytes in LZMA2 uncompressed chunks (`lzma2_bound()`).
fn lzma2_bound(n: u64) -> u64 {
    n + n.div_ceil(LZMA2_CHUNK_MAX as u64) * 3 + 1
}

/// Settings shared by all Blocks.
#[derive(Debug)]
pub(crate) struct Params {
    opts: EncoderOptions,
    block_size: usize,
    /// Output bound of a Block (`lzma_block_buffer_bound64(block_size)`):
    /// larger Blocks are stored uncompressed.
    out_size: usize,
    header_size: usize,
    /// Filter Flags of the chain.
    flags: Vec<u8>,
}

impl Params {
    /// `get_options()`: validates the options and works out the sizes.
    pub(crate) fn new(opts: &EncoderOptions) -> Result<Self> {
        opts.validate()?;
        if opts.threads > THREADS_MAX {
            return Err(Error::InvalidOptions);
        }
        // `lzma_mt_block_size()`: only LZMA2 suggests a size.
        let block_size = match opts.block_size {
            0 => (u64::from(opts.lzma.dict_size) * 3).max(1 << 20),
            n => n,
        };
        if block_size > BLOCK_SIZE_MAX {
            return Err(Error::InvalidOptions);
        }
        let out_size = HEADERS_BOUND + ((lzma2_bound(block_size) + 3) & !3);
        let flags = filter_flags(opts);
        let header_size = block_header_size(flags.len(), Some((out_size, block_size)));
        Ok(Self {
            opts: opts.clone(),
            block_size: usize::try_from(block_size).map_err(|_| Error::MemLimit)?,
            out_size: usize::try_from(out_size).map_err(|_| Error::MemLimit)?,
            header_size,
            flags,
        })
    }
}

/// A Block's bytes, `data[..len]`. `data` keeps its length between Blocks,
/// so it is zeroed only when it grows.
#[derive(Debug, Default)]
pub(crate) struct BlockBuf {
    data: Vec<u8>,
    len: usize,
}

impl BlockBuf {
    fn room(&mut self, n: usize) {
        if self.data.len() < n {
            self.data.resize(n, 0);
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

/// Encodes Blocks with a reused LZMA2 encoder (liblzma's `worker_encode()`).
#[derive(Debug, Default)]
pub(crate) struct BlockCoder {
    lz: Option<Box<LzEncoder<Lzma2Encoder>>>,
}

impl BlockCoder {
    /// Encodes `input` (1 to `block_size` bytes) as one Block into `out`.
    /// Returns its Index record `(unpadded, uncompressed)`, or None if
    /// `stop` was set.
    pub(crate) fn encode(
        &mut self,
        p: &Params,
        input: &[u8],
        out: &mut BlockBuf,
        stop: Option<&AtomicBool>,
    ) -> Result<Option<(u64, u64)>> {
        debug_assert!(!input.is_empty() && input.len() <= p.block_size);
        let lz = match &mut self.lz {
            Some(lz) => {
                lz.restart(&p.opts.filters)?;
                lz
            }
            None => self.lz.insert(Box::new(Lzma2Encoder::new_lz(
                &p.opts.lzma,
                &p.opts.filters,
            )?)),
        };
        let n = input.len();
        let hs = p.header_size;
        // Start with room for data that compresses well and grow up to the
        // bound when needed; liblzma allocates the bound for every Block.
        let mut cap = (hs + n / 4 + 4096).max(out.data.len()).min(p.out_size);
        out.room(cap);
        let mut ip = 0;
        let mut op = hs;
        loop {
            let end = if stop.is_some() { n.min(ip + STEP) } else { n };
            let action = if end == n {
                MfAction::Finish
            } else {
                MfAction::Run
            };
            if lz.code(
                &input[..end],
                &mut ip,
                &mut out.data[..cap],
                &mut op,
                action,
            )? {
                break;
            }
            if op == cap {
                if cap == p.out_size {
                    // The output bound is full before the end.
                    return Ok(Some(store(p, input, out)));
                }
                cap = (cap * 2).min(p.out_size);
                out.room(cap);
            } else if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
                return Ok(None);
            }
        }
        let compressed = op - hs;
        let pad = compressed.wrapping_neg() & 3;
        let check_size = p.opts.check.size();
        let total = op + pad + check_size;
        if total > p.out_size {
            return Ok(Some(store(p, input, out)));
        }
        out.room(total);
        out.data[op..op + pad].fill(0);
        write_check(p, input, &mut out.data[op + pad..total]);
        write_block_header(
            &mut out.data[..hs],
            p.opts.filters.len() + 1,
            &p.flags,
            Some((compressed as u64, n as u64)),
        );
        out.len = total;
        Ok(Some(((hs + compressed + check_size) as u64, n as u64)))
    }
}

fn write_check(p: &Params, input: &[u8], out: &mut [u8]) {
    let mut check = CheckState::new(p.opts.check);
    check.update(input);
    let mut buf = [0u8; 64];
    let n = check.finish(&mut buf);
    out.copy_from_slice(&buf[..n]);
}

/// Stores `input` as a Block of LZMA2 uncompressed chunks
/// (`lzma_block_uncomp_encode()`). Returns its Index record.
fn store(p: &Params, input: &[u8], out: &mut BlockBuf) -> (u64, u64) {
    // LZMA2 with a 4 KiB dictionary, whatever the filters were.
    const FLAGS: [u8; 3] = [FILTER_LZMA2 as u8, 1, 0];
    let n = input.len();
    let compressed = lzma2_bound(n as u64) as usize;
    let hs = block_header_size(FLAGS.len(), Some((compressed as u64, n as u64)));
    let pad = compressed.wrapping_neg() & 3;
    let check_size = p.opts.check.size();
    let total = hs + compressed + pad + check_size;
    debug_assert!(total <= p.out_size);
    out.room(total);
    write_block_header(
        &mut out.data[..hs],
        1,
        &FLAGS,
        Some((compressed as u64, n as u64)),
    );
    let mut pos = hs;
    let mut control = 1; // dictionary reset
    for chunk in input.chunks(LZMA2_CHUNK_MAX as usize) {
        let size = chunk.len() - 1;
        out.data[pos..pos + 3].copy_from_slice(&[control, (size >> 8) as u8, size as u8]);
        out.data[pos + 3..pos + 3 + chunk.len()].copy_from_slice(chunk);
        pos += 3 + chunk.len();
        control = 2;
    }
    out.data[pos] = 0;
    pos += 1;
    out.data[pos..pos + pad].fill(0);
    write_check(p, input, &mut out.data[pos + pad..total]);
    out.len = total;
    ((hs + compressed + check_size) as u64, n as u64)
}

/// Multithreaded one-shot compression (`threads != 1`).
pub(crate) fn compress(data: &[u8], opts: &EncoderOptions) -> Result<Vec<u8>> {
    let p = Params::new(opts)?;
    let blocks = data.len().div_ceil(p.block_size);
    let block = |i: usize| &data[i * p.block_size..data.len().min((i + 1) * p.block_size)];
    let mut out = Vec::with_capacity((data.len() / 4).clamp(4096, 1 << 26));
    out.extend_from_slice(&encode_stream_header(opts.check));
    let mut records = Vec::with_capacity(blocks);
    let workers = thread_count(opts.threads).min(blocks);
    if workers <= 1 {
        let mut coder = BlockCoder::default();
        let mut buf = BlockBuf::default();
        for i in 0..blocks {
            let r = coder.encode(&p, block(i), &mut buf, None)?;
            out.extend_from_slice(buf.bytes());
            records.push(r.expect("not stopped"));
        }
    } else {
        let window = 2 * workers;
        let state = Mutex::new(Ordered {
            claimed: 0,
            written: 0,
            ready: (0..window).map(|_| None).collect(),
            free: Vec::new(),
            out,
            records,
            error: None,
            panic: None,
        });
        let wake = Condvar::new();
        let spawned = std::thread::scope(|s| {
            let mut spawned = 0;
            for _ in 0..workers {
                let r = std::thread::Builder::new().spawn_scoped(s, || {
                    let mut coder = BlockCoder::default();
                    let mut st = state.lock().unwrap();
                    loop {
                        if st.claimed == blocks || st.error.is_some() || st.panic.is_some() {
                            break;
                        }
                        if st.claimed >= st.written + window {
                            st = wake.wait(st).unwrap();
                            continue;
                        }
                        let i = st.claimed;
                        st.claimed += 1;
                        let mut buf = st.free.pop().unwrap_or_default();
                        drop(st);
                        let r = catch_unwind(AssertUnwindSafe(|| {
                            coder.encode(&p, block(i), &mut buf, None)
                        }));
                        st = state.lock().unwrap();
                        match r {
                            Ok(Ok(rec)) => st.commit(i, buf, rec.expect("not stopped")),
                            Ok(Err(e)) => st.error = Some(e),
                            Err(panic) => st.panic = Some(panic),
                        }
                        wake.notify_all();
                    }
                });
                // If the OS refuses more threads, the ones there are do the
                // work.
                if r.is_err() {
                    break;
                }
                spawned += 1;
            }
            spawned
        });
        if spawned == 0 {
            return Err(Error::MemLimit);
        }
        let st = state.into_inner().unwrap();
        if let Some(panic) = st.panic {
            resume_unwind(panic);
        }
        if let Some(e) = st.error {
            return Err(e);
        }
        out = st.out;
        records = st.records;
    }
    let index = encode_index(&records);
    out.extend_from_slice(&index);
    out.extend_from_slice(&encode_stream_footer(opts.check, index.len() as u64));
    Ok(out)
}

/// One-shot state: Blocks are appended to `out` in order.
struct Ordered {
    /// Blocks claimed by workers.
    claimed: usize,
    /// Blocks appended to `out`.
    written: usize,
    /// Encoded Blocks waiting for the ones before them, by index modulo
    /// the window.
    ready: Vec<Option<(BlockBuf, (u64, u64))>>,
    free: Vec<BlockBuf>,
    out: Vec<u8>,
    records: Vec<(u64, u64)>,
    error: Option<Error>,
    panic: Option<Box<dyn Any + Send>>,
}

impl Ordered {
    /// Stores Block `i`, then appends every Block that is next in order.
    fn commit(&mut self, i: usize, buf: BlockBuf, rec: (u64, u64)) {
        let window = self.ready.len();
        self.ready[i % window] = Some((buf, rec));
        while let Some((buf, rec)) = self.ready[self.written % window].take() {
            self.out.extend_from_slice(buf.bytes());
            self.records.push(rec);
            self.free.push(buf);
            self.written += 1;
        }
    }
}

/// A Block for a worker.
struct Job {
    id: u64,
    input: Vec<u8>,
    /// Reused output buffer.
    out: BlockBuf,
}

struct Done {
    id: u64,
    input: Vec<u8>,
    out: BlockBuf,
    result: Result<Option<(u64, u64)>>,
    panic: Option<Box<dyn Any + Send>>,
}

fn worker(p: &Params, jobs: &Mutex<Receiver<Job>>, done: &Sender<Done>, stop: &AtomicBool) {
    let mut coder = BlockCoder::default();
    loop {
        let Ok(rx) = jobs.lock() else { return };
        let Ok(mut job) = rx.recv() else { return };
        drop(rx);
        if stop.load(Ordering::Relaxed) {
            continue;
        }
        let (result, panic) = match catch_unwind(AssertUnwindSafe(|| {
            coder.encode(p, &job.input, &mut job.out, Some(stop))
        })) {
            Ok(r) => (r, None),
            Err(panic) => (Ok(None), Some(panic)),
        };
        let d = Done {
            id: job.id,
            input: job.input,
            out: job.out,
            result,
            panic,
        };
        if done.send(d).is_err() {
            return;
        }
    }
}

/// Worker threads, started as Blocks arrive. Dropping it stops the Blocks
/// being encoded, skips the queued ones and joins the threads.
struct Pool {
    jobs: Option<Sender<Job>>,
    jobs_rx: Arc<Mutex<Receiver<Job>>>,
    done_tx: Sender<Done>,
    done: Mutex<Receiver<Done>>,
    stop: Arc<AtomicBool>,
    params: Arc<Params>,
    handles: Vec<JoinHandle<()>>,
}

impl Pool {
    fn new(params: Arc<Params>) -> Self {
        let (jobs, jobs_rx) = channel();
        let (done_tx, done) = channel();
        Self {
            jobs: Some(jobs),
            jobs_rx: Arc::new(Mutex::new(jobs_rx)),
            done_tx,
            done: Mutex::new(done),
            stop: Arc::new(AtomicBool::new(false)),
            params,
            handles: Vec::new(),
        }
    }

    /// Starts a worker; false if the OS refuses a new thread.
    fn spawn(&mut self) -> bool {
        let (p, jobs, done, stop) = (
            self.params.clone(),
            self.jobs_rx.clone(),
            self.done_tx.clone(),
            self.stop.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("xz-native-encoder".into())
            .spawn(move || worker(&p, &jobs, &done, &stop));
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
        self.stop.store(true, Ordering::Relaxed);
        // Workers exit once the queue is empty and the sender is gone.
        drop(self.jobs.take());
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// A Block given to a worker, in stream order.
struct Slot {
    id: u64,
    /// The encoded Block and its Index record.
    done: Option<(BlockBuf, (u64, u64))>,
    /// Bytes already copied to the caller.
    pos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    StreamHeader,
    Blocks,
    Index,
    StreamFooter,
    Done,
}

/// Streaming multithreaded encoder behind [`crate::XzEncoder`]
/// (`stream_encode_mt()`).
pub(crate) struct MtEncoder {
    params: Arc<Params>,
    threads: usize,
    seq: Seq,
    /// Pending Stream Header, Index or Stream Footer bytes.
    buf: Vec<u8>,
    buf_pos: usize,
    /// Input of the Block being filled.
    cur: Vec<u8>,
    queue: VecDeque<Slot>,
    pool: Option<Pool>,
    /// Blocks given to workers and not yet returned.
    busy: usize,
    free_in: Vec<Vec<u8>>,
    free_out: Vec<BlockBuf>,
    records: Vec<(u64, u64)>,
    next_id: u64,
    error: Option<Error>,
}

impl fmt::Debug for MtEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MtEncoder")
            .field("params", &self.params)
            .field("threads", &self.threads)
            .field("seq", &self.seq)
            .field("blocks", &self.queue.len())
            .finish_non_exhaustive()
    }
}

impl MtEncoder {
    pub(crate) fn new(opts: EncoderOptions) -> Result<Self> {
        let params = Arc::new(Params::new(&opts)?);
        Ok(Self {
            params,
            threads: thread_count(opts.threads),
            seq: Seq::StreamHeader,
            buf: encode_stream_header(opts.check).to_vec(),
            buf_pos: 0,
            cur: Vec::new(),
            queue: VecDeque::new(),
            pool: None,
            busy: 0,
            free_in: Vec::new(),
            free_out: Vec::new(),
            records: Vec::new(),
            next_id: 0,
            error: None,
        })
    }

    /// [`crate::XzEncoder::encode`]. An error ends the stream: the workers
    /// stop and later calls return the same error.
    pub(crate) fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if action == Action::SyncFlush {
            // liblzma's multithreaded encoder doesn't support it.
            return Err(Error::Usage);
        }
        let r = self.run(input, output, action);
        if let Err(e) = r {
            self.error = Some(e);
            self.queue.clear();
            if let Some(pool) = &self.pool {
                pool.stop.store(true, Ordering::Relaxed);
            }
        }
        r
    }

    fn run(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        let mut ip = 0;
        let mut op = 0;
        loop {
            match self.seq {
                Seq::StreamHeader | Seq::Index | Seq::StreamFooter => {
                    let n = (self.buf.len() - self.buf_pos).min(output.len() - op);
                    output[op..op + n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
                    op += n;
                    self.buf_pos += n;
                    if self.buf_pos < self.buf.len() {
                        return Ok((ip, op, Status::Ok));
                    }
                    self.buf.clear();
                    self.buf_pos = 0;
                    self.seq = match self.seq {
                        Seq::StreamHeader => Seq::Blocks,
                        Seq::Index => {
                            let index_size = encode_index(&self.records).len() as u64;
                            self.buf.extend_from_slice(&encode_stream_footer(
                                self.params.opts.check,
                                index_size,
                            ));
                            Seq::StreamFooter
                        }
                        _ => Seq::Done,
                    };
                }
                Seq::Blocks => {
                    self.collect(false)?;
                    self.copy_out(output, &mut op);
                    self.take_input(input, &mut ip, action)?;
                    if ip == input.len() && (action == Action::Run || self.cur.is_empty()) {
                        match action {
                            Action::Run => return Ok((ip, op, Status::Ok)),
                            Action::FullBarrier => return Ok((ip, op, Status::StreamEnd)),
                            _ if !self.queue.is_empty() => {}
                            Action::FullFlush => return Ok((ip, op, Status::StreamEnd)),
                            _ => {
                                self.buf = encode_index(&self.records);
                                self.seq = Seq::Index;
                                continue;
                            }
                        }
                    }
                    if op == output.len() {
                        return Ok((ip, op, Status::Ok));
                    }
                    // Input is waiting for a worker or the flush for the
                    // Blocks before it.
                    debug_assert!(self.busy > 0);
                    self.collect(true)?;
                }
                Seq::Done => return Ok((ip, op, Status::StreamEnd)),
            }
        }
    }

    /// Copies input into Blocks while a worker and room in the queue are
    /// free; a full Block, or the last one before a flush, goes to a worker.
    fn take_input(&mut self, input: &[u8], ip: &mut usize, action: Action) -> Result<()> {
        let bs = self.params.block_size;
        while self.busy < self.threads && self.queue.len() < 2 * self.threads {
            if *ip == input.len() {
                if action != Action::Run && !self.cur.is_empty() {
                    self.dispatch()?;
                }
                return Ok(());
            }
            if self.cur.capacity() == 0 {
                self.cur = self.free_in.pop().unwrap_or_default();
            }
            let n = (bs - self.cur.len()).min(input.len() - *ip);
            self.cur.extend_from_slice(&input[*ip..*ip + n]);
            *ip += n;
            if self.cur.len() == bs {
                self.dispatch()?;
            }
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<()> {
        let pool = self
            .pool
            .get_or_insert_with(|| Pool::new(self.params.clone()));
        // Start a worker when all the others are busy. If the OS refuses,
        // the workers there are carry on; without any, it is liblzma's
        // LZMA_MEM_ERROR.
        if pool.handles.len() <= self.busy && !pool.spawn() && pool.handles.is_empty() {
            return Err(Error::MemLimit);
        }
        self.busy += 1;
        let id = self.next_id;
        self.next_id += 1;
        let job = Job {
            id,
            input: core::mem::take(&mut self.cur),
            out: self.free_out.pop().unwrap_or_default(),
        };
        // If the workers are gone, `collect` reports it.
        let _ = pool.jobs.as_ref().unwrap().send(job);
        self.queue.push_back(Slot {
            id,
            done: None,
            pos: 0,
        });
        Ok(())
    }

    /// Stores encoded Blocks; with `wait`, waits for at least one.
    fn collect(&mut self, mut wait: bool) -> Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let rx = pool.done.lock().unwrap();
        loop {
            let d = if wait {
                wait = false;
                rx.recv().expect("xz-native encoder threads exited")
            } else {
                match rx.try_recv() {
                    Ok(d) => d,
                    Err(_) => return Ok(()),
                }
            };
            if let Some(panic) = d.panic {
                drop(rx);
                // A bug in the encoder. If the caller catches the panic,
                // later calls fail instead of waiting for the lost Block.
                self.error = Some(Error::Usage);
                self.queue.clear();
                pool.stop.store(true, Ordering::Relaxed);
                resume_unwind(panic);
            }
            self.busy -= 1;
            let mut input = d.input;
            input.clear();
            self.free_in.push(input);
            let rec = d.result?.ok_or(Error::Data)?;
            if let Some(slot) = self.queue.iter_mut().find(|s| s.id == d.id) {
                slot.done = Some((d.out, rec));
            }
        }
    }

    /// Copies encoded Blocks to `output` in order.
    fn copy_out(&mut self, output: &mut [u8], op: &mut usize) {
        while let Some(Slot {
            done: Some((buf, rec)),
            pos,
            ..
        }) = self.queue.front_mut()
        {
            let src = &buf.bytes()[*pos..];
            let n = src.len().min(output.len() - *op);
            output[*op..*op + n].copy_from_slice(&src[..n]);
            *op += n;
            *pos += n;
            if *pos < buf.len {
                return;
            }
            self.records.push(*rec);
            let slot = self.queue.pop_front().unwrap();
            self.free_out.push(slot.done.unwrap().0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample(n: usize) -> Vec<u8> {
        let mut x = 7u32;
        (0..n)
            .map(|i| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                if i % 4 == 0 {
                    (x >> 24) as u8
                } else {
                    b"xz-native "[i % 10]
                }
            })
            .collect()
    }

    #[test]
    fn sizes() {
        assert_eq!(lzma2_bound(0), 1);
        assert_eq!(lzma2_bound(1), 5);
        assert_eq!(lzma2_bound(65536), 65540);
        assert_eq!(lzma2_bound(65537), 65544);
        let o = EncoderOptions {
            threads: 2,
            ..EncoderOptions::preset(6).unwrap()
        };
        let p = Params::new(&o).unwrap();
        assert_eq!(p.block_size, 24 << 20);
        let o = EncoderOptions {
            threads: 2,
            ..EncoderOptions::preset(0).unwrap()
        };
        assert_eq!(Params::new(&o).unwrap().block_size, 1 << 20);
        let big = EncoderOptions {
            block_size: BLOCK_SIZE_MAX + 1,
            ..o.clone()
        };
        assert_eq!(Params::new(&big).unwrap_err(), Error::InvalidOptions);
        let many = EncoderOptions {
            threads: THREADS_MAX + 1,
            ..o
        };
        assert_eq!(Params::new(&many).unwrap_err(), Error::InvalidOptions);
    }

    #[test]
    fn roundtrip_and_thread_independence() {
        // Several Blocks per thread count either way.
        let (n, block_size) = if cfg!(miri) {
            (12_000, 4_000)
        } else {
            (300_000, 40_000)
        };
        let data = sample(n);
        for preset in [0, 6] {
            let mut want = None;
            for threads in [2, 3, 8] {
                let mut o = EncoderOptions {
                    threads,
                    block_size,
                    ..EncoderOptions::preset(preset).unwrap()
                };
                if cfg!(miri) {
                    o.lzma.dict_size = 1 << 16;
                }
                let c = compress(&data, &o).unwrap();
                assert_eq!(crate::decompress(&c).unwrap(), data);
                assert_eq!(*want.get_or_insert_with(|| c.clone()), c);
            }
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "32 MiB input; incompressible_chunks_are_stored covers stored chunks"
    )]
    fn stored_block() {
        // LZMA2 stores random data in chunks of about 61 KiB while the bound
        // allows for 64 KiB chunks, so a large random Block doesn't fit.
        let mut x = 1u64;
        let data: Vec<u8> = (0..32 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let o = EncoderOptions {
            threads: 2,
            block_size: 32 << 20,
            ..EncoderOptions::preset(0).unwrap()
        };
        let c = compress(&data, &o).unwrap();
        // A 20-byte header with both sizes and LZMA2 with a 4 KiB
        // dictionary, then uncompressed chunks.
        assert_eq!(&c[12..14], &[4, 0xC0]);
        assert_eq!(&c[12 + 10..12 + 13], &[0x21, 1, 0]);
        assert_eq!(c[12 + 20], 1);
        let compressed = lzma2_bound(data.len() as u64) as usize;
        assert_eq!(c.len(), 12 + 20 + compressed + 3 + 8 + 16 + 12);
        assert_eq!(crate::decompress(&c).unwrap(), data);
        let mut enc = MtEncoder::new(o).unwrap();
        let mut out = vec![0u8; c.len()];
        let (u, w, s) = enc.encode(&data, &mut out, Action::Finish).unwrap();
        assert_eq!((u, w, s), (data.len(), c.len(), Status::StreamEnd));
        assert_eq!(out, c);
    }
}
