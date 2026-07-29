# RawGeotag

A Rust CLI that geotags camera raw files from a GPX track.

Raw files carry a capture timestamp but no location; a GPS logger records a track
over the same period. RawGeotag correlates the two by time, linearly interpolating
position between track points, and writes the result as an XMP sidecar next to each
raw file.

**Raw files are never modified.** All output goes to sidecars, so the whole
operation is reversible by deleting the generated `.xmp` files.

## Status

**Planning complete. Implementation not started.**

The design is settled and written up in [`docs/PLAN.md`](docs/PLAN.md) — CLI shape,
crate selection, module layout, concurrency model, and a verification plan.

## Prerequisite

Rust is not yet installed on the development machine. Install it via
<https://rustup.rs> before building.

## Planned usage

```
rawgeotag <DIR> <EXT> <GPX> [OPTIONS]

  DIR    parent directory, searched recursively
  EXT    raw extension, e.g. "cr3"
  GPX    path to the GPX track file

  --utc-offset <±HHMM>  offset for files with no EXIF timezone, e.g. -0700
  --force               overwrite existing sidecars (default: skip with a warning)
  --dry-run             do all work, write nothing
  -j, --jobs <N>        worker threads (default: logical core count)
```

Canon CR3 ships first. Other formats (Nikon NEF and friends) are a small,
mechanical addition — see the Format extensibility section of the plan.

## Design constraints

- **Pure Rust.** No ExifTool, no C-library bindings.
- **Fast.** The work is parallel by design; optimize for wall-clock time.
- **Readable over clever.** No surprises for an experienced Rust reviewer.
