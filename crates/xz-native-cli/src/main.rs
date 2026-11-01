//! `xz-native`: an xz-compatible command line tool on the xz-native
//! library. It follows xz 5.8.4 (`src/xz` in `third_party/xz`) for the
//! options it supports, the file handling and the defaults, so its .xz
//! output is the same as xz's for the same options.

mod args;
mod chain;
mod coder;
mod files;
mod list;
mod names;
mod num;
mod progress;
mod ram;
mod signals;
mod sparse;

use args::{Action, Opts, V_ERROR, V_WARNING};
use std::ffi::OsString;
use std::process::ExitCode;
use xz_native::{FilterOptions, FilterStringFormat};

/// Like xz's `E_SUCCESS`, `E_ERROR` and `E_WARNING`.
pub const E_ERROR: u8 = 1;
const E_WARNING: u8 = 2;

pub const PROGRAM: &str = "xz-native";

/// The options and the exit status (`message.c`).
#[derive(Debug)]
pub struct Ctx {
    pub o: Opts,
    pub status: u8,
}

impl Ctx {
    pub fn error(&mut self, msg: impl std::fmt::Display) {
        if self.o.verbosity >= V_ERROR {
            eprintln!("{PROGRAM}: {msg}");
        }
        self.status = E_ERROR;
    }

    pub fn warning(&mut self, msg: impl std::fmt::Display) {
        if self.o.verbosity >= V_WARNING {
            eprintln!("{PROGRAM}: {msg}");
        }
        if !self.o.no_warn && self.status == 0 {
            self.status = E_WARNING;
        }
    }

    /// A message at the given verbosity that doesn't change the status.
    pub fn note(&self, level: i32, msg: impl std::fmt::Display) {
        if self.o.verbosity >= level {
            eprintln!("{PROGRAM}: {msg}");
        }
    }
}

/// Ends with an error before any file; `try_help` adds the pointer to
/// `--help` that xz gives after errors in the option syntax.
fn fatal(msg: &str, try_help: bool) -> ExitCode {
    eprintln!("{PROGRAM}: {msg}");
    if try_help {
        eprintln!("{PROGRAM}: Try '{PROGRAM} --help' for more information.");
    }
    ExitCode::from(E_ERROR)
}

fn main() -> ExitCode {
    let mut argv = std::env::args_os();
    let program = argv
        .next()
        .map(std::path::PathBuf::from)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let mut o = Opts::default();
    args::from_program_name(&program, &mut o);
    // XZ_DEFAULTS, then XZ_OPT, then the command line, as in xz.
    for var in ["XZ_DEFAULTS", "XZ_OPT"] {
        if let Some(value) = std::env::var_os(var) {
            let Some(value) = value.to_str() else {
                return fatal(&format!("{var}: invalid value"), false);
            };
            let words: Vec<OsString> = value.split_whitespace().map(OsString::from).collect();
            if let Err(e) = args::parse(&words, &mut o, Some(var)) {
                return fatal(&e.msg, e.try_help);
            }
        }
    }
    let rest: Vec<OsString> = argv.collect();
    match args::parse(&rest, &mut o, None) {
        Ok(Action::Run) => {}
        Ok(action @ (Action::Help | Action::LongHelp)) => {
            print!("{}", help(action == Action::LongHelp));
            return ExitCode::SUCCESS;
        }
        Ok(Action::FiltersHelp) => {
            // `message_filters_help()`.
            if !o.robot {
                print!(
                    "Filter chains are set using the --filters=FILTERS or --filters1=FILTERS ...
--filters9=FILTERS options. Each filter in the chain can be separated by
spaces or '--'. Alternatively a preset <0-9>[e] can be specified instead of
a filter chain.

The supported filters and their options are:
"
                );
            }
            let encoder = FilterStringFormat::new(FilterOptions::Encoder);
            println!("{}", encoder.list_filters(None, false).unwrap_or_default());
            return ExitCode::SUCCESS;
        }
        Ok(Action::InfoMemory) => {
            print!("{}", coder::Hardware::detect().info_memory(&o));
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!(
                "{PROGRAM} {} (compatible with XZ Utils {})",
                env!("CARGO_PKG_VERSION")
                    .split('+')
                    .next()
                    .unwrap_or_default(),
                xz_native::XZ_VERSION
            );
            return ExitCode::SUCCESS;
        }
        Err(e) => return fatal(&e.msg, e.try_help),
    }
    signals::init();
    let mut ctx = Ctx { o, status: 0 };
    let r = files::run(&mut ctx);
    // After a signal, end with it once the output is cleaned up.
    signals::exit_if_aborted();
    match r {
        Ok(()) => ExitCode::from(ctx.status),
        Err(e) => {
            ctx.error(e);
            ExitCode::from(E_ERROR)
        }
    }
}

/// `--help` (`long` false: the basic options) and `--long-help`, laid out
/// like xz's.
fn help(long: bool) -> String {
    let mut s = format!(
        "Usage: {PROGRAM} [OPTION]... [FILE]...
Compress or decompress FILEs in the .xz format (an xz-compatible tool
built on the pure Rust xz-native library).

Mandatory arguments to long options are mandatory for short options too.

Operation mode:
  -z, --compress      force compression
  -d, --decompress    force decompression
  -t, --test          test compressed file integrity
  -l, --list          list information about .xz files

Operation modifiers:
  -k, --keep          keep (don't delete) input files
  -f, --force         force overwrite of output file and (de)compress links
  -c, --stdout        write to standard output and don't delete input files
"
    );
    if long {
        s.push_str(
            "      --single-stream decompress only the first stream, and silently
                      ignore possible remaining input data
      --no-sparse     do not create sparse files when decompressing
  -S, --suffix=.SUF   use the suffix '.SUF' on compressed files
      --files[=FILE]  read filenames to process from FILE; if FILE is
                      omitted, filenames are read from the standard input;
                      filenames must be terminated with the newline character
      --files0[=FILE] like --files but use the null character as terminator
",
        );
    }
    s.push_str(
        "
Basic file format and compression options:
",
    );
    if long {
        s.push_str(
            "  -F, --format=FMT    file format to encode or decode; possible values are
                      'auto' (default), 'xz', 'lzma', 'lzip', and 'raw'
  -C, --check=CHECK   integrity check type: 'none' (use with caution),
                      'crc32', 'crc64' (default), or 'sha256'
      --ignore-check  don't verify the integrity check when decompressing
",
        );
    }
    s.push_str(
        "  -0 ... -9           compression preset; default is 6
  -e, --extreme       try to improve compression ratio by using more CPU time
  -T, --threads=NUM   use at most NUM threads; the default is 0 which uses
                      as many threads as there are processor cores
",
    );
    if long {
        s.push_str(
            "      --block-size=SIZE
                      start a new .xz block after every SIZE bytes of input
      --block-list=BLOCKS
                      start a new .xz block after the given comma-separated
                      intervals of uncompressed data; optionally, specify a
                      filter chain number (0-9) followed by a ':' before the
                      uncompressed data size
      --memlimit-compress=LIMIT
      --memlimit-decompress=LIMIT
      --memlimit-mt-decompress=LIMIT
  -M, --memlimit=LIMIT
                      set memory usage limit for compression, decompression,
                      threaded decompression, or all of these; LIMIT is in
                      bytes, % of RAM, or 0 for defaults
      --no-adjust     if compression settings exceed the memory usage limit,
                      give an error instead of adjusting the settings downwards
      --flush-timeout=TIMEOUT
                      when compressing, if more than TIMEOUT milliseconds has
                      passed since the previous flush and reading more input
                      would block, all pending data is flushed out

Custom filter chain for compression (alternative to using presets):
  --filters=FILTERS   set the filter chain using the liblzma filter string
                      syntax; use --filters-help for more information
  --filters1=FILTERS ... --filters9=FILTERS
                      set additional filter chains using the liblzma filter
                      string syntax to use with --block-list
  --filters-help      display more information about the liblzma filter string
                      syntax and exit

  --lzma1[=OPTS]      LZMA1 or LZMA2; OPTS is a comma-separated list of zero or
  --lzma2[=OPTS]      more of the following options (valid values; default):
                        preset=PRE reset options to a preset (0-9[e])
                        dict=NUM   dictionary size (4KiB - 1536MiB; 8MiB)
                        lc=NUM     number of literal context bits (0-4; 3)
                        lp=NUM     number of literal position bits (0-4; 0)
                        pb=NUM     number of position bits (0-4; 2)
                        mode=MODE  compression mode (fast, normal; normal)
                        nice=NUM   nice length of a match (2-273; 64)
                        mf=NAME    match finder (hc3, hc4, bt2, bt3, bt4; bt4)
                        depth=NUM  maximum search depth; 0=automatic (default)
  --x86[=OPTS]        x86 BCJ filter (32-bit and 64-bit)
  --arm[=OPTS]        ARM BCJ filter
  --armthumb[=OPTS]   ARM-Thumb BCJ filter
  --arm64[=OPTS]      ARM64 BCJ filter
  --powerpc[=OPTS]    PowerPC BCJ filter (big endian only)
  --ia64[=OPTS]       IA-64 (Itanium) BCJ filter
  --sparc[=OPTS]      SPARC BCJ filter
  --riscv[=OPTS]      RISC-V BCJ filter
                      Valid OPTS for all BCJ filters:
                        start=NUM  start offset for conversions (default=0)
  --delta[=OPTS]      Delta filter; valid OPTS (valid values; default):
                        dist=NUM   distance between bytes being subtracted
                                   from each other (1-256; 1)
",
        );
    }
    s.push_str(
        "
Other options:
  -q, --quiet         suppress warnings; specify twice to suppress errors too
  -v, --verbose       be verbose; specify twice for even more verbose
",
    );
    if long {
        s.push_str(
            "  -Q, --no-warn       make warnings not affect the exit status
      --robot         use machine-parsable messages (useful for scripts)

      --info-memory   display the total amount of RAM and the currently active
                      memory usage limits, and exit
  -h, --help          display the short help (lists only the basic options)
  -H, --long-help     display this long help and exit
",
        );
    } else {
        s.push_str(
            "  -h, --help          display this short help and exit
  -H, --long-help     display the long help (lists also the advanced options)
",
        );
    }
    s.push_str(
        "  -V, --version       display the version number and exit

With no FILE, or when FILE is -, read standard input.
",
    );
    s
}
