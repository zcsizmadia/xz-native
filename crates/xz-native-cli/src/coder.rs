//! Encoder and decoder settings and the coding loops (xz's `coder.c` and
//! `hardware.c`): thread counts, memory limits, and adjusting the
//! settings to the compression memory limit.

use crate::Ctx;
use crate::args::{Format, Limit, Threads, V_DEBUG, V_WARNING};
use crate::num::{int as group, mib};
use std::cell::RefCell;
use std::io::{self, BufReader, Read, Write};
use xz_native::{
    AutoReader, Check, DecoderOptions, EncoderOptions, LzipReader, LzmaOptions, LzmaReader,
    LzmaWriter, PRESET_EXTREME, RawMethod, RawOptions, XzReader, XzWriter,
};

/// `ASSUME_RAM`: the RAM size xz assumes when it can't find it, in MiB.
const ASSUME_RAM: u64 = 128;

const MIB: u64 = 1 << 20;

/// Memory sizes derived from the RAM (`hardware_init()`).
pub struct Hardware {
    total_ram: u64,
    /// The default budget of the threads: a quarter of the RAM.
    memlimit_mt_default: u64,
    cpus: u32,
}

impl Hardware {
    pub fn detect() -> Self {
        let total_ram = crate::ram::total().unwrap_or(ASSUME_RAM * MIB);
        let mut memlimit_mt_default = total_ram / 4;
        if cfg!(target_pointer_width = "32") {
            memlimit_mt_default = memlimit_mt_default.min(1400 * MIB);
        }
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
        Self {
            total_ram,
            memlimit_mt_default,
            cpus,
        }
    }

    /// A limit option in bytes; 0 (no limit) or unset is None.
    pub fn bytes(&self, l: Option<Limit>) -> Option<u64> {
        match l? {
            Limit::Bytes(0) => None,
            Limit::Bytes(n) => Some(n),
            Limit::Percent(p) => Some((u128::from(p) * u128::from(self.total_ram) / 100) as u64),
        }
    }
}

/// `message_mem_needed()`.
fn mem_needed(usage: u64, limit: Option<u64>) -> String {
    let usage = mib(usage);
    match limit {
        None => format!("{usage} MiB of memory is required. The limiter is disabled."),
        Some(l) if l < MIB => format!(
            "{usage} MiB of memory is required. The limit is {} B.",
            group(l)
        ),
        Some(l) => format!(
            "{usage} MiB of memory is required. The limit is {} MiB.",
            mib(l)
        ),
    }
}

/// The encoder for every file.
pub enum Encoder {
    Xz(EncoderOptions),
    Lzma(LzmaOptions),
}

/// `coder_set_compression_settings()` for the default filter chain:
/// fewer threads, then single-threaded mode, then a smaller dictionary
/// until the memory limit is met.
pub fn encoder(ctx: &mut Ctx, hw: &Hardware) -> Result<Encoder, String> {
    let o = &ctx.o;
    let preset = o.preset | if o.extreme { PRESET_EXTREME } else { 0 };
    let invalid = |e: xz_native::Error| e.to_string();
    let limit = hw.bytes(o.memlimit_compress);
    if o.format == Format::Lzip {
        return Err("Compression of lzip files (.lz) is not supported".into());
    }
    if o.format == Format::Lzma {
        let raw = RawOptions::preset(RawMethod::Lzma1, preset).map_err(invalid)?;
        let lzma = fit_dict(ctx, raw.lzma, limit, '1', |lzma| {
            let mut r = raw.clone();
            r.lzma = lzma;
            r.encoder_memusage()
        })?;
        return Ok(Encoder::Lzma(lzma));
    }

    let mut opts = EncoderOptions::preset(preset).map_err(invalid)?;
    opts.check = o.check.unwrap_or(Check::Crc64);
    opts.block_size = o.block_size;
    let (mut threads, mut mt) = match o.threads {
        Threads::Auto => (hw.cpus, true),
        Threads::Single => (1, false),
        Threads::Mt(n) => (n, true),
    };
    if mt {
        // The default budget applies only to an automatic thread count
        // without --memlimit-compress, and never makes xz fail or change
        // the output (`hardware_memlimit_mtenc_is_default()`).
        let soft = limit.is_none() && o.threads == Threads::Auto;
        let mt_limit = if soft {
            Some(hw.memlimit_mt_default)
        } else {
            limit
        };
        let usage_with = |n: u32| mt_options(&opts, n).encoder_memusage().map_err(invalid);
        let mut usage = usage_with(threads)?;
        while mt_limit.is_some_and(|l| usage > l) && threads > 1 {
            threads -= 1;
            usage = usage_with(threads)?;
        }
        ctx.note(V_DEBUG, format!("Using up to {threads} threads."));
        if mt_limit.is_some_and(|l| usage > l) && !soft {
            if !ctx.o.auto_adjust {
                return Err(too_small(usage, limit));
            }
            // Single-threaded mode needs less memory but gives other
            // output.
            mt = false;
            ctx.note(
                V_WARNING,
                format!(
                    "Switching to single-threaded mode to not exceed the memory usage \
                 limit of {} MiB",
                    mib(mt_limit.unwrap_or_default())
                ),
            );
        }
    }
    if mt {
        return Ok(Encoder::Xz(mt_options(&opts, threads)));
    }
    opts.threads = 1;
    opts.lzma = fit_dict(ctx, opts.lzma, limit, '2', |lzma| {
        let mut o = opts.clone();
        o.lzma = lzma;
        o.encoder_memusage()
    })?;
    Ok(Encoder::Xz(opts))
}

/// Multithreaded options with `n` threads, also for one (`xz -T+1`).
fn mt_options(opts: &EncoderOptions, n: u32) -> EncoderOptions {
    let mut o = opts.clone();
    o.threads = n as usize;
    o.multithreaded = true;
    o
}

fn too_small(usage: u64, limit: Option<u64>) -> String {
    format!(
        "Memory usage limit is too low for the given filter setup.\n{}: {}",
        crate::PROGRAM,
        mem_needed(usage, limit)
    )
}

/// Lowers the dictionary size in MiB steps until `memusage` fits the
/// limit, with a warning (the end of `coder_set_compression_settings()`).
fn fit_dict(
    ctx: &mut Ctx,
    mut lzma: LzmaOptions,
    limit: Option<u64>,
    lzma_num: char,
    memusage: impl Fn(LzmaOptions) -> xz_native::Result<u64>,
) -> Result<LzmaOptions, String> {
    let Some(limit) = limit else {
        return Ok(lzma);
    };
    let usage = memusage(lzma).map_err(|e| e.to_string())?;
    if usage <= limit {
        return Ok(lzma);
    }
    if !ctx.o.auto_adjust {
        return Err(too_small(usage, Some(limit)));
    }
    let orig = lzma.dict_size;
    lzma.dict_size &= !(MIB as u32 - 1);
    // The error shows the usage at the last size tried, as in xz.
    let mut usage = usage;
    loop {
        if lzma.dict_size < MIB as u32 {
            return Err(too_small(usage, Some(limit)));
        }
        usage = memusage(lzma).map_err(|e| e.to_string())?;
        if usage <= limit {
            break;
        }
        lzma.dict_size -= MIB as u32;
    }
    ctx.note(
        V_WARNING,
        format!(
            "Adjusted LZMA{lzma_num} dictionary size from {} MiB to {} MiB to not exceed the \
         memory usage limit of {} MiB",
            group(u64::from(orig >> 20)),
            group(u64::from(lzma.dict_size >> 20)),
            mib(limit)
        ),
    );
    Ok(lzma)
}

/// The decoder options (`coder_init()` for decompression).
pub fn decoder_options(ctx: &Ctx, hw: &Hardware) -> DecoderOptions {
    let o = &ctx.o;
    let mut d = DecoderOptions::new();
    let limit = hw.bytes(o.memlimit_decompress);
    d.memlimit = limit.unwrap_or(u64::MAX);
    d.concatenated = !o.single_stream;
    d.ignore_check = o.ignore_check;
    d.threads = match o.threads {
        Threads::Auto => hw.cpus as usize,
        Threads::Single => 1,
        Threads::Mt(n) => n as usize,
    };
    // `hardware_memlimit_mtdec_get()`.
    let mtdec = hw
        .bytes(o.memlimit_mt_decompress)
        .unwrap_or(hw.memlimit_mt_default);
    d.memlimit_threading = limit.map_or(mtdec, |l| mtdec.min(l));
    d
}

/// The decoder limit for messages: None when disabled.
pub fn decoder_limit(ctx: &Ctx, hw: &Hardware) -> Option<u64> {
    hw.bytes(ctx.o.memlimit_decompress)
}

/// Why coding a file failed.
pub enum Failure {
    /// Reading the input failed.
    Read(io::Error),
    /// Writing the output failed.
    Write(io::Error),
    /// The data or the coder: xz-native's error, and the memory the
    /// decoder needed.
    Coder(xz_native::Error, u64),
}

/// xz's text for liblzma's errors (`message_strm()`).
pub fn describe(e: xz_native::Error) -> &'static str {
    use xz_native::Error as E;
    match e {
        E::Format => "File format not recognized",
        E::Options | E::InvalidOptions => "Unsupported options",
        E::Data | E::CheckMismatch => "Compressed data is corrupt",
        E::MemLimit => "Memory usage limit reached",
        E::Truncated => "Unexpected end of input",
        _ => "Internal error (bug)",
    }
}

fn coder_error(e: &io::Error) -> Option<xz_native::Error> {
    e.get_ref()?.downcast_ref::<xz_native::Error>().copied()
}

const BUF_SIZE: usize = 1 << 17;

/// Compresses `input` into `output`.
pub fn compress(
    enc: &Encoder,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<(), Failure> {
    fn run<W: Write>(
        mut w: W,
        input: &mut dyn Read,
        finish: impl FnOnce(W) -> io::Result<()>,
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; BUF_SIZE];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Failure::Read(e)),
            };
            w.write_all(&buf[..n]).map_err(write_failure)?;
        }
        finish(w).map_err(write_failure)
    }
    match enc {
        Encoder::Xz(opts) => {
            let w =
                XzWriter::with_options(output, opts.clone()).map_err(|e| Failure::Coder(e, 0))?;
            run(w, input, |w| w.finish().map(drop))
        }
        Encoder::Lzma(lzma) => {
            let w = LzmaWriter::with_options(output, *lzma).map_err(|e| Failure::Coder(e, 0))?;
            run(w, input, |w| w.finish().map(drop))
        }
    }
}

fn write_failure(e: io::Error) -> Failure {
    match coder_error(&e) {
        Some(c) => Failure::Coder(c, 0),
        None => Failure::Write(e),
    }
}

/// A decoder of one of the formats, with its memory usage.
trait Decoder: Read {
    fn memusage(&self) -> u64;
}

macro_rules! decoder {
    ($($t:ident),*) => {$(
        impl<R: io::BufRead> Decoder for $t<R> {
            fn memusage(&self) -> u64 {
                $t::memusage(self)
            }
        }
    )*};
}
decoder!(AutoReader, XzReader, LzmaReader, LzipReader);

/// Reads from `inner`, keeping a copy of the bytes while `recorded` holds
/// a buffer, so that unrecognized input can be passed through.
struct Recording<'a, R: ?Sized> {
    inner: &'a mut R,
    recorded: &'a RefCell<Option<Vec<u8>>>,
}

impl<R: Read + ?Sized> Read for Recording<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(r) = self.recorded.borrow_mut().as_mut() {
            r.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}

/// Decompresses `input` into `output`, or only tests it without one.
/// With `passthrough` (`xz -dcf` with `--format=auto`), input that isn't
/// in a known format is copied as is, like xz does.
pub fn decompress(
    format: Format,
    opts: DecoderOptions,
    input: &mut dyn Read,
    mut output: Option<&mut dyn Write>,
    passthrough: bool,
) -> Result<(), Failure> {
    let recorded = RefCell::new(passthrough.then(Vec::new));
    let src = Recording {
        inner: &mut *input,
        recorded: &recorded,
    };
    let r = BufReader::with_capacity(BUF_SIZE, src);
    let mut d: Box<dyn Decoder + '_> = match format {
        Format::Auto => Box::new(AutoReader::with_options(r, opts)),
        Format::Xz => Box::new(XzReader::with_options(r, opts)),
        Format::Lzma => Box::new(LzmaReader::with_options(r, opts)),
        Format::Lzip => Box::new(LzipReader::with_options(r, opts)),
    };
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        let n = match d.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let Some(c) = coder_error(&e) else {
                    return Err(Failure::Read(e));
                };
                let head = recorded.borrow_mut().take();
                if let (xz_native::Error::Format, Some(head), Some(out)) = (c, head, output) {
                    drop(d);
                    return copy_through(&head, input, out);
                }
                return Err(Failure::Coder(c, d.memusage()));
            }
        };
        // Once data comes out, the input is in a known format.
        recorded.borrow_mut().take();
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buf[..n]).map_err(Failure::Write)?;
        }
    }
}

/// Writes `head`, the input read so far, then copies the rest.
fn copy_through(head: &[u8], input: &mut dyn Read, output: &mut dyn Write) -> Result<(), Failure> {
    output.write_all(head).map_err(Failure::Write)?;
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => output.write_all(&buf[..n]).map_err(Failure::Write)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Failure::Read(e)),
        }
    }
}

/// The memory-needed line of a memory limit error.
pub fn memlimit_detail(usage: u64, limit: Option<u64>) -> String {
    mem_needed(usage, limit)
}
