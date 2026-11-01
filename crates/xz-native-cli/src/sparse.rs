//! Sparse output files, and standard output when it is a file, when
//! decompressing (xz's `io_write()` with `dest_try_sparse`): output blocks
//! of zeros become a hole by seeking over them instead of writing. Not on
//! Windows, as in xz.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

/// xz's `IO_BUFFER_SIZE` on POSIX systems.
const BLOCK: usize = 8192;

/// Writes `inner` in whole blocks, seeking over the blocks that are all
/// zeros.
pub struct Sparse {
    inner: File,
    buf: Vec<u8>,
    /// Zeros not written yet (`dest_pending_sparse`).
    pending: u64,
}

impl Sparse {
    pub fn new(inner: File) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(BLOCK),
            pending: 0,
        }
    }

    /// Writes a block, or adds it to the hole if it is all zeros.
    fn block(&mut self, data: &[u8], full: bool) -> io::Result<()> {
        if full && data.iter().all(|&b| b == 0) {
            self.pending += data.len() as u64;
            return Ok(());
        }
        if data.is_empty() {
            return Ok(());
        }
        self.seek_over()?;
        self.inner.write_all(data)
    }

    fn seek_over(&mut self) -> io::Result<()> {
        if self.pending > 0 {
            self.inner.seek(SeekFrom::Current(self.pending as i64))?;
            self.pending = 0;
        }
        Ok(())
    }

    /// Writes what is left; a hole at the end gets its last byte written,
    /// so that the file has its full size (`io_close()`).
    pub fn finish(mut self) -> io::Result<File> {
        let rest = std::mem::take(&mut self.buf);
        self.block(&rest, false)?;
        if self.pending > 0 {
            self.pending -= 1;
            self.seek_over()?;
            self.inner.write_all(&[0])?;
        }
        Ok(self.inner)
    }
}

impl Write for Sparse {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut rest = data;
        while !rest.is_empty() {
            let n = (BLOCK - self.buf.len()).min(rest.len());
            self.buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if self.buf.len() == BLOCK {
                let block = std::mem::take(&mut self.buf);
                self.block(&block, true)?;
                self.buf = block;
                self.buf.clear();
            }
        }
        Ok(data.len())
    }

    /// Whole blocks are already out; the partial one waits for `finish`.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Standard output for sparse output, if it can have holes
/// (`io_open_dest_real()`): a regular file, written at its end. With
/// O_APPEND (`>>`), it moves to the end and clears the flag, and the old
/// flags are returned for [`restore_stdout`].
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn stdout() -> Option<(File, Option<libc::c_int>)> {
    use std::os::fd::{AsFd, AsRawFd};
    // A duplicate shares the offset and the flags with standard output.
    let mut file = File::from(io::stdout().as_fd().try_clone_to_owned().ok()?);
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let fd = file.as_raw_fd();
    // SAFETY: F_GETFL on an open descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return None;
    }
    if flags & libc::O_APPEND == 0 {
        // Holes elsewhere than at the end would overwrite data.
        return (file.stream_position().ok()? == meta.len()).then_some((file, None));
    }
    file.seek(SeekFrom::End(0)).ok()?;
    // SAFETY: F_SETFL on an open descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_APPEND) } == -1 {
        return None;
    }
    Some((file, Some(flags)))
}

/// Puts back the flags [`stdout`] changed (`io_close_dest()`).
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn restore_stdout(flags: libc::c_int) -> io::Result<()> {
    use std::os::fd::{AsFd, AsRawFd};
    // SAFETY: F_SETFL on an open descriptor.
    if unsafe { libc::fcntl(io::stdout().as_fd().as_raw_fd(), libc::F_SETFL, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn same_content() {
        let dir = std::env::temp_dir().join(format!("xz-native-sparse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut data = vec![0u8; 5 * BLOCK + 100];
        data[BLOCK + 7] = 1;
        for (i, tail) in [0usize, 1, 100, BLOCK].into_iter().enumerate() {
            let mut d = data.clone();
            d.extend(std::iter::repeat_n(0, tail));
            let path = dir.join(format!("f{i}"));
            let mut w = Sparse::new(File::create(&path).unwrap());
            for chunk in d.chunks(3000) {
                w.write_all(chunk).unwrap();
            }
            drop(w.finish().unwrap());
            let mut back = Vec::new();
            File::open(&path).unwrap().read_to_end(&mut back).unwrap();
            assert!(back == d, "tail {tail}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
