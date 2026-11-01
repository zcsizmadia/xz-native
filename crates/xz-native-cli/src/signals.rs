//! Signals (xz's `signals.c` and the progress signals of `message.c`):
//! SIGINT and SIGTERM stop the coding at the next chunk, so that the
//! partial output can be removed, and then end the process with the same
//! signal; SIGUSR1 (and SIGINFO where it exists) prints the progress.
//!
//! SIGHUP isn't caught: `signal-hook` can't tell whether it was ignored
//! (as `nohup` does), and catching it then would stop a `nohup` run.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The signal that asked to stop, or 0 (`exit_signal`).
fn abort_flag() -> &'static Arc<AtomicUsize> {
    static F: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// A progress line was asked for (`progress_needs_updating`).
fn progress_flag() -> &'static Arc<AtomicBool> {
    static F: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// `signals_init()`; failures leave the default handling.
pub fn init() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    for sig in [SIGINT, SIGTERM] {
        let _ = signal_hook::flag::register_usize(sig, abort_flag().clone(), sig as usize);
    }
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR1, progress_flag().clone());
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINFO, progress_flag().clone());
    }
}

/// The coding should stop (`user_abort`).
pub fn aborted() -> bool {
    abort_flag().load(Ordering::Relaxed) != 0
}

/// Takes a pending request for a progress line.
pub fn take_progress() -> bool {
    progress_flag().swap(false, Ordering::Relaxed)
}

/// `signals_exit()`: after a signal, ends the process with it (on Unix)
/// once the cleanup is done; returns otherwise.
pub fn exit_if_aborted() {
    let sig = abort_flag().load(Ordering::Relaxed);
    if sig == 0 {
        return;
    }
    #[cfg(unix)]
    {
        let _ = signal_hook::low_level::emulate_default_handler(sig as i32);
    }
    // Windows, or the signal didn't end the process: an error status.
    std::process::exit(i32::from(crate::E_ERROR));
}
