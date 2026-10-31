//! Numbers as xz prints them (`uint64_to_str()`, `uint64_to_nicestr()`).
//!
//! xz groups thousands with the locale's separator. Here that is a comma
//! unless the locale is unset or C/POSIX (then glibc's printf prints none
//! either). xz's MinGW builds for Windows always group.

use std::sync::OnceLock;

/// Whether to group thousands, from `LC_ALL`, `LC_NUMERIC` or `LANG`.
fn grouping() -> bool {
    static GROUP: OnceLock<bool> = OnceLock::new();
    *GROUP.get_or_init(|| {
        if cfg!(windows) {
            return true;
        }
        let locale = ["LC_ALL", "LC_NUMERIC", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty());
        match locale {
            None => false,
            Some(l) => !(l == "C" || l == "POSIX" || l.starts_with("C.")),
        }
    })
}

/// Inserts separators into the digits `s`.
fn group_digits(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 4 / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `uint64_to_str()`.
pub fn int(n: u64) -> String {
    if grouping() {
        group_digits(&n.to_string())
    } else {
        n.to_string()
    }
}

/// `n` with one decimal, the integer part grouped.
fn float1(d: f64) -> String {
    let s = format!("{d:.1}");
    match s.split_once('.') {
        Some((i, f)) if grouping() => format!("{}.{f}", group_digits(i)),
        _ => s,
    }
}

/// `uint64_to_nicestr(value, NICESTR_B, NICESTR_TIB, also_bytes)`: bytes
/// below 10000, otherwise KiB to TiB with one decimal, and with
/// `also_bytes` the exact bytes after it.
pub fn nice(value: u64, also_bytes: bool) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut s = if value < 10000 {
        format!("{} B", int(value))
    } else {
        let mut d = value as f64;
        let mut unit = 0;
        loop {
            d /= 1024.0;
            unit += 1;
            if !(d > 9999.9 && unit < 4) {
                break;
            }
        }
        format!("{} {}", float1(d), UNITS[unit])
    };
    if also_bytes && value >= 10000 {
        s.push_str(&format!(" ({} B)", int(value)));
    }
    s
}

/// MiB rounded up (`round_up_to_mib()`).
pub fn mib(n: u64) -> String {
    int(n.div_ceil(1 << 20))
}

/// `get_ratio()`: compressed / uncompressed with three decimals, or `---`.
pub fn ratio(compressed: u64, uncompressed: u64) -> String {
    if uncompressed == 0 {
        return "---".into();
    }
    let r = compressed as f64 / uncompressed as f64;
    if r > 9.999 {
        "---".into()
    } else {
        format!("{r:.3}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits() {
        assert_eq!(group_digits("0"), "0");
        assert_eq!(group_digits("999"), "999");
        assert_eq!(group_digits("1000"), "1,000");
        assert_eq!(group_digits("1234567"), "1,234,567");
    }

    #[test]
    fn ratios() {
        assert_eq!(ratio(1, 0), "---");
        assert_eq!(ratio(100_000, 10), "---");
        assert_eq!(ratio(1, 4), "0.250");
        assert_eq!(ratio(2, 3), "0.667");
    }
}
