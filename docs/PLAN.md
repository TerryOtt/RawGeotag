# RawGeotag — geotag raw files from GPX tracks

## Context

Camera raw files carry a capture timestamp but no location; a separate GPS logger (phone, watch, handheld) records a GPX track over the same period. Correlating the two by time recovers where each frame was shot.

This builds a Rust CLI that walks a directory tree for raw files, reads each capture time from EXIF, linearly interpolates position from the GPX track at that instant, and writes an XMP sidecar carrying latitude, longitude, and altitude. (One directory may be matched against **several** GPX files — a day is often split across separate recordings. That arrived after the original design; the rules keeping the merge honest are under track.rs.) Raw files are never modified — all output goes to sidecars, so the operation is fully reversible by deleting the `.xmp` files.

Three constraints shape the design:

1. **Pure Rust only.** No ExifTool, no C-library bindings (`rexiv2`/gexiv2, `libexif`, `xmp_toolkit`, `libopenraw`). This is an integration job — mature crates do the parsing; our code is correlation and serialization.
2. **Minimize wall-clock time.** The workload is embarrassingly parallel — each file's outcome depends only on that file and a read-only track index. See Concurrency, which drives several structural decisions rather than being a bolt-on.
3. **Readable over clever, and no surprises for an experienced Rust reviewer.** Where a choice is between a clever mechanism and an obvious one, take the obvious one. See Format extensibility, where this rules out an entire category of design.

CR3 ships first; the format seam is designed so that any format the EXIF parser already reads is a small, mechanical addition. NEF and the other TIFF-based raws are *not* in that set — see Format extensibility for what they actually cost.

## Prerequisite

**Satisfied — this section is retained only so the requirement is on record.** When the plan was written Rust was absent from this machine; it has since been installed and the project builds and ships from it.

Verified present (2026-07-31): `rustc` / `cargo` **1.97.1**, toolchain `stable-x86_64-pc-windows-msvc`, with the Visual Studio Build Tools "Desktop development with C++" workload that the MSVC target links against. ExifTool **12.76** is also installed and is useful as an independent oracle when verifying output — see Verification — but it is **not** a runtime dependency and appears nowhere in shipped code.

---

## Step 0 — persist this plan — ✅ COMPLETE (2026-07-28)

**This step is done; it is retained as a record. Nothing here needs redoing.** The
next session starts at the implementation, beginning with `cargo init`.

It was deliberately scoped to **scaffolding and version control only** — no Rust source, no crates, no `cargo init`.

**Why it existed.** The plan originally lived only at `C:\Users\TDO-XPS15-2024\.claude\plans\i-d-like-to-write-deep-thimble.md` — outside the project, under a machine-generated slug, in no version control. Its *content* was already sufficient to execute from cold; only its location was fragile. This document is now the versioned source of truth.

Actions taken:

1. **`git init`** in `C:\Users\TDO-XPS15-2024\Claude\RawGeotag`, with the initial branch named `main`.
2. **`docs/PLAN.md`** — this document, copied into the repo. Becomes the versioned source of truth; the `.claude/plans` copy is left alone as a harmless duplicate.
3. **`.gitignore`** — `/target` and `**/*.rs.bk`. **`Cargo.lock` is deliberately *not* ignored**: this crate is a binary, and Rust convention is to commit the lockfile for binaries so builds are reproducible. (The ignore-the-lockfile advice applies to libraries.)
4. **`README.md`** — short and human-facing: what the tool does, current status (*planning complete, implementation not started*), prerequisite (install Rust), and a pointer to `docs/PLAN.md`.
5. **`CLAUDE.md`** — loaded automatically into context at the start of every Claude Code session in this directory. This is what lets a future session resume without re-litigating settled decisions. Keep it brief and state the binding constraints:
   - Pure Rust only — no ExifTool, no C-library bindings, ever.
   - Optimize for wall-clock time; the work is parallel by design.
   - Readable and maintainable over clever; no surprises for an experienced Rust reviewer.
   - Design decisions are settled in `docs/PLAN.md` — read it before proposing changes.
6. **Initial commit** on `main`.

No remote is configured — this is a local repository unless a GitHub remote is requested separately.

**To resume later:** open Claude Code in `C:\Users\TDO-XPS15-2024\Claude\RawGeotag` and ask it to implement `docs/PLAN.md`. `CLAUDE.md` loads the constraints automatically; the plan supplies everything else. Nothing from this conversation is required.

---

## CLI

```
rawgeotag <DIR> <EXT> <GPX>... [OPTIONS]

  DIR    parent directory, searched recursively
  EXT    raw extension, e.g. "cr3" (case-insensitive, leading "." tolerated)
  GPX    path to a GPX track file; repeat for a day split across several tracks

  --utc-offset <±HHMM>     offset for files with no EXIF timezone, e.g. -0700, +0430
  --max-gap <SECONDS>      refuse to interpolate across a longer hole [default: 60]
  --max-distance <METERS>  refuse to interpolate across a wider hole [default: 100]
  --force                  overwrite existing sidecars (default: skip with a warning)
  --dry-run                do all work, write nothing
  -j, --jobs <N>           worker threads (default: 2; raise for network storage)
      --no-progress        suppress the progress bar
  -v, --verbose            per-file detail
```

`--help` is the authoritative list; this block is a design sketch and omits clap's automatic `-h` and `-V`. `--max-gap` and `--max-distance` were **not** in the original design — they arrived with the gap-rule reversal recorded under track.rs below.

Positional order follows the original spec. `--utc-offset` is a flag rather than a fourth positional since it is optional and sign-prefixed.

`EXT` is validated against the known-format table, so an unsupported value fails immediately with a message listing what *is* supported, and `--help` stays self-documenting as formats are added. (If that ever proves too strict — a TIFF-based raw that would have worked but isn't listed — relaxing it is a one-line change, and the better fix is adding the format properly.)

## Dependencies

| Crate | Ver | Role |
|---|---|---|
| `clap` (derive) | 4 | arg parsing |
| `walkdir` | 2 | recursive traversal |
| `nom-exif` | 3.6 | **pure-Rust EXIF, with explicit Canon CR3 support** |
| `gpx` | 0.10 | GPX parsing |
| `rayon` | 1 | data parallelism |
| `indicatif` | 0.18 | thread-safe progress bar |
| `chrono` | 0.4 | EXIF-side time (already nom-exif's public type) |
| `time` | 0.3 | GPX-side time (already gpx's public type) |
| `tempfile` | 3 | atomic sidecar writes — unique temp names, cleanup on drop |
| `anyhow` | 1 | error context |

These versions are indicative of what the design was written against, not a statement of what is current. Confirm against crates.io (`cargo search <crate> --limit 1`) before putting any of them in `Cargo.toml`; see CLAUDE.md for why the `0.x` crates in particular go stale silently.

**This list has been audited against the alternatives, and the rebuttals live in the code, not here.** The obvious "why didn't you use X?" for each spot is answered at the site that invites the question, where it cannot drift away from what it describes: `Cargo.toml` for `nom-exif` over the far more popular `kamadak-exif` (which cannot read CR3 at any version) and for why two time crates is not sloppiness; `track.rs` for hand-rolled haversine over `geo`; `xmp.rs` for a format template over `xmp-writer`; `raw.rs` for the offset parser; `format.rs` for `rawler`. Read those before proposing a swap.

Two time crates appear because they are the public types of two upstream crates. Do **not** write conversions between them — normalize both sides to `i64` Unix seconds at the boundary (`chrono::DateTime::timestamp()`, `time::OffsetDateTime::unix_timestamp()`) and do all correlation arithmetic in that single scalar domain.

## Module layout

```
src/
  main.rs      CLI, two-phase orchestration, reporting
  format.rs    RawFormat enum + per-format table   <-- the extension point
  raw.rs       capture time extraction (nom-exif)
  track.rs     GPX load, sort, interpolate
  xmp.rs       sidecar serialization
```

## Format extensibility

**What actually varies between raw formats is much less than it appears.** nom-exif dispatches on file *content*, not extension, so a format it already parses needs a row in the table and nothing else. A per-format parser layer would therefore start life as N modules with identical bodies, which is speculative abstraction, not extensibility.

**But the table's reach is nom-exif's reach, and that is narrower than this section originally assumed.** Its supported list is JPEG, PNG, HEIC/HEIF, AVIF, TIFF, Phase One IIQ, Fujifilm RAF and Canon CR3. **NEF, ARW, DNG, ORF, PEF and RW2 are not on it.**

An earlier draft here reasoned that NEF would come free because NEF is TIFF-based. **That has now been tested against 150 real Nikon D3300 files, and it is half right in the least convenient way:** NEF does not parse at all through `MediaSource::open`, which is the streaming source `raw.rs` uses, but parses perfectly through `MediaSource::from_memory`, which reads the whole ~22 MB file. So NEF is reachable, but not by adding a row — it needs a per-format choice of *how to open the file*, which is the first thing to genuinely earn a new column in the table below, plus the I/O cost of reading entire files, plus a mandatory `--utc-offset` because that body records no `OffsetTimeOriginal`. CLAUDE.md has the measurements; do not re-run them.

For the formats nom-exif does not read at all, the pure-Rust alternative is `rawler` — `Decoder::raw_metadata()` reads metadata without decoding the image — at a cost of ~106 transitive crates including a complete JPEG-XL decoder. That is the escape hatch when a second camera system actually arrives, not before; the note in `format.rs` says the same where someone adding a format will see it.

So the seam is placed where variation genuinely lives: **extension mapping and per-format tag preferences**, expressed as an enum with a data table.

```rust
/// A capture-time tag paired with the tag carrying its UTC offset.
///
/// The pairing must be explicit: nom-exif surfaces the two separately rather
/// than merging them. See the CR3 correction under raw.rs below — a bare
/// `ExifTag` list here is what caused every Canon raw to be gated as
/// "no timezone" on the first real-data run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTag {
    pub datetime: ExifTag,
    pub offset: ExifTag,
}

/// A raw format we know how to read a capture time from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    Cr3,
}

impl RawFormat {
    /// Every supported format, in help-text order.
    pub const ALL: &'static [RawFormat] = &[Self::Cr3];

    /// Extensions that select this format, lowercase.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Cr3 => &["cr3"],
        }
    }

    /// Capture-time tags to try, in priority order.
    pub fn capture_tags(self) -> &'static [CaptureTag] {
        match self {
            Self::Cr3 => &[
                CaptureTag { datetime: ExifTag::DateTimeOriginal, offset: ExifTag::OffsetTimeOriginal },
                CaptureTag { datetime: ExifTag::CreateDate,       offset: ExifTag::OffsetTimeDigitized },
            ],
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> { /* case-insensitive table scan */ }
}
```

**Adding a format is then:** add the variant → the compiler flags every `match` that no longer compiles → fill in those arms → add a fixture test against a real file of that format. Forgetting a spot is a **build error, not a runtime surprise**, which is precisely the property a trait-object registry or a `HashMap` of handlers would discard. This is the whole cost only when the parser already reads the format — Fujifilm RAF is such a case; NEF is not.

When two formats end up with identical arms, that is not duplication to factor away — it is the code stating plainly that these formats do not differ. Collapse them with `Self::Cr3 | Self::Raf => ...` and split the arm when one actually diverges.

**Start flat, promote later.** `format.rs` is one file. If some format eventually needs genuinely bespoke extraction, that match arm calls into its own module and `format.rs` becomes `format/mod.rs` + `format/rw2.rs`. In Rust that promotion is a rename, not a refactor — so there is no cost to deferring it, and the per-format subdirectory appears only once a format has earned one.

**Explicitly rejected: runtime plugin discovery.** Scanning a directory for format plugins requires `libloading` and an `extern "C"` boundary, because Rust has no stable ABI — a plugin compiled against a different rustc or crate version is undefined behavior rather than a link error. It also forfeits single-binary distribution, complicates cross-compilation, and removes all compile-time exhaustiveness checking. Rust deliberately has no module auto-discovery; `mod` declarations are explicit by design, and matching that expectation matters more here than any flexibility gained.

## Concurrency

**Shape of the work.** Per file: open and parse EXIF (I/O plus modest CPU), binary-search and interpolate (negligible), serialize and write a ~1 KB sidecar (I/O). Expect this to be **I/O-bound, not CPU-bound** — nom-exif seeks within the BMFF container rather than reading whole 30 MB files, so each input costs a few hundred KB of reads. The realistic ceiling is storage, not cores — and *saturating cores turned out not to be the goal at all*. See the measured note below: the default is **2**, not rayon's logical-core count, because on local storage extra threads contend on NTFS directory metadata and make the run slower.

> **Measured since, and it settles the open question above — in both directions.** There is no single good `--jobs` value; the optimum is set by storage latency. On **local NVMe** the read is nearly free and the run is write-bound, so throughput peaks at **`-j 2`** and degrades above it, because NTFS serializes directory metadata. On **SMB** the read dominates and parallelizes **~12×**, so `-j 16`–`20` is right. The shipped default is therefore **2** (`DEFAULT_JOBS` in `main.rs`), tuned for the common local case, with the flag available for network storage. The prediction that nom-exif seeks within the BMFF rather than reading whole files **did** hold: 3883 CR3s resolve in ~0.3 s locally, impossible if 30 MB were read per file. CLAUDE.md's *Measured behavior* section carries the numbers; do not restate them here, so there is one place to correct.
>
> **A warm cache will mislead you.** An early sweep that pre-warmed the cache showed reading parallelizing only ~3× and plateauing near 4 threads — that was measuring RAM, not storage, and it understated cold behavior by more than an order of magnitude. Evict, or use untouched data, before quoting read-scaling numbers.

**Structure.** Two parallel phases with a gate between them:

- **Phase A — collect and extract (parallel).** Walk the tree with `walkdir`, collecting matching paths into a `Vec`. Then `par_iter()` to extract each capture timestamp.
- **Gate (sequential, cheap).** If any file resolved to a naive timestamp with no `--utc-offset` available, print the list and exit non-zero **having written nothing**.
- **Phase B — interpolate and write (parallel).** `into_par_iter()` over the successful extractions. **One worker task does the entire remainder for one photo** — `track.lookup` interpolates, `xmp::render` serializes, `xmp::write_atomic` writes. Geotagging is *not* a separate phase from writing; the only step split out is the EXIF read. The progress bars say `reading capture times` and `writing sidecars`, which invites the opposite reading — see CLAUDE.md's *Execution shape* section.

The gate is why this is two phases rather than one. Forgetting `--utc-offset` on a body that does not record `OffsetTimeOriginal` would otherwise silently misplace every photo by the offset amount, and discovering that after half the sidecars are on disk is worse than discovering it before any are. In practice the gate rarely fires spuriously: `--utc-offset` applies *only* to naive files, so a mixed-camera shoot where one body records its zone and the other does not is handled correctly with a single flag. The cost of splitting is one `Vec<Extraction>` — a path plus a timestamp or a diagnostic per file. Phase A also yields an exact denominator for the progress bar.

**The barrier cannot be optimized away.** The gate needs every capture time, and *obtaining* a capture time is itself the expensive operation — the CR3 parse. There is no cheap pre-scan that validates timezones without doing the costly work, so the expensive pass and the gate are inherently the same pass. The barrier costs ~10% of wall clock (reads alone vs. reads plus writes on the 1024-file set), and a fused design would recover only part of that, since writes do not parallelize on NTFS anyway.

**Considered and rejected:** when `--utc-offset` *is* supplied, no file can reach `Extraction::NeedsOffset`, so the gate is provably vacuous and the phases could legally fuse into a single pass. That buys a few percent on one code path in exchange for two structurally different execution models to reason about and test. Constraint 3 (readable over clever) wins; do not re-propose it.

**Collect-then-parallelize, not `par_bridge`.** Materializing the walk into a `Vec` before parallelizing gives rayon contiguous slices to split, which load-balances far better than bridging a sequential iterator. Traversal is a small serial prefix; if it ever dominates (very deep trees, network storage), `jwalk` is a near drop-in replacement that parallelizes the walk itself.

**Per-worker parser state.** `MediaParser` pools internal buffers and is worth reusing — but sharing one across threads behind a `Mutex` would serialize the entire run and defeat the point. Use rayon's `map_init`, which constructs one parser per worker:

```rust
paths.par_iter()
    .map_init(MediaParser::new, |parser, path| extract(parser, path))
    .collect::<Vec<_>>()
```

**No shared mutable state.** The track index is built once, before *Phase A* (`Track::load` is the first thing `main` does after the clock starts), and shared immutably as `&Track` — read-only, no lock, no contention. Workers return an outcome enum rather than incrementing counters or printing; tallying happens sequentially afterward.

**Deterministic output.** Warnings are **never printed from worker threads** — interleaved stderr from N threads is unreadable and makes runs non-reproducible. Each worker returns its diagnostics in its outcome value; `main` sorts by path and prints after the phase completes. Progress goes through `indicatif`'s `ProgressBar`, which is internally synchronized; use `ProgressBar::suspend` if anything must print mid-phase.

**Writes.** Sidecar paths are unique per input, so parallel writes never target the same file and the temp-file name derived from the target is inherently collision-free.

Report elapsed wall time and throughput (files/sec) in the summary, so `--jobs` tuning is measurable rather than guesswork.

## raw.rs — capture time

```rust
let ms = MediaSource::open(path)?;
let exif = parser.parse_exif(ms)?;              // parser supplied by map_init
let dt = format.capture_tags().iter()           // per-format priority order
    .find_map(|tag| exif.get(tag.datetime));    // -> ExifDateTime
// ...then read tag.offset separately; see the CR3 correction below.
```

`ExifDateTime` is an enum with two cases:
- `Aware(DateTime<FixedOffset>)` — the date string itself carried a zone
- `Naive(NaiveDateTime)` — no zone information

> **Correction, found while verifying against a real CR3 (2026-07-30).** The
> assumption below — that `.aware()` is `Some` whenever the camera recorded
> `OffsetTimeOriginal` — **is false for CR3**. nom-exif returns
> `DateTimeOriginal` as `Naive` and exposes `OffsetTimeOriginal` as a *separate*
> `Text("+00:00")` entry; it never merges the two. (It *does* merge them for
> JPEG, which is why JPEG stand-in fixtures did not catch this — every one of
> 1024 real CR3s tripped the gate.) The implementation therefore pairs each
> capture tag with its offset tag in `format.rs` and reads both, preferring
> `.aware()` when present and falling back to the paired offset tag. The
> precedence rule stated here is unchanged; only the mechanism differs.

Resolution rule (**EXIF wins, warn on conflict**):
1. If `.aware()` is `Some` and `--utc-offset` was given and the two offsets differ → return a conflict warning naming the file and both offsets; proceed with the EXIF value.
2. Resolve with `.or_offset(cli_offset)` — attaches the fallback only to `Naive` values, returns `Aware` values untouched. This is the whole precedence rule in one call.
3. `Naive` with no `--utc-offset` → the gate condition above.

Convert the resolved `DateTime<FixedOffset>` with `.timestamp()` and hand off an `i64`. A file with no usable capture tag is skipped with a warning.

## track.rs — GPX and interpolation

> **Extended 2026-07-31: several GPX files may be given** (`<GPX>...`), for a day split across a driving log and an evening walk. They are flattened into one index, with two rules that keep the merge honest. **Segment numbering continues across files**, so the seam between two files is a segment break and is never interpolated across — restarting the counter per file would make the last point of one and the first of the next look contiguous. **Overlapping time ranges are a hard error**, raised while the index is built and so before any sidecar is written: two tracks covering one instant can disagree, only one point per timestamp survives, and the survivor would be chosen by argument order. The bound is inclusive — one shared second is an overlap. The remedy is separate passes, which work because photos outside a track are skipped.

Load once at startup, before *Phase A*. Flatten every `track.segments[].points[]` plus standalone `gpx.waypoints` into one `Vec<TrackPoint { ts: i64, lat: f64, lon: f64, ele: Option<f64>, segment: u32 }>`, dropping points with no timestamp, then sort by `ts` and dedupe. (`segment` postdates the original design — it identifies the contiguous recording run a point came from, and is what lets the reversed gap rule below refuse to bridge a `<trkseg>` break.) After construction it is immutable and freely shared across threads.

Look up by `slice::binary_search_by_key(&ts, |p| p.ts)`:
- exact hit → use that point
- between `i` and `i+1` → linear interpolation, fraction `f = (ts - a.ts) / (b.ts - a.ts)`
- **before the first or after the last point → skip the file and record it** (per the range decision: no tolerance window, no clamping, no extrapolation)

Interpolation details:
- **Longitude must take the shortest arc.** If `(b.lon - a.lon).abs() > 180.0`, adjust by ±360 before interpolating and normalize the result back into −180..180. Cheap to write, and without it any track crossing the antimeridian produces a point on the wrong side of the planet.
- **Altitude is interpolated only if both bracketing points have `ele`.** If either is `None`, omit `GPSAltitude` from the sidecar entirely rather than inventing a value.
- ~~No `--max-gap` limit, per the range decision — interpolation across a long recording gap is permitted.~~

> **Reversed 2026-07-30, by the project mantra "a geotag off by more than 5 m is
> worse than no geotag."** Interpolation now requires the bracketing points to be
> within **60 seconds AND 100 meters** of each other (`--max-gap`,
> `--max-distance`), and to come from the **same `<trkseg>`**. Exceeding any one of
> the three skips the photo and reports the gap.
>
> Both limits are needed, and neither implies the other. Endpoint separation does
> not bound the interpolation error — a subject can leave and return between two
> nearby fixes, so a 140 s hole with only 8 m of endpoint separation is still
> untrustworthy, and only the time limit rejects it. Conversely a short hole with
> large separation means genuine fast movement, and only the distance limit rejects
> that. Segment breaks are structural: the logger stopped, so nothing at all is
> known about the path between.
>
> Measured on the 2025-09-17 Malta shoot (1024 CR3s): 1002 tagged, 22 skipped —
> 10 across a segment break (460 s / 594 m), 9 in a 140 s / 8 m hole, 3 in a
> 775 s / 27 m hole.

## xmp.rs — sidecar

Path: replace the extension. `IMG_1234.CR3` → `IMG_1234.xmp` (Adobe/Lightroom convention).

Emit a fixed-structure XMP packet via a formatting template, not an XML library. Every value written is machine-generated numeric or a single cardinal letter, so there is no escaping surface for `quick-xml` to protect — a template is the simpler correct choice, and allocation-light in a hot parallel loop. (If sidecar *merging* is ever added, that changes: read-modify-write of third-party XMP requires a real parser.)

Coordinate encoding follows the XMP spec's `DDD,MM.mmk` form — degrees, comma, decimal minutes, hemisphere letter — **not** decimal degrees. Getting this wrong is the single most likely compatibility bug:

```
exif:GPSLatitude="47,26.7305N"
exif:GPSLongitude="122,20.1170W"
exif:GPSAltitude="123456/1000"      rational; ref 0 = above sea level, 1 = below (value absolute)
exif:GPSAltitudeRef="0"
exif:GPSVersionID="2.2.0.0"
exif:GPSMapDatum="WGS-84"
exif:GPSTimeStamp="2026-07-28T18:42:03Z"   capture instant in UTC
```

Wrapped in the standard `<?xpacket ...?>` / `<x:xmpmeta>` / `<rdf:RDF>` / `<rdf:Description>` scaffolding.

Write atomically — temp file in the destination directory, then rename — so an interrupted run cannot leave a half-written sidecar.

## Existing sidecars

Default: **skip and warn**, naming each file. `--force` overwrites wholesale. No merging — a `--force` run discards any develop settings or keywords another tool stored in that sidecar, which the `--help` text should say plainly.

## Summary report

Always print a tallied summary; collected warnings are the detail lines behind it.

```
Scanned   419 .cr3 files
Tagged    405
Skipped    14   9 outside track, 3 existing sidecar, 2 no capture time
Elapsed   3.2s  (131 files/sec, 16 threads)
```

Exit non-zero if any file errored or the gate fired; zero if everything was either tagged or deliberately skipped.

## Verification

1. `cargo build --release`, `cargo clippy -- -D warnings`.
2. **Unit tests** (`track.rs`, no fixtures needed): exact-timestamp hit; midpoint interpolation against hand-computed values; before-first and after-last both skip; antimeridian crossing stays near ±180; missing `ele` on one bracketing point suppresses altitude.
3. **Unit test** (`xmp.rs`): a known lat/lon renders to the exact expected `DDD,MM.mmk` strings, including a southern/western hemisphere case and a negative altitude.
4. **Unit test** (`format.rs`): iterate `RawFormat::ALL` and assert every declared extension round-trips through `from_extension`, in mixed case. This test fails if a new variant is added without a table entry, catching the one gap the compiler cannot.
5. **End-to-end** on a real CR3 plus its GPX track — *you will need to supply these*. Confirm the sidecar lands next to the raw with the right name. Also pass **two** non-overlapping tracks for one directory and confirm the merged span covers both, then pass two overlapping ones and confirm the run aborts with no sidecars written.
6. **Cross-check against ExifTool**, which is installed and is an independent implementation:
   - `exiftool -DateTimeOriginal -OffsetTimeOriginal <file>.cr3` should match what the tool extracted.
   - `exiftool <file>.xmp` should read back the GPS coordinates; compare against the track for that timestamp.
   - This is a test-time sanity check only — no ExifTool call exists anywhere in the shipped program.
7. **Determinism under parallelism:** ✅ verified 2026-07-31 on 3883 real CR3s at `--jobs 1`, `2` and `16`. All three produced byte-identical console output (excluding the timing line), an identical 1489-line warning list, and an identical SHA-256 manifest over all 2394 sidecars. This is the main regression risk the concurrency design introduces, so **re-run it after any change to the phase structure, the outcome enums, or the reporting order** — it is the check that would catch a worker printing directly, or a tally that depends on completion order.

   **Re-run the same day** after the gate's selection logic was extracted into `files_needing_offset()`. All three job counts agreed with each other *and* reproduced the pre-extraction output byte for byte, which is the stronger result — the refactor did not merely stay self-consistent. Two lessons worth keeping: extracting a function that owns a `sort` is exactly the change this check exists to police, and a careless `sed` during that work silently removed the unrelated sort in `collect_paths` as well, which nothing but this check would have caught before release. **When re-running, also diff against the previous run's artifacts, not just across job counts** — a bug that reorders output consistently at every `-j` passes the cross-job comparison and fails the historical one.
8. **Scaling:** ✅ done — see the measured note under Concurrency. **Do not expect more threads to be faster.** On local storage throughput *peaks at `-j 2` and degrades above it*, which is why that is the default; a slowdown at `-j 8` is the expected result, not a bug. Over network storage the opposite holds and parallelism is worth ~12×. If you re-measure, evict the page cache first and delete existing sidecars, or you will be timing RAM and cheap overwrites rather than the real workload.
9. **Behavior checks:** re-run and confirm existing sidecars are skipped with warnings; re-run with `--force` and confirm overwrite; `--dry-run` writes nothing; a deliberately wrong `--utc-offset` against a CR3 that has `OffsetTimeOriginal` fires the conflict warning *and still uses the EXIF value*; omitting `--utc-offset` on naive-timestamp files trips the gate with no sidecars written.
