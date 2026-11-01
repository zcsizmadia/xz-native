//! Filter chains as strings, like liblzma's `lzma_str_to_filters()` and
//! `lzma_str_from_filters()` (`string_conversion.c`): the syntax of
//! `xz --filters=`, the same error messages and positions, and the same
//! output.

use super::{BcjKind, Filter};
use crate::options::{LzmaOptions, MatchFinder, Mode, PRESET_EXTREME};
use crate::raw::{RawMethod, RawOptions};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// An error in a filter string: liblzma's message and the byte position
/// in the string where it was found (`error_pos`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterStringError {
    position: usize,
    message: &'static str,
}

impl FilterStringError {
    fn new(position: usize, message: &'static str) -> Self {
        Self { position, message }
    }

    /// Byte offset in the string, as liblzma's `error_pos`.
    pub fn position(&self) -> usize {
        self.position
    }

    /// liblzma's message, such as `"Unknown filter name"`.
    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for FilterStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.position)
    }
}

impl core::error::Error for FilterStringError {}

/// Which options [`RawOptions::to_filter_string`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FilterOptions {
    /// Only the filter names.
    #[default]
    Names,
    /// All options the encoder uses (`LZMA_STR_ENCODER`).
    Encoder,
    /// The options the decoder needs (`LZMA_STR_DECODER`): the dictionary
    /// size, for LZMA1 also lc, lp and pb, and the BCJ start offset.
    Decoder,
}

/// How [`RawOptions::to_filter_string`] writes a chain (liblzma's
/// `LZMA_STR_*` flags of `lzma_str_from_filters()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FilterStringFormat {
    /// The options to write.
    pub options: FilterOptions,
    /// `--name=options` as on xz's command line (`LZMA_STR_GETOPT_LONG`)
    /// instead of `name:options`.
    pub getopt_long: bool,
    /// No spaces between filters; the second and later filters start with
    /// `--` (`LZMA_STR_NO_SPACES`).
    pub no_spaces: bool,
}

crate::options::setters!(FilterStringFormat {
    options: FilterOptions,
    getopt_long: bool,
    no_spaces: bool,
});

impl FilterStringFormat {
    /// The given options, in liblzma's default layout.
    pub fn new(options: FilterOptions) -> Self {
        Self {
            options,
            ..Self::default()
        }
    }
}

const NAMES_BCJ: [(&str, BcjKind); 8] = [
    ("x86", BcjKind::X86),
    ("arm", BcjKind::Arm),
    ("armthumb", BcjKind::ArmThumb),
    ("arm64", BcjKind::Arm64),
    ("riscv", BcjKind::RiscV),
    ("powerpc", BcjKind::PowerPc),
    ("ia64", BcjKind::Ia64),
    ("sparc", BcjKind::Sparc),
];

fn bcj_name(kind: BcjKind) -> &'static str {
    NAMES_BCJ
        .iter()
        .find(|(_, k)| *k == kind)
        .map_or("x86", |(n, _)| n)
}

/// A filter of a parsed chain.
enum Item {
    Filter(Filter),
    Lzma(LzmaOptions, bool),
}

/// `parse_lzma12_preset()`; an error comes with its offset in `v`.
fn preset(v: &str) -> Result<u32, (usize, &'static str)> {
    let b = v.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_digit) {
        return Err((0, "Unsupported preset"));
    }
    let mut p = u32::from(b[0] - b'0');
    for (i, &c) in b.iter().enumerate().skip(1) {
        if c != b'e' {
            return Err((i, "Unsupported flag in the preset"));
        }
        p |= PRESET_EXTREME;
    }
    Ok(p)
}

/// A decimal value, with a KiB/MiB/GiB suffix where `suffix` allows it;
/// a bad suffix is an error at the suffix, as in liblzma.
fn number(v: &str, suffix: bool, min: u32, max: u32) -> Result<u32, OptError> {
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err("Value is not a non-negative decimal integer".into());
    }
    let mut n: u32 = v[..digits]
        .parse()
        .map_err(|_| OptError(0, "Value out of range"))?;
    let rest = &v[digits..];
    if !rest.is_empty() {
        if !suffix {
            return Err(OptError(
                digits,
                "This option does not support any multiplier suffixes",
            ));
        }
        let shift = match rest.as_bytes()[0] {
            b'k' | b'K' => 10,
            b'm' | b'M' => 20,
            b'g' | b'G' => 30,
            _ => {
                return Err(OptError(
                    digits,
                    "Invalid multiplier suffix (KiB, MiB, or GiB)",
                ));
            }
        };
        if !matches!(&rest[1..], "" | "i" | "B" | "iB") {
            return Err(OptError(
                digits,
                "Invalid multiplier suffix (KiB, MiB, or GiB)",
            ));
        }
        if n > u32::MAX >> shift {
            return Err("Value out of range".into());
        }
        n <<= shift;
    }
    if n < min || n > max {
        return Err("Value out of range".into());
    }
    Ok(n)
}

/// An error in an option: its offset in the value, and the message.
struct OptError(usize, &'static str);

impl From<&'static str> for OptError {
    fn from(m: &'static str) -> Self {
        Self(0, m)
    }
}

impl From<(usize, &'static str)> for OptError {
    fn from((off, m): (usize, &'static str)) -> Self {
        Self(off, m)
    }
}

/// `parse_options()`: `name=value` pairs separated by commas, starting at
/// byte `pos`. Errors in a value point at the value.
fn options(
    s: &str,
    pos: usize,
    mut set: impl FnMut(&str, &str) -> Result<(), OptError>,
) -> Result<(), FilterStringError> {
    let mut at = pos;
    for pair in s.split(',') {
        if !pair.is_empty() {
            let Some((name, value)) = pair.split_once('=').filter(|(n, _)| !n.is_empty()) else {
                return Err(FilterStringError::new(
                    at,
                    "Options must be 'name=value' pairs separated with commas",
                ));
            };
            let value_at = at + name.len() + 1;
            if value.is_empty() {
                return Err(FilterStringError::new(
                    value_at,
                    "Option value cannot be empty",
                ));
            }
            set(name, value).map_err(|OptError(off, m)| {
                let p = if m == "Unknown option name" {
                    at
                } else {
                    value_at + off
                };
                FilterStringError::new(p, m)
            })?;
        }
        at += pair.len() + 1;
    }
    Ok(())
}

fn mode(v: &str) -> Option<Mode> {
    match v {
        "fast" => Some(Mode::Fast),
        "normal" => Some(Mode::Normal),
        _ => None,
    }
}

fn match_finder(v: &str) -> Option<MatchFinder> {
    Some(match v {
        "hc3" => MatchFinder::Hc3,
        "hc4" => MatchFinder::Hc4,
        "bt2" => MatchFinder::Bt2,
        "bt3" => MatchFinder::Bt3,
        "bt4" => MatchFinder::Bt4,
        _ => return None,
    })
}

/// `str_to_filters()`.
fn parse(s: &str, only_xz: bool) -> Result<RawOptions, FilterStringError> {
    let err = FilterStringError::new;
    let mut pos = s.len() - s.trim_start_matches(' ').len();
    if pos == s.len() {
        return Err(err(
            pos,
            "Empty string is not allowed, try '6' if a default value is needed",
        ));
    }
    let rest = &s[pos..];
    // A preset alone, such as "6e" or "-9".
    if rest.as_bytes()[0].is_ascii_digit()
        || (rest.starts_with('-') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
    {
        let p = rest.strip_prefix('-').unwrap_or(rest);
        pos += rest.len() - p.len();
        let (word, tail) = p.split_once(' ').unwrap_or((p, ""));
        if tail.bytes().any(|c| c != b' ') {
            return Err(err(pos, "Unsupported preset"));
        }
        let preset = preset(word).map_err(|(i, m)| err(pos + i, m))?;
        let lzma = LzmaOptions::preset(preset).map_err(|_| err(pos, "Unsupported preset"))?;
        return Ok(RawOptions {
            method: RawMethod::Lzma2,
            lzma,
            filters: Vec::new(),
        });
    }
    let mut items = Vec::new();
    while pos < s.len() {
        if items.len() == 4 {
            return Err(err(pos, "The maximum number of filters is four"));
        }
        if s[pos..].starts_with("--") {
            pos += 2;
        }
        let end = s[pos..]
            .find(' ')
            .map_or(s.len(), |i| pos + i)
            .min(s[pos..].find("--").map_or(s.len(), |i| pos + i));
        if end == pos {
            return Err(err(pos, "Filter name is missing"));
        }
        let word = &s[pos..end];
        let (name, opts, opts_at) = match word.find([':', '=']) {
            Some(i) => (&word[..i], &word[i + 1..], pos + i + 1),
            None => (word, "", end),
        };
        let item = match name {
            "lzma1" if only_xz => {
                return Err(err(pos, "This filter cannot be used in the .xz format"));
            }
            "lzma1" | "lzma2" => {
                let mut o = LzmaOptions::preset(6).expect("preset 6");
                options(opts, opts_at, |n, v| {
                    match n {
                        "preset" => {
                            o = LzmaOptions::preset(preset(v)?)
                                .map_err(|_| OptError(0, "Unsupported preset"))?;
                        }
                        "dict" => o.dict_size = number(v, true, 4096, (1 << 30) + (1 << 29))?,
                        "lc" => o.lc = number(v, false, 0, 4)?,
                        "lp" => o.lp = number(v, false, 0, 4)?,
                        "pb" => o.pb = number(v, false, 0, 4)?,
                        "mode" => o.mode = mode(v).ok_or("Invalid option value")?,
                        "nice" => o.nice_len = number(v, false, 2, 273)?,
                        "mf" => o.mf = match_finder(v).ok_or("Invalid option value")?,
                        "depth" => o.depth = number(v, false, 0, u32::MAX)?,
                        _ => return Err("Unknown option name".into()),
                    }
                    Ok(())
                })?;
                if o.lc + o.lp > 4 {
                    return Err(err(end, "The sum of lc and lp must not exceed 4"));
                }
                Item::Lzma(o, name == "lzma1")
            }
            "delta" => {
                let mut distance = 1;
                options(opts, opts_at, |n, v| match n {
                    "dist" => {
                        distance = number(v, false, 1, 256)? as u16;
                        Ok(())
                    }
                    _ => Err("Unknown option name".into()),
                })?;
                Item::Filter(Filter::Delta { distance })
            }
            n => {
                let kind = NAMES_BCJ
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, kind)| *kind)
                    .ok_or_else(|| err(pos, "Unknown filter name"))?;
                let mut start_offset = 0;
                options(opts, opts_at, |n, v| match n {
                    "start" => {
                        start_offset = number(v, true, 0, u32::MAX)?;
                        Ok(())
                    }
                    _ => Err("Unknown option name".into()),
                })?;
                Item::Filter(Filter::Bcj { kind, start_offset })
            }
        };
        items.push(item);
        pos = end;
        while s[pos..].starts_with(' ') {
            pos += 1;
        }
    }
    // `lzma_validate_chain()`: Delta and BCJ filters, then LZMA1 or LZMA2.
    let invalid = || {
        err(
            s.len(),
            "Invalid filter chain ('lzma2' missing at the end?)",
        )
    };
    let Some(Item::Lzma(lzma, lzma1)) = items.pop() else {
        return Err(invalid());
    };
    let filters = items
        .into_iter()
        .map(|i| match i {
            Item::Filter(f) => Ok(f),
            Item::Lzma(..) => Err(invalid()),
        })
        .collect::<Result<_, _>>()?;
    Ok(RawOptions {
        method: if lzma1 {
            RawMethod::Lzma1
        } else {
            RawMethod::Lzma2
        },
        lzma,
        filters,
    })
}

/// A size with the largest KiB/MiB/GiB suffix it divides into
/// (`str_append_u32()`).
fn size(mut v: u32) -> String {
    let mut suffix = "";
    for s in ["KiB", "MiB", "GiB"] {
        if v == 0 || v & 1023 != 0 {
            break;
        }
        v >>= 10;
        suffix = s;
    }
    format!("{v}{suffix}")
}

impl RawOptions {
    /// Parses a filter chain like liblzma's `lzma_str_to_filters()` with
    /// `LZMA_STR_ALL_FILTERS`, as `xz --filters=` takes it: a preset such
    /// as `6e`, or filters separated by spaces or `--`, each `name`,
    /// `name:options` or `name=options`, the options `name=value` pairs
    /// separated by commas. The chain must end with `lzma1` or `lzma2`.
    ///
    /// ```
    /// use xz_native::{BcjKind, Filter, RawMethod, RawOptions};
    ///
    /// let o = RawOptions::from_filter_str("x86 lzma2:preset=1,dict=1MiB")?;
    /// assert_eq!(o.method, RawMethod::Lzma2);
    /// assert_eq!(o.lzma.dict_size, 1 << 20);
    /// assert_eq!(o.filters, [Filter::Bcj { kind: BcjKind::X86, start_offset: 0 }]);
    ///
    /// let e = RawOptions::from_filter_str("x86 bogus").unwrap_err();
    /// assert_eq!((e.position(), e.message()), (4, "Unknown filter name"));
    /// # Ok::<(), xz_native::FilterStringError>(())
    /// ```
    pub fn from_filter_str(s: &str) -> Result<Self, FilterStringError> {
        parse(s, false)
    }

    /// [`from_filter_str`](Self::from_filter_str) for the .xz format
    /// (without `LZMA_STR_ALL_FILTERS`): LZMA1 is an error, as in
    /// `xz --filters1=`.
    pub fn from_xz_filter_str(s: &str) -> Result<Self, FilterStringError> {
        parse(s, true)
    }

    /// Writes the chain like liblzma's `lzma_str_from_filters()`. A
    /// [`RawMethod::Lzma1Ext`] chain is written as `lzma1` (liblzma's
    /// function rejects LZMA1EXT).
    ///
    /// ```
    /// use xz_native::{FilterOptions, FilterStringFormat, RawOptions};
    ///
    /// let o = RawOptions::from_filter_str("6")?;
    /// let encoder = FilterStringFormat::new(FilterOptions::Encoder);
    /// assert_eq!(
    ///     o.to_filter_string(encoder),
    ///     "lzma2:dict=8MiB,lc=3,lp=0,pb=2,mode=normal,nice=64,mf=bt4,depth=0"
    /// );
    /// let decoder = FilterStringFormat::new(FilterOptions::Decoder).getopt_long(true);
    /// assert_eq!(o.to_filter_string(decoder), "--lzma2=dict=8MiB");
    /// # Ok::<(), xz_native::FilterStringError>(())
    /// ```
    pub fn to_filter_string(&self, format: FilterStringFormat) -> String {
        let delim = if format.getopt_long { "=" } else { ":" };
        let mut out = String::new();
        let lzma1 = !matches!(self.method, RawMethod::Lzma2);
        let count = self.filters.len() + 1;
        for i in 0..count {
            if i > 0 && !format.no_spaces {
                out.push(' ');
            }
            if format.getopt_long || (i > 0 && format.no_spaces) {
                out.push_str("--");
            }
            // `strfy_filter()`: the first option after the delimiter, the
            // others after commas.
            let mut opts: Vec<String> = Vec::new();
            if i + 1 < count {
                match self.filters[i] {
                    Filter::Delta { distance } => {
                        out.push_str("delta");
                        if format.options != FilterOptions::Names {
                            opts.push(format!("dist={distance}"));
                        }
                    }
                    Filter::Bcj { kind, start_offset } => {
                        out.push_str(bcj_name(kind));
                        if format.options != FilterOptions::Names && start_offset != 0 {
                            opts.push(format!("start={}", size(start_offset)));
                        }
                    }
                    Filter::Lzma2 { .. } => out.push_str("lzma2"),
                }
            } else {
                let o = &self.lzma;
                out.push_str(if lzma1 { "lzma1" } else { "lzma2" });
                match format.options {
                    FilterOptions::Names => {}
                    FilterOptions::Decoder => {
                        opts.push(format!("dict={}", size(o.dict_size)));
                        if lzma1 {
                            opts.push(format!("lc={}", o.lc));
                            opts.push(format!("lp={}", o.lp));
                            opts.push(format!("pb={}", o.pb));
                        }
                    }
                    FilterOptions::Encoder => {
                        let mode = match o.mode {
                            Mode::Fast => "fast",
                            _ => "normal",
                        };
                        let mf = match o.mf {
                            MatchFinder::Hc3 => "hc3",
                            MatchFinder::Hc4 => "hc4",
                            MatchFinder::Bt2 => "bt2",
                            MatchFinder::Bt3 => "bt3",
                            _ => "bt4",
                        };
                        opts.push(format!("dict={}", size(o.dict_size)));
                        opts.push(format!("lc={}", o.lc));
                        opts.push(format!("lp={}", o.lp));
                        opts.push(format!("pb={}", o.pb));
                        opts.push(format!("mode={mode}"));
                        opts.push(format!("nice={}", o.nice_len));
                        opts.push(format!("mf={mf}"));
                        opts.push(format!("depth={}", o.depth));
                    }
                }
            }
            if !opts.is_empty() {
                out.push_str(delim);
                out.push_str(&opts.join(","));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for s in [
            "lzma2:dict=8MiB,lc=3,lp=0,pb=2,mode=normal,nice=64,mf=bt4,depth=0",
            "x86 delta:dist=4 lzma1:dict=6KiB,lc=0,lp=2,pb=0,mode=fast,nice=273,mf=hc4,depth=8",
            "arm64:start=4KiB lzma2:dict=1536MiB,lc=4,lp=0,pb=4,mode=normal,nice=2,mf=bt2,depth=1",
        ] {
            let o = RawOptions::from_filter_str(s).unwrap();
            assert_eq!(
                o.to_filter_string(FilterStringFormat::new(FilterOptions::Encoder)),
                s
            );
        }
    }

    #[test]
    fn errors() {
        let e = |s: &str| {
            let e = RawOptions::from_filter_str(s).unwrap_err();
            (e.position(), e.message())
        };
        assert_eq!(e("").0, 0);
        assert_eq!(
            e("x86"),
            (3, "Invalid filter chain ('lzma2' missing at the end?)")
        );
        assert_eq!(e("lzma2:dict=1KiB"), (11, "Value out of range"));
        assert_eq!(
            e("lzma2:lc"),
            (
                6,
                "Options must be 'name=value' pairs separated with commas"
            )
        );
        assert_eq!(
            e("lzma2 x86"),
            (9, "Invalid filter chain ('lzma2' missing at the end?)")
        );
        assert_eq!(
            e("x86 x86 x86 x86 lzma2").1,
            "The maximum number of filters is four"
        );
        let x = RawOptions::from_xz_filter_str("x86 lzma1").unwrap_err();
        assert_eq!(
            (x.position(), x.message()),
            (4, "This filter cannot be used in the .xz format")
        );
    }
}
