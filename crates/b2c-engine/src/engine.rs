//! Pipeline orchestration: plan, allocate, run the workers, finalise.

use crate::arena::Arena;
use crate::device::{self, round_up, DeviceProfile};
use crate::error::{Error, Result};
use crate::fmt;
use crate::progress;
use crate::rings::{Chunk, Rings};
use crate::scanner::{self, FileTask, Plan};
use crate::scheduler::{Limits, Role, Scheduler};
use crate::stats::Stats;
use crate::win::file::File;
use crate::win::mem::memory_status;
use crate::win::privileges::{self, Privileges};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Options {
    pub arena_bytes: Option<u64>,
    pub workers: Option<usize>,
    pub block_size: Option<usize>,
    pub queue_depth: Option<usize>,
    /// Files below this go the simple route; the pipeline's per-chunk machinery
    /// is pure overhead for them.
    pub small_threshold: u64,
    pub excludes: Vec<String>,
    pub verify: bool,
    pub dry_run: bool,
    pub unbuffered: bool,
    /// Use SetFileValidData when the privilege is held.
    pub fast_preallocate: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            arena_bytes: None,
            workers: None,
            block_size: None,
            queue_depth: None,
            small_threshold: 4 << 20,
            excludes: Vec::new(),
            verify: false,
            dry_run: false,
            unbuffered: true,
            fast_preallocate: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub files: u64,
    pub bytes: u64,
    pub seconds: f64,
    pub errors: Vec<String>,
    pub arena_bytes: usize,
    pub large_pages: bool,
    pub verified: u64,
}

impl Outcome {
    pub fn rate(&self) -> f64 {
        if self.seconds > 0.0 {
            self.bytes as f64 / self.seconds
        } else {
            0.0
        }
    }
}

struct Entry {
    task: FileTask,
    chunks: usize,
    done: AtomicUsize,
    prepared: AtomicBool,
    /// Held open for the file's lifetime so NTFS cannot discard the
    /// preallocation, and so finalisation has a handle to trim with.
    master: Mutex<Option<File>>,
    failed: AtomicBool,
}

#[derive(Clone, Copy)]
struct Job {
    file: u32,
    offset: u64,
    len: u32,
}

struct Shared {
    entries: Vec<Entry>,
    jobs: Vec<Job>,
    cursor: AtomicUsize,
    remaining: AtomicUsize,
    arena: Arena,
    rings: Rings,
    stats: Arc<Stats>,
    sched: Arc<Scheduler>,
    align: u32,
    unbuffered: bool,
    use_valid_data: bool,
    errors: Mutex<Vec<String>>,
}

impl Shared {
    fn fail(&self, msg: String) {
        self.stats.errors.fetch_add(1, Relaxed);
        let mut e = self.errors.lock().unwrap();
        if e.len() < 64 {
            e.push(msg);
        }
    }
}

pub struct Copier {
    src: PathBuf,
    dst: PathBuf,
    opts: Options,
    privs: Privileges,
    src_dev: DeviceProfile,
    dst_dev: DeviceProfile,
    block_size: usize,
    arena_bytes: u64,
    workers: usize,
    stats: Arc<Stats>,
    sched: Arc<Scheduler>,
}

impl Copier {
    pub fn new(src: &Path, dst: &Path, opts: Options) -> Result<Copier> {
        let src = absolute(src)?;
        let dst = absolute(dst)?;

        progress::phase("probing devices");
        let privs = privileges::acquire();
        progress::note(format!(
            "privileges: elevated={}, SetFileValidData={}, large pages={}",
            yes(privs.elevated),
            yes(privs.manage_volume),
            yes(privs.lock_memory)
        ));

        let src_dev = device::profile(&src)?;
        let dst_parent = if dst.exists() {
            dst.clone()
        } else {
            dst.parent().unwrap_or(&dst).to_path_buf()
        };
        let dst_dev = device::profile(&dst_parent)?;
        progress::note(format!("source: {}", src_dev.describe()));
        progress::note(format!("dest:   {}", dst_dev.describe()));

        let same_spindle = device::shares_spindle(&src_dev, &dst_dev);
        if same_spindle {
            progress::note(
                "source and destination share a spindle; using alternating bursts",
            );
        }

        // One block size must satisfy both volumes' alignment, and should suit
        // the slower device, which is the one that will set the pace.
        let align = src_dev.alignment.max(dst_dev.alignment);
        let chosen = opts
            .block_size
            .unwrap_or_else(|| src_dev.block_size.max(dst_dev.block_size));
        let block_size = round_up(chosen, align as usize);

        let mem = memory_status();
        let arena_bytes = opts.arena_bytes.unwrap_or_else(|| {
            // A quarter of what is free, capped. Past a few GB the extra buffer
            // stops buying absorption and only delays startup.
            (mem.available / 4).clamp(256 << 20, 8 << 30)
        });

        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // Workers block on I/O rather than burning CPU, so exceeding the core
        // count is fine and is how queue depth is reached.
        let workers = opts
            .workers
            .unwrap_or_else(|| (cpus * 2).clamp(4, 32))
            .max(2);

        let qd = opts.queue_depth;
        let limits = Limits {
            workers,
            max_readers: qd.unwrap_or(src_dev.queue_depth).min(workers),
            max_writers: qd.unwrap_or(dst_dev.queue_depth).min(workers),
            same_spindle,
        };

        Ok(Copier {
            src,
            dst,
            opts,
            privs,
            src_dev,
            dst_dev,
            block_size,
            arena_bytes,
            workers,
            stats: Arc::new(Stats::default()),
            sched: Arc::new(Scheduler::new(limits)),
        })
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    pub fn scheduler(&self) -> Arc<Scheduler> {
        self.sched.clone()
    }

    pub fn privileges(&self) -> Privileges {
        self.privs
    }

    pub fn profiles(&self) -> (&DeviceProfile, &DeviceProfile) {
        (&self.src_dev, &self.dst_dev)
    }

    pub fn plan_summary(&self) -> String {
        format!(
            "{} workers, {} blocks, {} arena",
            self.workers,
            fmt::bytes(self.block_size as u64),
            fmt::bytes(self.arena_bytes)
        )
    }

    pub fn run(&self) -> Result<Outcome> {
        let started = Instant::now();
        let plan = scanner::scan(
            &self.src,
            &self.dst,
            self.opts.small_threshold,
            &self.opts.excludes,
        )?;

        self.stats.bytes_total.store(plan.total_bytes, Relaxed);
        self.stats
            .files_total
            .store(plan.file_count() as u64, Relaxed);

        if plan.total_bytes > self.dst_dev.volume.free_bytes {
            return Err(Error::config(format!(
                "destination has {} free but the copy needs {}",
                fmt::bytes(self.dst_dev.volume.free_bytes),
                fmt::bytes(plan.total_bytes)
            )));
        }

        let mut outcome = Outcome {
            arena_bytes: 0,
            large_pages: false,
            ..Default::default()
        };

        if self.opts.dry_run {
            progress::note("dry run: no data will be moved");
            outcome.files = plan.file_count() as u64;
            outcome.bytes = plan.total_bytes;
            outcome.seconds = started.elapsed().as_secs_f64();
            return Ok(outcome);
        }

        progress::phase(format!("creating {} directories", plan.dirs.len()));
        for d in &plan.dirs {
            if let Err(e) = std::fs::create_dir_all(d) {
                return Err(Error::config(format!("cannot create {}: {e}", d.display())));
            }
        }

        if !plan.bulk.is_empty() {
            let (bytes, errs, arena_bytes, large) = self.run_bulk(&plan)?;
            outcome.bytes += bytes;
            outcome.errors.extend(errs);
            outcome.arena_bytes = arena_bytes;
            outcome.large_pages = large;
        }

        if !plan.small.is_empty() {
            let (bytes, errs) = self.run_small(&plan);
            outcome.bytes += bytes;
            outcome.errors.extend(errs);
        }

        outcome.files = self.stats.files_done.load(Relaxed);
        outcome.seconds = started.elapsed().as_secs_f64();

        if self.opts.verify {
            outcome.verified = self.run_verify(&plan, &mut outcome.errors);
        }

        Ok(outcome)
    }

    // ---- bulk pipeline ---------------------------------------------------

    fn run_bulk(&self, plan: &Plan) -> Result<(u64, Vec<String>, usize, bool)> {
        // Sector alignment is a constraint of unbuffered I/O only. In buffered
        // mode an alignment of 1 disables the tail padding entirely, so the
        // fallback path is not quietly rounding writes up.
        let align = if self.opts.unbuffered {
            self.src_dev.alignment.max(self.dst_dev.alignment)
        } else {
            1
        };

        progress::phase(format!(
            "allocating {} arena ({} x {} buffers){}",
            fmt::bytes(self.arena_bytes),
            self.arena_bytes / self.block_size as u64,
            fmt::bytes(self.block_size as u64),
            if self.privs.lock_memory {
                ", trying large pages"
            } else {
                ""
            }
        ));
        let arena = Arena::new(
            self.arena_bytes as usize,
            self.block_size,
            self.privs.lock_memory,
        )?;
        progress::note(format!(
            "arena ready: {} buffers, large pages {}",
            arena.count(),
            if arena.large_pages {
                "yes"
            } else {
                "no (using normal pages)"
            }
        ));

        let mut entries = Vec::with_capacity(plan.bulk.len());
        let mut jobs = Vec::new();
        for (i, t) in plan.bulk.iter().enumerate() {
            let chunks = (t.size as usize).div_ceil(self.block_size).max(1);
            for c in 0..chunks {
                let offset = (c * self.block_size) as u64;
                let len = (t.size - offset).min(self.block_size as u64) as u32;
                jobs.push(Job {
                    file: i as u32,
                    offset,
                    len,
                });
            }
            entries.push(Entry {
                task: t.clone(),
                chunks,
                done: AtomicUsize::new(0),
                prepared: AtomicBool::new(false),
                master: Mutex::new(None),
                failed: AtomicBool::new(false),
            });
        }

        let total_chunks = jobs.len();
        let shared = Shared {
            entries,
            jobs,
            cursor: AtomicUsize::new(0),
            remaining: AtomicUsize::new(total_chunks),
            rings: Rings::new(arena.count()),
            arena,
            stats: self.stats.clone(),
            sched: self.sched.clone(),
            align,
            unbuffered: self.opts.unbuffered,
            // SetFileValidData needs the read access that only the unbuffered
            // destination handle asks for.
            use_valid_data: self.privs.manage_volume
                && self.opts.fast_preallocate
                && self.opts.unbuffered,
            errors: Mutex::new(Vec::new()),
        };

        progress::phase(format!(
            "copying {} bulk files ({} chunks) with {} workers",
            plan.bulk.len(),
            total_chunks,
            self.workers
        ));

        let before = self.stats.bytes_written.load(Relaxed);
        std::thread::scope(|scope| {
            let sched = self.sched.clone();
            let sh = &shared;
            let sched_thread = scope.spawn(move || sched.run(&sh.rings, &sh.stats));

            let workers: Vec<_> = (0..self.workers)
                .map(|w| {
                    let sh = &shared;
                    scope.spawn(move || worker(w, sh))
                })
                .collect();

            // Join the workers first, then stop the scheduler. A scope waits
            // for every thread it spawned, so signalling the scheduler after
            // the scope would deadlock: it would never be told to exit.
            for h in workers {
                let _ = h.join();
            }
            self.sched.stop();
            shared.rings.shutdown();
            let _ = sched_thread.join();
        });

        let moved = self.stats.bytes_written.load(Relaxed) - before;
        let errors = shared.errors.lock().unwrap().clone();
        Ok((
            moved,
            errors,
            shared.arena.total_bytes(),
            shared.arena.large_pages,
        ))
    }

    // ---- small files -----------------------------------------------------

    /// Small files are metadata-bound, so the pipeline buys nothing. This runs
    /// them in parallel through the platform copy, which also carries
    /// attributes and timestamps across for free.
    ///
    /// Batched directory enumeration and coalescing many files into one buffer
    /// are the real wins here and are not implemented yet.
    fn run_small(&self, plan: &Plan) -> (u64, Vec<String>) {
        progress::phase(format!("copying {} small files", plan.small.len()));
        let next = AtomicUsize::new(0);
        let errors = Mutex::new(Vec::new());
        let bytes = std::sync::atomic::AtomicU64::new(0);
        let threads = self.workers.min(16);

        std::thread::scope(|scope| {
            for _ in 0..threads {
                let next = &next;
                let errors = &errors;
                let bytes = &bytes;
                scope.spawn(move || loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(t) = plan.small.get(i) else { break };
                    match std::fs::copy(&t.src, &t.dst) {
                        Ok(n) => {
                            bytes.fetch_add(n, Relaxed);
                            self.stats.add_written(n);
                            self.stats.add_read(n);
                            self.stats.files_done.fetch_add(1, Relaxed);
                            self.stats.small_files_done.fetch_add(1, Relaxed);
                        }
                        Err(e) => {
                            self.stats.errors.fetch_add(1, Relaxed);
                            let mut g = errors.lock().unwrap();
                            if g.len() < 64 {
                                g.push(format!("{}: {e}", t.src.display()));
                            }
                        }
                    }
                });
            }
        });

        (bytes.load(Relaxed), errors.into_inner().unwrap())
    }

    // ---- verification ----------------------------------------------------

    fn run_verify(&self, plan: &Plan, errors: &mut Vec<String>) -> u64 {
        progress::phase("verifying (re-reading both sides)");
        let all: Vec<&FileTask> = plan.bulk.iter().chain(plan.small.iter()).collect();
        let next = AtomicUsize::new(0);
        let ok = std::sync::atomic::AtomicU64::new(0);
        let bad = Mutex::new(Vec::new());

        std::thread::scope(|scope| {
            for _ in 0..self.workers.min(8) {
                let (next, ok, bad, all) = (&next, &ok, &bad, &all);
                scope.spawn(move || loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(t) = all.get(i) else { break };
                    match (hash_file(&t.src), hash_file(&t.dst)) {
                        (Ok(a), Ok(b)) if a == b => {
                            ok.fetch_add(1, Relaxed);
                        }
                        (Ok(_), Ok(_)) => {
                            bad.lock()
                                .unwrap()
                                .push(format!("checksum mismatch: {}", t.dst.display()));
                        }
                        _ => {
                            bad.lock()
                                .unwrap()
                                .push(format!("unreadable during verify: {}", t.dst.display()));
                        }
                    }
                });
            }
        });

        let failures = bad.into_inner().unwrap();
        let n = ok.load(Relaxed);
        progress::note(format!("verified {n} files, {} mismatched", failures.len()));
        errors.extend(failures);
        n
    }
}

// ---- worker ---------------------------------------------------------------

struct Handles {
    src: HashMap<u32, File>,
    dst: HashMap<u32, File>,
}

fn worker(idx: usize, sh: &Shared) {
    let mut h = Handles {
        src: HashMap::new(),
        dst: HashMap::new(),
    };

    while sh.remaining.load(Relaxed) > 0 {
        // Try the assigned side first, then the other. Falling through keeps a
        // worker useful when its own side is momentarily empty, and is what
        // stops a mis-set split from deadlocking the pipeline.
        let worked = match sh.sched.role(idx) {
            Role::Read => try_read(sh, &mut h) || try_write(sh, &mut h),
            Role::Write => try_write(sh, &mut h) || try_read(sh, &mut h),
        };
        if !worked {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn try_read(sh: &Shared, h: &mut Handles) -> bool {
    // Claim the buffer first: the job cursor only moves forward, so a job taken
    // without a buffer to put it in could not be handed back.
    let Some(buf) = sh.rings.acquire_free(Duration::from_millis(20)) else {
        sh.stats.read_stalls.fetch_add(1, Relaxed);
        return false;
    };
    let i = sh.cursor.fetch_add(1, Relaxed);
    let Some(&job) = sh.jobs.get(i) else {
        sh.rings.release_free(buf);
        return false;
    };

    let entry = &sh.entries[job.file as usize];
    let file = match h.src.entry(job.file) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let opened = if sh.unbuffered {
                File::open_read_unbuffered(&entry.task.src)
            } else {
                File::open_read_buffered(&entry.task.src)
            };
            match opened {
                Ok(f) => v.insert(f),
                Err(e) => {
                    sh.fail(format!("{}: {e}", entry.task.src.display()));
                    entry.failed.store(true, Relaxed);
                    sh.rings.release_free(buf);
                    sh.remaining.fetch_sub(1, Relaxed);
                    return true;
                }
            }
        }
    };

    // Unbuffered reads must be whole sectors, so the tail is read rounded up.
    let padded = round_up(job.len as usize, sh.align as usize) as u32;
    let ptr = unsafe { sh.arena.ptr(buf) };
    let got = match unsafe { file.read_at(ptr, padded, job.offset) } {
        Ok(n) => n,
        Err(e) => {
            sh.fail(format!("{}: {e}", entry.task.src.display()));
            entry.failed.store(true, Relaxed);
            sh.rings.release_free(buf);
            sh.remaining.fetch_sub(1, Relaxed);
            return true;
        }
    };

    // Zero the slack past end-of-file so nothing stale from a previous chunk is
    // written into the destination's final sector, even though set_len will
    // trim it away afterwards.
    if got < padded {
        unsafe { std::ptr::write_bytes(ptr.add(got as usize), 0, (padded - got) as usize) };
    }

    sh.stats.add_read(job.len as u64);
    sh.rings.push_filled(Chunk {
        buf,
        file: job.file,
        offset: job.offset,
        len: job.len,
        padded,
    });
    true
}

fn try_write(sh: &Shared, h: &mut Handles) -> bool {
    let Some(chunk) = sh.rings.pop_filled(Duration::from_millis(20)) else {
        sh.stats.write_stalls.fetch_add(1, Relaxed);
        return false;
    };
    let entry = &sh.entries[chunk.file as usize];

    if let Err(e) = prepare_destination(sh, chunk.file) {
        sh.fail(format!("{}: {e}", entry.task.dst.display()));
        entry.failed.store(true, Relaxed);
        sh.rings.release_free(chunk.buf);
        sh.remaining.fetch_sub(1, Relaxed);
        return true;
    }

    let file = match h.dst.entry(chunk.file) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let opened = if sh.unbuffered {
                File::open_write_unbuffered(&entry.task.dst)
            } else {
                File::open_existing_write(&entry.task.dst)
            };
            match opened {
                Ok(f) => v.insert(f),
                Err(e) => {
                    sh.fail(format!("{}: {e}", entry.task.dst.display()));
                    entry.failed.store(true, Relaxed);
                    sh.rings.release_free(chunk.buf);
                    sh.remaining.fetch_sub(1, Relaxed);
                    return true;
                }
            }
        }
    };

    let ptr = unsafe { sh.arena.ptr(chunk.buf) };
    if let Err(e) = unsafe { file.write_at(ptr, chunk.padded, chunk.offset) } {
        sh.fail(format!("{}: {e}", entry.task.dst.display()));
        entry.failed.store(true, Relaxed);
    } else {
        sh.stats.add_written(chunk.len as u64);
    }

    sh.rings.release_free(chunk.buf);
    sh.remaining.fetch_sub(1, Relaxed);

    if entry.done.fetch_add(1, Relaxed) + 1 == entry.chunks {
        finalize(sh, chunk.file);
    }
    true
}

/// Create, size and preallocate the destination exactly once.
fn prepare_destination(sh: &Shared, file: u32) -> Result<()> {
    let entry = &sh.entries[file as usize];
    if entry.prepared.load(Relaxed) {
        return Ok(());
    }
    let mut master = entry.master.lock().unwrap();
    if entry.prepared.load(Relaxed) {
        return Ok(());
    }

    let f = if sh.unbuffered {
        File::create_write_unbuffered(&entry.task.dst)?
    } else {
        File::create_write_buffered(&entry.task.dst)?
    };
    let padded = round_up(entry.task.size as usize, sh.align as usize) as u64;

    // Set EOF before preallocating: NTFS trims an allocation back to
    // end-of-file, so preallocating against a zero-length file achieves
    // nothing.
    f.set_len(padded)?;
    let _ = f.preallocate(padded);
    if sh.use_valid_data {
        // Best effort. Failure only costs the zero-fill we were trying to skip.
        let _ = f.set_valid_data(padded);
    }

    *master = Some(f);
    entry.prepared.store(true, Relaxed);
    Ok(())
}

/// Trim the padding and carry timestamps across.
fn finalize(sh: &Shared, file: u32) {
    let entry = &sh.entries[file as usize];
    let master = entry.master.lock().unwrap();
    let Some(f) = master.as_ref() else { return };

    // The write path rounded the tail up to a whole sector; this is what makes
    // the file its real length again.
    if let Err(e) = f.set_len(entry.task.size) {
        sh.fail(format!("{}: {e}", entry.task.dst.display()));
    }

    if let Ok(md) = std::fs::metadata(&entry.task.src) {
        let created = md.created().map(to_filetime).unwrap_or(0);
        let accessed = md.accessed().map(to_filetime).unwrap_or(0);
        let written = md.modified().map(to_filetime).unwrap_or(0);
        let _ = f.set_times(created, accessed, written);
    }

    if !entry.failed.load(Relaxed) {
        sh.stats.files_done.fetch_add(1, Relaxed);
    }
}

// ---- helpers --------------------------------------------------------------

fn to_filetime(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        // FILETIME counts 100 ns ticks from 1601; UNIX_EPOCH is 11644473600
        // seconds later.
        Ok(d) => {
            (d.as_secs() as i64 + 11_644_473_600) * 10_000_000 + (d.subsec_nanos() as i64 / 100)
        }
        Err(_) => 0,
    }
}

fn hash_file(p: &Path) -> std::io::Result<blake3::Hash> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    std::env::current_dir()
        .map(|d| d.join(p))
        .map_err(|e| Error::config(format!("cannot resolve {}: {e}", p.display())))
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
