# b2cFastCopy

A bulk file copier for Windows that keeps every device in the transfer pinned at its
real sustained ceiling, using RAM as an elastic shock absorber between a fast side and
a slow side.

The target workload is tens to hundreds of gigabytes per run, with a mix that spans
single 10 GB+ files and directory trees of hundreds of thousands of small ones. Binary
name: `b2fc`.

## Why Windows copy bogs down

Explorer and `CopyFileEx` are not slow because the hardware is slow. They lose
throughput to a handful of specific, fixable mechanisms:

1. **Buffered I/O double-handling.** The default path reads through the system file
   cache, so every byte is copied user→kernel→cache→kernel→user. On a 200 GB transfer
   that is 200 GB of pointless `memcpy` plus cache pressure that evicts everything else
   the machine was using.
2. **Cache-manager collapse.** Sustained writes fill the standby/modified page lists
   faster than the lazy writer drains them. Windows then throttles the writer hard,
   which is the classic "starts at 900 MB/s, settles at 60 MB/s" curve.
3. **Queue depth of roughly one.** A single synchronous read/write loop leaves an NVMe
   drive about 80–90% idle. NVMe needs many concurrent requests in flight to reach its
   rated speed; one outstanding I/O reaches a fraction of it.
4. **Per-file overhead dominates small files.** `CreateFile` + directory update +
   `CloseHandle` costs roughly 0.2–1 ms. At 500k files that is minutes of pure metadata
   work, entirely independent of file size, and Explorer does it serially.
5. **Rigid pacing.** One fixed block size and one fixed thread count for every device.
   The settings that saturate NVMe actively harm a USB hard disk, and vice versa.
6. **No awareness of device state.** When an SSD's SLC write cache fills or a USB
   bridge's buffer drains, the correct response is to stop pushing writes and spend the
   stall reading ahead. Windows just keeps pushing and adds latency.

This project attacks all six.

## Design goals

- Saturate the **slowest** device in the pair, continuously, with no sawtooth.
- Use RAM as a large elastic buffer so a fast source keeps working through a slow
  destination's stalls, and a fast destination keeps working through a slow source's.
- Never evict the user's working set from the system cache — bypass it entirely for
  bulk data.
- Treat spinning disks, USB SSDs, and NVMe as different machines with different optimal
  parameters, discovered at runtime rather than hardcoded.
- Degrade gracefully: every privileged fast path has an unprivileged fallback.
- Be observable. If it is slow, the dashboard should make it obvious *which* stage is
  the bottleneck.

## Architecture

A three-stage pipeline connected by two ring buffers over one preallocated RAM arena.

```
  ┌──────────┐   free buffers    ┌──────────┐   filled buffers   ┌──────────┐
  │ SCANNER  │ ◄──────────────── │  READER  │ ─────────────────► │  WRITER  │
  │          │                   │   pool   │                    │   pool   │
  │ enumerate│    work items     │          │ ◄──────────────────│          │
  │ + plan   │ ─────────────────►│ (IOCP)   │   recycled buffers │  (IOCP)  │
  └──────────┘                   └──────────┘                    └──────────┘
                                       ▲                               ▲
                                       │      ┌──────────────┐         │
                                       └──────│  SCHEDULER   │─────────┘
                                              │ reallocates  │
                                              │ workers by   │
                                              │ queue state  │
                                              └──────────────┘
```

**Scanner** walks the source tree, pre-creates the destination directory structure in
one pass, classifies each file as bulk or small, and emits work items. It runs ahead of
the readers so the pipeline never waits on enumeration.

**Arena** is a single preallocated, sector-aligned block of RAM carved into fixed-size
buffers. Allocated once at startup; no allocation happens on the hot path.

**Free ring** holds buffers available to readers. **Filled ring** holds buffers holding
data awaiting a write. These two rings are the "read buffer" and "write buffer" whose
depletion drives the scheduler.

**Readers and writers** are role-agnostic worker threads. A worker asks the scheduler
what to do next and gets assigned to whichever side is starving. Both sides submit
overlapped I/O to an I/O completion port, so a small number of threads sustains a large
number of in-flight requests.

The engine is a library (`b2c-engine`) with the CLI/TUI (`b2c-cli`) as a thin consumer,
so the scheduler can be tested headlessly and driven by another front end later.

## The adaptive scheduler

This is the core of the tool. A control thread samples every 100 ms and computes:

- `free_occupancy` — fraction of the arena available to readers
- `filled_occupancy` — fraction of the arena holding data awaiting write
- short-window and long-window EWMA throughput, per device, for read and write
- in-flight request count and completion latency percentiles, per device

It then picks a state. Transitions use hysteresis (a state must be indicated for two
consecutive samples, and the thresholds for entering and leaving differ) so the pipeline
does not oscillate between modes on noisy USB links.

| State | Condition | Response |
|---|---|---|
| `BALANCED` | both rings 25–75% | Hold current split. |
| `FILL` | filled ring near empty, free plentiful | Destination is outrunning the source. Shift workers to reading, raise read queue depth toward the source's measured optimum, extend readahead. |
| `DRAIN` | free ring near empty, filled ring full | Source is outrunning the destination. RAM is full of dirty buffers. Shift workers to writing; readers block rather than spin. |
| `CLIFF` | short EWMA drops >30% below long EWMA while queue depth is at or above optimum | Device cache exhaustion (see below). Stop adding queue depth, settle to the observed sustained rate, and spend the headroom filling RAM from the other side. |
| `THRASH` | source and destination on the same physical spindle | Abandon concurrency. Alternate large read and write bursts sized to the arena so the head makes one long seek per burst instead of thousands. |
| `SEEK` | small-file region | Hand off to the small-file path; bulk workers stand down. |

### Cliff detection

The behavior the tool exists for. Two distinct stalls look identical from the
application's side but need the same response:

- A consumer SSD absorbs writes into a fast SLC cache, then falls to its native TLC/QLC
  rate once that cache fills — often a 3–4x drop, tens of GB into a transfer.
- A USB bridge or HDD fills its onboard DRAM buffer and the drain rate falls to the
  platter's real sequential rate.

In both cases, adding queue depth makes it worse: more outstanding requests means more
latency and no more bandwidth. The scheduler watches for a sustained divergence between
a short (~2 s) and long (~30 s) throughput window while queue depth is already at the
device's measured optimum. On detection it stops escalating, re-baselines the device's
expected rate, and redirects worker capacity to the *other* side of the pipeline —
reading ahead to fill RAM so that when the destination recovers, there is a full arena
ready to burst into it.

This is also what makes the slow-destination case fast: the source drive runs flat out
into RAM during the destination's stall instead of idling in lockstep with it.

## Device profiling

Paths are resolved to physical devices (volume → disk extents → device), so the
scheduler knows when two paths share a spindle or a USB host controller. Drives on the
same xHCI controller share bandwidth and are budgeted together, not treated as
independent.

Per device, probed once and cached in a profile file keyed by device serial:

- Seek penalty (`IOCTL_STORAGE_QUERY_PROPERTY` / `StorageDeviceSeekPenaltyProperty`) —
  the authoritative spinning-vs-solid signal, better than `MediaType`.
- Bus type, physical and logical sector size, NTFS cluster size.
- A short ramp benchmark: block size × queue depth sweep, recording the knee.

Starting points before a profile exists:

| Class | Block size | Queue depth | Notes |
|---|---|---|---|
| NVMe SSD | 1–4 MB | 32–64 | Many files concurrently; scales with depth. |
| SATA/USB SSD | 1–2 MB | 8–16 | Depth past ~16 adds latency, not bandwidth. |
| USB HDD | 8–16 MB | 1–2 | Large sequential blocks. Concurrency is actively harmful. |
| Network/SMB | 1–4 MB | 16–32 | Latency-bound; depth matters more than block size. |

Block size is always rounded to a multiple of the destination volume's cluster size, not
a global constant — see the machine notes below, where one drive uses 64 KB clusters and
the rest use 4 KB.

For HDD sources, work items are sorted by physical extent
(`FSCTL_GET_RETRIEVAL_POINTERS`) so the head sweeps in one direction rather than
following directory order.

## I/O mechanics

- `CreateFile` with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`. No buffering is
  what avoids both the double-copy and the cache-manager collapse. (`SEQUENTIAL_SCAN` is
  ignored when `NO_BUFFERING` is set; it is only used on the small-file path.)
- Unbuffered I/O requires buffer address, file offset, and length to be multiples of the
  volume's physical sector size. The arena is allocated via `VirtualAlloc`, which is
  page-aligned and therefore satisfies every sector size in play.
- **Tail handling.** A file's last fragment is rarely a sector multiple. Write the
  rounded-up length, then `SetEndOfFile` to the true length. Getting this wrong silently
  pads files with garbage, so it is covered by tests from the start.
- **Preallocation.** `SetFileInformationByHandle(FileAllocationInfo)` before writing
  avoids fragmenting the destination and repeated MFT updates.
- **Zero-fill avoidance.** `SetFileValidData` skips NTFS's zeroing of the preallocated
  range, a large win on big files — but it requires `SE_MANAGE_VOLUME_NAME` (admin) and
  can expose previously-deleted disk contents in the gap between valid data and file
  size. Used only when elevated, only when the file will be fully overwritten, and
  behind an explicit opt-in flag.
- **Sparse files** are detected with `FSCTL_QUERY_ALLOCATED_RANGES` and only their
  allocated ranges are transferred.

## Small-file path

Below a threshold (default 1 MB) the constraint is metadata operations, not bandwidth,
and the bulk pipeline's machinery is pure overhead.

- Enumerate with `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` in large
  batches rather than `FindFirstFile`/`FindNextFile` per entry.
- Pre-create the entire destination directory tree in one pass so no writer ever blocks
  on a missing parent.
- Dedicated open/create threads, since handle creation is the actual serialization
  point, with directory-relative opens to skip repeated full-path parsing.
- Coalesce many small files into one arena buffer and use buffered I/O, where the cache
  helps rather than hurts.

## Fast paths

Checked before any bytes move:

- **ReFS block cloning** (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) — same-volume copies become
  near-instant metadata operations.
- **Server-side copy** (`FSCTL_SRV_COPYCHUNK`) when source and destination are on the
  same SMB server, so data never crosses the network.
- **Hardlink dedup** — files sharing a file ID are copied once and linked, detected via
  a file-ID map during scanning.

## Memory management

- Arena default: 50% of *available* physical RAM, capped at 24 GB, floor of 512 MB.
  Overridable with `--ram`. Sized from `GlobalMemoryStatusEx` at startup and never grown
  on the hot path.
- Large pages (`MEM_LARGE_PAGES`) reduce TLB pressure on a multi-GB arena but require
  `SeLockMemoryPrivilege`, which is not granted by default. Attempted, with silent
  fallback to normal pages.
- The arena is deliberately *not* all given to one side. A hard reserve keeps a minimum
  number of free buffers so a `DRAIN` state can never fully deadlock the readers.

## Integrity, resume, and fidelity

- Optional BLAKE3 hashing on the read side, where the data is already in cache and the
  cost overlaps with I/O wait. `--verify` adds a destination read-back pass.
- A journal of completed files and partial offsets allows `--resume` after an
  interruption without rescanning or recopying.
- Metadata preserved: timestamps, attributes, alternate data streams
  (`FindFirstStreamW`), reparse points and junctions (copied as links by default rather
  than followed), and optionally ACLs.
- Destination free space and path-length (>260 char) feasibility are checked during
  scanning, before the first byte moves, rather than failing 180 GB in.

## CLI

```
b2fc <source> <dest> [options]

  --mirror              Make dest match source, deleting extras
  --ram <size>          Arena size (e.g. 16G). Default: 50% of available
  --threads <n>         Worker threads. Default: logical CPUs
  --block <size>        Override block size (normally auto-profiled)
  --qd <n>              Override queue depth (normally auto-profiled)
  --verify              Read back and compare hashes after writing
  --resume              Continue from the journal of a prior run
  --dry-run             Plan, profile, and report; move no data
  --exclude <glob>      Repeatable
  --json                Machine-readable progress on stdout
  --no-unbuffered       Fall back to buffered I/O (troubleshooting)
  --bench               Profile the device pair and print the tuning table
```

## Live dashboard

When stdout is a terminal, a TUI shows per-device read and write throughput with
sparklines, arena occupancy as a two-sided bar (free vs filled), in-flight queue depth
against the profiled optimum, current scheduler state with the reason it was entered,
files/sec on the small-file path, and an ETA computed from sustained rather than peak
rate. Piped or redirected output degrades to flushed progress lines, or JSON under
`--json`.

## Tuning notes for this machine

Measured at setup time (i7-9750H, 6C/12T, 64 GB RAM):

| Drive | Device | Bus | Physical sector | Cluster | Implication |
|---|---|---|---|---|---|
| C: | Kingston A2000 NVMe | NVMe | 4096 | 4 KB | High QD, boot drive — leave headroom |
| E: | Kingston A2000 NVMe | NVMe | 4096 | 4 KB | High QD target |
| G: | SSK Portable SSD | USB | 512 | 4 KB | Moderate QD; UASP-dependent |
| F: | Seagate Backup+ Hub 9 TB | USB | 4096 | 4 KB | HDD: QD 1–2, big blocks |
| H: | WD Game Drive 4.6 TB | USB | 4096 | **64 KB** | HDD **and** 64 KB clusters — block sizes must be multiples of 64 KB here |

Consequences worth remembering:

- The A2000 is a DRAM-less consumer NVMe drive with an SLC cache. Sustained large writes
  *will* hit a cliff. This is the primary local test case for `CLIFF` handling.
- F:, G:, and H: are all USB. Drives sharing an xHCI controller share bandwidth, so a
  simultaneous F:→H: copy is not two independent 200 MB/s streams. The scheduler budgets
  per controller.
- This is a 6-core laptop. The thread budget is small and thermal throttling is real;
  worker counts should stay modest and the profiler should be re-run rather than trusted
  across thermal states.
- The repository itself lives on H:, one of the drives under test. Benchmarks that read
  or write H: are measuring the repo drive too — use E:↔G: for clean numbers.

## Build

Prerequisite: the installed toolchain is **rustc 1.60 (2022), which is too old** for the
current `windows` crate. Update first:

```powershell
rustup self update
rustup update stable
```

Then:

```powershell
cargo build --release
```

Release builds use `lto = "fat"`, `codegen-units = 1`, and `panic = "abort"`.

Elevation is optional. Without it the tool works fully; it simply cannot use
`SetFileValidData` or large pages, and says so at startup rather than failing silently.

## Roadmap

1. **Skeleton** — CLI parsing, scanner, buffered single-threaded copy. Correct, slow,
   and a baseline to measure against.
2. **Unbuffered pipeline** — arena, rings, overlapped I/O + IOCP, fixed parameters.
   Tail handling and preallocation. This is where the first large win lands.
3. **Device profiling** — detection, ramp benchmark, persisted profiles, per-device
   parameters.
4. **Adaptive scheduler** — the state machine, cliff detection, worker reallocation.
5. **Small-file path** — batched enumeration, parallel creates, coalescing.
6. **TUI dashboard.**
7. **Fidelity and durability** — metadata, ADS, resume journal, verification.
8. **Fast paths** — ReFS cloning, server-side copy, hardlink dedup.

## Benchmarking

Every performance claim is measured against a fixed corpus, not estimated. Baselines:
`robocopy /MT:32 /J`, plain `robocopy`, and Explorer drag-and-drop.

Three corpora: one large-file set (10× 10 GB), one small-file set (200k files averaging
40 KB), and one mixed real-world tree. Each run reports wall time, mean and sustained
throughput, and peak RAM.

Caches must be dropped between runs or the numbers are fiction — the harness does this
explicitly and refuses to report a run where it could not.

## Non-goals

- Cross-platform support. This is Win32-specific by design; portable abstractions are
  what give up the performance.
- A general sync tool. No conflict resolution, no bidirectional sync, no cloud
  providers.
- Compression or dedup of the transferred stream.
- Replacing robocopy's full flag surface. Correct, fast bulk copy with fidelity —
  nothing more.

## Project conventions

Long-running phases must emit flushed progress output, including a line *before* each
slow phase begins and elapsed-time context. Scanning 500k files, running a device ramp
benchmark, and allocating a multi-GB arena are all multi-second operations that would
otherwise sit silent. Silence during a stall is treated as a bug.
