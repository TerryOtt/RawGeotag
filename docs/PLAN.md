# RawGeotag — geotag raw files from GPX tracks

## Context

Camera raw files carry a capture timestamp but no location; a separate GPS logger (phone, watch, handheld) records a GPX track over the same period. Correlating the two by time recovers where each frame was shot.

This builds a Rust CLI that walks a directory tree for raw files, reads each capture time from EXIF, linearly interpolates position from the GPX track at that instant, and writes an XMP sidecar carrying latitude, longitude, and altitude. (One directory may be matched against **several** GPX files — a day is often split across separate recordings. That arrived after the original design; the rules keeping the merge honest are under track.rs.) Raw files are never modified — all output goes to sidecars, so the operation is fully reversible by deleting the `.xmp` files.

Three constraints shape the design:

1. **Pure Rust only.** No ExifTool, no C-library bindings (`rexiv2`/gexiv2, `libexif`, `xmp_toolkit`, `libopenraw`). This is an integration job — mature crates do the parsing; our code is correlation and serialization.
2. **Minimize wall-clock time.** The workload is embarrassingly parallel — each file's outcome depends only on that file and a read-only track index. See Concurrency, which drives several structural decisions rather than being a bolt-on.
3. **Readable over clever, and no surprises for an experienced Rust reviewer.** Where a choice is between a clever mechanism and an obvious one, take the obvious one. See Format extensibility, where this rules out an entire category of design.

CR3 shipped first and **Nikon NEF has since been added**, which is what put the format seam under real load; see Format extensibility for what a second format actually cost. The remaining TIFF-based raws (ARW, DNG, ORF, PEF, RW2) are still out of reach, for the reason recorded there.

## CLI

```
rawgeotag <DIR> <GPX>... [OPTIONS]

  DIR    parent directory, searched recursively; every supported raw under it is tagged
  GPX    a .gpx file, or a directory of them (not recursive); repeat as needed

  --utc-offset <±HHMM>     offset for files with no EXIF timezone, e.g. -0700, +0430
  --max-gap <SECONDS>      refuse to interpolate across a longer hole [default: 60]
  --max-distance <METERS>  refuse to interpolate across a wider hole [default: 100]
  --force                  overwrite existing sidecars (default: skip with a warning)
  --dry-run                do all work, write nothing (add --force to preview one)
  -j, --jobs <N>           worker threads (default: 16; lower it only for warm re-runs)
      --no-progress        suppress the progress bar
  -v, --verbose            per-file detail
```

`--help` is the authoritative list; this block is a design sketch and omits clap's automatic `-h` and `-V`. `--max-gap` and `--max-distance` were **not** in the original design — they arrived with the gap-rule reversal recorded under track.rs below.

Positional order follows the original spec. `--utc-offset` is a flag rather than a positional since it is optional and sign-prefixed.

> **Reversed 2026-08-02: `EXT` is gone.** It used to be a required second positional, validated against the known-format table so an unsupported value failed immediately with a message listing what *is* supported. A run now tags **every** supported format found under `DIR`, choosing `read_strategy` and `capture_tags` per file.
>
> Why the reversal is safe rather than merely convenient: the extension was never the thing scoping a run — the *track* is, since photos outside it are skipped, and the *directory* is, which is where the real footgun always lived. It also made `RawFormat::ALL` load-bearing instead of a lookup the CLI immediately collapsed to one entry, and it handles a two-body shoot in one pass where it previously took two. A single `--utc-offset` remains correct in a mixed folder because it applies *only* to files with no timezone of their own.
>
> **What the reversal had to replace is the discoverability `EXT` was quietly providing.** Typing `arw` was the only way the tool ever told you what it supports; without it, a Sony shooter would get a silent no-op that reads as a mistyped path. So the walk now counts what it passed over and the summary names it — `Ignored 418   .arw 418  (supported: cr3, nef)`. That is strictly better, because it volunteers the fact about files actually present rather than waiting for a guess.
>
> The old note here warned against *relaxing* the check to accept an unlisted TIFF-based raw. That still stands and is untouched: the walk matches only extensions a `RawFormat` declares, and NEF proved an unlisted format is likelier to fail at the parse than to quietly work — it needed a `read_strategy` entry to work at all.

## Dependencies

| Crate | Ver | Role |
|---|---|---|
| `clap` (derive) | 4 | arg parsing |
| `walkdir` | 2 | recursive traversal |
| `nom-exif` | 3.6 | **pure-Rust EXIF; the only crate that reads Canon CR3, and reads NEF from a whole-file buffer** |
| `gpx` | 0.10 | GPX parsing |
| `rayon` | 1 | data parallelism |
| `indicatif` | 0.18 | thread-safe progress bar |
| `chrono` | 0.4 | **the program's internal time type** — `DateTime<Utc>` and `TimeDelta`; also nom-exif's public type |
| `time` | 0.3 | gpx's public type only; converted to chrono in `track_point` and used nowhere else |
| `tempfile` | 3 | atomic sidecar writes — unique temp names, cleanup on drop |
| `anyhow` | 1 | error context |

These versions are indicative of what the design was written against, not a statement of what is current. Confirm against crates.io (`cargo search <crate> --limit 1`) before putting any of them in `Cargo.toml`; see CLAUDE.md for why the `0.x` crates in particular go stale silently.

**This list has been audited against the alternatives, and the rebuttals live in the code, not here.** The obvious "why didn't you use X?" for each spot is answered at the site that invites the question, where it cannot drift away from what it describes: `Cargo.toml` for `nom-exif` over the far more popular `kamadak-exif` (which cannot read CR3 at any version) and for why two time crates is not sloppiness; `track.rs` for hand-rolled haversine over `geo`; `xmp.rs` for a format template over `xmp-writer`; `raw.rs` for the offset parser; `format.rs` for `rawler`. Read those before proposing a swap.

Two time crates appear because they are the public types of two upstream crates.

> **Reversed (2026-08-01). The original rule here was: "do not write conversions between them — normalize both sides to `i64` Unix seconds and do all correlation arithmetic in that single scalar domain."** It worked, but it was the wrong trade. A bare `i64` says nothing about unit, epoch or zone; instants and durations were the same type, so passing a duration where an instant belonged compiled fine; and it *manufactured* two error paths — `xmp::render` had to reject a Unix second it could not convert back, and `format_utc` needed an "(unrepresentable)" fallback — for states that cannot arise once the value is already a `DateTime`.
>
> **The rule now: `chrono::DateTime<Utc>` for instants, `chrono::TimeDelta` for durations, everywhere inside the program.** chrono is the one we depend on directly and the one `nom-exif` already hands us, so the EXIF side stops converting at all. `gpx`'s `time::OffsetDateTime` is converted in exactly one function, `track_point` in `track.rs`, and the *number* of conversions is what the original rule was really trying to keep at zero — one, at a named boundary, is not the sprawl it was guarding against.
>
> Both error paths are gone: `xmp::render` is now infallible. `--max-gap` stays a count of seconds on the command line, since that is the right interface for a user, and becomes a `TimeDelta` immediately (`GapLimits::DEFAULT_GAP_SECONDS` is the single place those meet).

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

An earlier draft here reasoned that NEF would come free because NEF is TIFF-based. **That was tested against 150 real Nikon D3300 files and proved half right in the least convenient way:** NEF does not parse at all through `MediaSource::open`, the streaming source `raw.rs` used, but parses perfectly through `MediaSource::from_memory`, which reads the whole ~22 MB file.

**NEF is now implemented, and it cost exactly one new column.** `RawFormat::read_strategy()` returns `Streaming` or `WholeFile`, and `raw.rs` picks the source accordingly. This is the design working as intended rather than an exception to it: the variation between formats turned out to be real, it turned out to be *data*, and it went into the data table. Two consequences are worth carrying forward — a `WholeFile` format reads every byte of every photo rather than a header, so its runs are bandwidth-bound instead of latency-bound; and a body that records no `OffsetTimeOriginal` (as the D3300 does not) makes `--utc-offset` mandatory, since every file otherwise reaches the gate. CLAUDE.md has the measurements; do not re-run them.

For the formats nom-exif does not read at all, the pure-Rust alternative is `rawler` — `Decoder::raw_metadata()` reads metadata without decoding the image — at a cost of ~106 transitive crates including a complete JPEG-XL decoder. That is the escape hatch when a second camera system actually arrives, not before; the note in `format.rs` says the same where someone adding a format will see it.

So the seam is placed where variation genuinely lives: **extension mapping and per-format tag preferences**, expressed as an enum with a data table.

**The sketch below is abridged and `format.rs` is authoritative** — it was written
for one format and updated when NEF landed, so the shape is right but the bodies are
not. Read the real file before adding a format; `read_strategy` in particular is easy
to leave out of a sketch and impossible to leave out of a working format.

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

/// How a format's bytes have to reach the parser. This column is what NEF cost;
/// see the paragraph above for why it is not optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStrategy {
    Streaming,   // let the parser seek — a ~30 MB CR3 costs a few header reads
    WholeFile,   // hand it every byte — required by the TIFF-based raws
}

/// A raw format we know how to read a capture time from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    Cr3,
    Nef,
}

impl RawFormat {
    /// Every supported format, in help-text order.
    pub const ALL: &'static [RawFormat] = &[Self::Cr3, Self::Nef];

    /// Extensions that select this format, lowercase.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Cr3 => &["cr3"],
            Self::Nef => &["nef"],
        }
    }

    /// How to hand this format's bytes to the parser. Verified against real files
    /// of each format, which is the only way to know.
    pub fn read_strategy(self) -> ReadStrategy {
        match self {
            Self::Cr3 => ReadStrategy::Streaming,
            Self::Nef => ReadStrategy::WholeFile,
        }
    }

    /// Capture-time tags to try, in priority order. Collapsed deliberately: these
    /// two formats genuinely do not differ here. Split the arm when one diverges.
    pub fn capture_tags(self) -> &'static [CaptureTag] {
        match self {
            Self::Cr3 | Self::Nef => &[
                CaptureTag { datetime: ExifTag::DateTimeOriginal, offset: ExifTag::OffsetTimeOriginal },
                CaptureTag { datetime: ExifTag::CreateDate,       offset: ExifTag::OffsetTimeDigitized },
            ],
        }
    }

    /// Case-insensitive table scan, tolerating one leading dot.
    pub fn from_extension(ext: &str) -> Option<Self> { /* ... */ }

    /// The matching rule itself, shared with the directory walk so that a format
    /// declaring two extensions finds files under both.
    pub fn matches_extension(self, ext: &str) -> bool { /* ... */ }
}
```

**Adding a format is then:** add the variant → the compiler flags every `match` that no longer compiles → fill in those arms → add a fixture test against a real file of that format. Forgetting a spot is a **build error, not a runtime surprise**, which is precisely the property a trait-object registry or a `HashMap` of handlers would discard. This is the whole cost only when the parser already reads the format — Fujifilm RAF is such a case; NEF is not.

When two formats end up with identical arms, that is not duplication to factor away — it is the code stating plainly that these formats do not differ. Collapse them into one arm, as `capture_tags` above already does for CR3 and NEF, and split it when one actually diverges.

**Start flat, promote later.** `format.rs` is one file. If some format eventually needs genuinely bespoke extraction, that match arm calls into its own module and `format.rs` becomes `format/mod.rs` + `format/rw2.rs`. In Rust that promotion is a rename, not a refactor — so there is no cost to deferring it, and the per-format subdirectory appears only once a format has earned one.

**Explicitly rejected: runtime plugin discovery.** Scanning a directory for format plugins requires `libloading` and an `extern "C"` boundary, because Rust has no stable ABI — a plugin compiled against a different rustc or crate version is undefined behavior rather than a link error. It also forfeits single-binary distribution, complicates cross-compilation, and removes all compile-time exhaustiveness checking. Rust deliberately has no module auto-discovery; `mod` declarations are explicit by design, and matching that expectation matters more here than any flexibility gained.

## Concurrency

**Shape of the work.** Per file: open and parse EXIF (I/O plus modest CPU), binary-search and interpolate (negligible), serialize and write a ~1 KB sidecar (I/O). Expect this to be **I/O-bound, not CPU-bound** — for CR3, nom-exif seeks within the BMFF container rather than reading whole 30 MB files, so each input costs a few hundred KB of reads. The realistic ceiling is storage, not cores — and *saturating cores turned out not to be the goal at all*. See the measured note below: the default is **16**, chosen because a first read of the files dominates everything else and threads hide it. It is not rayon's logical-core count either, and on *warm* local storage extra threads actively contend on NTFS directory metadata.

> **This shape is per-format, which was not anticipated here.** A `WholeFile` format (NEF) reads every byte of every photo instead of a header, so its reads are bandwidth-bound rather than latency-bound and gain only ~2× from threads where CR3 gains ~12×. The "few hundred KB per input" above is a CR3 statement, not a property of the tool.

> **Measured since, and it settles the open question above — in both directions.** There is no single good `--jobs` value; the optimum is set by storage latency. On **local NVMe** the read is nearly free and the run is write-bound, so throughput peaks at **`-j 2`** and degrades above it, because NTFS serializes entry creation against a directory's index. (Measured later, and it refines this: the lock is *per directory*, the atomic write is not the cause — one-stage writes scale slightly worse — and a recursive run spanning many folders does parallelize. See CLAUDE.md's sidecar-writes section.) On **SMB** the read dominates and parallelizes **~12×**, so `-j 16`–`20` is right. The shipped default was therefore **2** at first (`DEFAULT_JOBS` in `main.rs`), tuned for the common local case. **Corrected 2026-08-02: it is now 16.** Every local number above was measured warm; on a *first* read of the files, local NVMe behaves like SMB rather than like RAM, and `-j 20` beat `-j 2` by several fold on an uncached import. The warm case still prefers 2, and still loses — see CLAUDE.md for the asymmetry that decides it. The prediction that nom-exif seeks within the BMFF rather than reading whole files **did** hold: 3,883 CR3s resolve in ~0.3 s locally, impossible if 30 MB were read per file. CLAUDE.md's *Measured behavior* section carries the numbers; do not restate them here, so there is one place to correct.
>
> **A warm cache will mislead you.** An early sweep that pre-warmed the cache showed reading parallelizing only ~3× and plateauing near 4 threads — that was measuring RAM, not storage, and it understated cold behavior by more than an order of magnitude. Evict, or use untouched data, before quoting read-scaling numbers.

**Structure.** Two parallel phases with a gate between them:

- **Phase A — collect and extract (parallel).** Walk the tree with `walkdir`, collecting matching paths into a `Vec`. Then `par_iter()` to extract each capture timestamp.
- **Gate (sequential, cheap).** If any file resolved to a naive timestamp with no `--utc-offset` available, print the list and exit non-zero **having written nothing**.
- **Phase B — interpolate and write (parallel).** `into_par_iter()` over the successful extractions. **One worker task does the entire remainder for one photo** — `track.lookup` interpolates, `xmp::render` serializes, `xmp::write_atomic` writes. Geotagging is *not* a separate phase from writing; the only step split out is the EXIF read. The progress bars say `reading capture times` and `writing sidecars`, which invites the opposite reading — see CLAUDE.md's *Execution shape* section.

The gate is why this is two phases rather than one. Forgetting `--utc-offset` on a body that does not record `OffsetTimeOriginal` would otherwise silently misplace every photo by the offset amount, and discovering that after half the sidecars are on disk is worse than discovering it before any are. **This stopped being hypothetical when NEF arrived:** the Nikon D3300 records no `OffsetTimeOriginal` at all, so every one of its files reaches the gate and every NEF run needs the flag. The rule itself lives in `choose_offset` in `raw.rs` and is unit-tested branch by branch. In practice the gate rarely fires spuriously: `--utc-offset` applies *only* to naive files, so a mixed-camera shoot where one body records its zone and the other does not is handled correctly with a single flag. The cost of splitting is one `Vec<Extraction>` — a path plus a timestamp or a diagnostic per file. Phase A also yields an exact denominator for the progress bar.

**The barrier cannot be optimized away.** The gate needs every capture time, and *obtaining* a capture time is itself the expensive operation — the raw parse, which for a `WholeFile` format means reading the entire file. There is no cheap pre-scan that validates timezones without doing the costly work, so the expensive pass and the gate are inherently the same pass. The barrier costs ~10% of wall clock (reads alone vs. reads plus writes on the 1,024-file set), and a fused design would recover only part of that, since writes do not parallelize on NTFS anyway.

**Considered and rejected:** when `--utc-offset` *is* supplied, no file can reach `ExtractionKind::NeedsOffset`, so the gate is provably vacuous and the phases could legally fuse into a single pass. That buys a few percent on one code path in exchange for two structurally different execution models to reason about and test. Constraint 3 (readable over clever) wins; do not re-propose it.

**Collect-then-parallelize, not `par_bridge`.** Materializing the walk into a `Vec` before parallelizing gives rayon contiguous slices to split, which load-balances far better than bridging a sequential iterator. Traversal is a small serial prefix; if it ever dominates (very deep trees, network storage), `jwalk` is a near drop-in replacement that parallelizes the walk itself.

**Per-worker parser state.** `MediaParser` pools internal buffers and is worth reusing — but sharing one across threads behind a `Mutex` would serialize the entire run and defeat the point. Use rayon's `map_init`, which constructs one parser per worker:

```rust
paths.par_iter()
    .map_init(MediaParser::new, |parser, path| extract(parser, path))
    .collect::<Vec<_>>()
```

**No shared mutable state.** The track index is built once, before *Phase A* (`Track::load` is the first thing `main` does after the clock starts), and shared immutably as `&Track` — read-only, no lock, no contention. Workers return an outcome enum rather than incrementing counters or printing; tallying happens sequentially afterward.

**Deterministic output.** Warnings are **never printed from worker threads** — interleaved stderr from N threads is unreadable and makes runs non-reproducible. Each worker returns its diagnostics in its outcome value; `main` sorts by path and prints after the phase completes. Progress goes through `indicatif`'s `ProgressBar`, which is internally synchronized; use `ProgressBar::suspend` if anything must print mid-phase.

**Writes.** Sidecar paths are unique per input, so parallel workers never target the same file. The temp file that `xmp::write_atomic` renames from gets a **random** name in the destination directory, from `tempfile::NamedTempFile::new_in`. That randomness is load-bearing: a name derived from the target would be identical in two concurrent `rawgeotag` runs over one directory, and they would race on it. It is also the reason `tempfile` is a dependency rather than a hand-rolled temp-then-rename.

> **Corrected 2026-08-02.** This paragraph used to claim the temp name *was* derived from the target, and that this is what made it collision-free — the opposite of the real argument. `Cargo.toml` and `xmp.rs` state it correctly at the site; this was the one place that disagreed.

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
> 1,024 real CR3s tripped the gate.) The implementation therefore pairs each
> capture tag with its offset tag in `format.rs` and reads both, preferring
> `.aware()` when present and falling back to the paired offset tag. The
> precedence rule stated here is unchanged; only the mechanism differs.

Resolution rule (**EXIF wins, warn on conflict**):
1. If `.aware()` is `Some` and `--utc-offset` was given and the two offsets differ → return a conflict warning naming the file and both offsets; proceed with the EXIF value.
2. Resolve with `.or_offset(cli_offset)` — attaches the fallback only to `Naive` values, returns `Aware` values untouched. This is the whole precedence rule in one call.
3. `Naive` with no `--utc-offset` → the gate condition above.

Shift the resolved `DateTime<FixedOffset>` to UTC with `.with_timezone(&Utc)` and hand off a `DateTime<Utc>` — a change of representation, not of instant. A file with no usable capture tag is skipped with a warning.

## track.rs — GPX and interpolation

> **Extended 2026-07-31: several GPX files may be given** (`<GPX>...`), for a day split across a driving log and an evening walk. They are flattened into one index, with two rules that keep the merge honest. **Segment numbering continues across files**, so the seam between two files is a segment break and is never interpolated across — restarting the counter per file would make the last point of one and the first of the next look contiguous. **Overlapping time ranges are a hard error**, raised while the index is built and so before any sidecar is written: two tracks covering one instant can disagree, only one point per timestamp survives, and the survivor would be chosen by argument order. The bound is inclusive — one shared second is an overlap. The remedy is separate passes, which work because photos outside a track are skipped.

Load once at startup, before *Phase A*. Flatten every `track.segments[].points[]` plus standalone `gpx.waypoints` into one `Vec<TrackPoint { at: DateTime<Utc>, lat: f64, lon: f64, ele: Option<f64>, segment: u32 }>`, dropping points with no timestamp, then sort by `at` and dedupe. (`segment` postdates the original design — it identifies the contiguous recording run a point came from, and is what lets the reversed gap rule below refuse to bridge a `<trkseg>` break.) After construction it is immutable and freely shared across threads.

Look up by `slice::binary_search_by_key(&at, |p| p.at)` — `DateTime<Utc>` is `Ord`, so the search needs no key extraction into a scalar:
- exact hit → use that point
- between `i` and `i+1` → linear interpolation, fraction `f = (at - a.at).as_seconds_f64() / (b.at - a.at).as_seconds_f64()`, subtracting instants to get `TimeDelta`s and dividing those
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
> Measured on the 2025-09-17 Malta shoot (1,024 CR3s): 1,002 tagged, 22 skipped —
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
Scanned      419 raw files
Tagged       405
Skipped       14   9 outside track, 3 existing sidecar, 2 no capture time
Elapsed        3.2s  (131 files/sec, 16 threads)
```

The count column is **width 7**, which is wider than these numbers need: it is
sized to fit a seven-figure count once thousands separators are in. Widen it
rather than dropping the separators if it ever overflows.

**The elapsed line aligns its *whole seconds* in that column, not the whole value**,
which is why its decimal point hangs to the right of it. Formatting the seconds as one
number — `{:>7.1}` — right-aligns `3.2` as a unit, so the point and tenths eat two
columns and the units digit lands left of every integer above. Splitting the tenths
off is also what lets a run over 1,000 s carry separators, which a bare `{:.1}` cannot
produce.

Exit non-zero if any file errored or the gate fired; zero if everything was either tagged or deliberately skipped.

## Verification

**Moved to [`TESTING.md`](TESTING.md)**, which is now the project's testing standard:
the standing order, the mutation-checking bar, the doctrine on branches no fixture can
reach, what to run, and the running mutation and determinism logs.

It lived here for as long as this document was the only place decisions were written
down. [`WRITING.md`](WRITING.md) went the same way for the same reason and records the
rule both moves followed: when a section outgrows the document whose subject it is not,
move it and leave a pointer. It had grown to a fifth of the file and was still growing — two tables were
appended in a single day — and a standing order about how to work is a different genre
from a design that is settled and finished. What remains in this document is the
design; how it is held to account is next door.

The unit tests each module must keep covered are not listed there either, because
`cargo test` is the authoritative inventory and a hand-written list of them went stale
three times. What `TESTING.md` records instead is the *rule* that decides whether a
test is worth having.

---

# Appendix — completed groundwork

*Retained as a record. Nothing here needs redoing, and nothing here is design — it
opened this document from the day it was written, pushing the design itself below the
fold for every reader who came looking for it.*

## Prerequisite

**Satisfied — this section is retained only so the requirement is on record.** When the plan was written Rust was absent from this machine; it has since been installed and the project builds and ships from it.

Verified present (2026-07-31): `rustc` / `cargo` **1.97.1**, toolchain `stable-x86_64-pc-windows-msvc`, with the Visual Studio Build Tools "Desktop development with C++" workload that the MSVC target links against. ExifTool **12.76** is also installed and is useful as an independent oracle when verifying output — see Verification — but it is **not** a runtime dependency and appears nowhere in shipped code.

---

## Step 0 — persist this plan — ✅ COMPLETE (2026-07-28)

**This step is done; it is retained as a record. Nothing here needs redoing.** The
next session starts at the implementation, beginning with `cargo init`.

It was deliberately scoped to **scaffolding and version control only** — no Rust source, no crates, no `cargo init`.

**Why it existed.** The plan originally lived only at `~\.claude\plans\i-d-like-to-write-deep-thimble.md` — outside the project, under a machine-generated slug, in no version control. Its *content* was already sufficient to execute from cold; only its location was fragile. This document is now the versioned source of truth.

Actions taken:

1. **`git init`** in the repository root, with the initial branch named `main`.
2. **`docs/PLAN.md`** — this document, copied into the repo. Becomes the versioned source of truth; the `.claude/plans` copy is left alone as a harmless duplicate.
3. **`.gitignore`** — `/target` and `**/*.rs.bk`. **`Cargo.lock` is deliberately *not* ignored**: this crate is a binary, and Rust convention is to commit the lockfile for binaries so builds are reproducible. (The ignore-the-lockfile advice applies to libraries.)
4. **`README.md`** — short and human-facing: what the tool does, current status (*planning complete, implementation not started*), prerequisite (install Rust), and a pointer to `docs/PLAN.md`.
5. **`CLAUDE.md`** — loaded automatically into context at the start of every Claude Code session in this directory. This is what lets a future session resume without re-litigating settled decisions. Keep it brief and state the binding constraints:
   - Pure Rust only — no ExifTool, no C-library bindings, ever.
   - Optimize for wall-clock time; the work is parallel by design.
   - Readable and maintainable over clever; no surprises for an experienced Rust reviewer.
   - Design decisions are settled in `docs/PLAN.md` — read it before proposing changes.
6. **Initial commit** on `main`.

> **Item 5's "keep it brief" did not survive, and that was a decision rather than a
> drift** (noted 2026-08-02, because instruction and artifact had been disagreeing
> silently). CLAUDE.md became the record of what real data did to the design — the CR3
> timezone trap, NEF's `read_strategy`, the measured `-j` behavior — which is the one
> thing this plan cannot be, and each entry cost a session to learn. **The bar for
> adding to it is "would a session otherwise repeat this mistake", not "is this true".**

No remote is configured — this is a local repository unless a GitHub remote is requested separately.

**To resume later:** open Claude Code in the repository root and ask it to implement `docs/PLAN.md`. `CLAUDE.md` loads the constraints automatically; the plan supplies everything else. Nothing from this conversation is required.

---
