//! The adaptive scheduler.
//!
//! Workers are role-agnostic: each asks [`Scheduler::role`] what to do next and
//! is told to read or write based on which side is currently starving. Because
//! workers perform blocking I/O, the number assigned to a side *is* that
//! device's queue depth — so shifting a worker and re-tuning queue depth are
//! the same action.
//!
//! The controller is deliberately incremental. It moves one worker per sample
//! toward the indicated side rather than jumping to a computed split, which
//! keeps it stable on links whose throughput is naturally noisy.

use crate::rings::Rings;
use crate::stats::{Ewma, Stats};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Balanced = 0,
    /// Destination is outrunning the source: read harder, fill RAM.
    Fill = 1,
    /// Source is outrunning the destination: RAM is filling with dirty
    /// buffers, so drain them.
    Drain = 2,
    /// The destination's write cache is exhausted. More queue depth would buy
    /// latency and no bandwidth.
    Cliff = 3,
    /// Source and destination share a spindle; alternate large bursts.
    Thrash = 4,
}

impl State {
    pub fn from_u8(v: u8) -> State {
        match v {
            1 => State::Fill,
            2 => State::Drain,
            3 => State::Cliff,
            4 => State::Thrash,
            _ => State::Balanced,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            State::Balanced => "BALANCED",
            State::Fill => "FILL",
            State::Drain => "DRAIN",
            State::Cliff => "CLIFF",
            State::Thrash => "THRASH",
        }
    }

    pub fn reason(&self) -> &'static str {
        match self {
            State::Balanced => "both rings mid-range",
            State::Fill => "write side starved, reading ahead",
            State::Drain => "RAM full of dirty buffers, draining",
            State::Cliff => "destination cache exhausted, holding depth",
            State::Thrash => "same spindle, alternating bursts",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub workers: usize,
    /// Source device's useful queue depth.
    pub max_readers: usize,
    /// Destination device's useful queue depth.
    pub max_writers: usize,
    pub same_spindle: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Telemetry {
    pub read_rate: f64,
    pub write_rate: f64,
    pub write_short: f64,
    pub write_long: f64,
    pub read_short: f64,
    pub read_long: f64,
    pub free_occupancy: f64,
    pub filled_occupancy: f64,
    pub readers: usize,
    pub writers: usize,
    pub state: u8,
}

pub struct Scheduler {
    readers_target: AtomicUsize,
    state: AtomicU8,
    stop: AtomicBool,
    limits: Limits,
    telemetry: Mutex<Telemetry>,
}

/// Fraction below its long-run average at which a device's short-run rate is
/// treated as a cache cliff rather than noise.
const CLIFF_RATIO: f64 = 0.70;
/// Below this rate the ratio test is meaningless, so cliff detection is off.
const CLIFF_FLOOR_BPS: f64 = 8.0 * 1024.0 * 1024.0;

impl Scheduler {
    pub fn new(limits: Limits) -> Scheduler {
        // Start balanced; the controller finds the real split within a second.
        let start = (limits.workers / 2).clamp(1, limits.workers.saturating_sub(1).max(1));
        Scheduler {
            readers_target: AtomicUsize::new(start),
            state: AtomicU8::new(State::Balanced as u8),
            stop: AtomicBool::new(false),
            limits,
            telemetry: Mutex::new(Telemetry::default()),
        }
    }

    pub fn role(&self, worker: usize) -> Role {
        if worker < self.readers_target.load(Relaxed) {
            Role::Read
        } else {
            Role::Write
        }
    }

    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Relaxed))
    }

    pub fn telemetry(&self) -> Telemetry {
        *self.telemetry.lock().unwrap()
    }

    pub fn stop(&self) {
        self.stop.store(true, Relaxed);
    }

    /// Bounds on the reader count that keep both sides alive and respect each
    /// device's useful queue depth.
    fn reader_bounds(&self) -> (usize, usize) {
        let w = self.limits.workers;
        if self.limits.same_spindle {
            // Bursting needs the freedom to put everything on one side.
            return (0, w);
        }
        let lo = w.saturating_sub(self.limits.max_writers).max(1);
        let hi = self.limits.max_readers.min(w.saturating_sub(1)).max(lo);
        (lo, hi)
    }

    /// Sample and steer until [`Scheduler::stop`] is called or the rings shut
    /// down. Intended to be run on its own thread.
    pub fn run(&self, rings: &Rings, stats: &Stats) {
        let tick = Duration::from_millis(100);
        let mut last = Instant::now();
        let (mut last_read, mut last_written) = (0u64, 0u64);

        let mut read_short = Ewma::new(2.0);
        let mut read_long = Ewma::new(30.0);
        let mut write_short = Ewma::new(2.0);
        let mut write_long = Ewma::new(30.0);

        let mut candidate = State::Balanced;
        let mut candidate_count = 0u32;

        while !self.stop.load(Relaxed) && !rings.is_shutdown() {
            std::thread::sleep(tick);

            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f64().max(1e-6);
            last = now;

            let r = stats.bytes_read.load(Relaxed);
            let w = stats.bytes_written.load(Relaxed);
            let read_rate = r.saturating_sub(last_read) as f64 / dt;
            let write_rate = w.saturating_sub(last_written) as f64 / dt;
            last_read = r;
            last_written = w;

            read_short.update(read_rate, dt);
            read_long.update(read_rate, dt);
            write_short.update(write_rate, dt);
            write_long.update(write_rate, dt);

            let cap = rings.capacity().max(1);
            let free = rings.free_len();
            let filled = rings.filled_len();
            let free_occ = free as f64 / cap as f64;
            let filled_occ = filled as f64 / cap as f64;

            let readers = self.readers_target.load(Relaxed);
            let writers = self.limits.workers.saturating_sub(readers);

            // --- classify -------------------------------------------------
            // Starvation is an absolute question, not a proportional one: a
            // writer is starved when there is no chunk for it to take, full
            // stop. Judging by fraction of the arena instead means a large
            // arena reads as "nearly empty" while holding gigabytes of pending
            // work, and the controller starves the destination to read ahead it
            // does not need.
            let writers_starved = filled < writers.max(1);
            let readers_blocked = free < readers.max(1);

            let at_depth = writers >= self.limits.max_writers;
            let cliff = at_depth
                && write_long.get() > CLIFF_FLOOR_BPS
                && write_short.get() < CLIFF_RATIO * write_long.get();

            let next = if self.limits.same_spindle {
                State::Thrash
            } else if cliff {
                State::Cliff
            } else if readers_blocked {
                State::Drain
            } else if writers_starved {
                State::Fill
            } else {
                State::Balanced
            };

            // Hysteresis: a state must be indicated twice running before it
            // takes effect, so a single noisy sample cannot flip the pipeline.
            if next == candidate {
                candidate_count += 1;
            } else {
                candidate = next;
                candidate_count = 1;
            }
            let committed = if candidate_count >= 2 {
                self.state.store(candidate as u8, Relaxed);
                candidate
            } else {
                self.state()
            };

            // --- act ------------------------------------------------------
            let (lo, hi) = self.reader_bounds();
            let target = match committed {
                State::Thrash => {
                    // One long read burst, then one long write burst. Each
                    // direction gets a full arena's worth of sequential work
                    // instead of interleaving seeks.
                    if free_occ < 0.12 {
                        0
                    } else if filled_occ < 0.12 {
                        self.limits.workers
                    } else {
                        readers
                    }
                }
                // Both mean the same thing for worker placement: the write side
                // cannot use more help, so spend the capacity filling RAM.
                State::Fill | State::Cliff => readers.saturating_add(1),
                State::Drain => readers.saturating_sub(1),
                // Neither side is starved, so settle on the split the two
                // devices actually want, weighted by their useful queue depths.
                // For NVMe to NVMe that is an even split; for NVMe to a hard
                // disk it parks almost everything on the read side and leaves
                // the disk the shallow queue it prefers.
                State::Balanced => {
                    let want = self.limits.max_readers + self.limits.max_writers;
                    let base = (self.limits.workers * self.limits.max_readers) / want.max(1);
                    match readers.cmp(&base) {
                        std::cmp::Ordering::Less => readers.saturating_add(1),
                        std::cmp::Ordering::Greater => readers.saturating_sub(1),
                        std::cmp::Ordering::Equal => readers,
                    }
                }
            };
            self.readers_target.store(target.clamp(lo, hi), Relaxed);

            stats.readers.store(target.clamp(lo, hi), Relaxed);
            stats
                .writers
                .store(self.limits.workers - target.clamp(lo, hi), Relaxed);

            *self.telemetry.lock().unwrap() = Telemetry {
                read_rate,
                write_rate,
                write_short: write_short.get(),
                write_long: write_long.get(),
                read_short: read_short.get(),
                read_long: read_long.get(),
                free_occupancy: free_occ,
                filled_occupancy: filled_occ,
                readers: target.clamp(lo, hi),
                writers: self.limits.workers - target.clamp(lo, hi),
                state: committed as u8,
            };
        }
    }
}
