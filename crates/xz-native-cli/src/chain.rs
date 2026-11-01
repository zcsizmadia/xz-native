//! Custom filter chains: `--lzma1=`/`--lzma2=`, `--x86` and the other BCJ
//! filters, `--delta` (xz's `options.c`), and `--filters=` strings
//! (liblzma's `lzma_str_to_filters()`).

use crate::args::parse_u64;
use xz_native::{BcjKind, LzmaOptions, MatchFinder, Mode};

/// A filter of a custom chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Bcj(BcjKind, u32),
    Delta(u16),
    Lzma1(LzmaOptions),
    Lzma2(LzmaOptions),
}

/// The BCJ filter of an option or filter name.
pub fn bcj_kind(name: &str) -> Option<BcjKind> {
    Some(match name {
        "x86" => BcjKind::X86,
        "powerpc" => BcjKind::PowerPc,
        "ia64" => BcjKind::Ia64,
        "arm" => BcjKind::Arm,
        "armthumb" => BcjKind::ArmThumb,
        "arm64" => BcjKind::Arm64,
        "sparc" => BcjKind::Sparc,
        "riscv" => BcjKind::RiscV,
        _ => return None,
    })
}

/// The option and filter name of a BCJ filter.
pub fn bcj_name(kind: BcjKind) -> &'static str {
    match kind {
        BcjKind::X86 => "x86",
        BcjKind::PowerPc => "powerpc",
        BcjKind::Ia64 => "ia64",
        BcjKind::Arm => "arm",
        BcjKind::ArmThumb => "armthumb",
        BcjKind::Arm64 => "arm64",
        BcjKind::Sparc => "sparc",
        _ => "riscv",
    }
}

/// `name=value` pairs separated with commas (`parse_options()` of
/// `options.c`); empty pairs are skipped.
fn pairs(s: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut v = Vec::new();
    for pair in s.split(',').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some((n, val)) if !val.is_empty() => v.push((n, val)),
            _ => {
                return Err(format!(
                    "{s}: Options must be 'name=value' pairs separated with commas"
                ));
            }
        }
    }
    Ok(v)
}

fn invalid_name(name: &str) -> String {
    format!("{name}: Invalid option name")
}

/// `options_bcj()`: `start=OFFSET`.
pub fn bcj(s: Option<&str>) -> Result<u32, String> {
    let mut start = 0;
    for (n, v) in pairs(s.unwrap_or(""))? {
        match n {
            "start" => start = parse_u64(n, v, 0, u64::from(u32::MAX))? as u32,
            _ => return Err(invalid_name(n)),
        }
    }
    Ok(start)
}

/// `options_delta()`: `dist=1..256`.
pub fn delta(s: Option<&str>) -> Result<u16, String> {
    let mut dist = 1;
    for (n, v) in pairs(s.unwrap_or(""))? {
        match n {
            "dist" => dist = parse_u64(n, v, 1, 256)? as u16,
            _ => return Err(invalid_name(n)),
        }
    }
    Ok(dist)
}

/// `preset=0-9[e]` for xz's `set_lzma()`.
fn preset_value(v: &str) -> Option<u32> {
    let b = v.as_bytes();
    let d = b.first().filter(|c| c.is_ascii_digit())?;
    let mut p = u32::from(d - b'0');
    match &b[1..] {
        [] => {}
        [b'e'] => p |= xz_native::PRESET_EXTREME,
        _ => return None,
    }
    Some(p)
}

/// `options_lzma()`: preset 6 changed by the options, in order.
pub fn lzma(s: Option<&str>) -> Result<LzmaOptions, String> {
    let mut o = LzmaOptions::preset(6).map_err(|e| e.to_string())?;
    for (n, v) in pairs(s.unwrap_or(""))? {
        let num = |min: u64, max: u64| parse_u64(n, v, min, max).map(|x| x as u32);
        match n {
            "preset" => {
                let p = preset_value(v)
                    .ok_or_else(|| format!("Unsupported LZMA1/LZMA2 preset: {v}"))?;
                o = LzmaOptions::preset(p)
                    .map_err(|_| format!("Unsupported LZMA1/LZMA2 preset: {v}"))?;
            }
            "dict" => o.dict_size = num(4096, (1 << 30) + (1 << 29))?,
            "lc" => o.lc = num(0, 4)?,
            "lp" => o.lp = num(0, 4)?,
            "pb" => o.pb = num(0, 4)?,
            "mode" => o.mode = mode(v).ok_or_else(|| format!("{v}: Invalid option value"))?,
            "nice" => o.nice_len = num(2, 273)?,
            "mf" => o.mf = match_finder(v).ok_or_else(|| format!("{v}: Invalid option value"))?,
            "depth" => o.depth = num(0, u64::from(u32::MAX))?,
            _ => return Err(invalid_name(n)),
        }
    }
    if o.lc + o.lp > 4 {
        return Err("The sum of lc and lp must not exceed 4".into());
    }
    Ok(o)
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

/// An error of a `--filters=` string and where it is.
#[derive(Debug, PartialEq, Eq)]
pub struct StrError {
    pub pos: usize,
    pub msg: &'static str,
}

fn err(pos: usize, msg: &'static str) -> StrError {
    StrError { pos, msg }
}

/// The options of one filter in a `--filters=` string (`parse_options()`
/// of `string_conversion.c`), starting at `pos`.
fn str_options(
    s: &str,
    pos: usize,
    mut set: impl FnMut(&str, &str) -> Result<(), &'static str>,
) -> Result<(), StrError> {
    let mut at = pos;
    for pair in s.split(',') {
        if !pair.is_empty() {
            let Some((name, value)) = pair.split_once('=').filter(|(n, _)| !n.is_empty()) else {
                return Err(err(
                    at,
                    "Options must be 'name=value' pairs separated with commas",
                ));
            };
            // Errors in the value point at the value.
            let value_at = at + name.len() + 1;
            if value.is_empty() {
                return Err(err(value_at, "Option value cannot be empty"));
            }
            set(name, value).map_err(|m| {
                err(
                    if m == "Unknown option name" {
                        at
                    } else {
                        value_at
                    },
                    m,
                )
            })?;
        }
        at += pair.len() + 1;
    }
    Ok(())
}

/// A decimal value with an optional KiB/MiB/GiB suffix when `suffix`.
fn str_u32(v: &str, suffix: bool, min: u32, max: u32) -> Result<u32, &'static str> {
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err("Value is not a non-negative decimal integer");
    }
    let mut n: u32 = v[..digits].parse().map_err(|_| "Value out of range")?;
    let rest = &v[digits..];
    if !rest.is_empty() {
        if !suffix {
            return Err("This option does not support any multiplier suffixes");
        }
        let shift = match rest.as_bytes()[0] {
            b'k' | b'K' => 10,
            b'm' | b'M' => 20,
            b'g' | b'G' => 30,
            _ => return Err("Invalid multiplier suffix (KiB, MiB, or GiB)"),
        };
        if !matches!(&rest[1..], "" | "i" | "B" | "iB") {
            return Err("Invalid multiplier suffix (KiB, MiB, or GiB)");
        }
        if n > u32::MAX >> shift {
            return Err("Value out of range");
        }
        n <<= shift;
    }
    if n < min || n > max {
        return Err("Value out of range");
    }
    Ok(n)
}

/// `parse_lzma12_preset()`.
fn str_preset(v: &str) -> Result<u32, &'static str> {
    let b = v.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_digit) {
        return Err("Unsupported preset");
    }
    let mut p = u32::from(b[0] - b'0');
    for &c in &b[1..] {
        if c != b'e' {
            return Err("Unsupported flag in the preset");
        }
        p |= xz_native::PRESET_EXTREME;
    }
    Ok(p)
}

/// A `--filters=` string (`lzma_str_to_filters()`): a preset such as `6e`,
/// or filters separated by spaces or `--`, each `name`, `name:options` or
/// `name=options`. With `only_xz` (`--filters1..9`, no
/// `LZMA_STR_ALL_FILTERS`), LZMA1 isn't allowed.
pub fn from_str(s: &str, only_xz: bool) -> Result<Vec<Item>, StrError> {
    let mut pos = s.len() - s.trim_start_matches(' ').len();
    if pos == s.len() {
        return Err(err(
            pos,
            "Empty string is not allowed, try '6' if a default value is needed",
        ));
    }
    let rest = &s[pos..];
    if rest.as_bytes()[0].is_ascii_digit()
        || (rest.starts_with('-') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
    {
        let p = rest.strip_prefix('-').unwrap_or(rest);
        pos += rest.len() - p.len();
        let (word, tail) = p.split_once(' ').unwrap_or((p, ""));
        if tail.bytes().any(|c| c != b' ') {
            return Err(err(pos + word.len(), "Unsupported preset"));
        }
        let preset = str_preset(word).map_err(|m| err(pos, m))?;
        let o = LzmaOptions::preset(preset).map_err(|_| err(pos, "Unsupported preset"))?;
        return Ok(vec![Item::Lzma2(o)]);
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
        let (name, opts, opts_pos) = match word.find([':', '=']) {
            Some(i) => (&word[..i], &word[i + 1..], pos + i + 1),
            None => (word, "", end),
        };
        let item = match name {
            "lzma1" if only_xz => {
                return Err(err(pos, "This filter cannot be used in the .xz format"));
            }
            "lzma1" | "lzma2" => {
                let mut o = LzmaOptions::preset(6).expect("preset 6");
                str_options(opts, opts_pos, |n, v| {
                    match n {
                        "preset" => {
                            o = LzmaOptions::preset(str_preset(v)?)
                                .map_err(|_| "Unsupported preset")?;
                        }
                        "dict" => o.dict_size = str_u32(v, true, 4096, (1 << 30) + (1 << 29))?,
                        "lc" => o.lc = str_u32(v, false, 0, 4)?,
                        "lp" => o.lp = str_u32(v, false, 0, 4)?,
                        "pb" => o.pb = str_u32(v, false, 0, 4)?,
                        "mode" => o.mode = mode(v).ok_or("Invalid option value")?,
                        "nice" => o.nice_len = str_u32(v, false, 2, 273)?,
                        "mf" => o.mf = match_finder(v).ok_or("Invalid option value")?,
                        "depth" => o.depth = str_u32(v, false, 0, u32::MAX)?,
                        _ => return Err("Unknown option name"),
                    }
                    Ok(())
                })?;
                if o.lc + o.lp > 4 {
                    return Err(err(end, "The sum of lc and lp must not exceed 4"));
                }
                if name == "lzma1" {
                    Item::Lzma1(o)
                } else {
                    Item::Lzma2(o)
                }
            }
            "delta" => {
                let mut dist = 1;
                str_options(opts, opts_pos, |n, v| match n {
                    "dist" => {
                        dist = str_u32(v, false, 1, 256)? as u16;
                        Ok(())
                    }
                    _ => Err("Unknown option name"),
                })?;
                Item::Delta(dist)
            }
            n => {
                let kind = bcj_kind(n).ok_or_else(|| err(pos, "Unknown filter name"))?;
                let mut start = 0;
                str_options(opts, opts_pos, |n, v| match n {
                    "start" => {
                        start = str_u32(v, true, 0, u32::MAX)?;
                        Ok(())
                    }
                    _ => Err("Unknown option name"),
                })?;
                Item::Bcj(kind, start)
            }
        };
        items.push(item);
        pos = end;
        while s[pos..].starts_with(' ') {
            pos += 1;
        }
    }
    // `lzma_validate_chain()`: Delta and BCJ filters, then LZMA1 or LZMA2.
    let lzma = |i: &Item| matches!(i, Item::Lzma1(_) | Item::Lzma2(_));
    let (last, first) = items.split_last().expect("at least one filter");
    if !lzma(last) || first.iter().any(lzma) {
        return Err(err(
            s.len(),
            "Invalid filter chain ('lzma2' missing at the end?)",
        ));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        assert_eq!(bcj(Some("start=4096")), Ok(4096));
        assert_eq!(bcj(None), Ok(0));
        assert!(bcj(Some("x=1")).is_err());
        assert_eq!(delta(Some("dist=4")), Ok(4));
        assert!(delta(Some("dist=0")).is_err());
        let o = lzma(Some("preset=1e,dict=1MiB,lc=0,lp=2,mf=hc4,mode=normal")).unwrap();
        assert_eq!(
            (o.dict_size, o.lc, o.lp, o.mf),
            (1 << 20, 0, 2, MatchFinder::Hc4)
        );
        assert!(lzma(Some("lc=3,lp=2")).is_err());
        assert!(lzma(Some("preset=10")).is_err());
        assert!(lzma(Some("dict")).is_err());
    }

    #[test]
    fn strings() {
        let six = LzmaOptions::preset(6).unwrap();
        assert_eq!(from_str("6", false), Ok(vec![Item::Lzma2(six)]));
        assert_eq!(
            from_str("  -9e ", false),
            Ok(vec![Item::Lzma2(
                LzmaOptions::preset(9 | xz_native::PRESET_EXTREME).unwrap()
            )])
        );
        assert_eq!(
            from_str("--x86 --delta=dist=4 lzma2:dict=6KiB", false),
            Ok(vec![
                Item::Bcj(BcjKind::X86, 0),
                Item::Delta(4),
                Item::Lzma2({
                    let mut o = six;
                    o.dict_size = 6 << 10;
                    o
                }),
            ])
        );
        assert_eq!(from_str("arm64=start=64 lzma1", false).unwrap().len(), 2);
        assert_eq!(from_str("", false).unwrap_err().pos, 0);
        assert_eq!(
            from_str("x86 bogus", false).unwrap_err(),
            err(4, "Unknown filter name")
        );
        assert_eq!(
            from_str("lzma2:dict=1KiB", false).unwrap_err(),
            err(11, "Value out of range")
        );
        assert!(from_str("x86 x86 x86 x86 lzma2", false).is_err());
        assert_eq!(
            from_str("x86", false).unwrap_err(),
            err(3, "Invalid filter chain ('lzma2' missing at the end?)")
        );
        assert!(from_str("lzma2 x86", false).is_err());
        assert_eq!(
            from_str("x86 lzma1", true).unwrap_err(),
            err(4, "This filter cannot be used in the .xz format")
        );
    }
}
