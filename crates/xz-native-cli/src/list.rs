//! `--list` (xz's `list.c`): the basic table, `-v` and `-vv` details,
//! `--robot` output, and the totals, from the Stream and Block tables of
//! [`XzSeekableReader`].

use crate::Ctx;
use crate::args::{Format, V_DEBUG, V_VERBOSE, V_WARNING};
use crate::names::Names;
use crate::num::{int, mib, nice, ratio};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal};
use xz_native::{Check, DecoderOptions, Filter, XzSeekableReader};

/// Labels of the `-v` output, padded to the longest.
const COLON_WIDTH: usize = 18; // "Uncompressed size:"

/// Column widths (`headings[]`).
const W_STREAM: usize = 6;
const W_BLOCK: usize = 9;
const W_OFFSET: usize = 15;
const W_SIZE: usize = 15;
const W_RATIO: usize = 5;
const W_CHECK: usize = 10;
const W_CHECKVAL: usize = 8;
const W_PADDING: usize = 7;
const W_HEADER: usize = 6;
const W_FLAGS: usize = 5;
const W_MEMUSAGE: usize = 11;

/// What one file adds to the totals (`xz_file_info` and `totals`).
#[derive(Clone, Copy)]
struct Info {
    streams: u64,
    blocks: u64,
    compressed: u64,
    uncompressed: u64,
    padding: u64,
    /// Bit `id` set for each check ID.
    checks: u32,
    memusage_max: u64,
    all_have_sizes: bool,
    min_version: u32,
}

impl Default for Info {
    fn default() -> Self {
        Self {
            streams: 0,
            blocks: 0,
            compressed: 0,
            uncompressed: 0,
            padding: 0,
            checks: 0,
            memusage_max: 0,
            all_have_sizes: true,
            min_version: 50000002,
        }
    }
}

fn check_name(c: Check) -> String {
    match c {
        Check::None => "None".into(),
        Check::Crc32 => "CRC32".into(),
        Check::Crc64 => "CRC64".into(),
        Check::Sha256 => "SHA-256".into(),
        c => format!("Unknown-{}", c.id()),
    }
}

/// `get_check_names()`.
fn check_names(checks: u32, space: bool) -> String {
    let checks = if checks == 0 { 1 } else { checks };
    let names: Vec<String> = (0..16u8)
        .filter(|i| checks & (1 << i) != 0)
        .map(|i| check_name(Check::from_id(i).unwrap_or(Check::None)))
        .collect();
    names.join(if space { ", " } else { "," })
}

/// `xz_ver_to_str()`.
fn version(v: u32) -> String {
    let (major, minor, patch, stability) =
        (v / 10_000_000, v / 10_000 % 1000, v / 10 % 1000, v % 10);
    let s = match stability {
        0 => "alpha",
        1 => "beta",
        _ => "",
    };
    format!("{major}.{minor}.{patch}{s}")
}

/// The filter chain as `lzma_str_from_filters(LZMA_STR_DECODER |
/// LZMA_STR_GETOPT_LONG)` writes it.
fn filter_chain(filters: &[Filter]) -> String {
    fn size(mut v: u32) -> String {
        if v == 0 {
            return "0".into();
        }
        let mut suffix = "";
        for s in ["KiB", "MiB", "GiB"] {
            if v & 1023 != 0 {
                break;
            }
            v >>= 10;
            suffix = s;
        }
        format!("{v}{suffix}")
    }
    let parts: Vec<String> = filters
        .iter()
        .map(|f| match *f {
            Filter::Lzma2 { dict_size } => format!("--lzma2=dict={}", size(dict_size)),
            Filter::Delta { distance } => format!("--delta=dist={distance}"),
            Filter::Bcj { kind, start_offset } => {
                let name = match kind.filter_id() {
                    0x04 => "x86",
                    0x05 => "powerpc",
                    0x06 => "ia64",
                    0x07 => "arm",
                    0x08 => "armthumb",
                    0x09 => "sparc",
                    0x0A => "arm64",
                    _ => "riscv",
                };
                if start_offset == 0 {
                    format!("--{name}")
                } else {
                    format!("--{name}=start={}", size(start_offset))
                }
            }
            _ => "--unknown".into(),
        })
        .collect();
    parts.join(" ")
}

/// A Block's details for `-vv` (`block_header_info`).
struct Details {
    check_value: String,
    header_size: usize,
    flags: String,
    compressed: u64,
    memusage: u64,
    filters: String,
}

pub fn run(ctx: &mut Ctx) -> Result<(), String> {
    if !matches!(ctx.o.format, Format::Auto | Format::Xz) {
        let mut msg = "--list works only on .xz files (--format=xz or --format=auto)".to_owned();
        if ctx.o.format == Format::Lzma {
            msg.push_str(&format!(
                "\n{}: Try 'lzmainfo' with .lzma files.",
                crate::PROGRAM
            ));
        }
        return Err(msg);
    }
    let mut opts = DecoderOptions::new();
    opts.memlimit = crate::coder::Hardware::detect()
        .bytes(ctx.o.memlimit_decompress)
        .unwrap_or(u64::MAX);
    let mut names = Names::new(ctx.o.files.clone(), ctx.o.files_from.as_ref())?;
    let mut totals = Info::default();
    let mut files_ok = 0u64;
    let mut out = String::new();
    let mut first_printed = false;
    let mut i = 0;
    while let Some((name, stdin)) = names.next(ctx) {
        i += 1;
        let display = name.to_string_lossy().into_owned();
        if stdin && io::stdin().is_terminal() {
            ctx.error("Compressed data cannot be read from a terminal");
            continue;
        }
        if stdin && names.stdin_names {
            ctx.error(
                "Cannot read data from standard input when reading filenames from standard \n                 input",
            );
            continue;
        }
        let display = if stdin { "(stdin)".to_owned() } else { display };
        // `message_filename()`: the name heads each file's details.
        if ctx.o.verbosity >= V_VERBOSE && !ctx.o.robot {
            if first_printed {
                out.push('\n');
            }
            first_printed = true;
            match names.total {
                Some(n) => {
                    let _ = writeln!(out, "{display} ({i}/{n})");
                }
                None => {
                    let _ = writeln!(out, "{display} ({i})");
                }
            }
        }
        if stdin {
            print!("{out}");
            out.clear();
            ctx.error("--list does not support reading from standard input");
            continue;
        }
        match list_file(ctx, opts, name.as_ref(), &display, &mut out, files_ok == 0) {
            Ok(Some(info)) => {
                files_ok += 1;
                totals.streams += info.streams;
                totals.blocks += info.blocks;
                totals.compressed += info.compressed;
                totals.uncompressed += info.uncompressed;
                totals.padding += info.padding;
                totals.checks |= info.checks;
                totals.memusage_max = totals.memusage_max.max(info.memusage_max);
                totals.min_version = totals.min_version.max(info.min_version);
                totals.all_have_sizes &= info.all_have_sizes;
            }
            Ok(None) => {}
            Err(msg) => ctx.error(msg),
        }
        // Print as we go, so that output and errors stay in order.
        print!("{out}");
        out.clear();
    }
    print_totals(ctx, &totals, files_ok, &mut out);
    print!("{out}");
    Ok(())
}

/// Lists one file (`list_file()`); None if it was skipped with a warning.
fn list_file(
    ctx: &mut Ctx,
    opts: DecoderOptions,
    path: &std::path::Path,
    name: &str,
    out: &mut String,
    first: bool,
) -> Result<Option<Info>, String> {
    let os = |e: io::Error| format!("{name}: {}", crate::files::os_error(&e));
    let meta = fs::metadata(path).map_err(os)?;
    if meta.is_dir() {
        ctx.warning(format!("{name}: Is a directory, skipping"));
        return Ok(None);
    }
    if !meta.is_file() {
        ctx.warning(format!("{name}: Not a regular file, skipping"));
        return Ok(None);
    }
    if meta.len() == 0 {
        return Err(format!("{name}: File is empty"));
    }
    if meta.len() < 24 {
        return Err(format!("{name}: Too small to be a valid .xz file"));
    }
    let file = File::open(path).map_err(os)?;
    let mut r = match XzSeekableReader::with_options(BufReader::new(file), opts) {
        Ok(r) => r,
        Err(e) => {
            let Some(c) = e
                .get_ref()
                .and_then(|e| e.downcast_ref::<xz_native::Error>())
            else {
                return Err(os(e));
            };
            return Err(format!("{name}: {}", crate::coder::describe(*c)));
        }
    };
    let streams = r.streams().to_vec();
    let blocks = r.blocks().to_vec();
    let mut info = Info {
        streams: streams.len() as u64,
        blocks: blocks.len() as u64,
        compressed: streams.iter().map(|s| s.compressed_size + s.padding).sum(),
        uncompressed: r.uncompressed_size(),
        padding: streams.iter().map(|s| s.padding).sum(),
        checks: streams.iter().fold(0, |m, s| m | 1 << s.check.id()),
        ..Info::default()
    };
    let robot = ctx.o.robot;
    let v = ctx.o.verbosity;
    let detailed = v >= V_DEBUG;
    // Block details come first, so that a bad Block Header is an error
    // before anything about the file is printed.
    let mut details = Vec::new();
    if detailed {
        for (i, b) in blocks.iter().enumerate() {
            details.push(block_details(&mut r, i, b, &mut info, name)?);
        }
    }

    if robot {
        let _ = writeln!(out, "name\t{name}");
        let _ = writeln!(
            out,
            "file\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            info.streams,
            info.blocks,
            info.compressed,
            info.uncompressed,
            ratio(info.compressed, info.uncompressed),
            check_names(info.checks, false),
            info.padding
        );
        if v >= V_VERBOSE {
            for (i, s) in streams.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "stream\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    i + 1,
                    s.block_count,
                    s.compressed_offset,
                    s.uncompressed_offset,
                    s.compressed_size,
                    s.uncompressed_size,
                    ratio(s.compressed_size, s.uncompressed_size),
                    check_name(s.check),
                    s.padding
                );
            }
            for (i, b) in blocks.iter().enumerate() {
                let s = &streams[b.stream];
                let _ = write!(
                    out,
                    "block\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    b.stream + 1,
                    i - s.first_block + 1,
                    i + 1,
                    b.compressed_offset,
                    b.uncompressed_offset,
                    b.total_size(),
                    b.uncompressed_size,
                    ratio(b.total_size(), b.uncompressed_size),
                    check_name(b.check)
                );
                if let Some(d) = details.get(i) {
                    let _ = write!(
                        out,
                        "\t{}\t{}\t{}\t{}\t{}\t{}",
                        d.check_value, d.header_size, d.flags, d.compressed, d.memusage, d.filters
                    );
                }
                out.push('\n');
            }
        }
        if detailed {
            let _ = writeln!(
                out,
                "summary\t{}\t{}\t{}",
                info.memusage_max,
                if info.all_have_sizes { "yes" } else { "no" },
                info.min_version
            );
        }
        return Ok(Some(info));
    }

    if v <= V_WARNING {
        if first {
            out.push_str("Strms  Blocks   Compressed Uncompressed  Ratio  Check   Filename\n");
        }
        let _ = writeln!(
            out,
            "{:>5} {:>7}  {:>11}  {:>11}  {:>5}  {:<7} {name}",
            int(info.streams),
            int(info.blocks),
            nice(info.compressed, false),
            nice(info.uncompressed, false),
            ratio(info.compressed, info.uncompressed),
            check_names(info.checks, false)
        );
        return Ok(Some(info));
    }

    adv_summary(out, &info);
    let _ = writeln!(
        out,
        "  Streams:\n    {:>W_STREAM$} {:>W_BLOCK$} {:>W_OFFSET$} {:>W_OFFSET$} {:>W_SIZE$} \
         {:>W_SIZE$}  {:>W_RATIO$}  {:<W_CHECK$} {:>W_PADDING$}",
        "Stream",
        "Blocks",
        "CompOffset",
        "UncompOffset",
        "CompSize",
        "UncompSize",
        "Ratio",
        "Check",
        "Padding"
    );
    let mut check_max = 0;
    for (i, s) in streams.iter().enumerate() {
        let _ = writeln!(
            out,
            "    {:>W_STREAM$} {:>W_BLOCK$} {:>W_OFFSET$} {:>W_OFFSET$} {:>W_SIZE$} {:>W_SIZE$}  \
             {:>W_RATIO$}  {:<W_CHECK$} {:>W_PADDING$}",
            int(i as u64 + 1),
            int(s.block_count as u64),
            int(s.compressed_offset),
            int(s.uncompressed_offset),
            int(s.compressed_size),
            int(s.uncompressed_size),
            ratio(s.compressed_size, s.uncompressed_size),
            check_name(s.check),
            int(s.padding)
        );
        check_max = check_max.max(s.check.size());
    }
    if !blocks.is_empty() {
        let checkval_width = W_CHECKVAL.max(2 * check_max);
        let check_w = if detailed { W_CHECK } else { 1 };
        let _ = write!(
            out,
            "  Blocks:\n    {:>W_STREAM$} {:>W_BLOCK$} {:>W_OFFSET$} {:>W_OFFSET$} {:>W_SIZE$} \
             {:>W_SIZE$}  {:>W_RATIO$}  {:<check_w$}",
            "Stream",
            "Block",
            "CompOffset",
            "UncompOffset",
            "TotalSize",
            "UncompSize",
            "Ratio",
            "Check"
        );
        if detailed {
            let _ = write!(
                out,
                " {:<checkval_width$}  {:>W_HEADER$}  {:<W_FLAGS$} {:>W_SIZE$} {:>W_MEMUSAGE$}  Filters",
                "CheckVal", "Header", "Flags", "CompSize", "MemUsage"
            );
        }
        out.push('\n');
        for (i, b) in blocks.iter().enumerate() {
            let s = &streams[b.stream];
            let _ = write!(
                out,
                "    {:>W_STREAM$} {:>W_BLOCK$} {:>W_OFFSET$} {:>W_OFFSET$} {:>W_SIZE$} \
                 {:>W_SIZE$}  {:>W_RATIO$}  {:<check_w$}",
                int(b.stream as u64 + 1),
                int((i - s.first_block + 1) as u64),
                int(b.compressed_offset),
                int(b.uncompressed_offset),
                int(b.total_size()),
                int(b.uncompressed_size),
                ratio(b.total_size(), b.uncompressed_size),
                check_name(b.check)
            );
            if let Some(d) = details.get(i) {
                let mem_w = W_MEMUSAGE - 4;
                let _ = write!(
                    out,
                    " {:<checkval_width$}  {:>W_HEADER$}  {:<W_FLAGS$} {:>W_SIZE$} {:>mem_w$} MiB  {}",
                    d.check_value,
                    int(d.header_size as u64),
                    d.flags,
                    int(d.compressed),
                    mib(d.memusage),
                    d.filters
                );
            }
            out.push('\n');
        }
    }
    if detailed {
        adv_debug(out, &info);
    }
    Ok(Some(info))
}

/// `parse_details()` for Block `i`: the Block Header and the check value.
fn block_details(
    r: &mut XzSeekableReader<BufReader<File>>,
    i: usize,
    b: &xz_native::BlockInfo,
    info: &mut Info,
    name: &str,
) -> Result<Details, String> {
    let d = r.block_details(i).map_err(|e| {
        let c = e
            .get_ref()
            .and_then(|e| e.downcast_ref::<xz_native::Error>())
            .copied()
            .unwrap_or(xz_native::Error::Data);
        format!("{name}: {}", crate::coder::describe(c))
    })?;
    let has_sizes = d.compressed_size.is_some() && d.uncompressed_size.is_some();
    info.all_have_sizes &= has_sizes;
    info.memusage_max = info.memusage_max.max(d.memusage);
    // The oldest xz that decodes it (`parse_block_header()`).
    let bcj = |id: u64| {
        d.filters
            .iter()
            .any(|f| matches!(f, Filter::Bcj { kind, .. } if kind.filter_id() == id))
    };
    if bcj(0x0B) {
        info.min_version = info.min_version.max(50060002);
    }
    if bcj(0x0A) {
        info.min_version = info.min_version.max(50040002);
    }
    if matches!(d.filters.last(), Some(Filter::Lzma2 { .. })) && b.uncompressed_size == 0 {
        info.min_version = info.min_version.max(50000022);
    }
    let v = &d.check_value;
    let check_value = match v.len() {
        0 => "---".to_owned(),
        4 => format!("{:08x}", u32::from_le_bytes(v[..4].try_into().unwrap())),
        8 => format!("{:016x}", u64::from_le_bytes(v[..8].try_into().unwrap())),
        _ => v.iter().map(|x| format!("{x:02x}")).collect(),
    };
    Ok(Details {
        check_value,
        header_size: d.header_size,
        flags: format!(
            "{}{}",
            if d.compressed_size.is_some() {
                'c'
            } else {
                '-'
            },
            if d.uncompressed_size.is_some() {
                'u'
            } else {
                '-'
            }
        ),
        compressed: b.unpadded_size - d.header_size as u64 - b.check.size() as u64,
        memusage: d.memusage,
        filters: filter_chain(&d.filters),
    })
}

fn colon(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(out, "  {label:<COLON_WIDTH$} {value}");
}

/// `print_adv_helper()`.
fn adv_summary(out: &mut String, i: &Info) {
    colon(out, "Streams:", &int(i.streams));
    colon(out, "Blocks:", &int(i.blocks));
    colon(out, "Compressed size:", &nice(i.compressed, true));
    colon(out, "Uncompressed size:", &nice(i.uncompressed, true));
    colon(out, "Ratio:", &ratio(i.compressed, i.uncompressed));
    colon(out, "Check:", &check_names(i.checks, true));
    colon(out, "Stream Padding:", &nice(i.padding, true));
}

/// The `-vv` summary lines.
fn adv_debug(out: &mut String, i: &Info) {
    colon(
        out,
        "Memory needed:",
        &format!("{} MiB", mib(i.memusage_max)),
    );
    colon(
        out,
        "Sizes in headers:",
        if i.all_have_sizes { "Yes" } else { "No" },
    );
    let _ = writeln!(
        out,
        "  Minimum XZ Utils version: {}",
        version(i.min_version)
    );
}

/// `list_totals()`.
fn print_totals(ctx: &Ctx, t: &Info, files: u64, out: &mut String) {
    let v = ctx.o.verbosity;
    if ctx.o.robot {
        let _ = write!(
            out,
            "totals\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            t.streams,
            t.blocks,
            t.compressed,
            t.uncompressed,
            ratio(t.compressed, t.uncompressed),
            check_names(t.checks, false),
            t.padding,
            files
        );
        if v >= V_DEBUG {
            let _ = write!(
                out,
                "\t{}\t{}\t{}",
                t.memusage_max,
                if t.all_have_sizes { "yes" } else { "no" },
                t.min_version
            );
        }
        out.push('\n');
        return;
    }
    if files <= 1 {
        return;
    }
    if v <= V_WARNING {
        let _ = writeln!(out, "{}", "-".repeat(79));
        let _ = writeln!(
            out,
            "{:>5} {:>7}  {:>11}  {:>11}  {:>5}  {:<7} {} files",
            int(t.streams),
            int(t.blocks),
            nice(t.compressed, false),
            nice(t.uncompressed, false),
            ratio(t.compressed, t.uncompressed),
            check_names(t.checks, false),
            int(files)
        );
    } else {
        out.push_str("\nTotals:\n");
        colon(out, "Number of files:", &int(files));
        adv_summary(out, t);
        if v >= V_DEBUG {
            adv_debug(out, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xz_native::BcjKind;

    #[test]
    fn filter_chains() {
        assert_eq!(
            filter_chain(&[Filter::Lzma2 { dict_size: 8 << 20 }]),
            "--lzma2=dict=8MiB"
        );
        assert_eq!(
            filter_chain(&[
                Filter::Delta { distance: 4 },
                Filter::Bcj {
                    kind: BcjKind::Arm64,
                    start_offset: 4096
                },
                Filter::Lzma2 { dict_size: 3 << 11 }
            ]),
            "--delta=dist=4 --arm64=start=4KiB --lzma2=dict=6KiB"
        );
    }

    #[test]
    fn versions_and_checks() {
        assert_eq!(version(50000002), "5.0.0");
        assert_eq!(version(50060002), "5.6.0");
        assert_eq!(version(50000022), "5.0.2");
        assert_eq!(check_names(0, false), "None");
        assert_eq!(check_names(1 << 4 | 1 << 10, true), "CRC64, SHA-256");
    }
}
