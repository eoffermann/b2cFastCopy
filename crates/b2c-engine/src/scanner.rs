//! Source tree enumeration and planning.
//!
//! Splits the work into a bulk list and a small-file list, because the two are
//! bound by completely different things: bulk files by device bandwidth, small
//! files by per-file metadata cost. Feeding both through one pipeline makes each
//! worse.

use crate::error::{Error, Result};
use crate::progress;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct FileTask {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Destination directories to create, parents before children.
    pub dirs: Vec<PathBuf>,
    pub bulk: Vec<FileTask>,
    pub small: Vec<FileTask>,
    pub total_bytes: u64,
    pub skipped_links: usize,
}

impl Plan {
    pub fn file_count(&self) -> usize {
        self.bulk.len() + self.small.len()
    }
}

/// Walk `src`, planning a copy into `dst`.
///
/// Directory sources copy their *contents* into `dst`, matching robocopy rather
/// than `cp -r`.
pub fn scan(src: &Path, dst: &Path, small_threshold: u64, excludes: &[String]) -> Result<Plan> {
    let meta = std::fs::metadata(src)
        .map_err(|e| Error::config(format!("cannot read source {}: {e}", src.display())))?;

    let mut plan = Plan::default();

    if meta.is_file() {
        // A file source may target either a directory or an explicit filename.
        let target = if dst.is_dir() {
            match src.file_name() {
                Some(n) => dst.join(n),
                None => return Err(Error::config("source has no file name")),
            }
        } else {
            dst.to_path_buf()
        };
        if let Some(parent) = target.parent() {
            plan.dirs.push(parent.to_path_buf());
        }
        push(
            &mut plan,
            FileTask {
                src: src.into(),
                dst: target,
                size: meta.len(),
            },
            small_threshold,
        );
        return Ok(plan);
    }

    progress::phase(format!("scanning {}", src.display()));
    let started = Instant::now();
    let mut last_report = Instant::now();

    plan.dirs.push(dst.to_path_buf());
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];

    while let Some((from, to)) = stack.pop() {
        let entries = match std::fs::read_dir(&from) {
            Ok(e) => e,
            Err(e) => {
                progress::note(format!("skipping {}: {e}", from.display()));
                continue;
            }
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if excluded(&name_str, excludes) {
                continue;
            }

            let Ok(ft) = entry.file_type() else { continue };

            // Reparse points are copied as links or skipped, never followed:
            // following a junction can walk the same tree forever.
            if ft.is_symlink() {
                plan.skipped_links += 1;
                continue;
            }

            let child_src = entry.path();
            let child_dst = to.join(&name);

            if ft.is_dir() {
                plan.dirs.push(child_dst.clone());
                stack.push((child_src, child_dst));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                plan.total_bytes += size;
                push(
                    &mut plan,
                    FileTask {
                        src: child_src,
                        dst: child_dst,
                        size,
                    },
                    small_threshold,
                );
            }
        }

        // A large tree takes real time to walk; say so while it happens.
        if last_report.elapsed().as_secs_f64() >= 2.0 {
            progress::note(format!(
                "  scanned {} files, {} so far",
                plan.file_count(),
                crate::fmt::bytes(plan.total_bytes)
            ));
            last_report = Instant::now();
        }
    }

    progress::note(format!(
        "scan complete: {} files ({} bulk, {} small), {} in {:.1}s",
        plan.file_count(),
        plan.bulk.len(),
        plan.small.len(),
        crate::fmt::bytes(plan.total_bytes),
        started.elapsed().as_secs_f64()
    ));
    if plan.skipped_links > 0 {
        progress::note(format!("  skipped {} reparse points", plan.skipped_links));
    }

    // Parents before children, so directory creation never races.
    plan.dirs.sort();
    plan.dirs.dedup();
    // Largest first: big files start early and keep the pipeline saturated
    // while the tail of smaller ones fills in around them.
    plan.bulk.sort_by_key(|t| std::cmp::Reverse(t.size));
    Ok(plan)
}

fn push(plan: &mut Plan, task: FileTask, threshold: u64) {
    if task.size >= threshold {
        plan.bulk.push(task);
    } else {
        plan.small.push(task);
    }
}

fn excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Case-insensitive glob supporting `*` and `?`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
