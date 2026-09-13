//! Live terminal dashboard.
//!
//! Redraws a fixed block in place so the bottleneck is visible at a glance:
//! which side is moving data, how full the arena is, and which state the
//! scheduler picked. Drawn on stderr so `--json` can own stdout.

use b2c_engine::fmt;
use b2c_engine::progress;
use b2c_engine::scheduler::{Scheduler, State};
use b2c_engine::stats::Stats;
use crossterm::{cursor, terminal, ExecutableCommand};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const HISTORY: usize = 40;
const LINES: u16 = 10;
const SPARKS: [char; 9] = [
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

pub struct Handle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn finish(mut self) {
        self.stop.store(true, Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn spawn(stats: Arc<Stats>, sched: Arc<Scheduler>, title: String) -> Handle {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();

    let join = std::thread::Builder::new()
        .name("b2fc-dashboard".into())
        .spawn(move || run(stats, sched, title, s))
        .ok();

    Handle { stop, join }
}

/// Periodic one-line progress for non-interactive output.
///
/// Without this a redirected or piped run prints nothing between "copying" and
/// the final summary — which on a slow destination is minutes of silence with
/// no sign the process is alive.
pub fn spawn_ticker(stats: Arc<Stats>, sched: Arc<Scheduler>, every: Duration) -> Handle {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();

    let join = std::thread::Builder::new()
        .name("b2fc-ticker".into())
        .spawn(move || {
            let mut waited = Duration::ZERO;
            while !s.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
                if waited < every {
                    continue;
                }
                waited = Duration::ZERO;

                let t = sched.telemetry();
                let snap = stats.snapshot();
                let pct = if snap.bytes_total > 0 {
                    snap.bytes_written as f64 / snap.bytes_total as f64 * 100.0
                } else {
                    0.0
                };
                progress::note(format!(
                    "{:5.1}%  {} / {}  read {}  write {}  [{}]  {}r/{}w  arena {:.0}% filled",
                    pct,
                    fmt::bytes(snap.bytes_written),
                    fmt::bytes(snap.bytes_total),
                    fmt::rate(t.read_rate),
                    fmt::rate(t.write_rate),
                    State::from_u8(t.state).label(),
                    t.readers,
                    t.writers,
                    t.filled_occupancy * 100.0
                ));
            }
        })
        .ok();

    Handle { stop, join }
}

fn run(stats: Arc<Stats>, sched: Arc<Scheduler>, title: String, stop: Arc<AtomicBool>) {
    let mut read_hist = vec![0f64; HISTORY];
    let mut write_hist = vec![0f64; HISTORY];
    let started = Instant::now();
    let mut drawn = false;

    while !stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(150));

        let t = sched.telemetry();
        let snap = stats.snapshot();

        read_hist.remove(0);
        read_hist.push(t.read_rate);
        write_hist.remove(0);
        write_hist.push(t.write_rate);

        let mut err = std::io::stderr();
        if drawn {
            let _ = err.execute(cursor::MoveToPreviousLine(LINES));
        }
        drawn = true;

        let state = State::from_u8(t.state);
        let peak_r = read_hist.iter().cloned().fold(1.0f64, f64::max);
        let peak_w = write_hist.iter().cloned().fold(1.0f64, f64::max);
        let scale = peak_r.max(peak_w);

        let done = snap.bytes_written;
        let total = snap.bytes_total.max(1);
        let frac = (done as f64 / total as f64).clamp(0.0, 1.0);
        let elapsed = started.elapsed().as_secs_f64();
        // ETA from the sustained rate, not the peak: a cliff makes the
        // optimistic number a lie within seconds.
        let sustained = if elapsed > 0.0 {
            done as f64 / elapsed
        } else {
            0.0
        };
        let eta = if sustained > 1.0 {
            (total.saturating_sub(done)) as f64 / sustained
        } else {
            f64::NAN
        };

        let lines = [
            format!("  \u{1b}[1m{title}\u{1b}[0m"),
            format!("  {}", "\u{2500}".repeat(66)),
            format!(
                "  read   {} {:>10}   {}",
                bar(t.read_rate / scale, 12),
                fmt::rate(t.read_rate),
                spark(&read_hist, scale)
            ),
            format!(
                "  write  {} {:>10}   {}",
                bar(t.write_rate / scale, 12),
                fmt::rate(t.write_rate),
                spark(&write_hist, scale)
            ),
            format!(
                "  arena  {}  {:>3.0}% filled / {:>3.0}% free",
                bar(t.filled_occupancy, 24),
                t.filled_occupancy * 100.0,
                t.free_occupancy * 100.0
            ),
            format!(
                "  state  \u{1b}[1m{:<9}\u{1b}[0m {}",
                state.label(),
                state.reason()
            ),
            format!(
                "  split  {} readers / {} writers        stalls r:{} w:{}",
                t.readers, t.writers, snap.read_stalls, snap.write_stalls
            ),
            format!(
                "  files  {} / {}    {}  of  {}    ETA {}{}",
                snap.files_done,
                snap.files_total,
                fmt::bytes(done),
                fmt::bytes(snap.bytes_total),
                fmt::duration(eta),
                if snap.errors > 0 {
                    format!("    \u{1b}[1m{} ERRORS\u{1b}[0m", snap.errors)
                } else {
                    String::new()
                }
            ),
            format!("  {}  {:>5.1}%", bar(frac, 48), frac * 100.0),
            format!("  {}", truncate(&phase_line(), 68)),
        ];

        for l in lines {
            let _ = err.execute(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = writeln!(err, "{l}");
        }
        let _ = err.flush();
    }
}

fn phase_line() -> String {
    let phase = progress::current_phase();
    let last = progress::last_note();
    if last.is_empty() || last.contains(&phase) {
        phase
    } else {
        format!("{phase} \u{2014} {last}")
    }
}

fn bar(frac: f64, width: usize) -> String {
    let f = frac.clamp(0.0, 1.0);
    let filled = (f * width as f64).round() as usize;
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        s.push(if i < filled { '\u{2588}' } else { '\u{2591}' });
    }
    s.push(']');
    s
}

fn spark(hist: &[f64], scale: f64) -> String {
    if scale <= 0.0 {
        return String::new();
    }
    hist.iter()
        .map(|v| {
            let idx = ((v / scale) * (SPARKS.len() - 1) as f64).round() as usize;
            SPARKS[idx.min(SPARKS.len() - 1)]
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        format!("{s:<max$}")
    } else {
        s.chars()
            .take(max - 1)
            .chain(std::iter::once('\u{2026}'))
            .collect()
    }
}
