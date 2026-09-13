# b2cFastCopy

A bulk file copier for Windows that keeps every device in the transfer pinned at its
real sustained ceiling, using RAM as an elastic shock absorber between a fast side and
a slow side.

The target workload is tens to hundreds of gigabytes per run, with a mix that spans
single 10 GB+ files and directory trees of hundreds of thousands of small ones. Binary
name: `b2fc`.

```powershell
b2fc E:\projects G:\backup
```

## Measured results

Real numbers from this machine, 12 GiB corpora, release build. The honest summary is
that **most copies are limited by the destination device, and when that is true nothing
can win** — b2fc, robocopy and Explorer all converge on the same figure. The interesting
case is the one where the destination has headroom.

| Route | Corpus | Windows baseline | b2fc | Ratio |
|---|---|---|---|---|
| NVMe → NVMe (E:→C:) | 12 GiB, 12 files | robocopy `/J /MT:32` — 15.2 s, 809 MB/s | **13.4 s, 914 MB/s** | **1.13×** |
| NVMe → USB SSD (E:→G:) | 12.1 GiB, 4012 files | robocopy `/J /MT:32` — 14.3 s, 868 MB/s | 14.4 s, 859 MB/s | 0.99× |
| NVMe → USB HDD (E:→F:) | 12 GiB, 12 files | robocopy (plain) — 64.3 s, 191 MB/s | 66.3 s, 185 MB/s | 0.97× |
| Small files → USB SSD | 4000 × 32 KiB | robocopy `/MT:32` — 3,274 files/s | **3,933 files/s** | **1.20×** |

Read that table carefully before expecting miracles. The USB SSD saturates at ~865 MB/s
and the USB HDD at ~190 MB/s; every tool hits those walls, and on the HDD plain
single-threaded robocopy is actually the fastest of the three because the disk cannot use
concurrency at all. Where the destination is genuinely fast, the pipeline pulls ahead.

### The property that does not show up as MB/s

On the HDD run, b2fc read all 12 GiB into RAM within the first 24 seconds of a 66-second
copy, then spent the remaining 42 seconds draining RAM to the disk with the source
completely idle:

```
[  6.02s]   9.0%  1.08 GiB / 12.0 GiB  read 1.63 GB/s  write 209 MB/s  [DRAIN]  16r/8w  arena 87% filled
[ 24.10s]  35.7%  4.29 GiB / 12.0 GiB  read 0 MB/s     write 417 MB/s  [DRAIN]  16r/8w  arena 95% filled
[ 48.21s]  72.5%  8.70 GiB / 12.0 GiB  read 0 MB/s     write 209 MB/s  [BALANCED] ...
```

The transfer takes the same wall-clock time either way, but the source drive is released
a third of the way in instead of being held for the whole copy. If you are copying *off*
a drive you want back — or running several transfers at once — that is the difference.

### Cache cliffs are real and visible

The NVMe→NVMe run caught the destination's SLC cache running out, mid-copy:

```
[  6.02s]  66.1%  read 1.29 GB/s  write 1.33 GB/s  [FILL]
[  8.03s]  71.9%  read 1.09 GB/s  write 209 MB/s   [BALANCED]   <- SLC cache exhausted
[ 12.04s]  94.1%  read 0 MB/s     write 838 MB/s   [BALANCED]
```

Both drives are DRAM-less Kingston A2000s, which absorb writes into a fast SLC region and
fall off a cliff once it fills. This is exactly the condition `CLIFF` detection exists
for.

## Status

Working today: the unbuffered pipeline, the RAM arena with large-page support, the
adaptive scheduler including cliff detection and same-spindle burst mode, device
profiling with a random-read latency probe, preallocation with `SetFileValidData`,
parallel small-file copying, BLAKE3 verification, the live dashboard, and JSON output.

Not built yet: resume journals, mirror/delete, ReFS block cloning, server-side copy,
hardlink dedup, alternate data streams, ACLs, sparse-file awareness, and the batched
small-file enumeration described below. Reparse points are skipped rather than recreated.

## Why Windows copy bogs down

Explorer and `CopyFileEx` are not slow because the hardware is slow:

1. **Buffered I/O double-handling.** The default path reads through the system file
   cache, so every byte is copied user→kernel→cache→kernel→user, plus cache pressure
   that evicts everything else the machine was using.
2. **Cache-manager collapse.** Sustained writes fill the modified page list faster than
   the lazy writer drains it. Windows then throttles hard — the classic "starts at
   900 MB/s, settles at 60 MB/s" curve.
3. **Queue depth of roughly one.** A single synchronous read/write loop leaves an NVMe
   drive mostly idle; it needs many concurrent requests to reach its rated speed.
4. **Per-file overhead dominates small files.** `CreateFile` + directory update +
   `CloseHandle` costs roughly 0.2–1 ms, and Explorer does it serially.
5. **Rigid pacing.** One block size and thread count for every device. The settings that
   saturate NVMe actively harm a USB hard disk.
6. **No awareness of device state.** When an SSD's SLC cache fills, the right response is
   to stop pushing writes and spend the stall reading ahead.

`robocopy /J /MT:32` already addresses 1–3, which is why it is the baseline above rather
than Explorer.

## Architecture

```
  ┌──────────┐   free buffers    ┌──────────┐   filled buffers   ┌──────────┐
  │ SCANNER  │ ◄──────────────── │  READER  │ ─────────────────► │  WRITER  │
  │ enumerate│    work items     │   pool   │ ◄──────────────────│   pool   │
  │ + plan   │ ─────────────────►│          │   recycled buffers │          │
  └──────────┘                   └──────────┘                    └──────────┘
                                       ▲                               ▲
                                       │      ┌──────────────┐         │
                                       └──────│  SCHEDULER   │─────────┘
                                              └──────────────┘
```

**Arena** — one `VirtualAlloc` block carved into fixed-size buffers at startup. Nothing
allocates on the hot path. Page-aligned, which satisfies sector alignment everywhere.

**Free ring** holds buffers a reader may fill; **filled ring** holds chunks a writer must
drain. Their depletion drives everything.

**Workers are role-agnostic.** Each asks the scheduler what to do and is assigned to
whichever side is starving. Because they perform *blocking* I/O, the number on a side
**is** that device's queue depth — shifting a worker and re-tuning queue depth are the
same action.

**Writes are positioned.** Every buffer carries its file id and byte offset and is
written there, so chunks need no ordering and any worker can drain any chunk.

Split as a library (`b2c-engine`) with a thin CLI/TUI consumer (`b2c-cli`), so the
scheduler is testable headlessly.

## The adaptive scheduler

Samples every 100 ms; transitions require two consecutive indications, so one noisy
sample cannot flip the pipeline.

| State | Condition | Response |
|---|---|---|
| `BALANCED` | neither side starved | Settle on the split the two devices want, weighted by their queue depths. |
| `FILL` | fewer filled chunks than writers | Destination is starved for work. Shift workers to reading. |
| `DRAIN` | fewer free buffers than readers | RAM is saturated with dirty buffers. Shift workers to writing. |
| `CLIFF` | short-window rate <70% of long-window while already at queue depth | Device cache exhausted. Stop escalating; spend the capacity filling RAM. |
| `THRASH` | source and destination share a spindle | Alternate full-arena read and write bursts instead of interleaving seeks. |

### Starvation is absolute, not proportional

The first version judged starvation by *fraction* of the arena, and it cost 30% of
throughput on the NVMe→NVMe route: with an 8 GiB arena, "35% filled" still reads as
nearly-empty, so the controller kept adding readers until it ran 23 readers against
1 writer — throttling a destination that could have taken far more.

A writer is starved when there is no chunk for it to take. That is a count, not a ratio,
and the fix took the same route from 637 MB/s to 914 MB/s. Any future change here must
preserve that distinction.

## Device profiling

Paths resolve to physical devices, so the scheduler knows when two paths share a spindle.

| Class | Block size | Queue depth |
|---|---|---|
| NVMe SSD | 4 MiB | 32 |
| SATA/USB SSD | 2 MiB | 12 |
| HDD | 8 MiB | 2 |
| Unknown | 2 MiB | 8 |

Classification uses `IOCTL_STORAGE_QUERY_PROPERTY`'s seek-penalty descriptor, refined by
bus type. **When the device will not answer — which USB bridges frequently do not — it
falls back to timing 16 random 4 KiB reads against the raw device.** On this machine the
8 TB Seagate reports no seek penalty and was being classified as `unknown`; the probe
measures 15.19 ms and classifies it as spinning, which moves it from 2 MiB/QD 8 to
8 MiB/QD 2. The probe needs raw device read access, so it returns nothing unelevated and
classification falls back to the conservative default.

## I/O mechanics

- `CreateFileW` with `FILE_FLAG_NO_BUFFERING`, and every read/write supplies its own
  `OVERLAPPED` carrying an explicit offset. The call stays blocking — no completion
  plumbing — while dropping any dependence on a shared file pointer.
- **One handle per worker per file.** Windows serialises I/O on a *synchronous* file
  object, so sharing one handle would queue every worker behind the last and collapse
  queue depth to one.
- **Do not use `FILE_GENERIC_WRITE` with `FILE_FLAG_NO_BUFFERING`.** It includes
  `FILE_APPEND_DATA`, and append access is rejected outright in combination with
  unbuffered I/O — every destination open fails with a bare `ERROR_INVALID_PARAMETER`
  naming nothing. Raw `GENERIC_WRITE` is mapped after that validation and is accepted.
  There is a regression test pinning this down.
- **Tail handling.** The final chunk is written rounded up to a sector multiple, then
  `SetEndOfFile` trims the file to its true length. Getting this wrong pads files with
  garbage that no error surfaces, which is why the round-trip test asserts exact lengths
  and exact bytes across every boundary size.
- **Preallocation.** EOF is set *before* `FileAllocationInfo`, because NTFS trims an
  allocation back to end-of-file and preallocating a zero-length file achieves nothing.
- **`SetFileValidData`** skips NTFS zero-fill when the privilege is held. It can expose
  previously-deleted disk contents, so it is used only when elevated, only when the range
  will be fully overwritten, and can be switched off.

## Small-file path

Below `--small` (default 4 MiB) files run through a parallel pool rather than the
pipeline, whose per-chunk machinery is pure overhead for them. This currently uses the
platform copy, which carries attributes and timestamps across for free and measures about
1.2× robocopy.

The real wins here — batched directory enumeration via
`GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, dedicated open/create threads,
and coalescing many files into one buffer — are **not implemented**. Small-file work is
bound by metadata operations, so that is where the remaining headroom is.

## Memory

Arena defaults to a quarter of available RAM, capped at 8 GiB, floor 256 MiB; override
with `--ram`. Pages are committed but not touched, so a multi-GB arena costs nothing at
startup and faults in on use.

Large pages are attempted when `SeLockMemoryPrivilege` is held and fall back silently —
they need physically contiguous memory, so a large request genuinely can fail on a
machine that has been up a while. Normal pages are a fully supported configuration.

## CLI

```
b2fc <source> <dest> [options]

  --ram <size>          Arena size (e.g. 8G). Default: 25% of available, capped at 8G
  --threads <n>         Worker threads. Default: 2x cores, clamped to 4..32
  --block <size>        Block size override (normally from the device profile)
  --qd <n>              Queue depth override for both sides
  --small <size>        Small-file threshold. Default 4M
  --verify              Re-read both sides and compare BLAKE3 hashes
  --dry-run             Scan, profile and report; move nothing
  --exclude <glob>      Repeatable; matches file and directory names
  --json                Machine-readable summary on stdout
  --no-unbuffered       Buffered I/O fallback, for filesystems that refuse NO_BUFFERING
  --no-dashboard        Suppress the live dashboard
  --no-fast-prealloc    Do not use SetFileValidData even when available
```

## Output

On a terminal, a live dashboard shows per-side throughput with sparklines, arena
occupancy, the reader/writer split, the current scheduler state and why it was entered,
and an ETA computed from the sustained rather than peak rate.

Redirected or piped, it emits a flushed progress line every two seconds instead, because
a copy to a slow disk is minutes long and silence is indistinguishable from a hang:

```
[  10.04s]  14.5%  1.73 GiB / 12.0 GiB  read 292 MB/s  write 250 MB/s  [DRAIN]  16r/8w  arena 99% filled
```

## Build

```powershell
rustup update stable      # needs a current toolchain for windows-sys
cargo build --release
cargo test
```

Elevation is optional. Without it the tool works fully; it simply cannot use
`SetFileValidData`, large pages, or the raw-device latency probe, and says so at startup.

## Reproducing the benchmarks

Corpus: 12 × 1 GiB files plus 4000 × 32 KiB files. Baselines are `robocopy /E /J /MT:32`
(unbuffered, multi-threaded — the strongest one) and plain `robocopy` (what Windows does
by default). Both tools read unbuffered under `/J`, so neither gains from the file cache.

Every run writes to a fresh destination directory, since robocopy skips files that are
already present and b2fc does not.

## Non-goals

- Cross-platform support. This is Win32-specific by design.
- A general sync tool: no conflict resolution, no bidirectional sync, no cloud providers.
- Replacing robocopy's full flag surface.

## Conventions

Long-running phases emit flushed progress output — a line *before* each slow phase
begins, plus elapsed-time context. Silence during a stall is treated as a bug.
