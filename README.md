# RawGeotag

A Rust CLI that geotags camera raw files from a GPX track.

Raw files carry a capture timestamp but no location; a GPS logger records a track
over the same period. RawGeotag correlates the two by time, linearly interpolating
position between track points, and writes the result as an XMP sidecar next to each
raw file.

**Raw files are never modified.** All output goes to sidecars, so the whole
operation is reversible by deleting the generated `.xmp` files.

## Status

**Implemented and building.** The unit test suite passes and `cargo clippy -- -D
warnings` is clean; run `cargo test` for the current tally.

Verified against two real Canon EOS R5 shoots and their GPX tracks:

- **1024 CR3s** — 1002 tagged, 22 correctly refused as falling in track gaps.
  Interpolated positions agree with hand-computed values from the raw track points
  to within the coordinate encoding's resolution (~0.2 m).
- **3883 CR3s, 188 GB** — 2394 tagged, 1489 refused (772 across `<trkseg>` breaks,
  the rest in 5-to-60-minute dropouts). This body had its clock on `+01:00`, which
  exercises the EXIF timezone path that a `+00:00` camera leaves as a no-op; spot
  checks match the raw GPX exactly.

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
rawgeotag <DIR> <EXT> <GPX> [OPTIONS]

  DIR    parent directory, searched recursively
  EXT    raw extension, e.g. "cr3" (case-insensitive, leading "." tolerated)
  GPX    path to the GPX track file

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

Canon CR3 ships first. Other formats (Nikon NEF and friends) are a small,
mechanical addition — see the Format extensibility section of the plan.

### Behavior worth knowing

- **Timezone.** EXIF `OffsetTimeOriginal` wins over `--utc-offset`; a disagreement
  is warned about but the EXIF value is used. Files with a naive timestamp and no
  `--utc-offset` abort the run *before anything is written*, rather than silently
  misplacing every photo by the offset.
- **Outside the track.** Photos taken before the track starts or after it ends are
  skipped and reported. No clamping, no extrapolation, no tolerance window.
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
best thread count is set by storage latency, and local and network storage want
opposite answers.

Measured on a real shoot — 3883 Canon R5 CR3s (188 GB) on local NVMe, 20 logical
cores, creating 2394 sidecars from scratch:

| `-j` | 1 | **2** | 4 | 20 |
|---|---|---|---|---|
| Full run | 1.9 s | **1.7 s** | 1.9 s | 1.9 s |

The EXIF read is nearly free locally (~0.3 s for all 3883 files, since nom-exif
seeks within the BMFF rather than reading whole 30 MB files), so the run is
dominated by *creating* sidecars. **Writing does not parallelize on NTFS**: the
temp-file create and the rename are two directory-metadata operations each, and
NTFS serializes those within a directory, so extra threads only add contention.

Network storage inverts this entirely. Cold reads over SMB:

| `-j` | 1 | 4 | 20 |
|---|---|---|---|
| Read throughput | 25 files/s | 107 files/s | 296 files/s |

Nearly **12×** from parallelism — about 155 s single-threaded versus 13 s for a
3883-file day. If your raws live on a NAS or network share, pass `-j 16` or higher.

Two traps if you benchmark this yourself: a warm page cache measures RAM rather than
storage and understates cold read scaling by more than an order of magnitude, and
*overwriting* an existing sidecar costs about 2.3× less than *creating* one, so
delete the `.xmp` files between runs instead of using `--force`.

## Design constraints

- **Pure Rust.** No ExifTool, no C-library bindings. (ExifTool is used only as a
  verification oracle by hand; it appears nowhere in shipped code.)
- **Fast.** Optimize for wall-clock time. The work is parallel by design, but more
  threads is not automatically faster — see Performance.
- **Readable over clever.** No surprises for an experienced Rust reviewer.
