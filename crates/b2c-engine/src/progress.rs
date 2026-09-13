//! Flushed, timestamped progress output.
//!
//! Every phase that can take more than a moment announces itself *before* it
//! starts, not after it finishes. Redirected stdout on Windows is block
//! buffered, so unflushed output would not appear until a buffer filled, which
//! defeats the point entirely — hence the explicit flush on every line.
//!
//! Goes to stderr so that `--json` can own stdout.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(true);
static PHASE: OnceLock<Mutex<String>> = OnceLock::new();
static LAST: OnceLock<Mutex<String>> = OnceLock::new();

pub fn init(enabled: bool) {
    START.get_or_init(Instant::now);
    ENABLED.store(enabled, Relaxed);
}

/// Silence line output without losing phase tracking, so the dashboard can own
/// the terminal while still showing what stage the run is in.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Relaxed);
}

fn slot(cell: &'static OnceLock<Mutex<String>>) -> &'static Mutex<String> {
    cell.get_or_init(|| Mutex::new(String::new()))
}

/// The slow phase currently in progress.
pub fn current_phase() -> String {
    slot(&PHASE).lock().unwrap().clone()
}

/// Most recent progress line, for surfaces that show only one.
pub fn last_note() -> String {
    slot(&LAST).lock().unwrap().clone()
}

pub fn elapsed_secs() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Announce something, with seconds since process start for spotting stalls.
pub fn note(msg: impl AsRef<str>) {
    *slot(&LAST).lock().unwrap() = msg.as_ref().to_string();
    if !ENABLED.load(Relaxed) {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "[{:7.2}s] {}", elapsed_secs(), msg.as_ref());
    let _ = err.flush();
}

/// Announce a phase that is known to be slow, so a long pause is expected
/// rather than alarming.
pub fn phase(msg: impl AsRef<str>) {
    *slot(&PHASE).lock().unwrap() = msg.as_ref().to_string();
    note(format!("── {}", msg.as_ref()));
}

#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => { $crate::progress::note(format!($($arg)*)) };
}

#[macro_export]
macro_rules! phase {
    ($($arg:tt)*) => { $crate::progress::phase(format!($($arg)*)) };
}
