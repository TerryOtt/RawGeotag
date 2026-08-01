# RawGeotag — working notes for Claude

A Rust CLI that geotags camera raw files from one or more GPX tracks, writing XMP sidecars.

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
   I/O-bound, not CPU-bound. **Match the thread count to storage latency, not to
   core count** — "keep all cores busy" is actively wrong here, and `--jobs`
   defaults to 2 for that reason (see *Measured behavior*). Do not introduce
   shared mutable state on the hot path, and never share a `MediaParser` across
   threads behind a mutex — use rayon's `map_init` for per-worker parsers.

3. **Readable and maintainable over clever.** Strive not to violate the principle
   of least surprise for an experienced Rust developer reviewing this codebase.
   Prefer the obvious mechanism to the clever one. Notably: format extensibility is
   an enum plus a data table, *not* a plugin registry or runtime module discovery —
   the plan explains why.

4. **Raw files are never modified.** Output is sidecars only.

## Execution shape: two phases with a gate between them

**The phase boundary is not where the progress bars suggest.** They read `reading
capture times` and `writing sidecars`, which invites the conclusion that phase one
geotags and phase two writes. It does not. Interpolation, XMP rendering, and the
write are all **fused in one worker task per photo**; the only step hoisted into its
own pass is the EXIF capture-time read.

| Step | Where | Parallel? | Does |
|---|---|---|---|
| Phase A | `main`, `par_iter().map_init(MediaParser::new, ..)` → `extract` | yes | EXIF capture time **only** |
| Gate | `main`, sequential scan for `Extraction::NeedsOffset` | no | abort the run, or let it proceed |
| Phase B | `main`, `into_par_iter()` → `write_one` → `write_sidecar` | yes | `track.lookup` interpolate → `xmp::render` → `xmp::write_atomic` |

**Why phase A exists at all — the gate.** A capture time with no timezone and no
`--utc-offset` could misplace a photo by a whole day of travel. The tool refuses the
entire run in that case, which is only expressible if *every* capture time is known
before *any* sidecar is written. That requirement, and nothing else, forces the
barrier.

**Why it cannot be fused away.** The gate needs the capture time, and obtaining the
capture time *is* the expensive operation — a ~30 MB CR3 parse. There is no cheap
pre-scan that validates timezones without doing the costly work, so the barrier
cannot be moved earlier or made cheaper. The expensive pass and the gate are
inherently the same pass.

**What the barrier costs.** The lost overlap between reading and writing, which is
bounded by whichever phase is shorter — so it is ~10% over SMB (Malta: 3.0 s of reads
against ~0.3 s of writes) and up to ~20% locally (Rockies: ~0.3 s of reads against
~1.2 s of writes). A fused single-pass design could recover part of that, and less
than it sounds, since sidecar writes do not parallelize on NTFS anyway (see *Measured
behavior* below). Not a good trade against the guarantee.

**Rejected, do not re-propose:** when `--utc-offset` *is* supplied no file can reach
`NeedsOffset`, so the gate is provably vacuous and the two phases could legally fuse
into one pass. This buys a few percent on one code path in exchange for two
structurally different execution models to reason about and test. Constraint 3 wins.

**Workers never print.** Each returns its diagnostics inside its outcome value
(`Extraction`, `Written`); `main` sorts by path and prints after the phase completes.
This is what makes output byte-identical at any `--jobs`. Do not add an `eprintln!`
inside a worker.

## Dependency versions: check crates.io, never recall from memory

The version numbers in [`docs/PLAN.md`](docs/PLAN.md)'s dependency table are
**indicative, not authoritative**. Before any of them lands in `Cargo.toml` — a new
dep, or a bump — confirm the current release with `cargo search <crate> --limit 1`.

This is not hypothetical. `indicatif` was pinned at `"0.17"` because that version was
familiar, not because it was current; 0.18 had already shipped six patch releases by
then. It sat stale until a human noticed.

What makes this bite is Cargo's `0.x` rule: for a pre-1.0 crate the **minor** is the
breaking-change position, so `"0.17"` means `>=0.17.0, <0.18.0` and *can never*
resolve to 0.18. `cargo update` respects that ceiling and reports "Locking 0
packages" while three minor releases behind — reassuring and wrong. **A clean `cargo
update` is not evidence of being current.** Only crates.io can tell you.

`1.x` deps are mostly self-correcting (`anyhow = "1"` keeps picking up 1.0.x), so the
risk concentrates in the `0.x` crates: `gpx`, `chrono`, `time`, `indicatif`.

## Settled decisions worth not rediscovering

- Sidecar naming: `IMG_1234.CR3` → `IMG_1234.xmp` (Adobe convention, extension replaced).
- Timezone: EXIF `OffsetTimeOriginal` wins over `--utc-offset`; warn on conflict.
- Photos outside the track are skipped and reported — no clamping, no
  extrapolation, no tolerance window. With several GPX files that means outside the
  *union* of them; a photo landing in the seam between two files is a gap, not an
  outside-track case.
- **Gap rule (reverses the plan's original "no `--max-gap`" decision).**
  Interpolate only when the bracketing points are within **60 s AND 100 m**
  (`--max-gap`, `--max-distance`) *and* share a `<trkseg>`. Both limits are load-
  bearing: endpoint separation does not bound the error, since a subject can leave
  and return between two nearby fixes, so a 140 s / 8 m hole is still untrustworthy.
  Do not "simplify" this to a single condition.
- Existing sidecars are skipped with a warning; `--force` overwrites. No merging.
- **Multiple GPX files** are accepted (`<GPX>...`) and merged into one index, for a
  day split across several tracks. Two rules make that safe, and neither is
  optional:
  - **Segment numbering continues across files**, so a seam between two files is a
    segment break and is never interpolated across. Restarting the counter per file
    would make the last point of one and the first of the next look contiguous.
  - **Overlapping time ranges are a hard error**, checked while the track is built
    and therefore before any sidecar is written. Two tracks covering one instant can
    disagree, and the index keeps one point per timestamp — so the winner would be
    decided by argument order. A geotag decided by argument order is the mantra's
    exact failure mode. Inclusive bound: sharing one second is an overlap. The remedy
    is separate passes, which work because photos outside a track are skipped.

## Status

Implemented. Builds clean, the unit test suite passes, `cargo clippy -- -D warnings`
is clean. (No count here on purpose — it went stale three times; `cargo test` is the
authoritative answer.) Toolchain on this machine: Rust 1.97.1 MSVC, with the VS Build Tools C++
workload installed.

**Verified against four real shoots**, all Canon EOS R5.

*Malta 2025-09-17* (`Q:\Lightroom\Images\2025\2025-09-17`, 1024 files, with
`2025-09-17 - Malta Car Tour.gpx`): 1002 resolve and tag, ~3.0 s over SMB. The 22 skips
are **three distinct holes, not one** — 10 across a segment break (460 s / 594 m), 9 in
a 140 s / 8 m hole, 3 in a 775 s / 27 m hole. The 140 s / 8 m cluster is exactly what
the two-limit rule exists for: 8 m clears the distance limit easily and only the time
limit rejects it.

*Canadian Rockies 2022-09-27* (3883 files, 188 GB, local NVMe, with `2022-09-27- Peyto
Lake, Bow Lake, Yoho.gpx`): 2394 tag, 1489 skip, 772 of those across `<trkseg>` breaks.
This body's clock was on **`+01:00`**, so unlike the 2025 trips it actually exercises
the EXIF offset conversion instead of a no-op. Spot-checked against the raw GPX on an
exact-hit photo: longitude and altitude identical to the track point.

*Jackson WY 2025-11-24* (726 files, SMB, `-j 16`): **726 of 726 tagged, nothing
skipped** — the cleanest run so far. The shoot (17:40–19:43 Z) sat wholly inside the
track (17:11–21:57 Z) with no dropout large enough to trip the gap rule. Position
matched the raw track point exactly.

*Malta/Sorrento multi-track, 2025-09* — the first real exercise of **multiple GPX
files**. All seven tracks in `Q:\Photo GPX Tracks\2025\2025-09 - Malta, Sorrento` were
passed to each of the four photo folders that exist (`09-17/18/19/21`), writing 1967
new sidecars: 95/95, 297/297, 1575/1689, and 09-17 already done. Passing every track to
every folder is the safe idiom here — the tracks are disjoint in time, so each photo
matches whichever one covers it and **nothing depends on pairing a filename to a folder
name**. The 114 skips on 09-21 were 108 across a 2122 s / 844 m segment break and 6 in
a 100 s / 189 m hole. Six sidecars spot-checked against the raw GPX agreed to **under
0.11 m**, all exact-timestamp hits (the logger samples at 1 Hz, so most photos land on
a recorded point and are never interpolated at all).

Beware that GPX filenames can lie about their own date: `2025-09-21 - Amalfi Coast
Drive.gpx` held data from 09-24 and was renamed to `2025-09-24 - Sorrento to Rome
Drive.gpx`. Read the span, never the name — `rawgeotag --verbose --dry-run <empty-dir>
cr3 <gpx>` prints it and exits before touching any photo.

Interpolation agrees to within the coordinate encoding's resolution: `xmp.rs` writes
ten-thousandths of a minute, and 0.0001 minute of latitude is ~0.19 m, so that is the
floor on any agreement these checks can demonstrate. ExifTool reads the sidecars back
correctly and `-validate` is OK.

**Output is deterministic** — same input at `--jobs 1`, `2` and `16` produced
byte-identical sidecars and identical warning lists over the 3883-file set. Re-run
that check after any change to the phase structure, the outcome enums, reporting
order, or the GPX load path (which is parallel too, see below).

Note ExifTool calls the XMP `exif:GPSTimeStamp` property **`GPSDateTime`**; asking it
for `GPSTimeStamp` on a sidecar returns nothing, which is a naming difference, not a
bug.

## The CR3 timezone trap — do not regress this

nom-exif returns CR3 `DateTimeOriginal` as **`Naive`** and exposes
`OffsetTimeOriginal` as a **separate `Text` entry** (`"+00:00"` on the 2025 Malta
files, `"+01:00"` on the 2022 Rockies ones — the offset is a per-trip camera setting,
so never hardcode or assume it). It never merges them.
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

## Measured behavior worth not rediscovering

**`--jobs` defaults to 2, deliberately, and that is not a typo.** The optimum depends
entirely on storage latency, and the two cases point in opposite directions:

| | read phase | best `-j` | why |
|---|---|---|---|
| **Local NVMe** | ~0.3 s / 3883 CR3s — nearly free | **2** | run is write-bound; NTFS serializes directory metadata, so threads contend |
| **SMB / network** | dominates the run | **16-20** | latency-bound; threads keep requests in flight |

Local NVMe, full workflow on 3883 files creating 2394 sidecars — `-j 2` measured
~1470 ms warm and ~1723 ms cold, against ~1790-1850 ms at `-j 20` and ~1640-1920 ms
at `-j 1`. `-j 2` won at min, median and max, warm *and* cold, so it is not noise.

Cold SMB read throughput: **25 files/s at `-j 1`, 107 at `-j 4`, 296 at `-j 20`** —
nearly **12x** from parallelism. Projected onto a 3883-file day that is ~155 s
single-threaded versus ~13 s. This is why the rayon design stays even though the
local case gains nothing from it: dropping threads would optimize the case that is
already under two seconds and wreck the case that takes minutes.

**Warm benchmarks lie here.** An earlier sweep with a cache warm-up showed reading
parallelizing only ~3x and plateauing near 4 threads; that measured RAM, not storage.
Always evict or use untouched data before quoting read-scaling numbers.

**GPX parsing is a serial-feeling cost that turned out to be worth parallelizing.**
Seven tracks of one trip (15.4 MB, 75,728 points) took ~700 ms to parse — comparable
to an entire local 3883-file run — and all of it lands before a single photo is
touched. `Track::load` now parses the files with `par_iter`: **658 ms at `-j 1`,
390 at the `-j 2` default, 269 at `-j 4`, 215 at `-j 8`.** It scales with *file
count*, not total bytes — the floor is the largest single file, ~170 ms for a 4 MB
track. The slowness is `xml-rs`, which the `gpx` crate uses internally; quick-xml
would be far faster, but forking `gpx` to get it is not worth it.

This *softens* the `--jobs` reasoning above without overturning it. Threads now help
a phase the local-NVMe case previously had no use for them in, so a local run with
many tracks is a case where a higher `-j` is defensible. But the 175 ms between
`-j 2` and `-j 8` is smaller than the write contention a high `-j` costs on NTFS, so
the default stays 2. Everything after the parse runs in argument order — segment
ids, which of several bad files is reported, the overlap message — so none of this
is visible in output at any `-j`.

Sidecar *writing* does not parallelize at all on NTFS — temp-create plus rename
(via `tempfile`) are two directory metadata operations per file and NTFS serializes
those within a directory. Note also that *creating* a new sidecar costs ~2.3x
*overwriting* an existing one, so a `--force` re-run is not a valid benchmark of a fresh import;
delete the `.xmp` files first. Do not "fix" any of this by dropping the atomic write.

One deviation from the plan, deliberately: a file with **no EXIF at all** returns
`nom_exif::Error::ExifNotFound`, and `raw.rs` maps that to `Capture::NoCaptureTime`
rather than letting it become a hard error. Otherwise two indistinguishable
situations — no EXIF, versus EXIF without a date tag — would produce different exit
codes.

## Field patterns from real shoots

**Which gap limit binds depends on how the camera was moving.** Knowing which knob to
reach for saves a wasted pass:

| Platform | Binding limit | Evidence |
|---|---|---|
| Ship under way | **distance** | NZ cruise 2025-03-14: 541 photos rejected on distance alone, **zero** on time. Typical hole `83 s / 599 m` — the vessel covers ~600 m between fixes seconds apart. |
| Boat stationary or drifting | **time** | Cabo sunset cruise 2025-01-22: largest cluster `154 s / 74 m` — a 2½ minute hole in which the boat moved 74 m. |
| Walking or driving on land | neither | Valletta, Jackson, St. Lucia all reached full or near-full coverage on the defaults. |

So at sea raise `--max-distance`, at anchor raise `--max-gap`, on land change nothing.
`--max-distance 2000` took the NZ sailing pass from 74% to 88%; `--max-gap 200`
recovered 174 Cabo frames. Both were hand-checked against the raw track and agreed to
centimetres — but **be clear what that proves**. It proves the interpolation
*arithmetic* is right. It cannot prove the vessel held course between two fixes,
because there are no intermediate observations to check against. Only relax a limit
when the *other* limit shows the subject was barely moving, and say so when reporting.

**Multi-track workflow that works.**

- **Pass every disjoint track to every folder.** Nothing then depends on matching a GPX
  filename to a folder name — which is the one failure mode the tool cannot detect,
  since a wrongly-paired track just reports everything as outside-track.
- **When one track overlaps others** — a multi-day cruise log spanning port days — the
  overlap check makes a single run impossible, by design. Run the *specific* tracks
  first, then the broad one **without `--force`**, so the more authoritative fix wins
  and the broad track only fills what is left.
- **GPX filenames lie.** Seen in the wild: a file named `2025-09-21` holding 09-24
  data, a `2015` typo for `2025`, and an en dash instead of a hyphen. Read the span,
  never the name: `rawgeotag --verbose --dry-run <empty-dir> cr3 <gpx>` prints it and
  exits before touching a photo.
- A track can be 8-9 MB on a single line. `grep -oE` over that on an SMB share is
  unusably slow — copy it local first, or parse it as XML.

**No cumulative sidecar tally lives here, deliberately.** It would change on every run,
tells a future session nothing about how to write or review code, and is exactly the
kind of hand-maintained number that went stale three times as a test count. The Status
section records verification of *correctness*; volume is not evidence of that.
