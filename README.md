# RawGeotag

A Rust CLI that geotags camera raw files from one or more GPX tracks.

Raw files carry a capture timestamp but no location; a GPS logger records a track
over the same period. RawGeotag correlates the two by time, linearly interpolating
position between track points, and writes the result as an XMP sidecar next to each
raw file.

**Raw files are never modified.** All output goes to sidecars, so the whole
operation is reversible by deleting the generated `.xmp` files.

## Status

**Implemented and building.** The unit test suite passes and `cargo clippy -- -D
warnings` is clean; run `cargo test` for the current tally.

Verified against real shoots and their GPX tracks, on two camera bodies:

- **1,024 CR3s** (Canon EOS R5) — 1,002 tagged, 22 correctly refused as falling in
  track gaps. Interpolated positions agree with hand-computed values from the raw
  track points to within the coordinate encoding's resolution (~0.2 m).
- **3,883 CR3s, 188 GB** — 2,394 tagged, 1,489 refused (772 across `<trkseg>` breaks,
  the rest in 5-to-60-minute dropouts). This body had its clock on `+01:00`, which
  exercises the EXIF timezone path that a `+00:00` camera leaves as a no-op; spot
  checks match the raw GPX exactly.
- **NEFs** (Nikon D3300, Sedona 2019) — 30 of 30 tagged against the day's track.
  This body writes no EXIF timezone, so it also confirms the refusal path: without
  `--utc-offset` the run aborts having written nothing. An interpolated position
  recomputed by hand from the raw GPX agreed to **under 5 cm**, and the altitude
  (1,323 m) is right for Sedona.

Output is deterministic: the same input at `--jobs 1`, `2` and `16` produces
byte-identical sidecars and an identical warning list. ExifTool is used throughout
as an independent oracle to read the sidecars back and validate them. See
[`docs/PLAN.md`](docs/PLAN.md) for the full verification plan.

## Build

Requires Rust (via <https://rustup.rs>) and, on Windows, the MSVC toolchain from
the Visual Studio Build Tools "Desktop development with C++" workload.

```
cargo build --release
cargo test
```

The binary lands at `target/release/rawgeotag` (`rawgeotag.exe` on Windows).

## Usage

```
rawgeotag <DIR> <EXT> <GPX>... [OPTIONS]

  DIR    parent directory, searched recursively
  EXT    raw extension: "cr3" or "nef" (case-insensitive, leading "." tolerated)
  GPX    path to a GPX track file; repeat for a day split across several tracks

  --utc-offset <±HHMM>     offset for files with no EXIF timezone, e.g. -0700
  --max-gap <SECONDS>      refuse to interpolate across a longer hole [default: 60]
  --max-distance <METERS>  refuse to interpolate across a wider hole [default: 100]
  --force                  overwrite existing sidecars (default: skip with a warning)
  --dry-run                do all work, write nothing
  -j, --jobs <N>           worker threads (default: 2; raise for network storage)
      --no-progress        suppress the progress bar
  -v, --verbose            per-file detail
```

Example:

```
rawgeotag ./shoot cr3 ./track.gpx --utc-offset -0700
```

**Canon CR3 and Nikon NEF are supported.** They differ in one way worth knowing
before a NEF run: many Nikon bodies write no EXIF timezone at all, so every file
hits the "no timezone" refusal and `--utc-offset` becomes mandatory rather than
optional. NEF also costs more to read — see Performance.

```
rawgeotag ./shoot nef ./track.gpx --utc-offset +0000
```

A day is often split across several tracks — a driving log and a separate evening
walk. Pass them all and each photo is matched against whichever one covers its
capture time:

```
rawgeotag ./shoot cr3 ./daytime.gpx ./evening.gpx
```

**Tracks that overlap in time are a fatal error** and nothing is written. Where two
tracks both cover an instant they can disagree about where you were, and picking
one would make the geotag depend on the order you listed the files. Run overlapping
tracks as separate passes instead — photos outside a track are skipped, so a later
pass tags only what the earlier one left alone.

Adding another format is a small change to a data table *for anything the EXIF
parser already reads* — which also covers Fujifilm RAF, Phase One IIQ and TIFF.
Sony ARW, DNG, ORF, PEF and RW2 are not readable by that parser at all and would
need a different one. See the Format extensibility section of the plan.

### Behavior worth knowing

- **Timezone.** EXIF `OffsetTimeOriginal` wins over `--utc-offset`; a disagreement
  is warned about but the EXIF value is used. Files with a naive timestamp and no
  `--utc-offset` abort the run *before anything is written*, rather than silently
  misplacing every photo by the offset.
- **Outside the track.** Photos taken before the track starts or after it ends are
  skipped and reported. No clamping, no extrapolation, no tolerance window.
- **Several tracks.** Files are merged into one index, but a seam between two files
  is treated exactly like a `<trkseg>` break — never interpolated across, however
  close the two ends happen to be in time and distance. Nothing is known about where
  you went between one recording and the next.
- **Gaps in the track.** A photo is tagged only if the two track points bracketing
  its capture time are within **60 seconds AND 100 meters** of each other, and come
  from the same `<trkseg>` recording run. Anything else is skipped and reported with
  the size of the gap.

  Both limits are needed. Endpoint separation does not bound the error: a subject
  can leave and return between two nearby fixes, so a 140-second hole with only 8 m
  between its endpoints is still untrustworthy, and only the time limit rejects it.
  A short hole with large separation means genuine fast movement, and only the
  distance limit rejects that. This follows the project's guiding rule — **a geotag
  off by more than 5 m is worse than no geotag at all.**
- **Existing sidecars** are skipped with a warning. `--force` overwrites them
  wholesale, discarding any develop settings or keywords another tool stored there.
- **Exit code** is non-zero if any file errored or the run was gated; deliberate
  skips are still a clean exit.

## Performance

**`--jobs` defaults to 2.** That is well below the core count, and deliberate: the
best thread count is set by your storage, and local and network storage want
opposite answers.

The numbers below are CR3. **NEF behaves differently enough to have its own section
at the end** — read that one before tuning `-j` for a Nikon import.

Measured on a real shoot — 3,883 Canon R5 CR3s (188 GB) on local NVMe, 20 logical
cores, creating 2,394 sidecars from scratch:

| `-j` | 1 | **2** | 4 | 20 |
|---|---|---|---|---|
| Full run | 1.9 s | **1.7 s** | 1.9 s | 1.9 s |

The EXIF read is nearly free locally (~0.3 s for all 3,883 files, since for CR3
nom-exif seeks within the container rather than reading whole 30 MB files), so the
run is dominated by *creating* sidecars. **Writing does not parallelize within a
single directory on NTFS**, which serializes creates and renames against that
directory's index — so for a one-folder import, extra threads only add contention.
Measured, this is not the atomic write's fault: plain one-stage writes scale slightly
*worse*. Spread the same work over 16 folders and it scales 1.8×, so **a recursive
import across many date folders does benefit from a higher `-j`.**

Network storage inverts this entirely. Cold reads over SMB:

| `-j` | 1 | 4 | 20 |
|---|---|---|---|
| Read throughput | 25 files/s | 107 files/s | 296 files/s |

Nearly **12×** from parallelism — about 155 s single-threaded versus 13 s for a
3,883-file day. If your CR3s live on a NAS or network share, pass `-j 16` or higher.

Two traps if you benchmark this yourself: a warm page cache measures RAM rather than
storage and understates cold read scaling by more than an order of magnitude, and
*overwriting* an existing sidecar costs about 2.3× less than *creating* one, so
delete the `.xmp` files between runs instead of using `--force`.

**Tracks are parsed in parallel too**, which matters when you pass a lot of them:
seven tracks of one trip (15.4 MB, 75,728 points) take 658 ms at `-j 1` and 215 ms
at `-j 8`. This scales with the number of files, not their total size — one large
track sets the floor.

### NEF is a different shape

CR3 files are read by seeking to a header. **NEF files have to be read whole** —
about 22 MB each — because they do not parse any other way. Two things follow.

Threads buy much less. Cold over SMB, a different uncached folder per measurement:

| `-j` | 1 | 2 | 8 |
|---|---|---|---|
| Read throughput | 129 MB/s | 159 MB/s | 256 MB/s |

That is roughly **2×**, against CR3's 12× on the same link — with 22 MB of payload
per file there is far less latency to hide and mostly just bytes to move. Raising
`-j` is still worth it for a network NEF import, just not dramatic.

Faster storage does not change that conclusion; it sharpens it. Repeated against an
NVMe array on the same network, throughput ran 375 MB/s at `-j 1` to 517 MB/s at
`-j 16` — better in absolute terms, but only **1.4×** from threads, because at that
rate the bottleneck has moved off the disk and onto the network link. `-j 4` was
within 6% of the best result. **The faster your storage, the less `-j` buys you on
NEF**, until the wire is the limit.

And the read stops being the cheap part of the run. A NEF import moves as much data
as the files themselves occupy, so on any storage it is the reads, not the sidecar
writes, that set the wall clock.

## Design constraints

- **Pure Rust.** No ExifTool, no C-library bindings. (ExifTool is used only as a
  verification oracle by hand; it appears nowhere in shipped code.)
- **Fast.** Optimize for wall-clock time. The work is parallel by design, but more
  threads is not automatically faster — see Performance.
- **Readable over clever.** No surprises for an experienced Rust reviewer.
