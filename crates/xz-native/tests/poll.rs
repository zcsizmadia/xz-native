//! `poll_encode` and `poll_decode` (#90): with threads, a call that would
//! wait for a worker returns `Pending` and a worker wakes the task later;
//! the output is the same as from `encode`/`decode`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::{Duration, Instant};
use xz_native::{
    Action, AutoDecoder, DecoderOptions, EncoderOptions, PushDecoder, PushEncoder, Status,
    XzDecoder, XzEncoder,
};

/// Counts wakes and unparks the polling thread.
struct Counter {
    wakes: AtomicUsize,
    thread: Mutex<Thread>,
}

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.thread.lock().unwrap().unpark();
    }
}

/// A minimal executor for one poll function: after `Pending`, waits for a
/// wake (a lost wake-up fails the test) and polls again.
struct Driver {
    counter: Arc<Counter>,
    waker: Waker,
    pending: usize,
}

impl Driver {
    fn new() -> Self {
        let counter = Arc::new(Counter {
            wakes: AtomicUsize::new(0),
            thread: Mutex::new(std::thread::current()),
        });
        Self {
            waker: Waker::from(counter.clone()),
            counter,
            pending: 0,
        }
    }

    fn run<T>(&mut self, mut f: impl FnMut(&mut Context<'_>) -> Poll<T>) -> T {
        loop {
            let before = self.counter.wakes.load(Ordering::SeqCst);
            let mut cx = Context::from_waker(&self.waker);
            match f(&mut cx) {
                Poll::Ready(r) => return r,
                Poll::Pending => {
                    self.pending += 1;
                    let start = Instant::now();
                    while self.counter.wakes.load(Ordering::SeqCst) == before {
                        assert!(
                            start.elapsed() < Duration::from_secs(60),
                            "Pending without a wake"
                        );
                        std::thread::park_timeout(Duration::from_millis(100));
                    }
                }
            }
        }
    }
}

fn data() -> Vec<u8> {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    (0..4u32 << 20)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Compressible text-like runs with some noise, so that Blocks
            // take the workers a while at preset 6.
            if x.is_multiple_of(8) {
                x as u8
            } else {
                b'a' + (i / 7 % 26) as u8
            }
        })
        .collect()
}

fn mt_options() -> EncoderOptions {
    EncoderOptions::preset(6)
        .unwrap()
        .threads(2)
        .block_size(256 << 10)
}

fn encode_polled<E: PushEncoder>(enc: &mut E, data: &[u8], driver: &mut Driver) -> Vec<u8> {
    let (mut input, mut out, mut buf) = (data, Vec::new(), vec![0u8; 64 << 10]);
    for action in [Action::Run, Action::Finish] {
        loop {
            let (used, written, status) = driver
                .run(|cx| enc.poll_encode(cx, input, &mut buf, action))
                .unwrap();
            input = &input[used..];
            out.extend_from_slice(&buf[..written]);
            if action == Action::Run && input.is_empty() || status == Status::StreamEnd {
                break;
            }
        }
    }
    out
}

fn decode_polled<D: PushDecoder>(dec: &mut D, packed: &[u8], driver: &mut Driver) -> Vec<u8> {
    let (mut input, mut out, mut buf) = (packed, Vec::new(), vec![0u8; 64 << 10]);
    loop {
        let (used, written, status) = driver
            .run(|cx| dec.poll_decode(cx, input, &mut buf, true))
            .unwrap();
        input = &input[used..];
        out.extend_from_slice(&buf[..written]);
        if status == Status::StreamEnd {
            return out;
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn threaded_poll_encode_and_decode() {
    let data = data();
    let expected = xz_native::compress_with_options(&data, &mt_options()).unwrap();

    let mut driver = Driver::new();
    let mut enc = XzEncoder::with_options(mt_options()).unwrap();
    let packed = encode_polled(&mut enc, &data, &mut driver);
    assert!(packed == expected, "same output as without polling");
    // 16 Blocks at preset 6 for 2 workers: the caller outruns them.
    assert!(driver.pending > 0, "the encoder never returned Pending");

    for threads in [2, 4] {
        let opts = DecoderOptions::default().threads(threads);
        let mut driver = Driver::new();
        let out = decode_polled(&mut XzDecoder::with_options(opts), &packed, &mut driver);
        assert!(out == data, "XzDecoder, {threads} threads");
        let out = decode_polled(&mut AutoDecoder::with_options(opts), &packed, &mut driver);
        assert!(out == data, "AutoDecoder, {threads} threads");
    }
}

/// Single-threaded coders never return `Pending`.
#[test]
#[cfg_attr(miri, ignore)]
fn single_threaded_is_always_ready() {
    let data = &data()[..64 << 10];
    let mut driver = Driver::new();
    let packed = encode_polled(&mut XzEncoder::new(1).unwrap(), data, &mut driver);
    let out = decode_polled(&mut XzDecoder::new(), &packed, &mut driver);
    assert!(out == data);
    let lzma = xz_native::compress_lzma(data, 1).unwrap();
    let out = decode_polled(&mut AutoDecoder::new(), &lzma, &mut driver);
    assert!(out == data);
    assert_eq!(driver.pending, 0);
}

/// On a current-thread runtime, other tasks run while the workers
/// compress: the writer returns `Pending` instead of blocking the thread.
#[cfg(feature = "tokio")]
#[test]
#[cfg_attr(miri, ignore)]
fn tokio_writer_yields_to_other_tasks() {
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let data = data();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (packed, ticks_while_writing) = rt.block_on(async {
        let ticker = {
            let (ticks, done) = (ticks.clone(), done.clone());
            tokio::spawn(async move {
                while !done.load(Ordering::SeqCst) {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut w = xz_native::tokio::XzWriter::with_options(Vec::new(), mt_options()).unwrap();
        w.write_all(&data).await.unwrap();
        // Before the shutdown: `write_all` alone, with input left to take.
        let seen = ticks.load(Ordering::SeqCst);
        w.shutdown().await.unwrap();
        done.store(true, Ordering::SeqCst);
        ticker.await.unwrap();
        (w.into_inner(), seen)
    });
    assert!(ticks_while_writing > 0, "the writer blocked the executor");

    let out = rt.block_on(async {
        let mut r = xz_native::tokio::XzReader::with_options(
            &packed[..],
            DecoderOptions::default().threads(2),
        );
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        out
    });
    assert!(out == data);
}
