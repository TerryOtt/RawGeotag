# RawGeotag — working notes for Claude

A Rust CLI that geotags camera raw files from a GPX track, writing XMP sidecars.

**Read [`docs/PLAN.md`](docs/PLAN.md) before proposing or writing anything.** The
design is settled there — CLI shape, crates, module layout, concurrency model, and
verification plan. Do not re-litigate decisions it already records; if you think one
is wrong, say so explicitly rather than quietly diverging.

## Project mantra

**"Geotags off by more than 5 m from actual are worse than no geotags."** A missing
tag is visibly missing; a wrong one looks authoritative and silently corrupts the
photo's provenance. Whenever coverage trades against positional accuracy, take
accuracy — never clamp, extrapolate, or bridge a hole to raise the tagged count.

## Binding constraints

1. **Pure Rust only.** No ExifTool, no C-library bindings (`rexiv2`/gexiv2,
   `libexif`, `xmp_toolkit`, `libopenraw`). This is non-negotiable and applies to
   every dependency added, forever. ExifTool is installed on this machine and is
   useful as an independent oracle *when verifying output by hand* — it must never
   appear in shipped code.

2. **Optimize for wall-clock time.** The workload is embarrassingly parallel and is
   expected to be I/O-bound, not CPU-bound. Keep all cores busy. Do not introduce
   shared mutable state on the hot path, and never share a `MediaParser` across
   threads behind a mutex — use rayon's `map_init` for per-worker parsers.

3. **Readable and maintainable over clever.** Strive not to violate the principle
   of least surprise for an experienced Rust developer reviewing this codebase.
   Prefer the obvious mechanism to the clever one. Notably: format extensibility is
   an enum plus a data table, *not* a plugin registry or runtime module discovery —
   the plan explains why.

4. **Raw files are never modified.** Output is sidecars only.

## Settled decisions worth not rediscovering

- Sidecar naming: `IMG_1234.CR3` → `IMG_1234.xmp` (Adobe convention, extension replaced).
- Timezone: EXIF `OffsetTimeOriginal` wins over `--utc-offset`; warn on conflict.
- Photos outside the GPX track are skipped and reported — no clamping, no
  extrapolation, no tolerance window.
- **Gap rule (reverses the plan's original "no `--max-gap`" decision).**
  Interpolate only when the bracketing points are within **60 s AND 100 m**
  (`--max-gap`, `--max-distance`) *and* share a `<trkseg>`. Both limits are load-
  bearing: endpoint separation does not bound the error, since a subject can leave
  and return between two nearby fixes, so a 140 s / 8 m hole is still untrustworthy.
  Do not "simplify" this to a single condition.
- Existing sidecars are skipped with a warning; `--force` overwrites. No merging.

## Status

Implemented. Builds clean, 37 unit tests pass, `cargo clippy -- -D warnings` is
clean. Toolchain on this machine: Rust 1.97.1 MSVC, with the VS Build Tools C++
workload installed.

**Verified against real CR3s** (Canon EOS R5, `Q:\Lightroom\Images\2025\2025-09-17`,
1024 files, with `Q:\Photo GPX Tracks\2025\...\2025-09-17- Malta Car Tour.gpx`): 1002
resolve and tag, 3.3s over SMB. The other 22 are correctly skipped — they fall in a
775 s / 27 m hole in the track, which the gap rule rejects on time even though the
endpoints are close, exactly the case that rule exists for. Interpolation cross-checked
by hand against the raw GPX points and agrees to ~2-4 cm, which is the resolution of
the 4-decimal-minute coordinate encoding. ExifTool reads the sidecars back correctly
and `-validate` is OK.

Note ExifTool calls the XMP `exif:GPSTimeStamp` property **`GPSDateTime`**; asking it
for `GPSTimeStamp` on a sidecar returns nothing, which is a naming difference, not a
bug.

## The CR3 timezone trap — do not regress this

nom-exif returns CR3 `DateTimeOriginal` as **`Naive`** and exposes
`OffsetTimeOriginal` as a **separate `Text("+00:00")` entry**. It never merges them.
It *does* merge them for JPEG. So `ExifDateTime::aware()` is always `None` on CR3,
and any code that trusts `.aware()` alone will gate every single Canon raw as
"no timezone" — which is exactly what happened on the first real-data run.

`format.rs` therefore pairs each capture tag with its offset tag (`DateTimeOriginal`
with `OffsetTimeOriginal`, `CreateDate` with `OffsetTimeDigitized`) and `raw.rs`
prefers `.aware()` when present, falling back to the paired tag. **Test any new
format against a real file of that format**; JPEG stand-ins do not exercise this path.

Beware also that ExifTool reports CR3 `CreateDate` in *local machine time* from the
BMFF container, which differs from the EXIF `DateTimeOriginal`. Compare against
`DateTimeOriginal`, not `CreateDate`, when sanity-checking by hand.

## Measured behaviour worth not rediscovering

Reading parallelises ~3x and plateaus near 4 threads. Sidecar *writing* does not
parallelise at all on NTFS — temp-create plus rename are two directory metadata
operations per file and NTFS serialises those within a directory, so more threads
add contention. This does not make the rayon design wrong: real CR3s are ~30 MB and
cost far more to parse than the 184 KB fixtures used here, so the read phase should
dominate on real input. Do not "fix" this by dropping the atomic write.

One deviation from the plan, deliberately: a file with **no EXIF at all** returns
`nom_exif::Error::ExifNotFound`, and `raw.rs` maps that to `Capture::NoCaptureTime`
rather than letting it become a hard error. Otherwise two indistinguishable
situations — no EXIF, versus EXIF without a date tag — would produce different exit
codes.
