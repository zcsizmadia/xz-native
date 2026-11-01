//! Command line parsing like xz's `args.c`: getopt_long rules (combined
//! short options such as `-9ec`, `-T4` or `-T 4`, long options as
//! `--name=value` or `--name value`, unique prefixes of long names, `--`),
//! `XZ_DEFAULTS` and `XZ_OPT`, and the mode implied by the program name.

use crate::chain::{self, Item};
use std::ffi::OsString;
use xz_native::{Check, RawOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Compress,
    Decompress,
    Test,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Auto,
    Xz,
    Lzma,
    Lzip,
    Raw,
}

/// `--threads` (`hardware_threads_set()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threads {
    /// 0, the default: one per CPU, multithreaded mode even with one.
    Auto,
    /// 1: single-threaded mode.
    Single,
    /// `+1`, or more than one: multithreaded mode with this many.
    Mt(u32),
}

/// A memory limit option: bytes (0 = no limit) or a percentage of RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Bytes(u64),
    Percent(u64),
}

/// Message verbosity (`enum message_verbosity`).
pub const V_SILENT: i32 = 0;
pub const V_ERROR: i32 = 1;
pub const V_WARNING: i32 = 2;
pub const V_VERBOSE: i32 = 3;
pub const V_DEBUG: i32 = 4;

#[derive(Debug, Clone)]
pub struct Opts {
    pub mode: Mode,
    pub format: Format,
    pub stdout: bool,
    pub keep: bool,
    pub force: bool,
    pub preset: u32,
    pub extreme: bool,
    pub check: Option<Check>,
    pub threads: Threads,
    pub block_size: u64,
    pub memlimit_compress: Option<Limit>,
    pub memlimit_decompress: Option<Limit>,
    pub memlimit_mt_decompress: Option<Limit>,
    pub auto_adjust: bool,
    /// `--flush-timeout`: milliseconds without input before a sync flush
    /// (0: never).
    pub flush_timeout: u64,
    pub single_stream: bool,
    /// `--no-sparse`: don't make holes in decompressed files.
    pub no_sparse: bool,
    pub ignore_check: bool,
    pub suffix: Option<String>,
    /// A custom filter chain; empty for the preset.
    pub chain: Vec<Item>,
    /// The chain came from `--filters=`, so the next filter option
    /// starts a new one.
    pub chain_from_str: bool,
    /// `--filters1..9=`: the chains `--block-list` can name (index 0
    /// unused; chain 0 is `chain` or the preset).
    pub block_chains: [Option<Vec<Item>>; 10],
    /// `--block-list`: `(chain, size)` entries, a size of `u64::MAX` for
    /// the rest of the input.
    pub block_list: Option<Vec<(usize, u64)>>,
    /// `--files` or `--files0`: more names from a file or standard input.
    pub files_from: Option<crate::names::FilesFrom>,
    pub verbosity: i32,
    /// `--no-warn`: warnings don't change the exit status.
    pub no_warn: bool,
    /// `--robot`: machine-readable `--list` output.
    pub robot: bool,
    pub files: Vec<OsString>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mode: Mode::Compress,
            format: Format::Auto,
            stdout: false,
            keep: false,
            force: false,
            preset: 6,
            extreme: false,
            check: None,
            threads: Threads::Auto,
            block_size: 0,
            memlimit_compress: None,
            memlimit_decompress: None,
            memlimit_mt_decompress: None,
            auto_adjust: true,
            flush_timeout: 0,
            single_stream: false,
            no_sparse: false,
            ignore_check: false,
            suffix: None,
            chain: Vec::new(),
            chain_from_str: false,
            block_chains: Default::default(),
            block_list: None,
            files_from: None,
            verbosity: V_WARNING,
            no_warn: false,
            robot: false,
            files: Vec::new(),
        }
    }
}

/// A command line error. Errors of the option syntax (getopt's) are
/// followed by a pointer to `--help`, as in xz.
#[derive(Debug)]
pub struct ArgError {
    pub msg: String,
    pub try_help: bool,
}

impl From<String> for ArgError {
    fn from(msg: String) -> Self {
        Self {
            msg,
            try_help: false,
        }
    }
}

fn getopt(msg: String) -> ArgError {
    ArgError {
        msg,
        try_help: true,
    }
}

/// A `--filters=` or `--filtersN=` string, with xz's error message.
fn filters_str(s: &str, n: usize) -> Result<Vec<Item>, String> {
    let raw = if n == 0 {
        RawOptions::from_filter_str(s)
    } else {
        RawOptions::from_xz_filter_str(s)
    };
    raw.map(chain::from_raw).map_err(|e| {
        let p = crate::PROGRAM;
        let num = if n == 0 { String::new() } else { n.to_string() };
        // `str_to_filters()` in xz's coder.c: the string, a caret under
        // the error, then the message.
        format!(
            "Error in --filters{num}=FILTERS option:
{p}: {s}
{p}: {:pos$}^
{p}: {}",
            "",
            e.message(),
            pos = e.position()
        )
    })
}

/// `parse_block_list()`: comma-separated sizes, each optionally `N:` for
/// filter chain N; an empty entry repeats the previous one, and 0, last,
/// means the rest of the input.
fn block_list(s: &str) -> Result<Vec<(usize, u64)>, String> {
    if s.is_empty() || s.starts_with(',') {
        return Err(format!("{s}: Invalid argument to --block-list"));
    }
    let parts: Vec<&str> = s.split(',').collect();
    let mut list: Vec<(usize, u64)> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        let b = part.as_bytes();
        let (chain, size) = if b.len() >= 2 && b[0].is_ascii_digit() && b[1] == b':' {
            if b.len() == 2 {
                return Err(format!(
                    "In --block-list, block size is missing after filter chain number '{}:'",
                    b[0] as char
                ));
            }
            ((b[0] - b'0') as usize, &part[2..])
        } else {
            (0, *part)
        };
        if size.is_empty() {
            // Only possible after a comma, so there is a previous entry.
            list.push(list[i - 1]);
            continue;
        }
        let mut n = parse_u64("block-list", size, 0, u64::MAX)?;
        if n == 0 {
            if i + 1 != parts.len() {
                return Err("0 can only be used as the last element in --block-list".into());
            }
            n = u64::MAX;
        }
        list.push((chain, n));
    }
    Ok(list)
}

/// `coder_add_filter()`: appends to the custom chain, which replaces the
/// preset.
fn add_filter(o: &mut Opts, item: Item) -> Result<(), String> {
    if o.chain_from_str {
        o.chain.clear();
        o.chain_from_str = false;
    }
    if o.chain.len() == 4 {
        return Err("Maximum number of filters is four".into());
    }
    o.chain.push(item);
    o.preset = 6;
    o.extreme = false;
    Ok(())
}

/// What the command line asks for besides coding files.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Run,
    Help,
    LongHelp,
    Version,
    InfoMemory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HasArg {
    No,
    Yes,
    /// Only as `--name=value`.
    Optional,
}

/// Long options: name, argument, and the short option or internal key.
const LONG: &[(&str, HasArg, &str)] = &[
    ("compress", HasArg::No, "z"),
    ("decompress", HasArg::No, "d"),
    ("uncompress", HasArg::No, "d"),
    ("test", HasArg::No, "t"),
    ("list", HasArg::No, "l"),
    ("keep", HasArg::No, "k"),
    ("force", HasArg::No, "f"),
    ("stdout", HasArg::No, "c"),
    ("to-stdout", HasArg::No, "c"),
    ("single-stream", HasArg::No, "single-stream"),
    ("no-sparse", HasArg::No, "no-sparse"),
    ("suffix", HasArg::Yes, "S"),
    ("format", HasArg::Yes, "F"),
    ("check", HasArg::Yes, "C"),
    ("ignore-check", HasArg::No, "ignore-check"),
    ("block-size", HasArg::Yes, "block-size"),
    ("memlimit-compress", HasArg::Yes, "memlimit-compress"),
    ("memlimit-decompress", HasArg::Yes, "memlimit-decompress"),
    (
        "memlimit-mt-decompress",
        HasArg::Yes,
        "memlimit-mt-decompress",
    ),
    ("memlimit", HasArg::Yes, "M"),
    ("memory", HasArg::Yes, "M"),
    ("no-adjust", HasArg::No, "no-adjust"),
    ("flush-timeout", HasArg::Yes, "flush-timeout"),
    ("threads", HasArg::Yes, "T"),
    ("extreme", HasArg::No, "e"),
    ("fast", HasArg::No, "0"),
    ("best", HasArg::No, "9"),
    ("quiet", HasArg::No, "q"),
    ("verbose", HasArg::No, "v"),
    ("no-warn", HasArg::No, "Q"),
    ("robot", HasArg::No, "robot"),
    ("help", HasArg::No, "h"),
    ("long-help", HasArg::No, "H"),
    ("version", HasArg::No, "V"),
    ("filters", HasArg::Yes, "filters"),
    ("filters1", HasArg::Yes, "filters1"),
    ("filters2", HasArg::Yes, "filters2"),
    ("filters3", HasArg::Yes, "filters3"),
    ("filters4", HasArg::Yes, "filters4"),
    ("filters5", HasArg::Yes, "filters5"),
    ("filters6", HasArg::Yes, "filters6"),
    ("filters7", HasArg::Yes, "filters7"),
    ("filters8", HasArg::Yes, "filters8"),
    ("filters9", HasArg::Yes, "filters9"),
    ("block-list", HasArg::Yes, "block-list"),
    ("files", HasArg::Optional, "files"),
    ("files0", HasArg::Optional, "files0"),
    ("info-memory", HasArg::No, "info-memory"),
    ("lzma1", HasArg::Optional, "lzma1"),
    ("lzma2", HasArg::Optional, "lzma2"),
    ("x86", HasArg::Optional, "x86"),
    ("powerpc", HasArg::Optional, "powerpc"),
    ("ia64", HasArg::Optional, "ia64"),
    ("arm", HasArg::Optional, "arm"),
    ("armthumb", HasArg::Optional, "armthumb"),
    ("arm64", HasArg::Optional, "arm64"),
    ("sparc", HasArg::Optional, "sparc"),
    ("riscv", HasArg::Optional, "riscv"),
    ("delta", HasArg::Optional, "delta"),
];

/// Short options and whether they take an argument.
fn short_has_arg(c: char) -> Option<HasArg> {
    match c {
        'z' | 'd' | 't' | 'l' | 'k' | 'f' | 'c' | 'e' | 'q' | 'v' | 'Q' | 'h' | 'H' | 'V' => {
            Some(HasArg::No)
        }
        '0'..='9' => Some(HasArg::No),
        'S' | 'F' | 'C' | 'T' | 'M' => Some(HasArg::Yes),
        _ => None,
    }
}

/// The settings implied by the program name, like xz's `main()`:
/// `unxz` and `xzcat` decompress, `lzma`, `unlzma` and `lzcat` use .lzma.
pub fn from_program_name(name: &str, o: &mut Opts) {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    if name.contains("xzcat") {
        o.mode = Mode::Decompress;
        o.stdout = true;
    } else if name.contains("unxz") {
        o.mode = Mode::Decompress;
    } else if name.contains("lzcat") {
        o.format = Format::Lzma;
        o.mode = Mode::Decompress;
        o.stdout = true;
    } else if name.contains("unlzma") {
        o.format = Format::Lzma;
        o.mode = Mode::Decompress;
    } else if name.contains("lzma") {
        o.format = Format::Lzma;
    }
}

/// Parses `args` (without the program name) into `o`. With `env_name`,
/// the arguments come from that environment variable, which may not name
/// files. Returns what to do, or an error message.
pub fn parse(args: &[OsString], o: &mut Opts, env_name: Option<&str>) -> Result<Action, ArgError> {
    let mut action = Action::Run;
    let mut i = 0;
    let mut only_files = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let s = arg.to_str();
        if only_files || s == Some("-") || !s.is_some_and(|s| s.starts_with('-')) {
            if let Some(var) = env_name {
                return Err(format!("{var}: Non-option arguments are not allowed").into());
            }
            o.files.push(arg.clone());
            continue;
        }
        let s = s.unwrap();
        if s == "--" {
            only_files = true;
            continue;
        }
        if let Some(long) = s.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let (full, has_arg, key) = find_long(name).map_err(getopt)?;
            let value = match (has_arg, inline) {
                (HasArg::No, Some(_)) => {
                    return Err(getopt(format!(
                        "option '--{full}' doesn't allow an argument"
                    )));
                }
                (HasArg::Optional, v) => v.map(str::to_owned),
                (HasArg::No, None) => None,
                (HasArg::Yes, Some(v)) => Some(v.to_owned()),
                (HasArg::Yes, None) => {
                    Some(next_value(args, &mut i, &format!("--{full}")).map_err(getopt)?)
                }
            };
            apply(key, value.as_deref(), o, &mut action)?;
            // --help, --version and --info-memory act at once, as in xz.
            if action != Action::Run {
                return Ok(action);
            }
            continue;
        }
        // Short options, possibly combined.
        for (pos, c) in s[1..].char_indices() {
            let has_arg =
                short_has_arg(c).ok_or_else(|| getopt(format!("invalid option -- '{c}'")))?;
            if has_arg == HasArg::Yes {
                let rest = &s[1 + pos + c.len_utf8()..];
                let value = if rest.is_empty() {
                    next_value(args, &mut i, &format!("-{c}")).map_err(getopt)?
                } else {
                    rest.to_owned()
                };
                apply(&c.to_string(), Some(&value), o, &mut action)?;
                if action != Action::Run {
                    return Ok(action);
                }
                break;
            }
            apply(&c.to_string(), None, o, &mut action)?;
            if action != Action::Run {
                return Ok(action);
            }
        }
    }
    Ok(action)
}

fn next_value(args: &[OsString], i: &mut usize, opt: &str) -> Result<String, String> {
    let v = args
        .get(*i)
        .ok_or_else(|| format!("option '{opt}' requires an argument"))?;
    *i += 1;
    v.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{opt}: invalid argument"))
}

/// An exact long name, or the only one it is a prefix of.
fn find_long(name: &str) -> Result<(&'static str, HasArg, &'static str), String> {
    if let Some(&e) = LONG.iter().find(|e| e.0 == name) {
        return Ok(e);
    }
    let matches: Vec<_> = LONG.iter().filter(|e| e.0.starts_with(name)).collect();
    // Aliases of the same option don't make a prefix ambiguous.
    match matches.as_slice() {
        [] => Err(format!("unrecognized option '--{name}'")),
        [first, rest @ ..] if rest.iter().all(|e| e.2 == first.2) => Ok(**first),
        _ => Err(format!("option '--{name}' is ambiguous")),
    }
}

fn apply(key: &str, value: Option<&str>, o: &mut Opts, action: &mut Action) -> Result<(), String> {
    let v = || value.unwrap_or_default();
    match key {
        "z" => o.mode = Mode::Compress,
        "d" => o.mode = Mode::Decompress,
        "t" => o.mode = Mode::Test,
        "l" => o.mode = Mode::List,
        "robot" => o.robot = true,
        "k" => o.keep = true,
        "f" => o.force = true,
        "c" => o.stdout = true,
        "e" => {
            o.extreme = true;
            o.chain.clear();
        }
        "filters" => {
            o.chain = filters_str(v(), 0)?;
            o.chain_from_str = true;
            o.preset = 6;
            o.extreme = false;
        }
        k if k.starts_with("filters") => {
            let n = (k.as_bytes()[7] - b'0') as usize;
            o.block_chains[n] = Some(filters_str(v(), n)?);
        }
        "block-list" => o.block_list = Some(block_list(v())?),
        "files" | "files0" => {
            if o.files_from.is_some() {
                return Err("Only one file can be specified with '--files' or '--files0'.".into());
            }
            let delim = if key == "files" { b'\n' } else { 0 };
            o.files_from = Some((value.map(str::to_owned), delim));
        }
        "lzma1" => add_filter(o, Item::Lzma1(chain::lzma(value)?))?,
        "lzma2" => add_filter(o, Item::Lzma2(chain::lzma(value)?))?,
        "delta" => add_filter(o, Item::Delta(chain::delta(value)?))?,
        k if chain::bcj_kind(k).is_some() => add_filter(
            o,
            Item::Bcj(chain::bcj_kind(k).unwrap(), chain::bcj(value)?),
        )?,
        "q" => o.verbosity = (o.verbosity - 1).max(V_SILENT),
        "v" => o.verbosity = (o.verbosity + 1).min(V_DEBUG),
        "Q" => o.no_warn = true,
        "h" => *action = Action::Help,
        "H" => *action = Action::LongHelp,
        "info-memory" => *action = Action::InfoMemory,
        "V" => *action = Action::Version,
        "single-stream" => o.single_stream = true,
        "no-sparse" => o.no_sparse = true,
        "ignore-check" => o.ignore_check = true,
        "no-adjust" => o.auto_adjust = false,
        "flush-timeout" => o.flush_timeout = parse_u64("flush-timeout", v(), 0, u64::MAX)?,
        "S" => {
            let s = v();
            if s.is_empty() || s.contains('/') || (cfg!(windows) && s.contains(['\\', ':'])) {
                return Err(format!("{s}: Invalid filename suffix"));
            }
            o.suffix = Some(s.to_owned());
        }
        "F" => {
            o.format = match v() {
                "auto" => Format::Auto,
                "xz" => Format::Xz,
                "lzma" | "alone" => Format::Lzma,
                "lzip" => Format::Lzip,
                "raw" => Format::Raw,
                s => return Err(format!("{s}: Unknown file format type")),
            }
        }
        "C" => {
            o.check = Some(match v() {
                "none" => Check::None,
                "crc32" => Check::Crc32,
                "crc64" => Check::Crc64,
                "sha256" => Check::Sha256,
                s => return Err(format!("{s}: Unsupported integrity check type")),
            })
        }
        "T" => {
            let s = v();
            let plus = s.starts_with('+');
            let n = parse_u64("threads", s.strip_prefix('+').unwrap_or(s), 0, 16384)? as u32;
            o.threads = match n {
                0 => Threads::Auto,
                1 if !plus => Threads::Single,
                n => Threads::Mt(n),
            };
        }
        "block-size" => o.block_size = parse_u64("block-size", v(), 1, u64::MAX >> 1)?,
        "M" => {
            let l = parse_limit("memlimit", v())?;
            o.memlimit_compress = Some(l);
            o.memlimit_decompress = Some(l);
            o.memlimit_mt_decompress = Some(l);
        }
        "memlimit-compress" => o.memlimit_compress = Some(parse_limit(key, v())?),
        "memlimit-decompress" => o.memlimit_decompress = Some(parse_limit(key, v())?),
        "memlimit-mt-decompress" => o.memlimit_mt_decompress = Some(parse_limit(key, v())?),
        d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
            o.preset = u32::from(d.as_bytes()[0] - b'0');
            o.extreme = false;
            o.chain.clear();
        }
        _ => unreachable!("option {key}"),
    }
    Ok(())
}

/// `str_to_uint64()`: a decimal integer with an optional KiB, MiB or GiB
/// multiplier (also `k`, `KB`, `Ki` and so on), or `max`.
pub fn parse_u64(name: &str, value: &str, min: u64, max: u64) -> Result<u64, String> {
    let value = value.trim_start_matches([' ', '\t']);
    if value == "max" {
        return Ok(max);
    }
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(format!(
            "{value}: Value is not a non-negative decimal integer"
        ));
    }
    let range = || format!("Value of the option '{name}' must be in the range [{min}, {max}]");
    let mut n: u64 = value[..digits].parse().map_err(|_| range())?;
    let suffix = &value[digits..];
    if !suffix.is_empty() {
        let mult = match suffix.as_bytes()[0] {
            b'k' | b'K' => 1u64 << 10,
            b'm' | b'M' => 1 << 20,
            b'g' | b'G' => 1 << 30,
            _ => 0,
        };
        let mult = if matches!(&suffix[1..], "" | "i" | "iB" | "B") {
            mult
        } else {
            0
        };
        if mult == 0 {
            return Err(format!(
                "{suffix}: Invalid multiplier suffix. Valid suffixes are 'KiB' (2^10), \
                 'MiB' (2^20), and 'GiB' (2^30)."
            ));
        }
        n = n.checked_mul(mult).ok_or_else(range)?;
    }
    if n < min || n > max {
        return Err(range());
    }
    Ok(n)
}

/// `parse_memlimit()`: bytes, or `N%` of the RAM.
fn parse_limit(name: &str, value: &str) -> Result<Limit, String> {
    match value.strip_suffix('%') {
        Some(p) => Ok(Limit::Percent(parse_u64(&format!("{name}%"), p, 1, 100)?)),
        None => Ok(Limit::Bytes(parse_u64(name, value, 0, u64::MAX)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<(Opts, Action), String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut o = Opts::default();
        let a = parse(&args, &mut o, None).map_err(|e| e.msg)?;
        Ok((o, a))
    }

    #[test]
    fn combined_short_options() {
        let (o, _) = parse_strs(&["-9ekcT4", "file"]).unwrap();
        assert_eq!(
            (o.preset, o.extreme, o.keep, o.stdout),
            (9, true, true, true)
        );
        assert_eq!(o.threads, Threads::Mt(4));
        assert_eq!(o.files, ["file"]);
        // A digit after -e resets it, as in xz.
        let (o, _) = parse_strs(&["-e3"]).unwrap();
        assert_eq!((o.preset, o.extreme), (3, false));
        let (o, _) = parse_strs(&["-T", "1", "-dq", "-"]).unwrap();
        assert_eq!(
            (o.threads, o.mode, o.verbosity),
            (Threads::Single, Mode::Decompress, 1)
        );
        assert_eq!(o.files, ["-"]);
        let (o, _) = parse_strs(&["-T+1"]).unwrap();
        assert_eq!(o.threads, Threads::Mt(1));
    }

    #[test]
    fn long_options() {
        let (o, _) =
            parse_strs(&["--decomp", "--format=lzma", "--check", "sha256", "--", "-x"]).unwrap();
        assert_eq!(
            (o.mode, o.format, o.check),
            (Mode::Decompress, Format::Lzma, Some(Check::Sha256))
        );
        assert_eq!(o.files, ["-x"]);
        assert!(
            parse_strs(&["--memlimit-"])
                .unwrap_err()
                .contains("ambiguous")
        );
        // Aliases: --memlimit and --memory.
        let (o, _) = parse_strs(&["--memory=50%"]).unwrap();
        assert_eq!(o.memlimit_compress, Some(Limit::Percent(50)));
        assert!(parse_strs(&["--keep=1"]).is_err());
        assert!(parse_strs(&["--nope"]).is_err());
        assert_eq!(parse_strs(&["--vers"]).unwrap().1, Action::Version);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_u64("x", "8MiB", 0, u64::MAX), Ok(8 << 20));
        assert_eq!(parse_u64("x", "3k", 0, u64::MAX), Ok(3 << 10));
        assert_eq!(parse_u64("x", "2GB", 0, u64::MAX), Ok(2 << 30));
        assert_eq!(parse_u64("x", "max", 0, 77), Ok(77));
        assert!(parse_u64("x", "1KX", 0, u64::MAX).is_err());
        assert!(parse_u64("x", "-1", 0, u64::MAX).is_err());
        assert!(parse_u64("x", "20000", 0, 16384).is_err());
        assert!(parse_u64("x", "99999999999999999999", 0, u64::MAX).is_err());
    }

    #[test]
    fn program_names() {
        for (name, mode, format, stdout) in [
            ("xz-native", Mode::Compress, Format::Auto, false),
            ("unxz", Mode::Decompress, Format::Auto, false),
            ("XZCAT.EXE", Mode::Decompress, Format::Auto, true),
            ("lzma", Mode::Compress, Format::Lzma, false),
            ("unlzma", Mode::Decompress, Format::Lzma, false),
            ("lzcat", Mode::Decompress, Format::Lzma, true),
        ] {
            let mut o = Opts::default();
            from_program_name(name, &mut o);
            assert_eq!(
                (o.mode, o.format, o.stdout),
                (mode, format, stdout),
                "{name}"
            );
        }
    }

    #[test]
    fn env_rejects_files() {
        let mut o = Opts::default();
        let args = [OsString::from("-9"), OsString::from("file")];
        assert!(parse(&args, &mut o, Some("XZ_OPT")).is_err());
    }
}
