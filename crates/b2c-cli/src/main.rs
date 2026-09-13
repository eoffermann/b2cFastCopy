//! b2fc — adaptive high-throughput bulk file copy for Windows.

mod dashboard;

use b2c_engine::{fmt, progress, Copier, Options};
use clap::Parser;
use crossterm::tty::IsTty;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "b2fc",
    version,
    about = "Adaptive high-throughput bulk file copy for Windows",
    long_about = "Copies with unbuffered I/O through a preallocated RAM arena, shifting \
                  workers between reading and writing as each device's buffers deplete."
)]
struct Args {
    /// Source file or directory. A directory copies its contents into DEST.
    source: PathBuf,

    /// Destination directory.
    dest: PathBuf,

    /// RAM arena size, e.g. 8G. Default: a quarter of available memory, capped at 8G.
    #[arg(long, value_name = "SIZE")]
    ram: Option<String>,

    /// Worker threads. Default: twice the core count, clamped to 4..32.
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// Block size override, e.g. 4M. Normally chosen from the device profile.
    #[arg(long, value_name = "SIZE")]
    block: Option<String>,

    /// Queue depth override for both sides. Normally chosen per device.
    #[arg(long, value_name = "N")]
    qd: Option<usize>,

    /// Files below this size take the simple path instead of the pipeline.
    #[arg(long, value_name = "SIZE", default_value = "4M")]
    small: String,

    /// Re-read both sides afterwards and compare BLAKE3 hashes.
    #[arg(long)]
    verify: bool,

    /// Scan, profile and report without moving any data.
    #[arg(long)]
    dry_run: bool,

    /// Exclude entries matching a glob. Repeatable.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Emit a machine-readable summary on stdout.
    #[arg(long)]
    json: bool,

    /// Use ordinary buffered I/O. Slower; for troubleshooting.
    #[arg(long)]
    no_unbuffered: bool,

    /// Do not draw the live dashboard.
    #[arg(long)]
    no_dashboard: bool,

    /// Do not use SetFileValidData even when the privilege is held.
    #[arg(long)]
    no_fast_prealloc: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    progress::init(!args.json);

    let small = match fmt::parse_size(&args.small) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let ram = match args.ram.as_deref().map(fmt::parse_size).transpose() {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let block = match args.block.as_deref().map(fmt::parse_size).transpose() {
        Ok(v) => v.map(|b| b as usize),
        Err(e) => return fail(&e),
    };

    let opts = Options {
        arena_bytes: ram,
        workers: args.threads,
        block_size: block,
        queue_depth: args.qd,
        small_threshold: small,
        excludes: args.exclude.clone(),
        verify: args.verify,
        dry_run: args.dry_run,
        unbuffered: !args.no_unbuffered,
        fast_preallocate: !args.no_fast_prealloc,
    };

    let copier = match Copier::new(&args.source, &args.dest, opts) {
        Ok(c) => c,
        Err(e) => return fail(&e.to_string()),
    };
    progress::note(format!("plan: {}", copier.plan_summary()));

    // The dashboard owns the terminal while it runs, so line output steps
    // aside; phase names still reach it through the progress module.
    let interactive =
        !args.json && !args.no_dashboard && !args.dry_run && std::io::stderr().is_tty();
    let dash = if interactive {
        progress::set_enabled(false);
        Some(dashboard::spawn(
            copier.stats(),
            copier.scheduler(),
            format!(
                "b2fc  {}  \u{2192}  {}",
                args.source.display(),
                args.dest.display()
            ),
        ))
    } else if !args.json && !args.dry_run {
        // Redirected output still needs a heartbeat, or a slow copy looks hung.
        Some(dashboard::spawn_ticker(
            copier.stats(),
            copier.scheduler(),
            std::time::Duration::from_secs(2),
        ))
    } else {
        None
    };

    let result = copier.run();

    if let Some(d) = dash {
        d.finish();
        progress::set_enabled(true);
    }

    match result {
        Ok(o) => {
            if args.json {
                println!("{}", json_summary(&o));
            } else {
                report(&o, &args);
            }
            if o.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn report(o: &b2c_engine::Outcome, args: &Args) {
    println!();
    if args.dry_run {
        println!(
            "  Dry run: {} files, {} to copy.",
            o.files,
            fmt::bytes(o.bytes)
        );
        return;
    }
    // Lead with the shortfall. A run that copies 20,217 of 20,299 files is a
    // failure, and burying that under a cheerful "Copied N files" is how it
    // gets mistaken for success.
    let missing = o.files_expected.saturating_sub(o.files);
    if missing > 0 {
        println!(
            "  \u{1b}[1mINCOMPLETE: {} of {} file(s) did not copy.\u{1b}[0m",
            missing, o.files_expected
        );
    }
    println!(
        "  Copied {} of {} files, {} in {} \u{2014} {}",
        o.files,
        o.files_expected,
        fmt::bytes(o.bytes),
        fmt::duration(o.seconds),
        fmt::rate(o.rate())
    );
    println!(
        "  Arena {} on {}.",
        fmt::bytes(o.arena_bytes as u64),
        if o.large_pages {
            "large pages"
        } else {
            "normal pages"
        }
    );
    if o.verified > 0 {
        println!("  Verified {} files by checksum.", o.verified);
    }
    if !o.errors.is_empty() {
        println!("\n  {} error(s):", o.errors.len());
        for e in o.errors.iter().take(10) {
            println!("    {e}");
        }
        if o.errors.len() > 10 {
            println!("    ... and {} more", o.errors.len() - 10);
        }
    }
}

fn json_summary(o: &b2c_engine::Outcome) -> String {
    let errs: Vec<String> = o
        .errors
        .iter()
        .take(20)
        .map(|e| format!("\"{}\"", e.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "{{\"files\":{},\"bytes\":{},\"seconds\":{:.3},\"bytes_per_sec\":{:.0},\
         \"arena_bytes\":{},\"large_pages\":{},\"verified\":{},\"errors\":[{}]}}",
        o.files,
        o.bytes,
        o.seconds,
        o.rate(),
        o.arena_bytes,
        o.large_pages,
        o.verified,
        errs.join(",")
    )
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("b2fc: {msg}");
    ExitCode::FAILURE
}
