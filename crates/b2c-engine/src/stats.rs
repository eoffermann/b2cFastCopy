//! Shared counters. Cheap to update from the hot path, sampled by the scheduler
//! and the dashboard.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

#[derive(Default)]
pub struct Stats {
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub bytes_total: AtomicU64,
    pub files_done: AtomicU64,
    pub files_total: AtomicU64,
    pub small_files_done: AtomicU64,
    /// Times a reader found no free buffer: RAM full of unwritten data.
    pub read_stalls: AtomicU64,
    /// Times a writer found nothing to write: the destination was starved.
    pub write_stalls: AtomicU64,
    pub readers: AtomicUsize,
    pub writers: AtomicUsize,
    pub errors: AtomicU64,
}

impl Stats {
    pub fn add_read(&self, n: u64) {
        self.bytes_read.fetch_add(n, Relaxed);
    }

    pub fn add_written(&self, n: u64) {
        self.bytes_written.fetch_add(n, Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            bytes_read: self.bytes_read.load(Relaxed),
            bytes_written: self.bytes_written.load(Relaxed),
            bytes_total: self.bytes_total.load(Relaxed),
            files_done: self.files_done.load(Relaxed),
            files_total: self.files_total.load(Relaxed),
            read_stalls: self.read_stalls.load(Relaxed),
            write_stalls: self.write_stalls.load(Relaxed),
            readers: self.readers.load(Relaxed),
            writers: self.writers.load(Relaxed),
            errors: self.errors.load(Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub read_stalls: u64,
    pub write_stalls: u64,
    pub readers: usize,
    pub writers: usize,
    pub errors: u64,
}

/// Exponentially weighted rate estimate in bytes/sec.
///
/// Two of these at different time constants are what make a cache cliff
/// visible: a short window drops immediately, a long window remembers what the
/// device was doing before it ran out of cache.
#[derive(Debug, Clone, Copy)]
pub struct Ewma {
    value: f64,
    tau: f64,
    primed: bool,
}

impl Ewma {
    pub fn new(tau_secs: f64) -> Ewma {
        Ewma {
            value: 0.0,
            tau: tau_secs,
            primed: false,
        }
    }

    pub fn update(&mut self, sample: f64, dt: f64) {
        if !self.primed {
            self.value = sample;
            self.primed = true;
            return;
        }
        let alpha = 1.0 - (-dt / self.tau).exp();
        self.value += alpha * (sample - self.value);
    }

    pub fn get(&self) -> f64 {
        self.value
    }
}
