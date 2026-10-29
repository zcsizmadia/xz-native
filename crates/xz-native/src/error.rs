use core::fmt;

/// Errors returned by the decoders and encoders.
///
/// The variants mirror the `lzma_ret` error codes of liblzma so that behavior
/// can be compared one-to-one with xz 5.8.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// The input is not in a recognized format (`LZMA_FORMAT_ERROR`).
    Format,
    /// The input uses options or features this implementation doesn't
    /// support, for example an unknown filter (`LZMA_OPTIONS_ERROR`).
    Options,
    /// The input is corrupt (`LZMA_DATA_ERROR`).
    Data,
    /// The integrity check (CRC32, CRC64 or SHA-256) didn't match.
    /// liblzma reports this as `LZMA_DATA_ERROR`.
    CheckMismatch,
    /// Decoding would need more memory than the configured limit
    /// (`LZMA_MEMLIMIT_ERROR`).
    MemLimit,
    /// The input ended before the end of the stream (`LZMA_BUF_ERROR` at
    /// the end of input).
    Truncated,
    /// An encoder option is out of range (`LZMA_OPTIONS_ERROR` on encode).
    InvalidOptions,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Format => "file format not recognized",
            Error::Options => "unsupported options",
            Error::Data => "compressed data is corrupt",
            Error::CheckMismatch => "integrity check mismatch",
            Error::MemLimit => "memory usage limit reached",
            Error::Truncated => "unexpected end of input",
            Error::InvalidOptions => "invalid encoder options",
        })
    }
}

impl core::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::Truncated => std::io::ErrorKind::UnexpectedEof,
            Error::MemLimit => std::io::ErrorKind::OutOfMemory,
            Error::Options | Error::InvalidOptions => std::io::ErrorKind::Unsupported,
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, e)
    }
}

/// Shorthand for `Result<T, xz_native::Error>`.
pub type Result<T> = core::result::Result<T, Error>;
