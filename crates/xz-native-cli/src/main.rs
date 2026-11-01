//! `xz-native`: an xz-compatible command line tool on the xz-native
//! library. It follows xz 5.8.4 (`src/xz` in `third_party/xz`) for the
//! options it supports, the file handling and the defaults, so its .xz
//! output is the same as xz's for the same options.

mod args;
mod chain;
mod coder;
mod files;
mod list;
mod num;
mod ram;

use args::{Action, Opts, V_ERROR, V_WARNING};
use std::ffi::OsString;
use std::process::ExitCode;

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
        Ok(Action::Help) => {
            print!("{}", help());
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!(
                "{PROGRAM} {} (compatible with XZ Utils 5.8.4)",
                env!("CARGO_PKG_VERSION")
                    .split('+')
                    .next()
                    .unwrap_or_default()
            );
            return ExitCode::SUCCESS;
        }
        Err(e) => return fatal(&e.msg, e.try_help),
    }
    let mut ctx = Ctx { o, status: 0 };
    match files::run(&mut ctx) {
        Ok(()) => ExitCode::from(ctx.status),
        Err(e) => {
            ctx.error(e);
            ExitCode::from(E_ERROR)
        }
    }
}

fn help() -> String {
    format!(
        "Usage: {PROGRAM} [OPTION]... [FILE]...
Compress or decompress FILEs in the .xz format (an xz-compatible tool
built on the pure Rust xz-native library).

  -z, --compress      force compression
  -d, --decompress    force decompression
  -t, --test          test compressed file integrity
  -k, --keep          keep (don't delete) input files
  -f, --force         force overwrite of output file and (de)compress links
  -c, --stdout        write to standard output and don't delete input files
      --single-stream decompress only the first stream, and silently
                      ignore possible remaining input data
  -0 ... -9           compression preset; default is 6
  -e, --extreme       try to improve compression ratio by using more CPU time
  -T, --threads=NUM   use at most NUM threads; the default is 0 which uses
                      as many threads as there are processor cores
  -F, --format=FMT    file format to encode or decode; possible values are
                      'auto' (default), 'xz', 'lzma', and 'lzip'
  -C, --check=CHECK   integrity check type: 'none', 'crc32', 'crc64' (default),
                      or 'sha256'
      --ignore-check  don't verify the integrity check when decompressing
      --block-size=SIZE
                      start a new .xz block after every SIZE bytes of input
      --memlimit-compress=LIMIT
      --memlimit-decompress=LIMIT
      --memlimit-mt-decompress=LIMIT
  -M, --memlimit=LIMIT
                      set memory usage limit for compression, decompression,
                      threaded decompression, or all of these; LIMIT is in
                      bytes, % of RAM, or 0 for defaults
      --no-adjust     if compression settings exceed the memory usage limit,
                      give an error instead of adjusting the settings downwards
  -S, --suffix=.SUF   use the suffix '.SUF' on compressed files
  -q, --quiet         suppress warnings; specify twice to suppress errors too
  -v, --verbose       be verbose
  -Q, --no-warn       make warnings not affect the exit status
  -h, --help          display this help and exit
  -V, --version       display the version number and exit

With no FILE, or when FILE is -, read standard input.
  -l, --list          list information about .xz files (-v, -vv for more)
      --robot         use machine-parsable messages (with --list)

      --lzma1[=OPTS], --lzma2[=OPTS], --x86[=OPTS], --arm[=OPTS],
      --armthumb[=OPTS], --arm64[=OPTS], --powerpc[=OPTS], --ia64[=OPTS],
      --sparc[=OPTS], --riscv[=OPTS], --delta[=OPTS]
                      add a filter to a custom filter chain (as in xz)
      --filters=FILTERS
                      set the filter chain with a liblzma filter string

      --filters1=FILTERS ... --filters9=FILTERS
                      filter chains for --block-list
      --block-list=BLOCKS
                      start a new .xz block after the given comma-separated
                      intervals of uncompressed data; optionally, specify a
                      filter chain number (0-9) followed by a ':' before the
                      uncompressed data size

Not supported yet: --format=raw, --files.
"
    )
}
