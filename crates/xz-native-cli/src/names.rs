//! The files to process: the command line, then `--files`/`--files0`
//! names read from a file or standard input (xz's `read_name()`).

use crate::Ctx;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};

/// Where `--files` or `--files0` reads names: `(file, delimiter)`, the
/// file being None for standard input.
pub type FilesFrom = (Option<String>, u8);

/// The names in order.
pub struct Names {
    args: std::vec::IntoIter<OsString>,
    from: Option<(String, Box<dyn BufRead>, u8)>,
    /// Names come from standard input (`files_name == stdin_filename`).
    pub stdin_names: bool,
    /// The number of names if known (not with `--files`).
    pub total: Option<usize>,
}

impl Names {
    /// The command line names, then those of `from`. Without either,
    /// standard input. An unreadable names file is an error.
    pub fn new(args: Vec<OsString>, from: Option<&FilesFrom>) -> Result<Self, String> {
        let args = if args.is_empty() && from.is_none() {
            vec![OsString::from("-")]
        } else {
            args
        };
        let total = from.is_none().then_some(args.len());
        let from = match from {
            None => None,
            Some((None, delim)) => {
                let r: Box<dyn BufRead> = Box::new(BufReader::new(io::stdin()));
                Some(("(stdin)".to_owned(), r, *delim))
            }
            Some((Some(path), delim)) => {
                let f = File::open(path)
                    .map_err(|e| format!("{path}: {}", crate::files::os_error(&e)))?;
                let r: Box<dyn BufRead> = Box::new(BufReader::new(f));
                Some((path.clone(), r, *delim))
            }
        };
        Ok(Self {
            args: args.into_iter(),
            stdin_names: matches!(from, Some((ref n, _, _)) if n == "(stdin)"),
            from,
            total,
        })
    }

    /// The next name, and whether it is standard input: "-" on the command
    /// line, but not in a names file. Errors reading names are reported and
    /// end them.
    pub fn next(&mut self, ctx: &mut Ctx) -> Option<(OsString, bool)> {
        if let Some(a) = self.args.next() {
            let stdin = a == "-";
            return Some((a, stdin));
        }
        let (name, r, delim) = self.from.as_mut()?;
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8];
            match r.read(&mut byte) {
                Ok(0) => {
                    if !buf.is_empty() {
                        ctx.error(format!(
                            "{name}: Unexpected end of input when reading filenames"
                        ));
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    ctx.error(format!(
                        "{name}: Error reading filenames: {}",
                        crate::files::os_error(&e)
                    ));
                    break;
                }
            }
            match byte[0] {
                c if c == *delim => {
                    // xz opens the --files list in text mode, which on
                    // Windows reads CRLF as LF.
                    if cfg!(windows) && c == b'\n' && buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    if buf.is_empty() {
                        continue;
                    }
                    return Some((to_os(buf), false));
                }
                0 => {
                    ctx.error(format!(
                        "{name}: Null character found when reading filenames; maybe you \
                         meant to use '--files0' instead of '--files'?"
                    ));
                    break;
                }
                c => buf.push(c),
            }
        }
        self.from = None;
        None
    }
}

#[cfg(unix)]
fn to_os(b: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(b)
}

#[cfg(not(unix))]
fn to_os(b: Vec<u8>) -> OsString {
    String::from_utf8_lossy(&b).into_owned().into()
}
