//! Custom filter chains: `--lzma1=`/`--lzma2=`, `--x86` and the other BCJ
//! filters, `--delta` (xz's `options.c`), and `--filters=` strings
//! parsed by `RawOptions::from_filter_str()`.

use crate::args::parse_u64;
use xz_native::{BcjKind, Filter, LzmaOptions, MatchFinder, Mode, RawMethod, RawOptions};

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

/// A parsed `--filters=` string as a custom chain.
pub fn from_raw(raw: RawOptions) -> Vec<Item> {
    let mut items: Vec<Item> = raw
        .filters
        .iter()
        .map(|f| match *f {
            Filter::Delta { distance, .. } => Item::Delta(distance),
            Filter::Bcj {
                kind, start_offset, ..
            } => Item::Bcj(kind, start_offset),
            _ => unreachable!("only Delta and BCJ filters come before LZMA"),
        })
        .collect();
    items.push(if raw.method == RawMethod::Lzma2 {
        Item::Lzma2(raw.lzma)
    } else {
        Item::Lzma1(raw.lzma)
    });
    items
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
}
