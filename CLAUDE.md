# RawGeotag — working notes for Claude

A Rust CLI that geotags camera raw files from a GPX track, writing XMP sidecars.

**Read [`docs/PLAN.md`](docs/PLAN.md) before proposing or writing anything.** The
design is settled there — CLI shape, crates, module layout, concurrency model, and
verification plan. Do not re-litigate decisions it already records; if you think one
is wrong, say so explicitly rather than quietly diverging.

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
- Existing sidecars are skipped with a warning; `--force` overwrites. No merging.

## Status

Planning complete, implementation not started. The next session's job is to
implement `docs/PLAN.md`, starting with `cargo init`.
