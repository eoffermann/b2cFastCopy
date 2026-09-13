# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Greenfield. The repository currently contains only `README.md`, which is the design
specification — read it before writing code. There is no `Cargo.toml`, no source, and
no git repository yet. Nothing here is legacy; if a decision in the README looks wrong,
it is still cheap to change.

Language is Rust (chosen for the reader/writer thread pools and shared buffer arena).
Front end is a CLI plus a live TUI dashboard. Binary name: `b2fc`.

## Toolchain prerequisite

The installed toolchain is **rustc 1.60.0 (2022)**, which is too old for the current
`windows` crate. Before the first build:

```powershell
rustup self update
rustup update stable
```

Verify with `rustc --version` before debugging any dependency-resolution failure — a
confusing `windows`-crate error is far more likely to be the stale toolchain than a bad
version pin.

## Commands

Not yet scaffolded; these apply once `Cargo.toml` exists.

```powershell
cargo build --release        # release profile: lto="fat", codegen-units=1, panic="abort"
cargo test                   # all tests
cargo test <name>            # single test by substring match
cargo test -- --nocapture    # show progress/println output during tests
cargo clippy --all-targets
cargo fmt
```

Performance work must be measured on `--release`. Unbuffered I/O paths in a debug build
are dominated by unoptimized buffer handling and the numbers are meaningless.

## Architecture

Three-stage pipeline joined by two ring buffers over one preallocated RAM arena:

```
SCANNER ──work items──► READER pool ──filled ring──► WRITER pool
                             ▲                            │
                             └───────free ring────────────┘
                                       ▲
                                  SCHEDULER
```

- **Arena** — one sector-aligned `VirtualAlloc` block carved into fixed-size buffers at
  startup. No allocation on the hot path, ever.
- **Free ring** (buffers available to readers) and **filled ring** (buffers awaiting
  write) are the two queues whose depletion drives everything. When reasoning about a
  performance bug, start by asking which ring was empty.
- **Workers are role-agnostic.** A thread asks the scheduler what to do and is assigned
  to whichever side is starving. Do not introduce dedicated reader threads and writer
  threads — that reintroduces the lockstep behavior the design exists to avoid.
- **Scheduler** samples every 100 ms and drives a state machine: `BALANCED`, `FILL`
  (destination outrunning source → read more), `DRAIN` (source outrunning destination →
  write more), `CLIFF`, `THRASH` (same spindle both ends), `SEEK` (small-file path).
  Transitions use hysteresis; changes here must preserve it or the pipeline oscillates
  on noisy USB links.
- Split as a library (`b2c-engine`) with a thin CLI/TUI consumer (`b2c-cli`) so the
  scheduler is testable headlessly.

`CLIFF` is the point of the project: when short-window throughput diverges from the long
window *while queue depth is already at the device optimum*, the device's write cache
(SSD SLC, or a USB bridge buffer) is exhausted. Adding queue depth then adds latency and
no bandwidth. The correct response is to stop escalating and redirect capacity to the
other side of the pipeline to fill RAM.

## Correctness traps specific to this codebase

These are the places where bugs are silent rather than loud:

- **Unbuffered tail writes.** With `FILE_FLAG_NO_BUFFERING`, buffer address, offset, and
  length must all be sector multiples. A file's last fragment almost never is: write the
  rounded-up length, then `SetEndOfFile` to the true length. Getting this wrong pads
  files with garbage that no error surfaces. Cover it with tests from the first commit.
- **Cluster size is not global.** Block sizes round to the *destination volume's* cluster
  size. On this machine H: uses 64 KB clusters while every other volume uses 4 KB, so a
  single hardcoded block constant is wrong.
- **`SetFileValidData`** skips NTFS zero-fill (a large win) but requires
  `SE_MANAGE_VOLUME_NAME` and can expose previously-deleted disk contents. Elevated +
  full-overwrite + explicit opt-in only.
- **The dev environment is not elevated.** Large pages (`SeLockMemoryPrivilege`) and
  `SetFileValidData` are both unavailable by default. Every privileged fast path needs a
  working unprivileged fallback, reported at startup rather than failing silently.
- **Queue depth is not monotonically good.** On a USB HDD, concurrency actively costs
  throughput. Never apply an NVMe-shaped default globally.

## Test environment

i7-9750H (6C/12T), 64 GB RAM. C:/E: are Kingston A2000 NVMe (DRAM-less, SLC-cached — the
primary local test case for `CLIFF`). G: is a USB SSD; F: and H: are USB HDDs.

- F:, G:, H: share USB host controllers, so concurrent transfers between them are not
  independent bandwidth. The scheduler budgets per controller, not per disk.
- **This repository lives on H:, one of the drives under test.** Benchmarks touching H:
  are also measuring the repo drive; prefer E:↔G: for clean numbers.
- Benchmarks must drop caches between runs or the results are fiction. Baselines are
  `robocopy /MT:32 /J`, plain `robocopy`, and Explorer.
- 6 cores and a laptop thermal envelope: keep worker counts modest and re-run device
  profiling rather than trusting cached profiles across thermal states.

## Conventions

Long-running phases emit flushed progress output — a line *before* each slow phase
starts, plus elapsed-time context. Tree scanning, device ramp benchmarks, and multi-GB
arena allocation are all multi-second and would otherwise sit silent. Silence during a
stall is treated as a bug, not a style preference.
