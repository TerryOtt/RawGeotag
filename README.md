# RawGeotag

A Rust CLI that geotags camera raw files from one or more [GPS Exchange Format][gpx]
(GPX) tracks, **so that Adobe Lightroom Classic picks the coordinates up when the
images are imported.**

That is the goal, stated narrowly on purpose. Raw files carry a capture timestamp but
no location; a GPS logger records a track over the same period. RawGeotag correlates
the two by time, linearly interpolating position between track points, and writes the
result as an [Extensible Metadata Platform][xmp] (XMP) sidecar next to each raw file —
in the form current Lightroom Classic writes and reads, so an import lands already
positioned with no round trip through Lightroom's Map module.

**Other uses are plausible but incidental.** The sidecars are ordinary XMP and other
tools may well read them, but Lightroom is the only consumer this is built and verified
for, and the only one a conflict is resolved in favour of. See
[XMP sidecars](#xmp-sidecars-and-who-they-are-written-for) for what that does and does
not promise.

**The first goal is accuracy, and everything defers to it: a geotag off by more than
5 m is worse than no geotag at all.** A missing tag is visibly missing, and you know to
go and fix it. A wrong one looks authoritative and quietly corrupts the photo's own
record of where it was taken. So wherever coverage trades against accuracy this tool
takes accuracy — it will not clamp, extrapolate, or bridge a hole in the track to raise
the number of photos it can claim to have tagged.

## Do no harm

**The second goal, and the one that shapes every default.** A geotagging pass runs
unattended over thousands of irreplaceable files, so it is designed to be incapable of
quietly costing you something.

**Raw files are never modified.** All output goes to sidecars, so the whole operation is
reversible by deleting the generated `.xmp` files.

**Photos that already have a sidecar are skipped, with a warning.** Never merged, never
partially updated. This matters more than it sounds, because a sidecar is not just a
geotag — once Lightroom has written one it holds develop settings, keywords, ratings,
labels and crop data, **none of which exist anywhere else.** They are not derived from
the raw and cannot be regenerated from it. A missing geotag is an afternoon; a lost
develop history is years.

That is also why the intended order is **geotag first, import second**: run this before
Lightroom has written anything and there is simply nothing of Lightroom's to collide
with.

### About `--force`

`--force` overwrites existing sidecars **wholesale** — it does not merge, and whatever
was in the file is gone. It will not be softened with a confirmation prompt or a
heuristic about whose file it is; a destructive flag that sometimes declines to be
destructive is worse than one that is honest about it. If you genuinely mean it — you
wrote the sidecars, you know what is in them, you want them replaced — it does exactly
what you asked.

**Be clear-eyed about what reaching for it usually means, though.** Pointing `--force`
at a real photo library is the shape of a bad day: the failure mode is silent,
immediate and unrecoverable, and there is no undo. The common reason for wanting it —
re-running a pass to get different results — is better served by **copying the photos
to a temp directory and working there.** Nothing about the output depends on where the
raws live, so a scratch copy costs you a copy and nothing else.

## XMP sidecars, and who they are written for

Each sidecar takes the Adobe naming convention — `IMG_1234.CR3` → `IMG_1234.xmp` — and
holds GPS coordinates, altitude and a timestamp as `exif:` properties in an ordinary
attribute-form RDF packet. Nothing exotic; ExifTool reads them back and `-validate`
passes.

XMP is standardized as [ISO 16684-1][iso] and published by Adobe as the [XMP
Specifications][xmp]; sidecar files specifically are the subject of Part 3, *Storage in
Files*. Worth knowing that the spec leaves a great deal optional — the packet wrapper
and property set here are both places where conforming implementations legitimately
differ — which is why conformance alone is not the bar this tool is held to.

**The explicit target is current Adobe Lightroom Classic.** The intended workflow is to
geotag *before* import, so photos arrive in the catalog already positioned. Output was
verified against **Lightroom Classic 15.4.1** by geotagging the same photos from the
same tracks in Lightroom itself and diffing: the coordinate encoding, GPS namespace,
`GPSVersionID` and serialization form all match what Lightroom writes, and positions
agree to **0.02–0.12 m on CR3 and 0.33–0.53 m on NEF**. (That residual is sub-second
capture times — Lightroom uses `SubSecTimeOriginal`, this tool truncates to whole
seconds. It is an order of magnitude inside the 5 m rule.)

**Caveat emptor beyond that.** Two limits worth being explicit about:

- **Older Lightroom is untested.** The GPS encoding has not changed across Adobe XMP
  Core 5.6-c140 (2019), 7.0-c000 (2024) and 15.4.1, which is reason for optimism and
  not evidence.
- **Other XMP consumers are untested entirely** — Capture One, Bridge, digiKam,
  darktable, Photo Mechanic. The packet is deliberately conventional, so there is no
  particular reason to expect trouble, but no one has checked.

**Where they conflict, Lightroom wins.** The packet is not changed except to follow a
change in what current Lightroom emits — that is the one thing that justifies touching
it. If some other tool wants a different spelling of the same data, that is a
compatibility request, not a bug in this one. See
[`docs/LIGHTROOM-XMP.md`](docs/LIGHTROOM-XMP.md) for the comparison method and results.

## Status

**Implemented and building.** The unit test suite passes and `cargo clippy
--all-targets -- -D warnings` is clean; run `cargo test` for the current tally.

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

Dependencies are refreshed deliberately rather than continuously — the intended cadence
is once before each trip, so anything surprising surfaces at home rather than on the
road. [`docs/UPDATING.md`](docs/UPDATING.md) has the process, including the `0.x` version
ceiling that makes a clean `cargo update` a poor indicator of being current.

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

`rawgeotag --help` is the authoritative list; this block is hand-maintained and
omits clap's automatic `-h` and `-V`.

Example:

```
rawgeotag ./shoot cr3 ./track.gpx --utc-offset -0700
```

**Canon CR3 and Nikon NEF are supported.** They differ in one way worth knowing
before a NEF run: many Nikon bodies write no EXIF timezone at all, so every file
hits the "no timezone" refusal and `--utc-offset` becomes mandatory rather than
optional. NEF also costs more to read — see
[NEF is a different shape](#nef-is-a-different-shape).

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
need a different one. See the Format extensibility section of
[`docs/PLAN.md`](docs/PLAN.md).

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
  distance limit rejects that. This is the 5 m accuracy rule from the top of this
  file, applied.
- **Existing sidecars** are skipped with a warning. `--force` overwrites them
  wholesale, discarding any develop settings, keywords or crops another tool stored
  there — read [Do no harm](#do-no-harm) before using it.
- **Exit code** is non-zero if any file errored or the run was gated; deliberate
  skips are still a clean exit.

## Performance

**`--jobs` defaults to 2.** That is well below the core count, and deliberate: the
best thread count is set by your storage, and local and network storage want
opposite answers.

The numbers below are CR3. **NEF behaves differently enough to have its own section
at the end** — read [NEF is a different shape](#nef-is-a-different-shape) before
tuning `-j` for a Nikon import.

These are a summary. The full measurement record, including the methodology each
figure was taken under, lives in `CLAUDE.md`'s *Measured behavior* section — correct
the two together.

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

- **Accurate before complete.** A geotag off by more than 5 m is worse than none, so
  coverage is never bought at the cost of position.
- **Do no harm.** Raws are never touched, existing sidecars are never merged or
  partially written, and the only destructive operation is one you have to ask for by
  name — see [Do no harm](#do-no-harm).
- **Pure Rust.** No ExifTool, no C-library bindings. (ExifTool is used only as a
  verification oracle by hand; it appears nowhere in shipped code.)
- **Fast.** Optimize for wall-clock time. The work is parallel by design, but more
  threads is not automatically faster — see [Performance](#performance).
- **Readable over clever.** No surprises for an experienced Rust reviewer.
- **Current Lightroom is the XMP reference.** The XMP spec is loose enough that
  conforming to it proves little, so what current Lightroom Classic emits is the
  standard the sidecars are held to — see
  [XMP sidecars](#xmp-sidecars-and-who-they-are-written-for).

[gpx]: https://www.topografix.com/gpx.asp
[xmp]: https://developer.adobe.com/xmp/docs/xmp-specifications/
[iso]: https://www.iso.org/standard/75163.html
