//! Sparse output files when decompressing (xz's `io_write()` with
//! `dest_try_sparse`): output blocks of zeros become a hole by seeking
//! over them instead of writing. Not on Windows, as in xz.

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
