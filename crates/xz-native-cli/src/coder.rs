//! Encoder and decoder settings and the coding loops (xz's `coder.c` and
//! `hardware.c`): thread counts, memory limits, and adjusting the
//! settings to the compression memory limit.

use crate::Ctx;
use crate::args::{Format, Limit, Threads, V_DEBUG, V_WARNING};
use crate::chain::Item;
use crate::num::{int as group, mib};
use crate::progress::Meter;
use std::cell::{Cell, RefCell};
use std::io::{self, BufReader, Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};
use xz_native::{
    Action, AutoReader, Check, DecoderOptions, EncoderOptions, Filter, FilterOptions,
    FilterStringFormat, LzipReader, LzmaOptions, LzmaReader, LzmaWriter, PRESET_EXTREME,
    RawDecoder, RawEncoder, RawMethod, RawOptions, Status, XzEncoder, XzReader, XzWriter,
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
    /// `--info-memory` (`hardware_memlimit_show()`): the RAM, the CPU threads
    /// and the memory limits, or with `--robot` the same as numbers.
    pub fn info_memory(&self, o: &crate::args::Opts) -> String {
        let compress = self.bytes(o.memlimit_compress).unwrap_or(0);
        let decompress = self.bytes(o.memlimit_decompress).unwrap_or(0);
        // `hardware_memlimit_mtdec_get()`.
        let mut mtdec = self
            .bytes(o.memlimit_mt_decompress)
            .unwrap_or(self.memlimit_mt_default);
        if decompress != 0 {
            mtdec = mtdec.min(decompress);
        }
        if o.robot {
            return format!(
                "{}	{compress}	{decompress}	{mtdec}	{}	{}
",
                self.total_ram, self.memlimit_mt_default, self.cpus
            );
        }
        // `memlimit_show()`.
        let show = |label: &str, v: u64| {
            let v = if v == 0 || v == u64::MAX {
                "Disabled".to_owned()
            } else {
                format!("{} MiB ({} B)", mib(v), group(v))
            };
            format!(
                "  {label:<32}  {v}
"
            )
        };
        let mut s = String::from(
            "Hardware information:
",
        );
        s += &show("Amount of physical memory (RAM):", self.total_ram);
        s += &format!(
            "  {:<32}  {}

",
            "Number of processor threads:", self.cpus
        );
        s += "Memory usage limits:
";
        s += &show("Compression:", compress);
        s += &show("Decompression:", decompress);
        s += &show("Multi-threaded decompression:", mtdec);
        s += &show("Default for -T0:", self.memlimit_mt_default);
        s
    }

    pub fn detect() -> Self {
        let total_ram = crate::ram::total().unwrap_or(ASSUME_RAM * MIB);
        let mut memlimit_mt_default = total_ram / 4;
        // Below the resource limits, with a margin for xz itself; at least
        // 1, as 0 means no limit.
        if let Some(limit) = crate::ram::rlimit() {
            memlimit_mt_default = memlimit_mt_default.min(limit.saturating_sub(64 * MIB).max(1));
        }
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
    Xz(Box<XzPlan>),
    Lzma(LzmaOptions),
    Raw(RawOptions),
}

/// A filter chain for .xz: the filters before LZMA2, and LZMA2.
pub type XzChain = (Vec<Filter>, LzmaOptions);

/// .xz settings: the encoder options with the first Block's chain, and
/// with `--block-list`, the Blocks and the chains they use.
pub struct XzPlan {
    pub opts: EncoderOptions,
    pub chains: [Option<XzChain>; 10],
    pub list: Option<Vec<(usize, u64)>>,
    /// Single-threaded `--block-size` with `--block-list`: Blocks longer
    /// than this are split (xz's `split_block()`).
    pub split: u64,
}

/// The chain for `--format=raw`, compressing or decompressing: the custom
/// chain (LZMA1 or LZMA2 last), or the preset with xz's warning. Raw
/// settings are never adjusted to the memory limit.
pub fn raw_options(ctx: &Ctx, hw: &Hardware) -> Result<RawOptions, String> {
    let o = &ctx.o;
    let unsupported = || "Unsupported filter chain or filter options".to_owned();
    let raw = match o.chain.split_last() {
        None => {
            ctx.note(V_WARNING, "Using a preset in raw mode is discouraged.");
            ctx.note(
                V_WARNING,
                "The exact options of the presets may vary between software versions.",
            );
            let preset = o.preset | if o.extreme { PRESET_EXTREME } else { 0 };
            RawOptions::preset(RawMethod::Lzma2, preset).map_err(|e| e.to_string())?
        }
        Some((last, first)) => {
            let (method, lzma) = match last {
                Item::Lzma1(l) => (RawMethod::Lzma1, l),
                Item::Lzma2(l) => (RawMethod::Lzma2, l),
                _ => return Err(unsupported()),
            };
            let mut r = RawOptions::preset(method, 6).map_err(|e| e.to_string())?;
            r.lzma = *lzma;
            r.filters = first
                .iter()
                .map(|i| match *i {
                    Item::Bcj(kind, start_offset) => Ok(Filter::Bcj { kind, start_offset }),
                    Item::Delta(distance) => Ok(Filter::Delta { distance }),
                    _ => Err(unsupported()),
                })
                .collect::<Result<_, _>>()?;
            r
        }
    };
    let (usage, limit) = if o.mode == crate::args::Mode::Compress {
        let u = raw.encoder_memusage().map_err(|_| unsupported())?;
        (u, hw.bytes(o.memlimit_compress))
    } else {
        (raw.decoder_memusage(), hw.bytes(o.memlimit_decompress))
    };
    ctx.note(V_DEBUG, format!("Filter chain: {}", chain_string(&raw)));
    let compress = o.mode == crate::args::Mode::Compress;
    debug_memory(ctx, usage, limit, compress.then(|| raw.decoder_memusage()));
    if limit.is_some_and(|l| usage > l) {
        return Err(too_small(usage, limit));
    }
    Ok(raw)
}

/// A filter chain as xz's `-vv` shows it (`lzma_str_from_filters()` with
/// `LZMA_STR_ENCODER | LZMA_STR_GETOPT_LONG`).
fn chain_string(raw: &RawOptions) -> String {
    raw.to_filter_string(FilterStringFormat::new(FilterOptions::Encoder).getopt_long(true))
}

/// The `-vv` lines about the settings: the filter chain, then the memory
/// needed (`message_filters_show()`, `message_mem_needed()`).
fn debug_memory(ctx: &Ctx, usage: u64, limit: Option<u64>, decoder: Option<u64>) {
    ctx.note(V_DEBUG, mem_needed(usage, limit));
    if let Some(d) = decoder {
        ctx.note(
            V_DEBUG,
            format!("Decompression will need {} MiB of memory.", mib(d)),
        );
    }
}

/// A custom chain for .xz (`lzma_validate_chain()`: Delta and BCJ
/// filters, then LZMA2).
fn xz_chain(items: &[Item]) -> Result<XzChain, String> {
    let unsupported = || "Unsupported filter chain or filter options".to_owned();
    let (last, first) = items.split_last().ok_or_else(unsupported)?;
    let Item::Lzma2(lzma) = last else {
        return Err(unsupported());
    };
    let filters = first
        .iter()
        .map(|i| match *i {
            Item::Bcj(kind, start_offset) => Ok(Filter::Bcj { kind, start_offset }),
            Item::Delta(distance) => Ok(Filter::Delta { distance }),
            _ => Err(unsupported()),
        })
        .collect::<Result<_, _>>()?;
    Ok((filters, *lzma))
}

pub fn with_chain(opts: &EncoderOptions, chain: &XzChain) -> EncoderOptions {
    let mut o = opts.clone();
    o.filters = chain.0.clone();
    o.lzma = chain.1;
    o
}

/// `--flush-timeout` needs filters that can sync-flush (LZMA2 and Delta) and
/// one thread, as in xz. Chains that are invalid for other reasons are left
/// to [`encoder`], so that its error comes first, as in xz.
fn flush_timeout_check(ctx: &mut Ctx) -> Result<(), String> {
    let o = &ctx.o;
    let ok = |items: &[Item]| {
        items
            .iter()
            .all(|i| matches!(i, Item::Lzma2(_) | Item::Delta(_)))
    };
    let bad = match o.format {
        // .lzma is always LZMA1.
        Format::Lzma => matches!(o.chain.as_slice(), [] | [Item::Lzma1(_)]).then_some(0),
        Format::Raw => (!ok(&o.chain)).then_some(0),
        _ if o.chain.iter().any(|i| matches!(i, Item::Lzma1(_))) => None,
        _ => {
            let used: Vec<usize> = match &o.block_list {
                Some(list) => list.iter().map(|e| e.0).collect(),
                None => vec![0],
            };
            let mut used = used;
            used.sort_unstable();
            used.into_iter().find(|&n| {
                let items = if n == 0 {
                    &o.chain[..]
                } else {
                    o.block_chains[n].as_deref().unwrap_or_default()
                };
                !ok(items)
            })
        }
    };
    if let Some(n) = bad {
        return Err(format!(
            "Filter chain {n} is incompatible with --flush-timeout"
        ));
    }
    if o.threads != Threads::Single {
        ctx.note(
            V_WARNING,
            "Switching to single-threaded mode due to --flush-timeout",
        );
        ctx.o.threads = Threads::Single;
    }
    Ok(())
}

/// `coder_set_compression_settings()`: the preset or the custom chains,
/// then fewer threads, single-threaded mode and a smaller dictionary until
/// the memory limit is met.
pub fn encoder(ctx: &mut Ctx, hw: &Hardware) -> Result<Encoder, String> {
    if ctx.o.flush_timeout != 0 {
        flush_timeout_check(ctx)?;
    }
    let o = &ctx.o;
    let preset = o.preset | if o.extreme { PRESET_EXTREME } else { 0 };
    let invalid = |e: xz_native::Error| e.to_string();
    let limit = hw.bytes(o.memlimit_compress);
    if o.format == Format::Lzip {
        return Err("Compression of lzip files (.lz) is not supported".into());
    }
    if o.format == Format::Raw {
        return raw_options(ctx, hw).map(Encoder::Raw);
    }
    let unsupported = || "Unsupported filter chain or filter options".to_owned();
    if o.format == Format::Lzma {
        let mut raw = RawOptions::preset(RawMethod::Lzma1, preset).map_err(invalid)?;
        match o.chain.as_slice() {
            [] => {}
            [Item::Lzma1(l)] => raw.lzma = *l,
            _ => return Err("The .lzma format supports only the LZMA1 filter".into()),
        }
        let usage = raw.encoder_memusage().map_err(|_| unsupported())?;
        ctx.note(V_DEBUG, format!("Filter chain: {}", chain_string(&raw)));
        debug_memory(ctx, usage, limit, Some(raw.decoder_memusage()));
        let lzma = fit_dict(ctx, raw.lzma, limit, "LZMA1 dictionary size", |lzma| {
            let mut r = raw.clone();
            r.lzma = lzma;
            r.encoder_memusage()
        })?;
        return Ok(Encoder::Lzma(lzma));
    }

    // The chains in use: chain 0 (the preset or the custom chain), or
    // those `--block-list` names (`chains_used_mask`).
    let used: Vec<usize> = match &o.block_list {
        Some(list) => {
            let mut u: Vec<usize> = list.iter().map(|e| e.0).collect();
            u.sort_unstable();
            u.dedup();
            if let Some(&n) = u.iter().find(|&&n| n != 0 && o.block_chains[n].is_none()) {
                return Err(format!(
                    "filter chain {n} used by --block-list but not specified with --filters{n}="
                ));
            }
            u
        }
        None => vec![0],
    };
    let mut chains: [Option<XzChain>; 10] = Default::default();
    for &n in &used {
        chains[n] = Some(match &o.block_chains[n] {
            Some(items) if n != 0 => xz_chain(items)?,
            _ if o.chain.is_empty() => {
                let l = LzmaOptions::preset(preset).map_err(invalid)?;
                (Vec::new(), l)
            }
            _ => {
                if o.chain.iter().any(|i| matches!(i, Item::Lzma1(_))) {
                    return Err("LZMA1 cannot be used with the .xz format".into());
                }
                xz_chain(&o.chain)?
            }
        });
    }
    let mut opts = EncoderOptions::preset(preset).map_err(invalid)?;
    opts.check = o.check.unwrap_or(Check::Crc64);
    opts.block_size = o.block_size;
    for c in chains.iter().flatten() {
        with_chain(&opts, c)
            .encoder_memusage()
            .map_err(|_| unsupported())?;
    }
    if let Some(c) = &chains[0] {
        ctx.note(
            V_DEBUG,
            format!(
                "Filter chain: {}",
                chain_string(
                    &RawOptions::default()
                        .method(RawMethod::Lzma2)
                        .lzma(c.1)
                        .filters(c.0.clone())
                )
            ),
        );
    }
    // What decoding needs: the largest chain.
    let decoder = chains
        .iter()
        .flatten()
        .map(|c| {
            let mut r = RawOptions::preset(RawMethod::Lzma2, 6).expect("preset 6");
            r.lzma = c.1;
            r.filters = c.0.clone();
            r.decoder_memusage()
        })
        .max()
        .unwrap_or(0);
    let first = o.block_list.as_ref().map_or(0, |l| l[0].0);
    let list = o.block_list.clone();
    let (mut threads, mut mt) = match o.threads {
        Threads::Auto => (hw.cpus, true),
        Threads::Single => (1, false),
        Threads::Mt(n) => (n, true),
    };
    if mt
        && opts.block_size == 0
        && let Some(list) = &list
    {
        // `lzma_mt_block_size()` of the chains, but no larger than the
        // largest Block of the list.
        let size = chains
            .iter()
            .flatten()
            .map(|c| (u64::from(c.1.dict_size) * 3).max(1 << 20))
            .max()
            .unwrap_or(1 << 20);
        let largest = list.iter().map(|e| e.1).max().unwrap_or(0);
        opts.block_size = size.min(largest);
    }
    // The memory of the largest chain (`get_chains_memusage()`).
    let usage_of = |opts: &EncoderOptions| {
        chains
            .iter()
            .flatten()
            .map(|c| with_chain(opts, c).encoder_memusage())
            .try_fold(0, |m, u| u.map(|u| m.max(u)))
            .map_err(invalid)
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
        let mut usage = usage_of(&mt_options(&opts, threads))?;
        ctx.note(V_DEBUG, format!("Using up to {threads} threads."));
        debug_memory(ctx, usage, limit, Some(decoder));
        while mt_limit.is_some_and(|l| usage > l) && threads > 1 {
            threads -= 1;
            usage = usage_of(&mt_options(&opts, threads))?;
        }
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
    let mut split = 0;
    if mt {
        opts = mt_options(&opts, threads);
    } else {
        opts.threads = 1;
        if o.threads == Threads::Single {
            // Shown once: after a switch from MT, xz doesn't repeat it.
            debug_memory(ctx, usage_of(&opts)?, limit, Some(decoder));
        }
        if list.is_some() {
            // With a Block list, the Blocks are cut here, as xz does.
            split = opts.block_size;
            opts.block_size = 0;
        }
        // Every chain over the limit gets a smaller dictionary.
        if limit.is_some_and(|l| usage_of(&opts).is_ok_and(|u| u > l)) {
            for (n, c) in chains.iter_mut().enumerate() {
                let Some(c) = c else { continue };
                let label = if n == 0 {
                    "LZMA2 dictionary size".to_owned()
                } else {
                    format!("LZMA2 dictionary size for --filters{n}")
                };
                let base = opts.clone();
                let filters = c.0.clone();
                c.1 = fit_dict(ctx, c.1, limit, &label, |lzma| {
                    with_chain(&base, &(filters.clone(), lzma)).encoder_memusage()
                })?;
            }
        }
    }
    let first_chain = chains[first].clone().expect("a used chain");
    Ok(Encoder::Xz(Box::new(XzPlan {
        opts: with_chain(&opts, &first_chain),
        chains,
        list,
        split,
    })))
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
    label: &str,
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
            "Adjusted {label} from {} MiB to {} MiB to not exceed the memory usage \
             limit of {} MiB",
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
    /// A signal asked to stop (`user_abort`).
    Aborted,
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
    meter: &mut Meter,
) -> Result<(), Failure> {
    /// Writes `input` to `w` and finishes it, reporting `progress(w)`.
    fn run<W: Write>(
        w: &mut W,
        input: &mut dyn Read,
        meter: &mut Meter,
        progress: impl Fn(&W) -> (u64, u64),
        finish: impl FnOnce(&mut W) -> io::Result<()>,
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; BUF_SIZE];
        loop {
            if crate::signals::aborted() {
                return Err(Failure::Aborted);
            }
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Failure::Read(e)),
            };
            w.write_all(&buf[..n]).map_err(write_failure)?;
            meter.tick(progress(w));
        }
        finish(w).map_err(write_failure)?;
        meter.tick(progress(w));
        Ok(())
    }
    match enc {
        Encoder::Xz(plan) if plan.list.is_some() => compress_blocks(plan, input, output, meter),
        Encoder::Xz(plan) => {
            let mut w = XzWriter::with_options(output, plan.opts.clone())
                .map_err(|e| Failure::Coder(e, 0))?;
            run(
                &mut w,
                input,
                meter,
                XzWriter::progress,
                XzWriter::try_finish,
            )
        }
        Encoder::Raw(opts) => compress_raw(opts, input, output, meter),
        Encoder::Lzma(lzma) => {
            let mut w =
                LzmaWriter::with_options(output, *lzma).map_err(|e| Failure::Coder(e, 0))?;
            let progress = |w: &LzmaWriter<_>| (w.total_in(), w.total_out());
            run(&mut w, input, meter, progress, LzmaWriter::try_finish)
        }
    }
}

/// Reads standard input on a thread, so that waiting for it can time out
/// (xz's non-blocking `io_read()` with `io_wait()`). The channel ends at
/// the end of the input.
fn stdin_chunks() -> Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = sync_channel(4);
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        loop {
            let mut buf = vec![0u8; BUF_SIZE];
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Ok(buf)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    rx
}

/// Compresses standard input with `--flush-timeout` (xz's `coder_normal()`
/// with `flush_needed`): when no input arrives for `timeout_ms` after
/// some has, the encoder sync-flushes and the output is written out, so
/// that what came so far can be decompressed.
pub fn compress_flushing(
    enc: &Encoder,
    timeout_ms: u64,
    output: &mut dyn Write,
    meter: &mut Meter,
) -> Result<(), Failure> {
    enum Push {
        Xz(XzEncoder),
        Raw(RawEncoder),
    }
    let coder = |e| Failure::Coder(e, 0);
    let mut push = match enc {
        Encoder::Xz(plan) => Push::Xz(XzEncoder::with_options(plan.opts.clone()).map_err(coder)?),
        Encoder::Raw(opts) => Push::Raw(RawEncoder::with_options(opts).map_err(coder)?),
        Encoder::Lzma(_) => unreachable!("--flush-timeout rejects .lzma"),
    };
    let mut outbuf = vec![0u8; BUF_SIZE];
    let mut totals = (0u64, 0u64);
    // Encodes `input` with `action`: Run until it is used, the others until
    // the end of the flush or the stream.
    let mut code = |input: &[u8], action: Action, output: &mut dyn Write| {
        let mut pos = 0;
        loop {
            let (used, written, status) = match &mut push {
                Push::Xz(e) => e.encode(&input[pos..], &mut outbuf, action),
                Push::Raw(e) => e.encode(&input[pos..], &mut outbuf, action),
            }
            .map_err(coder)?;
            pos += used;
            totals.0 += used as u64;
            totals.1 += written as u64;
            output
                .write_all(&outbuf[..written])
                .map_err(Failure::Write)?;
            meter.tick(totals);
            let done = match action {
                Action::Run => pos == input.len() && written < outbuf.len(),
                _ => status == Status::StreamEnd,
            };
            if done {
                return Ok(());
            }
        }
    };
    let chunks = stdin_chunks();
    let timeout = Duration::from_millis(timeout_ms);
    // When to flush: the timeout after the first input since the last
    // flush (`mytime_set_flush_time()`).
    let mut deadline: Option<Instant> = None;
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        // Wake up now and then to notice a signal.
        let wait = deadline.map_or(Duration::from_millis(100), |d| {
            d.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        });
        match chunks.recv_timeout(wait) {
            Ok(Ok(chunk)) => {
                deadline.get_or_insert_with(|| Instant::now() + timeout);
                code(&chunk, Action::Run, output)?;
            }
            Ok(Err(e)) => return Err(Failure::Read(e)),
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    code(&[], Action::SyncFlush, output)?;
                    output.flush().map_err(Failure::Write)?;
                    deadline = None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return code(&[], Action::Finish, output),
        }
    }
}

/// Compresses to a raw stream with [`RawEncoder`].
fn compress_raw(
    opts: &RawOptions,
    input: &mut dyn Read,
    output: &mut dyn Write,
    meter: &mut Meter,
) -> Result<(), Failure> {
    let coder = |e| Failure::Coder(e, 0);
    let mut enc = RawEncoder::with_options(opts).map_err(coder)?;
    let mut inbuf = vec![0u8; BUF_SIZE];
    let mut outbuf = vec![0u8; BUF_SIZE];
    let (mut total_in, mut total_out) = (0u64, 0u64);
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        let (n, eof) = read_full(input, &mut inbuf)?;
        let action = if eof { Action::Finish } else { Action::Run };
        let mut start = 0;
        loop {
            let (used, written, status) = enc
                .encode(&inbuf[start..n], &mut outbuf, action)
                .map_err(coder)?;
            start += used;
            total_in += used as u64;
            total_out += written as u64;
            output
                .write_all(&outbuf[..written])
                .map_err(Failure::Write)?;
            meter.tick((total_in, total_out));
            if status == Status::StreamEnd {
                return Ok(());
            }
            if !eof && start == n && written < outbuf.len() {
                break;
            }
        }
    }
}

/// Decompresses a raw stream with [`RawDecoder`] (`lzma_raw_decoder()`);
/// data after the end of the stream is an error, as in xz.
pub fn decompress_raw(
    opts: &RawOptions,
    input: &mut dyn Read,
    mut output: Option<&mut dyn Write>,
    meter: &mut Meter,
) -> Result<(), Failure> {
    let coder = |e| Failure::Coder(e, 0);
    let mut dec = RawDecoder::with_options(opts).map_err(coder)?;
    let mut inbuf = vec![0u8; BUF_SIZE];
    let mut outbuf = vec![0u8; BUF_SIZE];
    let (mut total_in, mut total_out) = (0u64, 0u64);
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        let (n, eof) = read_full(input, &mut inbuf)?;
        let mut start = 0;
        loop {
            let (used, written, status) = dec
                .decode(&inbuf[start..n], &mut outbuf, eof)
                .map_err(coder)?;
            start += used;
            total_in += used as u64;
            total_out += written as u64;
            if let Some(out) = output.as_deref_mut() {
                out.write_all(&outbuf[..written]).map_err(Failure::Write)?;
            }
            meter.tick((total_in, total_out));
            if status == Status::StreamEnd {
                // Only the end of the input may follow.
                let rest = if start < n {
                    1
                } else {
                    read_full(input, &mut inbuf[..1])?.0
                };
                return if rest == 0 {
                    Ok(())
                } else {
                    Err(coder(xz_native::Error::Data))
                };
            }
            if start == n && written < outbuf.len() {
                if eof {
                    return Err(coder(xz_native::Error::Truncated));
                }
                break;
            }
        }
    }
}

/// Reads until `buf` is full or the input ends (`io_read()`); true at the
/// end.
fn read_full(input: &mut dyn Read, buf: &mut [u8]) -> Result<(usize, bool), Failure> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..]) {
            Ok(0) => return Ok((n, true)),
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Failure::Read(e)),
        }
    }
    Ok((n, false))
}

/// Compresses with `--block-list` like xz's `coder_normal()`: each Block
/// ends with a full barrier, and the chain changes with
/// [`XzEncoder::update_filters`] where the list says so (`split_block()`).
fn compress_blocks(
    plan: &XzPlan,
    input: &mut dyn Read,
    output: &mut dyn Write,
    meter: &mut Meter,
) -> Result<(), Failure> {
    let list = plan.list.as_deref().expect("a Block list");
    let coder = |e| Failure::Coder(e, 0);
    let mut enc = XzEncoder::with_options(plan.opts.clone()).map_err(coder)?;
    let mt = plan.opts.threads != 1 || plan.opts.multithreaded;
    let split = plan.split;
    let mut pos = 0;
    let mut next_remaining = 0;
    let mut remaining = if !mt && split > 0 { split } else { u64::MAX };
    if remaining < list[0].1 {
        next_remaining = list[0].1 - remaining;
    } else {
        remaining = list[0].1;
    }
    let mut inbuf = vec![0u8; BUF_SIZE];
    let mut outbuf = vec![0u8; BUF_SIZE];
    let (mut start, mut end) = (0, 0);
    let mut action = Action::Run;
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        if start == end && action == Action::Run {
            let size = (BUF_SIZE as u64).min(remaining) as usize;
            let (n, eof) = read_full(input, &mut inbuf[..size])?;
            (start, end) = (0, n);
            if eof {
                action = Action::Finish;
            } else if remaining != u64::MAX {
                remaining -= n as u64;
                if remaining == 0 {
                    action = Action::FullBarrier;
                }
            }
        }
        let (used, written, status) = enc
            .encode(&inbuf[start..end], &mut outbuf, action)
            .map_err(coder)?;
        start += used;
        output
            .write_all(&outbuf[..written])
            .map_err(Failure::Write)?;
        meter.tick(enc.progress());
        if status != Status::StreamEnd {
            continue;
        }
        if action == Action::Finish {
            return Ok(());
        }
        // `split_block()`: the next piece of a long Block, or the next
        // entry of the list (the last one repeats).
        if next_remaining > 0 {
            remaining = next_remaining.min(split);
            next_remaining -= remaining;
        } else {
            if pos + 1 < list.len() {
                pos += 1;
                if list[pos - 1].0 != list[pos].0 {
                    let (filters, lzma) = plan.chains[list[pos].0].as_ref().expect("a used chain");
                    enc.update_filters(filters, *lzma).map_err(coder)?;
                }
            }
            remaining = list[pos].1;
            if !mt && split > 0 && remaining > split {
                next_remaining = remaining - split;
                remaining = split;
            }
        }
        action = Action::Run;
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
    /// `(in, out)` like `lzma_get_progress()`, where the reader has it.
    fn progress(&self) -> Option<(u64, u64)>;
}

macro_rules! decoder {
    ($($t:ident: $progress:expr),*) => {$(
        impl<R: io::BufRead> Decoder for $t<R> {
            fn memusage(&self) -> u64 {
                $t::memusage(self)
            }

            fn progress(&self) -> Option<(u64, u64)> {
                $progress(self)
            }
        }
    )*};
}
decoder!(
    AutoReader: |r: &AutoReader<R>| Some(r.progress()),
    XzReader: |r: &XzReader<R>| Some(r.progress()),
    LzmaReader: |_| None,
    LzipReader: |_| None
);

/// Reads from `inner`, keeping a copy of the bytes while `recorded` holds
/// a buffer, so that unrecognized input can be passed through.
struct Recording<'a, R: ?Sized> {
    inner: &'a mut R,
    recorded: &'a RefCell<Option<Vec<u8>>>,
    /// Bytes read so far.
    count: &'a Cell<u64>,
}

impl<R: Read + ?Sized> Read for Recording<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
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
    meter: &mut Meter,
) -> Result<(), Failure> {
    let recorded = RefCell::new(passthrough.then(Vec::new));
    let count = Cell::new(0);
    let src = Recording {
        inner: &mut *input,
        recorded: &recorded,
        count: &count,
    };
    let mut written = 0u64;
    let r = BufReader::with_capacity(BUF_SIZE, src);
    let mut d: Box<dyn Decoder + '_> = match format {
        Format::Auto => Box::new(AutoReader::with_options(r, opts)),
        Format::Xz => Box::new(XzReader::with_options(r, opts)),
        Format::Lzma => Box::new(LzmaReader::with_options(r, opts)),
        Format::Lzip => Box::new(LzipReader::with_options(r, opts)),
        Format::Raw => unreachable!("raw data goes to decompress_raw"),
    };
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        let n = match d.read(&mut buf) {
            Ok(0) => {
                meter.tick(d.progress().unwrap_or((count.get(), written)));
                return Ok(());
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let Some(c) = coder_error(&e) else {
                    return Err(Failure::Read(e));
                };
                let head = recorded.borrow_mut().take();
                if let (xz_native::Error::Format, Some(head), Some(out)) = (c, head, output) {
                    drop(d);
                    let r = copy_through(&head, input, out);
                    meter.tick(r.as_ref().map_or((0, 0), |&n| (n, n)));
                    return r.map(drop);
                }
                return Err(Failure::Coder(c, d.memusage()));
            }
        };
        // Once data comes out, the input is in a known format.
        recorded.borrow_mut().take();
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buf[..n]).map_err(Failure::Write)?;
        }
        written += n as u64;
        meter.tick(d.progress().unwrap_or((count.get(), written)));
    }
}

/// Writes `head`, the input read so far, then copies the rest. Returns the
/// bytes copied.
fn copy_through(head: &[u8], input: &mut dyn Read, output: &mut dyn Write) -> Result<u64, Failure> {
    output.write_all(head).map_err(Failure::Write)?;
    let mut total = head.len() as u64;
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        if crate::signals::aborted() {
            return Err(Failure::Aborted);
        }
        match input.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => {
                output.write_all(&buf[..n]).map_err(Failure::Write)?;
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Failure::Read(e)),
        }
    }
}

/// The memory-needed line of a memory limit error.
pub fn memlimit_detail(usage: u64, limit: Option<u64>) -> String {
    mem_needed(usage, limit)
}
