//! Processing the files (xz's `main.c`, `file_io.c` and `suffix.c`):
//! output names, which inputs are skipped, refusing to overwrite,
//! copying permissions and times, and removing the input on success.

use crate::Ctx;
use crate::args::{Format, Mode};
use crate::coder::{self, Encoder, Failure, Hardware};
use crate::progress::Meter;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::Path;
use xz_native::DecoderOptions;

const STDIN_NAME: &str = "(stdin)";

pub fn run(ctx: &mut Ctx) -> Result<(), String> {
    if ctx.o.mode == Mode::List {
        return crate::list::run(ctx);
    }
    let o = &mut ctx.o;
    if o.files.is_empty() {
        o.files.push("-".into());
    }
    // The input isn't removed when the output isn't a file.
    if o.stdout || o.mode == Mode::Test {
        o.keep = true;
        o.stdout = true;
    }
    if o.mode == Mode::Compress && o.format == Format::Auto {
        o.format = Format::Xz;
    }
    // Raw data has no suffix of its own (`args_parse()`).
    if o.format == Format::Raw
        && o.suffix.is_none()
        && !o.stdout
        && matches!(o.mode, Mode::Compress | Mode::Decompress)
        && o.files.iter().any(|f| f != "-")
    {
        return Err("With --format=raw, --suffix=.SUF is required unless writing to stdout".into());
    }
    if o.block_list.is_some() && !(o.mode == Mode::Compress && o.format == Format::Xz) {
        o.block_list = None;
        if o.verbosity >= crate::args::V_WARNING {
            eprintln!(
                "{}: --block-list is ignored unless compressing to the .xz format",
                crate::PROGRAM
            );
        }
    }
    if o.mode == Mode::Compress
        && (o.stdout || (o.files.len() == 1 && o.files[0] == "-"))
        && io::stdout().is_terminal()
    {
        return Err(format!(
            "Compressed data cannot be written to a terminal\n{0}: Try '{0} --help' for more \
             information.",
            crate::PROGRAM
        ));
    }
    let hw = Hardware::detect();
    let job = Job {
        encoder: match ctx.o.mode {
            Mode::Compress => Some(coder::encoder(ctx, &hw)?),
            _ => None,
        },
        decoder: coder::decoder_options(ctx, &hw),
        raw: match ctx.o.mode {
            Mode::Decompress | Mode::Test if ctx.o.format == Format::Raw => {
                Some(coder::raw_options(ctx, &hw)?)
            }
            _ => None,
        },
        limit: coder::decoder_limit(ctx, &hw),
    };
    let files = ctx.o.files.clone();
    for (i, name) in files.iter().enumerate() {
        let pos = (i + 1, files.len());
        if name == "-" {
            stdin(ctx, &job, pos);
        } else {
            file(ctx, &job, name, pos);
        }
    }
    Ok(())
}

/// The settings shared by the files.
struct Job {
    encoder: Option<Encoder>,
    decoder: DecoderOptions,
    /// The chain for decoding `--format=raw`.
    raw: Option<xz_native::RawOptions>,
    /// The decompression memory limit, for messages.
    limit: Option<u64>,
}

fn display(name: &OsStr) -> String {
    name.to_string_lossy().into_owned()
}

fn stdin(ctx: &mut Ctx, job: &Job, pos: (usize, usize)) {
    if ctx.o.mode == Mode::Compress {
        if io::stdout().is_terminal() {
            ctx.error("Compressed data cannot be written to a terminal");
            return;
        }
    } else if io::stdin().is_terminal() {
        ctx.error("Compressed data cannot be read from a terminal");
        return;
    }
    let mut meter = meter(ctx, STDIN_NAME, pos);
    let mut input = io::stdin().lock();
    code_to_stdout(ctx, job, STDIN_NAME, &mut input, &mut meter);
}

/// Codes `input` to standard output (or nowhere when testing).
/// The `-v` progress of a file (`message_filename()` and
/// `message_progress_start()`).
fn meter(ctx: &Ctx, name: &str, (pos, files): (usize, usize)) -> Meter {
    Meter::start(
        ctx.o.verbosity >= crate::args::V_VERBOSE,
        name,
        pos,
        files,
        ctx.o.mode == Mode::Compress,
    )
}

/// Codes `input` to standard output (or nowhere when testing).
fn code_to_stdout(ctx: &mut Ctx, job: &Job, name: &str, input: &mut dyn Read, meter: &mut Meter) {
    let mut out = BufWriter::with_capacity(1 << 16, io::stdout().lock());
    let r = if ctx.o.mode == Mode::Test {
        code(ctx, job, input, None, meter)
    } else {
        code(ctx, job, input, Some(&mut out), meter)
            .and_then(|()| out.flush().map_err(Failure::Write))
    };
    let ok = r.is_ok();
    if let Err(f) = r {
        report(ctx, job, name, f);
    }
    meter.finish(ok);
}

fn code(
    ctx: &Ctx,
    job: &Job,
    input: &mut dyn Read,
    output: Option<&mut dyn Write>,
    meter: &mut Meter,
) -> Result<(), Failure> {
    match (&job.encoder, output) {
        (Some(enc), Some(out)) => coder::compress(enc, input, out, meter),
        (Some(_), None) => unreachable!("compressing to nowhere"),
        (None, out) if job.raw.is_some() => {
            coder::decompress_raw(job.raw.as_ref().unwrap(), input, out, meter)
        }
        (None, out) => {
            // `xz -dcf` copies input in an unknown format as is.
            let passthrough = ctx.o.mode == Mode::Decompress && ctx.o.stdout && ctx.o.force;
            coder::decompress(ctx.o.format, job.decoder, input, out, passthrough, meter)
        }
    }
}

fn report(ctx: &mut Ctx, job: &Job, name: &str, f: Failure) {
    match f {
        Failure::Read(e) => ctx.error(format!("{name}: Read error: {}", os_error(&e))),
        Failure::Write(e) => {
            // A closed pipe on standard output isn't worth a message.
            if e.kind() != io::ErrorKind::BrokenPipe {
                ctx.error(format!("{name}: Write error: {}", os_error(&e)));
            } else {
                ctx.status = crate::E_ERROR;
            }
        }
        Failure::Coder(e, usage) => {
            ctx.error(format!("{name}: {}", coder::describe(e)));
            if e == xz_native::Error::MemLimit && usage > 0 {
                ctx.note(
                    crate::args::V_ERROR,
                    coder::memlimit_detail(usage, job.limit),
                );
            }
        }
    }
}

/// The OS error text without Rust's " (os error N)". On Windows the
/// common ones read as xz's C library prints them.
pub fn os_error(e: &io::Error) -> String {
    if cfg!(windows) {
        match e.kind() {
            io::ErrorKind::NotFound => return "No such file or directory".into(),
            io::ErrorKind::PermissionDenied => return "Permission denied".into(),
            io::ErrorKind::AlreadyExists => return "File exists".into(),
            _ => {}
        }
    }
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

fn file(ctx: &mut Ctx, job: &Job, src: &OsStr, pos: (usize, usize)) {
    let name = display(src);
    let mut meter = meter(ctx, &name, pos);
    if src.is_empty() {
        ctx.warning("Empty filename, skipping");
        return;
    }
    let dest = if ctx.o.stdout {
        None
    } else {
        match dest_name(ctx, src) {
            Some(d) => Some(d),
            None => return,
        }
    };
    let path = Path::new(src);
    // Symlinks are followed only for stdout, --force or --keep.
    let follow = ctx.o.stdout || ctx.o.force || ctx.o.keep;
    if !follow && fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        ctx.warning(format!("{name}: Is a symbolic link, skipping"));
        return;
    }
    // Directories first: opening one fails on Windows.
    if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        ctx.warning(format!("{name}: Is a directory, skipping"));
        return;
    }
    let mut input = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            ctx.error(format!("{name}: {}", os_error(&e)));
            return;
        }
    };
    let meta = match input.metadata() {
        Ok(m) => m,
        Err(e) => {
            ctx.error(format!("{name}: {}", os_error(&e)));
            return;
        }
    };
    if !ctx.o.stdout && !meta.is_file() {
        ctx.warning(format!("{name}: Not a regular file, skipping"));
        return;
    }
    meter.set_expected(meta.len());
    let Some(dest) = dest else {
        code_to_stdout(ctx, job, &name, &mut input, &mut meter);
        return;
    };
    let dest_display = display(&dest);
    let dest_path = Path::new(&dest);
    if ctx.o.force {
        // Replace a regular file only (`io_open_dest_real()`).
        match fs::symlink_metadata(dest_path) {
            Ok(m) if m.is_file() || m.file_type().is_symlink() => {
                if let Err(e) = fs::remove_file(dest_path) {
                    ctx.error(format!("{dest_display}: Cannot remove: {}", os_error(&e)));
                    return;
                }
            }
            Ok(_) => {
                ctx.error(format!("{dest_display}: Destination is not a regular file"));
                return;
            }
            Err(_) => {}
        }
    }
    let out = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest_path)
    {
        Ok(f) => f,
        Err(e) => {
            ctx.error(format!("{dest_display}: {}", os_error(&e)));
            return;
        }
    };
    let mut w = BufWriter::with_capacity(1 << 16, out);
    let r = code(ctx, job, &mut input, Some(&mut w), &mut meter)
        .and_then(|()| w.into_inner().map_err(|e| Failure::Write(e.into_error())));
    let out = match r {
        Ok(f) => f,
        Err(f) => {
            let _ = fs::remove_file(dest_path);
            report(ctx, job, &name, f);
            meter.finish(false);
            return;
        }
    };
    meter.finish(true);
    copy_metadata(ctx, &meta, &out, &dest_display);
    if let Err(e) = out.sync_all().and(Ok(())) {
        let _ = fs::remove_file(dest_path);
        ctx.error(format!(
            "{dest_display}: Closing the file failed: {}",
            os_error(&e)
        ));
        return;
    }
    drop(out);
    drop(input);
    if !ctx.o.keep
        && let Err(e) = fs::remove_file(path)
    {
        ctx.error(format!("{name}: Cannot remove: {}", os_error(&e)));
    }
}

/// Copies the permissions and the access and modification times
/// (`io_copy_attrs()`); failures are warnings.
fn copy_metadata(ctx: &mut Ctx, meta: &fs::Metadata, out: &File, dest: &str) {
    if let Err(e) = out.set_permissions(meta.permissions()) {
        ctx.warning(format!(
            "{dest}: Cannot set the file permissions: {}",
            os_error(&e)
        ));
    }
    let mut times = FileTimes::new();
    if let Ok(t) = meta.accessed() {
        times = times.set_accessed(t);
    }
    if let Ok(t) = meta.modified() {
        times = times.set_modified(t);
    }
    // xz ignores a failure to set the times.
    let _ = out.set_times(times);
}

/// `suffix_get_dest_name()`: the output name, or None after a warning.
fn dest_name(ctx: &mut Ctx, src: &OsStr) -> Option<OsString> {
    let name = display(src);
    let custom = ctx.o.suffix.clone();
    if ctx.o.mode == Mode::Compress {
        let known: &[&str] = match ctx.o.format {
            Format::Lzma => &[".lzma", ".tlz"],
            // Raw files get only the --suffix that `run` requires.
            Format::Raw => &[],
            _ => &[".xz", ".txz"],
        };
        for s in known.iter().copied().chain(custom.as_deref()) {
            if has_suffix(src, s) {
                ctx.warning(format!("{name}: File already has '{s}' suffix, skipping"));
                return None;
            }
        }
        let mut d = src.to_owned();
        // `run` makes sure raw data has a --suffix.
        d.push(custom.as_deref().unwrap_or_else(|| known[0]));
        return Some(d);
    }
    // Decompressing: replace a known suffix (`uncompressed_name()`).
    const SUFFIXES: [(&str, &str); 5] = [
        (".xz", ""),
        (".txz", ".tar"),
        (".lzma", ""),
        (".tlz", ".tar"),
        (".lz", ""),
    ];
    let builtin: &[(&str, &str)] = if ctx.o.format == Format::Raw {
        &[]
    } else {
        &SUFFIXES
    };
    let replace = builtin
        .iter()
        .find(|(s, _)| has_suffix(src, s))
        .map(|&(s, r)| (s.len(), r))
        .or_else(|| {
            custom
                .as_deref()
                .filter(|s| has_suffix(src, s))
                .map(|s| (s.len(), ""))
        });
    let Some((cut, new)) = replace else {
        ctx.warning(format!("{name}: Filename has an unknown suffix, skipping"));
        return None;
    };
    let s = src.to_str()?;
    Some(format!("{}{new}", &s[..s.len() - cut]).into())
}

/// `test_suffix()`: `name` ends with `suffix` and something before it
/// that isn't a directory separator. Case-insensitive on Windows. A name
/// that isn't valid Unicode has no known suffix.
fn has_suffix(name: &OsStr, suffix: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.len() <= suffix.len() || !name.is_char_boundary(name.len() - suffix.len()) {
        return false;
    }
    let (stem, end) = name.split_at(name.len() - suffix.len());
    let sep = |c: char| c == '/' || (cfg!(windows) && (c == '\\' || c == ':'));
    if stem.ends_with(sep) {
        return false;
    }
    if cfg!(windows) {
        end.eq_ignore_ascii_case(suffix)
    } else {
        end == suffix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes() {
        assert!(has_suffix(OsStr::new("a.xz"), ".xz"));
        assert!(!has_suffix(OsStr::new(".xz"), ".xz"));
        assert!(!has_suffix(OsStr::new("dir/.xz"), ".xz"));
        assert!(!has_suffix(OsStr::new("a.xz.bak"), ".xz"));
        assert_eq!(has_suffix(OsStr::new("A.XZ"), ".xz"), cfg!(windows));
    }
}
