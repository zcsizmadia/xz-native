//! Every public type is `Send + Sync + Unpin` (for the readers and writers,
//! when what they wrap is). Part of the 1.0 API promise: a change inside a
//! coder (a thread pool, a cache) must not silently take these away.

use std::io::Cursor;

fn check<T: Send + Sync + Unpin>() {}

#[test]
fn coders_options_and_errors() {
    use xz_native::*;
    check::<XzDecoder>();
    check::<XzEncoder>();
    check::<LzmaDecoder>();
    check::<LzmaEncoder>();
    check::<LzipDecoder>();
    check::<AutoDecoder>();
    check::<RawDecoder>();
    check::<RawEncoder>();
    check::<Sha256>();
    check::<DecoderOptions>();
    check::<EncoderOptions>();
    check::<LzmaOptions>();
    check::<RawOptions>();
    check::<RawMethod>();
    check::<Filter>();
    check::<FilterStringFormat>();
    check::<FilterStringError>();
    check::<Check>();
    check::<Format>();
    check::<Status>();
    check::<Action>();
    check::<Error>();
}

#[test]
fn readers_and_writers() {
    use xz_native::*;
    check::<XzReader<&[u8]>>();
    check::<LzmaReader<&[u8]>>();
    check::<LzipReader<&[u8]>>();
    check::<AutoReader<&[u8]>>();
    check::<XzWriter<Vec<u8>>>();
    check::<LzmaWriter<Vec<u8>>>();
    check::<XzSeekableReader<Cursor<Vec<u8>>>>();
    check::<StreamInfo>();
    check::<BlockInfo>();
    check::<BlockDetails>();
}

#[test]
fn compat() {
    use xz_native::compat::{bufread, read, write};
    check::<bufread::XzDecoder<&[u8]>>();
    check::<bufread::XzEncoder<&[u8]>>();
    check::<read::XzDecoder<&[u8]>>();
    check::<read::XzEncoder<&[u8]>>();
    check::<write::XzDecoder<Vec<u8>>>();
    check::<write::XzEncoder<Vec<u8>>>();
}

#[cfg(feature = "tokio")]
#[test]
fn tokio_adapters() {
    use xz_native::tokio::*;
    check::<XzReader<&[u8]>>();
    check::<LzmaReader<&[u8]>>();
    check::<LzipReader<&[u8]>>();
    check::<AutoReader<&[u8]>>();
    check::<XzWriter<Vec<u8>>>();
    check::<LzmaWriter<Vec<u8>>>();
}

#[cfg(feature = "futures-io")]
#[test]
fn futures_adapters() {
    use xz_native::futures::*;
    check::<XzReader<&[u8]>>();
    check::<LzmaReader<&[u8]>>();
    check::<LzipReader<&[u8]>>();
    check::<AutoReader<&[u8]>>();
    check::<XzWriter<Vec<u8>>>();
    check::<LzmaWriter<Vec<u8>>>();
}
