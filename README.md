# RawGeotag

A Rust CLI that geotags camera raw files from a GPX track.

Raw files carry a capture timestamp but no location; a GPS logger records a track
over the same period. RawGeotag correlates the two by time, linearly interpolating
position between track points, and writes the result as an XMP sidecar next to each
raw file.

**Raw files are never modified.** All output goes to sidecars, so the whole
operation is reversible by deleting the generated `.xmp` files.

## Status

**Implemented and building.** 26 unit tests pass; `cargo clippy -- -D warnings` is
clean.

Verified against 1024 real Canon EOS R5 CR3 files and their GPX track: 1002 tagged
and 22 correctly refused as falling in track gaps. Interpolated positions agree with
hand-computed values from the raw track points to within the coordinate encoding's
resolution (~0.2 m). ExifTool is used as an independent oracle to read the sidecars
back. See [`docs/PLAN.md`](docs/PLAN.md) for the full verification plan.

## Build

Requires Rust (via <https://rustup.rs>) and, on Windows, the MSVC toolchain from
the Visual Studio Build Tools "Desktop development with C++" workload.

```
cargo build --release
cargo test
```

The binary lands at `target/release/rawgeotag`.

## Usage

```
rawgeotag <DIR> <EXT> <GPX> [OPTIONS]

  DIR    parent directory, searched recursively
  EXT    raw extension, e.g. "cr3" (case-insensitive, leading "." tolerated)
  GPX    path to the GPX track file

  --utc-offset <±HHMM>  offset for files with no EXIF timezone, e.g. -0700
  --max-gap <SECONDS>   refuse to interpolate across a longer hole [default: 60]
  --max-distance <M>    refuse to interpolate across a wider hole [default: 100]
  --force               overwrite existing sidecars (default: skip with a warning)
  --dry-run             do all work, write nothing
  -j, --jobs <N>        worker threads (default: logical core count)
      --no-progress     suppress the progress bar
  -v, --verbose         per-file detail
```

Example:

```
rawgeotag ./shoot cr3 ./track.gpx --utc-offset -0700
```

Canon CR3 ships first. Other formats (Nikon NEF and friends) are a small,
mechanical addition — see the Format extensibility section of the plan.

### Behaviour worth knowing

- **Timezone.** EXIF `OffsetTimeOriginal` wins over `--utc-offset`; a disagreement
  is warned about but the EXIF value is used. Files with a naive timestamp and no
  `--utc-offset` abort the run *before anything is written*, rather than silently
  misplacing every photo by the offset.
- **Outside the track.** Photos taken before the track starts or after it ends are
  skipped and reported. No clamping, no extrapolation, no tolerance window.
- **Gaps in the track.** A photo is tagged only if the two track points bracketing
  its capture time are within **60 seconds AND 100 metres** of each other, and come
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

The two phases behave differently, measured on 1000 files with a warm cache
(20 logical cores, NVMe):

| Phase | 1 thread | 4 threads | 20 threads |
|---|---|---|---|
| Read EXIF + interpolate (`--dry-run`) | 16k files/s | 48k files/s | 47k files/s |
| Including sidecar writes | 2.9k files/s | 2.2k files/s | 2.4k files/s |

Reading parallelises about 3× and plateaus around 4 threads. **Writing does not
parallelise on NTFS**: creating and renaming the temp file are two directory
metadata operations per sidecar, and NTFS serialises those within a directory, so
extra threads only add contention. The write phase dominates this workload.

That balance should shift with real input — these fixtures are 184 KB stand-ins,
whereas a real CR3 is ~30 MB and costs far more to parse, while the per-sidecar
write cost stays fixed. `--jobs` exists so this is tunable rather than guesswork.

## Design constraints

- **Pure Rust.** No ExifTool, no C-library bindings. (ExifTool is used only as a
  verification oracle by hand; it appears nowhere in shipped code.)
- **Fast.** The work is parallel by design; optimize for wall-clock time.
- **Readable over clever.** No surprises for an experienced Rust reviewer.
