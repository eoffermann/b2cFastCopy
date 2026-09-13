//! The two queues whose occupancy drives the scheduler.
//!
//! `free` holds buffer indices a reader may fill. `filled` holds chunks a writer
//! must drain. Their depletion is the signal the whole design turns on: an empty
//! `filled` means the destination is starved and we should read; an empty `free`
//! means RAM is saturated with dirty data and we should write.
//!
//! Locking is a plain mutex and condvar rather than anything lock-free. Buffers
//! are megabytes, so these queues are touched a few thousand times a second at
//! most — contention is irrelevant next to the I/O, and the simplicity is worth
//! more than the nanoseconds.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// One unit of work in flight: a slice of one file living in one buffer.
#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub buf: u32,
    pub file: u32,
    /// Absolute byte offset within the file.
    pub offset: u64,
    /// Real bytes of file data.
    pub len: u32,
    /// Bytes actually written, rounded up to the destination's alignment. Equal
    /// to `len` except for a final chunk, which is trimmed by `set_len` after.
    pub padded: u32,
}

pub struct Rings {
    free: Mutex<Vec<u32>>,
    free_cv: Condvar,
    filled: Mutex<VecDeque<Chunk>>,
    filled_cv: Condvar,
    capacity: usize,
    shutdown: AtomicBool,
}

impl Rings {
    pub fn new(buffers: usize) -> Rings {
        Rings {
            free: Mutex::new((0..buffers as u32).collect()),
            free_cv: Condvar::new(),
            filled: Mutex::new(VecDeque::with_capacity(buffers)),
            filled_cv: Condvar::new(),
            capacity: buffers,
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Claim an empty buffer, waiting up to `wait`.
    ///
    /// Returns `None` on timeout so the caller can re-ask the scheduler whether
    /// it should still be reading — that is how a worker parked on a starved
    /// side gets redirected instead of blocking forever.
    pub fn acquire_free(&self, wait: Duration) -> Option<u32> {
        let mut q = self.free.lock().unwrap();
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(id) = q.pop() {
                return Some(id);
            }
            let (next, timeout) = self.free_cv.wait_timeout(q, wait).unwrap();
            q = next;
            if timeout.timed_out() {
                return None;
            }
        }
    }

    pub fn release_free(&self, buf: u32) {
        self.free.lock().unwrap().push(buf);
        self.free_cv.notify_one();
    }

    pub fn push_filled(&self, c: Chunk) {
        self.filled.lock().unwrap().push_back(c);
        self.filled_cv.notify_one();
    }

    pub fn pop_filled(&self, wait: Duration) -> Option<Chunk> {
        let mut q = self.filled.lock().unwrap();
        loop {
            if let Some(c) = q.pop_front() {
                return Some(c);
            }
            if self.shutdown.load(Ordering::Relaxed) {
                return None;
            }
            let (next, timeout) = self.filled_cv.wait_timeout(q, wait).unwrap();
            q = next;
            if timeout.timed_out() {
                return None;
            }
        }
    }

    pub fn free_len(&self) -> usize {
        self.free.lock().unwrap().len()
    }

    pub fn filled_len(&self) -> usize {
        self.filled.lock().unwrap().len()
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.free_cv.notify_all();
        self.filled_cv.notify_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}
