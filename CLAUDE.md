# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`b2fc` — an adaptive bulk file copier for Windows. Rust workspace: `b2c-engine` (library,
all the logic) and `b2c-cli` (thin CLI/TUI consumer, binary `b2fc`). `README.md` carries
the design rationale and the measured benchmark numbers; read it before changing the
pipeline.

## Environment: check, never assume

Two host facts change which code paths are available, and both vary between sessions.
**Probe them at the start of a session rather than trusting anything written here.**

```bash
WA=/c/Windows/System32/whoami.exe
"$WA" //groups | grep -i "Mandatory Label"          # High = elevated
"$WA" //priv | grep -iE "SeLockMemory|SeManageVolume"
rustc --version
```

- **Elevation.** Claude Code may be launched elevated or not; the same repo sees both.
  `High Mandatory Level` means elevated.
- **Privileges.** A privilege listed as `Disabled` is *present and usable* — it just has
  to be enabled at runtime with `AdjustTokenPrivileges` (which `win::privileges::acquire`
  does). Only a privilege missing from the list entirely is unavailable. Do not read
  `Disabled` as "off".
  - `SeManageVolumePrivilege` → `SetFileValidData`. Follows elevation.
  - `SeLockMemoryPrivilege` → large pages. Needs an explicit "Lock pages in memory" grant
    in Local Security Policy *plus a re-logon*; elevation alone does not confer it. It is
    granted on this machine but will not be on a fresh one.
- Elevation also gates the raw-device latency probe used for device classification.

Every privileged fast path has a working unprivileged fallback, reported at startup.
Those fallbacks are supported configurations, not error cases.

## Commands

```powershell
cargo build --release        # lto="fat", codegen-units=1
cargo test                   # 12 tests
cargo test copies_boundary   # single test by substring
cargo test -- --nocapture    # show progress output
cargo clippy --workspace --all-targets   # currently clean; keep it that way
cargo fmt --all
```

Performance work must be measured on `--release`; debug numbers are meaningless. The
`windows-sys` crate needs a current toolchain — if dependency resolution produces a
confusing error, check `rustc --version` first.

## Architecture

```
SCANNER ──work items──► READER pool ──filled ring──► WRITER pool
                             ▲                            │
                             └───────free ring────────────┘
                                       ▲
                                  SCHEDULER
```

- **Arena** (`arena.rs`) — one `VirtualAlloc` block carved into fixed buffers at startup.
  Nothing allocates on the hot path.
- **Rings** (`rings.rs`) — `free` holds buffers readers may fill, `filled` holds chunks
  writers must drain. When chasing a performance bug, start by asking which ring was
  empty.
- **Workers are role-agnostic** (`engine.rs::worker`). Each asks the scheduler what to do.
  Do not introduce dedicated reader and writer threads — that reintroduces the lockstep
  the design exists to avoid. A worker whose own side is empty falls through to the other
  side, which is what stops a bad split from deadlocking.
- **Queue depth is thread count.** Workers block on I/O, so the number on a side *is* that
  device's queue depth.
- **Writes are positioned**, so chunks need no ordering.

### Starvation is absolute, not proportional

The single most important lesson so far. The first scheduler judged starvation by
*fraction* of arena occupancy; with an 8 GiB arena, "35% filled" still looked nearly
empty, so it ramped to 23 readers against 1 writer and throttled the destination. That
cost 30% of throughput on NVMe→NVMe (637 MB/s; the fix gave 914 MB/s, beating robocopy).

A writer is starved when `filled < writers` — a count, not a ratio. Preserve that
distinction in any change to `scheduler.rs`.

## Correctness traps

Places where a bug is silent rather than loud:

- **`FILE_GENERIC_WRITE` must not be combined with `FILE_FLAG_NO_BUFFERING`.** It carries
  `FILE_APPEND_DATA`, and append access is refused with unbuffered I/O — every
  destination open fails with a bare `ERROR_INVALID_PARAMETER` (87) that names nothing.
  Use the raw `GENERIC_*` rights (`ACCESS_READ`/`ACCESS_WRITE` in `win/file.rs`). Pinned
  by `append_access_is_rejected_with_unbuffered_io`.
- **Synchronous handles serialise I/O.** Sharing one handle across workers collapses
  queue depth to one. Each worker opens its own handle per file.
- **Unbuffered tail writes.** The last chunk is written rounded up to a sector multiple,
  then `set_len` trims it. Skip the trim and files are padded with garbage that no error
  reports. `copies_boundary_sizes_byte_exact` asserts exact lengths and bytes.
- **Set EOF before preallocating.** NTFS trims an allocation back to end-of-file, so
  `FileAllocationInfo` against a zero-length file does nothing.
- **Cluster size is not global.** Block sizes round to the destination volume's cluster
  size. H: uses 64 KiB clusters while everything else uses 4 KiB.
- **Queue depth is not monotonically good.** On a USB HDD, concurrency costs throughput.

## Test environment and benchmark reality

i7-9750H (6C/12T), 64 GB RAM. C:/E: Kingston A2000 NVMe (DRAM-less, SLC-cached — the
local `CLIFF` test case). G: USB SSD. F:/H: USB HDDs. F:, G: and H: share USB host
controllers, so concurrent transfers between them are not independent bandwidth.

**Most routes are destination-bound, and no tool can win there.** Measured walls: USB SSD
~865 MB/s, USB HDD ~190 MB/s. b2fc, robocopy `/J /MT:32` and plain robocopy all land
within a few percent on those routes; on the HDD, plain single-threaded robocopy is
marginally fastest because the disk cannot use concurrency. Only NVMe→NVMe has headroom,
and that is where b2fc wins (1.13×). Do not treat a tie on a saturated device as a
regression — check which device was the wall first.

- Benchmarks need a fresh destination directory each run; robocopy skips existing files
  and b2fc does not.
- **This repository lives on H:, one of the drives under test.** Prefer E:↔G: or E:↔C:
  for clean numbers.
- 6 cores and a laptop thermal envelope: keep worker counts modest and re-run profiling
  rather than trusting figures across thermal states.

## Conventions

Long-running phases emit flushed progress output via `progress::note` / `progress::phase`
— a line *before* each slow phase starts, plus elapsed-time context. When the dashboard
owns the terminal, line output is suppressed and phase names reach it through
`progress::current_phase()`; when output is redirected, a ticker prints every two seconds.
Silence during a stall is treated as a bug, not a style preference.
