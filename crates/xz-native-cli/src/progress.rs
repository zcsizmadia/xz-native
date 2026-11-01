//! The `-v` progress indicator (xz's `message.c`): on a terminal, a line
//! redrawn every second while a file is coded, then the final one;
//! otherwise one line per file at the end.

use crate::num::{int, nice};
use std::io::{IsTerminal, Write};
use std::time::Instant;

/// Progress of one file.
pub struct Meter {
    /// Show progress at all (`-v`).
    enabled: bool,
    /// Standard error is a terminal (`progress_automatic`).
    automatic: bool,
    name: String,
    compress: bool,
    /// The input size if known, for the percentage (`expected_in_size`).
    expected_in: u64,
    start: Instant,
    /// Milliseconds after the start of the next redraw.
    next_update: u64,
    /// A progress line is on the screen (`progress_active`).
    active: bool,
    /// The last `(in, out)` seen.
    last: (u64, u64),
    /// The `name (i/n)` heading, if the file gets one, and whether it is
    /// out (`current_filename_printed`).
    heading: Option<String>,
    heading_printed: bool,
}

impl Meter {
    /// Starts a file: `name` as xz shows it and its position among `files`. On a terminal, the name heads the progress
    /// (`message_filename()`).
    pub fn start(verbose: bool, name: &str, file_pos: usize, files: usize, compress: bool) -> Self {
        let automatic = std::io::stderr().is_terminal();
        // `print_filename()`; 0 files: the number isn't known (`--files`).
        let heading = (files != 1 || name != "(stdin)").then(|| match files {
            0 => format!("{name} ({file_pos})"),
            _ => format!("{name} ({file_pos}/{files})"),
        });
        let heading_printed = verbose && automatic;
        if heading_printed && let Some(h) = &heading {
            eprintln!("{h}");
        }
        Self {
            enabled: verbose,
            heading,
            heading_printed,
            automatic,
            name: name.to_owned(),
            compress,
            expected_in: 0,
            start: Instant::now(),
            next_update: 1000,
            active: false,
            last: (0, 0),
        }
    }

    /// The size of the input file, for the percentage.
    pub fn set_expected(&mut self, size: u64) {
        self.expected_in = size;
    }

    /// `(compressed, uncompressed)` from `(in, out)` as `progress()` gives
    /// them.
    fn sizes(&self, (input, output): (u64, u64)) -> (u64, u64) {
        if self.compress {
            (output, input)
        } else {
            (input, output)
        }
    }

    /// Records the position `(in, out)` and redraws the line once a second
    /// on a terminal (`message_progress_update()`).
    pub fn tick(&mut self, pos: (u64, u64)) {
        self.last = pos;
        // SIGUSR1 or SIGINFO asks for a line even without -v.
        let requested = crate::signals::take_progress();
        let live = self.enabled && self.automatic;
        if !live && !requested {
            return;
        }
        let elapsed = self.start.elapsed().as_millis() as u64;
        if !requested && elapsed < self.next_update {
            return;
        }
        self.next_update = elapsed + 1000;
        if !self.heading_printed {
            self.heading_printed = true;
            if let Some(h) = &self.heading {
                eprintln!("{h}");
            }
        }
        let (input, output) = pos;
        let (c, u) = self.sizes((input, output));
        let line = format!(
            "\r {:>6} {:>35}   {:>9} {:>10}   {:>10}\r",
            percentage(input, self.expected_in),
            sizes(c, u, false),
            speed(u, elapsed),
            time(elapsed),
            remaining(input, self.expected_in, elapsed)
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        if live {
            self.active = true;
        } else {
            // Not redrawn later: keep it on its own line.
            let _ = std::io::stderr().write_all(
                b"
",
            );
        }
    }

    /// The final line at the last position (`progress_flush()`); `finished`
    /// is false after an error.
    pub fn finish(&mut self, finished: bool) {
        if !self.enabled {
            return;
        }
        let pos = self.last;
        let (input, _) = pos;
        let (c, u) = self.sizes(pos);
        if !finished && !self.active && (c == 0 || u == 0) {
            return;
        }
        self.active = false;
        let elapsed = self.start.elapsed().as_millis() as u64;
        let line = if self.automatic {
            format!(
                "\r {:>6} {:>35}   {:>9} {:>10}   {:>10}\n",
                if finished {
                    "100 %".to_owned()
                } else {
                    percentage(input, self.expected_in)
                },
                sizes(c, u, true),
                speed(u, elapsed),
                time(elapsed),
                if finished {
                    String::new()
                } else {
                    remaining(input, self.expected_in, elapsed)
                }
            )
        } else {
            let mut s = format!("{}: ", self.name);
            if !finished {
                let p = percentage(input, self.expected_in);
                if !p.starts_with('-') {
                    s.push_str(&format!("{p}, "));
                }
            }
            s.push_str(&sizes(c, u, true));
            for part in [speed(u, elapsed), time(elapsed)] {
                if !part.is_empty() {
                    s.push_str(&format!(", {part}"));
                }
            }
            s.push('\n');
            s
        };
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

/// `progress_percentage()`: at most 99.9 until the end.
fn percentage(input: u64, expected: u64) -> String {
    if expected == 0 || input > expected {
        return "--- %".into();
    }
    format!("{:.1} %", input as f64 / expected as f64 * 99.9)
}

/// `progress_sizes()`: MiB while running, any unit at the end.
fn sizes(compressed: u64, uncompressed: u64, final_: bool) -> String {
    let show = |n: u64| {
        if final_ { nice(n, false) } else { nice_mib(n) }
    };
    let ratio = if uncompressed > 0 {
        compressed as f64 / uncompressed as f64
    } else {
        16.0
    };
    let r = if ratio > 9.999 {
        format!(" > {:.3}", 9.999)
    } else {
        format!(" = {ratio:.3}")
    };
    format!("{} / {}{r}", show(compressed), show(uncompressed))
}

/// `uint64_to_nicestr(value, NICESTR_MIB, NICESTR_TIB, false)`.
fn nice_mib(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut d = value as f64;
    let mut unit = 0;
    loop {
        d /= 1024.0;
        unit += 1;
        if !(unit < 2 || (d > 9999.9 && unit < 4)) {
            break;
        }
    }
    // Grouped like the other sizes.
    let s = format!("{d:.1}");
    let (i, f) = s.split_once('.').unwrap_or((&s, "0"));
    let i: u64 = i.parse().unwrap_or(0);
    format!("{}.{f} {}", int(i), UNITS[unit])
}

/// `progress_speed()`: after three seconds.
fn speed(uncompressed: u64, elapsed: u64) -> String {
    if elapsed < 3000 {
        return String::new();
    }
    let mut speed = uncompressed as f64 / (elapsed as f64 * (1024.0 / 1000.0));
    let mut unit = 0;
    while speed > 999.0 {
        speed /= 1024.0;
        unit += 1;
        if unit == 3 {
            return String::new();
        }
    }
    let u = ['K', 'M', 'G'][unit];
    if speed > 9.9 {
        format!("{speed:.0} {u}iB/s")
    } else {
        format!("{speed:.1} {u}iB/s")
    }
}

/// `progress_time()`: `m:ss` or `h:mm:ss`, empty before a second.
fn time(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds == 0 || seconds > ((9999 * 60) + 59) * 60 + 59 {
        return String::new();
    }
    let (m, s) = (seconds / 60, seconds % 60);
    if m >= 60 {
        format!("{}:{:02}:{s:02}", m / 60, m % 60)
    } else {
        format!("{m}:{s:02}")
    }
}

/// `progress_remaining()`: rounded more coarsely the longer it is.
fn remaining(input: u64, expected: u64, elapsed: u64) -> String {
    if expected == 0 || input > expected || input < (1 << 19) || elapsed < 8000 {
        return String::new();
    }
    let mut r = ((expected - input) as f64 * (elapsed as f64 / 1000.0) / input as f64) as u64;
    r = r.max(1);
    if r <= 10 {
        format!("{r} s")
    } else if r <= 50 {
        format!("{} s", r.div_ceil(5) * 5)
    } else if r <= 590 {
        let r = r.div_ceil(10) * 10;
        format!("{} min {} s", r / 60, r % 60)
    } else if r <= 59 * 60 {
        format!("{} min", r.div_ceil(60))
    } else if r <= 9 * 3600 + 50 * 60 {
        let r = r.div_ceil(600) * 10;
        format!("{} h {} min", r / 60, r % 60)
    } else if r <= 23 * 3600 {
        format!("{} h", r.div_ceil(3600))
    } else if r <= 9 * 24 * 3600 + 23 * 3600 {
        let r = r.div_ceil(3600);
        format!("{} d {} h", r / 24, r % 24)
    } else if r <= 999 * 24 * 3600 {
        format!("{} d", r.div_ceil(24 * 3600))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        assert_eq!(percentage(5, 10), "50.0 %");
        assert_eq!(percentage(5, 0), "--- %");
        assert_eq!(time(999), "");
        assert_eq!(time(61_000), "1:01");
        assert_eq!(time(3_661_000), "1:01:01");
        assert_eq!(speed(10 << 20, 2000), "");
        assert_eq!(speed(10 << 20, 5000), "2.0 MiB/s");
        assert_eq!(speed(100 << 20, 5000), "20 MiB/s");
        assert_eq!(remaining(1 << 20, 4 << 20, 10_000), "30 s");
        assert_eq!(remaining(1 << 20, 100 << 20, 10_000), "17 min");
        assert!(sizes(1, 0, true).ends_with("> 9.999"));
        assert_eq!(nice_mib(0), "0.0 MiB");
    }
}
