//! Geotag camera raw files from one or more GPX tracks by writing XMP sidecars.
//!
//! Raw files are never modified; the whole operation is undone by deleting the
//! `.xmp` files.
//!
//! Every supported raw format under the given directory is tagged in one pass;
//! `format.rs` holds the table that decides which those are. Several GPX tracks may
//! be given, as files or as directories of them, since a day's shooting is often
//! split across separate recordings — `track.rs` documents what keeps that merge
//! honest.
//!
//! The run is two parallel phases with a gate between them. Phase A extracts every
//! capture time; the gate refuses to continue if any file has a naive timestamp
//! and no `--utc-offset` to resolve it; Phase B interpolates and writes. Splitting
//! them costs one `Vec` and buys the guarantee that a forgotten `--utc-offset`
//! fails before a single sidecar lands on disk rather than halfway through.
//!
//! Workers never print. Each returns its diagnostics in its outcome value and
//! `main` sorts by path and prints afterwards, so output is identical at any
//! `--jobs` setting.

mod format;
mod raw;
mod track;
mod xmp;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use nom_exif::MediaParser;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::format::RawFormat;
use crate::raw::Capture;
use crate::track::{format_utc, Fix, GapLimits, Lookup, Track};

/// Default worker count, tuned for the common case: raws on a local SSD.
///
/// Sized for the case a photographer is actually in — a fresh import, where the
/// machine has never read these files. A first-touch read is a real read, and it
/// dominates everything else: 3,883 CR3s take ~48 s at `-j 2` against 5.8 s at
/// `-j 20`, on local NVMe. Cold local storage behaves like the network case
/// below, not like the cached one.
///
/// Warm, the ordering reverses and 2 wins: the read collapses to ~0.3 s and all
/// that is left is *creating* sidecars, which NTFS serializes within a single
/// directory, so extra threads only contend. But that case is a re-run, and the
/// asymmetry is what picks the default — being wrong about a warm run costs
/// ~70 ms, being wrong about a first import costs ~40 s.
///
/// High-latency storage inverts this completely: over SMB the read dominates
/// and parallelizes ~12x, so `-j 16` or more is the right call there. That case
/// is rare enough to be worth a flag rather than a worse default.
const DEFAULT_JOBS: usize = 16;

#[derive(Parser)]
#[command(
    name = "rawgeotag",
    version,
    about = "Geotag camera raw files from one or more GPX tracks by writing XMP sidecars",
    after_help = "Raw files are never modified. Existing sidecars are skipped unless --force \
                  is given, which overwrites them wholesale — discarding any develop settings \
                  or keywords another tool stored there.\n\n\
                  A photo is only tagged when the two track points bracketing its capture time \
                  are close in BOTH time and distance, and come from the same recording run. \
                  A geotag that is wrong is worse than one that is missing, so anything the \
                  track does not actually support is skipped and reported.\n\n\
                  Several GPX files may be given for a day split across separate recordings. \
                  They are merged, but the seam between two files is never interpolated \
                  across, and files whose time ranges overlap are rejected before anything \
                  is written — run those as separate passes instead.\n\n\
                  Every supported raw format under DIR is tagged in one pass — there is \
                  no extension to name. Files of a format this tool does not read are \
                  counted and reported rather than passed over in silence."
)]
struct Args {
    /// Parent directory, searched recursively
    dir: PathBuf,

    /// GPX track file, or a directory of them (not recursive). Repeat as needed
    #[arg(required = true, num_args = 1.., value_name = "GPX")]
    gpx: Vec<PathBuf>,

    /// Offset for files with no EXIF timezone, e.g. -0700, +0430
    #[arg(long, value_name = "±HHMM", allow_hyphen_values = true, value_parser = parse_utc_offset)]
    utc_offset: Option<FixedOffset>,

    /// Refuse to interpolate across a hole longer than this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = GapLimits::DEFAULT_GAP_SECONDS)]
    max_gap: i64,

    /// Refuse to interpolate across a hole wider than this many meters
    #[arg(long, value_name = "METERS", default_value_t = GapLimits::DEFAULT.max_meters)]
    max_distance: f64,

    /// Overwrite existing sidecars instead of skipping them
    #[arg(long)]
    force: bool,

    /// Do all the work, write nothing; add --force to preview a forced run
    #[arg(long)]
    dry_run: bool,

    /// Worker threads. Sized for reading files the machine has not cached
    #[arg(short, long, value_name = "N", default_value_t = DEFAULT_JOBS)]
    jobs: usize,

    /// Suppress the progress bar
    #[arg(long)]
    no_progress: bool,

    /// Per-file detail
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(Outcome::Clean) => ExitCode::SUCCESS,
        Ok(Outcome::HadFailures) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the run finished cleanly, for the process exit code. Deliberate skips
/// are clean; only errors and the gate are not.
enum Outcome {
    Clean,
    HadFailures,
}

fn run() -> Result<Outcome> {
    let args = Args::parse();

    if args.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.jobs)
        .build_global()
        .context("configuring the worker thread pool")?;

    let started = Instant::now();

    let track = load_track(&args.gpx, args.verbose)?;

    let Walk {
        files,
        ignored,
        errors: walk_errors,
    } = collect_paths(&args.dir)?;

    // Reported here, not with the per-file warnings after Phase B: these are about
    // paths the walk could not read, and they must reach the two early returns
    // below — the empty-tree return used to swallow them and exit clean, reading
    // as "no raws there" when the truth was "could not look".
    for error in &walk_errors {
        eprintln!("warning: {error}");
    }

    if files.is_empty() {
        println!(
            "Scanned  {:>7} raw files under {}",
            count(0),
            args.dir.display()
        );
        // The only signal that a whole shoot was invisible rather than absent.
        print_ignored(&ignored);
        return if walk_errors.is_empty() {
            Ok(Outcome::Clean)
        } else {
            Ok(Outcome::HadFailures)
        };
    }
    let scanned = files.len();
    let by_format = tally_formats(&files);

    // ---- Phase A: extract capture times -------------------------------------
    let progress = progress_bar(files.len(), args.no_progress, "reading capture times");
    let extractions: Vec<Extraction> = files
        .par_iter()
        .map_init(MediaParser::new, |parser, file| {
            let extraction = extract(parser, &file.path, file.format, args.utc_offset);
            progress.inc(1);
            extraction
        })
        .collect();
    progress.finish_and_clear();

    // ---- Gate ---------------------------------------------------------------
    let Extracted {
        photos,
        offsets,
        mut warnings,
        no_capture_time,
        failed: read_failures,
    } = match gate(extractions) {
        Ok(extracted) => extracted,
        Err(needs_offset) => {
            report_missing_offsets(&needs_offset);
            return Ok(Outcome::HadFailures);
        }
    };

    // ---- Phase B: interpolate and write -------------------------------------
    let settings = WriteSettings::from_args(&args);
    let progress = progress_bar(photos.len(), args.no_progress, "writing sidecars");
    let results: Vec<Written> = photos
        .into_par_iter()
        .map(|photo| {
            let written = write_one(photo, &track, settings);
            progress.inc(1);
            written
        })
        .collect();
    progress.finish_and_clear();

    let tally = tally_writes(results, args.verbose);

    // ---- Report -------------------------------------------------------------
    // The only two figures either phase contributes to. Adding them here, rather
    // than sharing a counter across both loops, is what keeps each phase's tally
    // readable on its own.
    warnings.extend(tally.warnings);
    let failed = read_failures + tally.failed;

    let mut details = tally.details;
    details.sort_unstable();
    for (path, detail) in details {
        println!("{}  {}", path.display(), detail);
    }

    warnings.sort_unstable();
    for (path, warning) in &warnings {
        eprintln!("warning: {}: {warning}", path.display());
    }

    print_summary(&Summary {
        by_format: &by_format,
        offsets: &offsets,
        ignored: &ignored,
        scanned,
        tagged: tally.tagged,
        outside_track: tally.outside_track,
        in_gap: tally.in_gap,
        sidecar_exists: tally.sidecar_exists,
        no_capture_time,
        failed,
        elapsed: started.elapsed().as_secs_f64(),
        threads: rayon::current_num_threads(),
        dry_run: args.dry_run,
    });

    if failed > 0 || !walk_errors.is_empty() {
        Ok(Outcome::HadFailures)
    } else {
        Ok(Outcome::Clean)
    }
}

/// A file that made it through Phase A with an absolute capture instant.
struct Photo {
    path: PathBuf,
    captured: DateTime<Utc>,
}

/// Phase A's per-file result. Diagnostics travel in the value rather than being
/// printed, so workers stay silent and output stays deterministic.
///
/// Path beside kind rather than repeated in every variant — the same shape as
/// `Written`, since the two model the same thing. It also means `extract` builds
/// the `PathBuf` once instead of once per arm.
struct Extraction {
    path: PathBuf,
    kind: ExtractionKind,
}

enum ExtractionKind {
    /// Resolved to an absolute instant. The warning is set when EXIF and
    /// `--utc-offset` disagreed; the EXIF value was used either way.
    Resolved {
        captured: DateTime<Utc>,
        /// The zone this instant was resolved through, for the summary.
        offset: FixedOffset,
        conflict_warning: Option<String>,
    },
    NeedsOffset,
    NoCaptureTime,
    Failed {
        error: String,
    },
}

fn extract(
    parser: &mut MediaParser,
    path: &Path,
    format: RawFormat,
    utc_offset: Option<FixedOffset>,
) -> Extraction {
    let kind = match raw::capture_time(parser, path, format, utc_offset) {
        Ok(Capture::Resolved {
            at,
            offset,
            conflict,
        }) => ExtractionKind::Resolved {
            captured: at,
            offset,
            conflict_warning: conflict.map(|conflict| {
                format!(
                    "EXIF timezone {} disagrees with --utc-offset {}; using the EXIF value",
                    conflict.exif, conflict.cli
                )
            }),
        },
        Ok(Capture::NeedsOffset) => ExtractionKind::NeedsOffset,
        Ok(Capture::NoCaptureTime) => ExtractionKind::NoCaptureTime,
        Err(error) => ExtractionKind::Failed {
            error: format!("{error:#}"),
        },
    };

    Extraction {
        path: path.to_path_buf(),
        kind,
    }
}

/// Phase A's results once the gate has let the run through: the photos Phase B
/// will write, and the diagnostics for everything that will not be written.
#[derive(Default)]
struct Extracted {
    photos: Vec<Photo>,
    /// How many capture times resolved through each zone. More than one entry, or
    /// one that is not UTC, is reported: a body on the wrong clock displaces a
    /// whole shoot silently, which is the failure the mantra exists to prevent.
    /// Keyed by seconds east of UTC, because `FixedOffset` is not `Ord`.
    offsets: BTreeMap<i32, usize>,
    warnings: Vec<(PathBuf, String)>,
    no_capture_time: usize,
    /// Files that could not be read at all.
    failed: usize,
}

/// The gate: sort Phase A's results into what Phase B needs, or refuse the run.
///
/// `Err` carries the files whose capture time has no timezone and no
/// `--utc-offset` to resolve it — sorted, because the list is printed and the run
/// must read the same at any `--jobs`. A non-empty list means no sidecar is
/// written at all; guessing an offset would misplace every photo by that amount.
///
/// Taking the extractions **by value** is the point of the signature: once they
/// have been consumed here, "a `NeedsOffset` that got past the gate" is not a
/// state the rest of the program can be handed, so there is nothing left to
/// assert about it downstream.
fn gate(extractions: Vec<Extraction>) -> Result<Extracted, Vec<PathBuf>> {
    let mut extracted = Extracted::default();
    let mut needs_offset = Vec::new();

    for Extraction { path, kind } in extractions {
        match kind {
            ExtractionKind::Resolved {
                captured,
                offset,
                conflict_warning,
            } => {
                *extracted
                    .offsets
                    .entry(offset.local_minus_utc())
                    .or_default() += 1;
                if let Some(warning) = conflict_warning {
                    // The one case that clones: the path is needed again below.
                    extracted.warnings.push((path.clone(), warning));
                }
                extracted.photos.push(Photo { path, captured });
            }
            ExtractionKind::NeedsOffset => needs_offset.push(path),
            ExtractionKind::NoCaptureTime => {
                extracted.no_capture_time += 1;
                extracted
                    .warnings
                    .push((path, "no capture time in EXIF".to_string()));
            }
            ExtractionKind::Failed { error } => {
                extracted.failed += 1;
                extracted.warnings.push((path, error));
            }
        }
    }

    if needs_offset.is_empty() {
        Ok(extracted)
    } else {
        needs_offset.sort_unstable();
        Err(needs_offset)
    }
}

fn report_missing_offsets(needs_offset: &[PathBuf]) {
    eprintln!(
        "error: {} file(s) have a capture time with no timezone, and no --utc-offset was given:",
        count(needs_offset.len())
    );
    for path in needs_offset {
        eprintln!("  {}", path.display());
    }
    eprintln!("\nRe-run with --utc-offset <±HHMM>. No sidecars were written.");
}

/// Phase B's per-file result.
struct Written {
    path: PathBuf,
    kind: WrittenKind,
}

#[derive(Debug)]
enum WrittenKind {
    Tagged { fix: Fix },
    OutsideTrack,
    InGap { description: String },
    SidecarExists,
    Failed { error: String },
}

/// What Phase B's workers actually need from the command line.
///
/// Resolved once before the loop rather than rebuilt per photo, and narrower than
/// `&Args` so a worker cannot reach a flag that has nothing to do with writing.
/// `Copy`, so handing it to every worker costs nothing.
#[derive(Debug, Clone, Copy)]
struct WriteSettings {
    limits: GapLimits,
    force: bool,
    dry_run: bool,
}

impl WriteSettings {
    /// `--max-gap` is a count of seconds on the command line and a `TimeDelta`
    /// everywhere inside; this is the one place the two representations meet.
    fn from_args(args: &Args) -> Self {
        Self {
            limits: GapLimits {
                max_gap: TimeDelta::seconds(args.max_gap),
                max_meters: args.max_distance,
            },
            force: args.force,
            dry_run: args.dry_run,
        }
    }
}

/// Takes the `Photo` by value so its path can be moved into the result rather
/// than cloned — Phase B owns them by this point.
fn write_one(photo: Photo, track: &Track, settings: WriteSettings) -> Written {
    let kind = write_sidecar(&photo, track, settings);
    Written {
        path: photo.path,
        kind,
    }
}

/// Decide one photo's fate and, unless told not to, write its sidecar.
///
/// The order of the checks is the behaviour, so it is worth stating: a photo the
/// track cannot place never reaches the filesystem at all; an existing sidecar
/// stops the write unless `--force`; and `--dry-run` returns after rendering but
/// before writing.
///
/// **The two flags do not compose the way their names suggest.** `force` only gets
/// past the skip-existing check — it does not force a write — so `--dry-run
/// --force` still writes nothing. Both are single `bool` reads whose polarity the
/// compiler cannot check, and inverting either is silent and destructive; the tests
/// named after them are the only guard.
fn write_sidecar(photo: &Photo, track: &Track, settings: WriteSettings) -> WrittenKind {
    let fix = match track.lookup(photo.captured, settings.limits) {
        Lookup::Found(fix) => fix,
        Lookup::OutsideTrack => return WrittenKind::OutsideTrack,
        Lookup::InGap(gap) => {
            let reason = if gap.across_segments {
                " (different recording runs)".to_string()
            } else {
                String::new()
            };
            return WrittenKind::InGap {
                description: format!(
                    "falls in a track gap of {}s / {} m{reason}",
                    thousands(gap.duration.num_seconds()),
                    thousands(gap.meters.round() as i64)
                ),
            };
        }
    };

    let sidecar = xmp::sidecar_path(&photo.path);
    if !settings.force && sidecar.exists() {
        return WrittenKind::SidecarExists;
    }

    let packet = xmp::render(&fix, photo.captured);

    // --dry-run still does every bit of work above, so it exercises the same code
    // paths and reports the same counts; it just stops before touching the disk.
    if settings.dry_run {
        return WrittenKind::Tagged { fix };
    }

    match xmp::write_atomic(&sidecar, &packet) {
        Ok(()) => WrittenKind::Tagged { fix },
        Err(error) => WrittenKind::Failed {
            error: format!("{error:#}"),
        },
    }
}

/// Phase B's outcomes, counted for the summary.
#[derive(Default)]
struct Tally {
    tagged: usize,
    outside_track: usize,
    in_gap: usize,
    sidecar_exists: usize,
    /// Sidecars that could not be written.
    failed: usize,
    warnings: Vec<(PathBuf, String)>,
    /// Per-file positions, collected only under `--verbose`.
    details: Vec<(PathBuf, String)>,
}

fn tally_writes(results: Vec<Written>, verbose: bool) -> Tally {
    let mut tally = Tally::default();

    for Written { path, kind } in results {
        match kind {
            WrittenKind::Tagged { fix } => {
                tally.tagged += 1;
                if verbose {
                    tally
                        .details
                        .push((path, format!("{:.6}, {:.6}", fix.lat, fix.lon)));
                }
            }
            WrittenKind::OutsideTrack => {
                tally.outside_track += 1;
                tally
                    .warnings
                    .push((path, "capture time is outside the track".to_string()));
            }
            WrittenKind::InGap { description } => {
                tally.in_gap += 1;
                tally.warnings.push((path, description));
            }
            WrittenKind::SidecarExists => {
                tally.sidecar_exists += 1;
                tally.warnings.push((
                    path,
                    "sidecar already exists (use --force to overwrite)".to_string(),
                ));
            }
            WrittenKind::Failed { error } => {
                tally.failed += 1;
                tally.warnings.push((path, error));
            }
        }
    }

    tally
}

/// Resolve the GPX arguments and build the index, reporting what it resolved to.
///
/// Split out of `run` because it is one coherent step — resolve, load, describe —
/// and because `run` reads better as a list of phases than as their contents.
fn load_track(gpx: &[PathBuf], verbose: bool) -> Result<Track> {
    let tracks = collect_tracks(gpx)?;
    let track = Track::load(&tracks)?;

    if verbose {
        // Worth listing, not just counting: a directory argument expands to whatever
        // was in it, and GPX filenames have been seen to lie about their own dates.
        // The span is the authority; these are what produced it.
        for path in &tracks {
            println!("  track: {}", path.display());
        }
        // The span is the union of every file given. It is not a claim of continuous
        // coverage — holes between the tracks are still holes, and a photo falling in
        // one is skipped like any other gap.
        let (start, end) = track.span();
        println!(
            "Track: {} points from {} file(s), {} to {}",
            count(track.point_count()),
            count(tracks.len()),
            format_utc(start),
            format_utc(end)
        );
    }

    Ok(track)
}

/// Resolve the GPX arguments: each may be a `.gpx` file or a directory of them.
///
/// **Not recursive, and that asymmetry with `DIR` is deliberate.** Photos are filed
/// in a tree — year, then date — so a run wants the whole thing. Tracks are filed
/// one folder per trip, and recursing would silently pull in neighbouring trips
/// whose tracks overlap this one. One level is the unit a human means by "the GPX
/// for this trip".
///
/// Sorted within each directory, because argument order is visible: it decides which
/// of several unreadable files is reported and the order of names in the overlap
/// error. Left in argument order *between* arguments, for the same reason.
fn collect_tracks(arguments: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut tracks = Vec::new();

    for argument in arguments {
        if argument.is_file() {
            tracks.push(argument.clone());
            continue;
        }
        if !argument.is_dir() {
            bail!(
                "{} is neither a GPX file nor a directory",
                argument.display()
            );
        }

        let mut found: Vec<PathBuf> = std::fs::read_dir(argument)
            .with_context(|| format!("reading GPX directory {}", argument.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("gpx"))
            })
            .collect();

        // An empty directory would otherwise surface much later as "the track
        // contains no points with timestamps", which names the wrong problem.
        if found.is_empty() {
            bail!("no .gpx files in {}", argument.display());
        }

        found.sort_unstable();
        tracks.extend(found);
    }

    Ok(tracks)
}

/// One raw to process, paired with the format that claims it.
///
/// The format travels with the path because a single run now spans all of them:
/// `read_strategy` and `capture_tags` differ per format, so the choice is made per
/// file rather than once for the whole run.
struct RawFile {
    path: PathBuf,
    format: RawFormat,
}

/// What the walk found.
struct Walk {
    files: Vec<RawFile>,
    /// Extensions no format claims, and how many files carried each. Reported
    /// rather than dropped: with no extension argument to reject, this is the only
    /// thing that tells someone their ARW files were never going to be read.
    ignored: BTreeMap<String, usize>,
    errors: Vec<String>,
}

/// Walk the tree, collecting every raw of every supported format.
///
/// Materializing into a `Vec` before parallelizing gives rayon contiguous slices
/// to split, which load-balances far better than bridging a sequential iterator.
/// It also yields an exact denominator for the progress bar.
fn collect_paths(dir: &Path) -> Result<Walk> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut files = Vec::new();
    let mut ignored: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = Vec::new();

    for entry in WalkDir::new(dir) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => match format_of(entry.path()) {
                Some(format) => files.push(RawFile {
                    path: entry.into_path(),
                    format,
                }),
                None => {
                    if let Some(extension) = ignorable_extension(entry.path()) {
                        *ignored.entry(extension).or_default() += 1;
                    }
                }
            },
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

    // Load-bearing twice, and neither reason is visible from here: it makes every
    // report independent of filesystem enumeration order, and it hands each rayon
    // worker a contiguous run of one directory, which is what lets a recursive run
    // parallelize its writes across NTFS directory locks. Deleting it breaks
    // nothing that fails loudly.
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(Walk {
        files,
        ignored,
        errors,
    })
}

/// The format that claims this file, if any.
///
/// Every format is tried, because a run is no longer scoped to one: a directory
/// holding both CR3 and NEF has both tagged in a single pass, each through its own
/// read strategy.
fn format_of(path: &Path) -> Option<RawFormat> {
    let extension = path.extension().and_then(|ext| ext.to_str())?;
    RawFormat::ALL
        .iter()
        .copied()
        .find(|format| format.matches_extension(extension))
}

/// The extension to count this unreadable file under, lowercased so `.ARW` and
/// `.arw` are one line in the report.
///
/// `.xmp` is excluded because those are this tool's own output — a re-run would
/// otherwise report its previous results back as ignored files — and `.gpx`
/// because tracks are its input: one often sits beside the photos it covers, and
/// "Ignored 1 .gpx" would read as the run disowning its own track. Extensionless
/// files are excluded because there is nothing useful to name them by.
fn ignorable_extension(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    (extension != "xmp" && extension != "gpx").then_some(extension)
}

fn progress_bar(len: usize, hidden: bool, message: &'static str) -> ProgressBar {
    if hidden {
        return ProgressBar::hidden();
    }

    let style = ProgressStyle::with_template("{msg:<22} [{bar:40}] {pos}/{len}")
        .expect("the progress template is a constant and is known to parse")
        .progress_chars("=> ");

    let bar = ProgressBar::new(len as u64);
    bar.set_style(style);
    bar.set_message(message);
    bar
}

struct Summary<'a> {
    /// Counts per format, in `RawFormat::ALL` order. Printed only when a run spans
    /// more than one — a single-format run has nothing to disambiguate, and the
    /// mixed case is the one worth making visible.
    by_format: &'a [(RawFormat, usize)],
    /// Extensions the walk passed over, and how many of each.
    ignored: &'a BTreeMap<String, usize>,
    /// Zones the capture times resolved through, by seconds east of UTC.
    offsets: &'a BTreeMap<i32, usize>,
    scanned: usize,
    tagged: usize,
    outside_track: usize,
    in_gap: usize,
    sidecar_exists: usize,
    no_capture_time: usize,
    failed: usize,
    elapsed: f64,
    threads: usize,
    dry_run: bool,
}

/// The timezone line, or `None` when there is nothing worth saying.
///
/// **Every camera here is meant to be on UTC**, so a non-zero offset is a slip
/// rather than a decision — see *Whose clock is it* in CLAUDE.md. Two shapes follow
/// from that, the same concern from opposite ends: **more than one zone in a run**,
/// meaning two bodies whose clocks disagree, and **a single zone that is not UTC**,
/// which displaces a whole shoot silently if the camera was set wrong. An all-UTC
/// run says nothing at all.
///
/// There is deliberately no flag to silence this. The line firing on every run of a
/// wrongly-set body is the point rather than a nuisance to gate; read CLAUDE.md
/// before adding one.
fn describe_offsets(offsets: &BTreeMap<i32, usize>) -> Option<String> {
    let all_utc = offsets.keys().all(|&seconds| seconds == 0);
    if offsets.len() < 2 && all_utc {
        return None;
    }

    let parts: Vec<String> = offsets
        .iter()
        .map(|(&seconds, files)| {
            let offset = FixedOffset::east_opt(seconds).expect("a resolved offset is in range");
            format!("{offset} ({} file{})", count(*files), plural(*files))
        })
        .collect();

    let tail = match offsets.len() {
        2 => "two clocks in one run",
        3.. => "several clocks in one run",
        // One zone, and it is not UTC — an all-UTC run returned above.
        _ => "cameras are normally on UTC",
    };
    Some(format!("{} — {tail}", parts.join(", ")))
}

/// The one place the ignored-files line is laid out. It is printed from two paths —
/// a run that found no raws at all, and a normal summary — and having each print
/// site format it itself is how they drifted into different column widths.
fn print_ignored(ignored: &BTreeMap<String, usize>) {
    if ignored.is_empty() {
        return;
    }
    println!(
        "Ignored  {:>7}   {}  (supported: {})",
        count(ignored.values().sum::<usize>()),
        describe_ignored(ignored),
        RawFormat::supported_extensions()
    );
}

/// Count the raws found, per format, in `RawFormat::ALL` order. Formats with no
/// files are omitted.
fn tally_formats(files: &[RawFile]) -> Vec<(RawFormat, usize)> {
    RawFormat::ALL
        .iter()
        .copied()
        .filter_map(|format| {
            let n = files.iter().filter(|f| f.format == format).count();
            (n > 0).then_some((format, n))
        })
        .collect()
}

/// `"   (500 cr3, 30 nef)"`, or empty for a single-format run.
///
/// Nothing is gained by telling someone their 40 CR3s were 40 CR3s. The breakdown
/// exists for the case a run spans formats, which is the one that might not have
/// been intended.
fn describe_formats(by_format: &[(RawFormat, usize)]) -> Option<String> {
    if by_format.len() < 2 {
        return None;
    }
    let parts: Vec<String> = by_format
        .iter()
        .map(|(format, n)| format!("{} {}", count(*n), format.extensions().join("/")))
        .collect();
    Some(parts.join(", "))
}

/// `".arw 418, .jpg 42"`, busiest first, and truncated so a messy directory cannot
/// push the summary off the screen.
fn describe_ignored(ignored: &BTreeMap<String, usize>) -> String {
    const SHOWN: usize = 3;

    let mut sorted: Vec<(&String, &usize)> = ignored.iter().collect();
    // Count descending, then extension, so the order is stable for equal counts.
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

    let mut parts: Vec<String> = sorted
        .iter()
        .take(SHOWN)
        .map(|(ext, n)| format!(".{ext} {}", count(**n)))
        .collect();
    if sorted.len() > SHOWN {
        parts.push(format!("+{} more", count(sorted.len() - SHOWN)));
    }
    parts.join(", ")
}

/// How many files were not tagged, and the breakdown by reason in report order.
///
/// Both come from one list on purpose. They used to be a five-term sum and five
/// separate `if` blocks, which is two places to remember a new outcome — and the
/// count going quietly wrong is not something any fixture would notice, since
/// they compare sidecars rather than the summary.
fn skip_breakdown(summary: &Summary) -> (usize, Vec<String>) {
    let categories = [
        (summary.outside_track, "outside track"),
        (summary.in_gap, "in track gap"),
        (summary.sidecar_exists, "existing sidecar"),
        (summary.no_capture_time, "no capture time"),
        (summary.failed, "errored"),
    ];

    let total: usize = categories.iter().map(|(files, _)| files).sum();
    let reasons = categories
        .iter()
        .filter(|(files, _)| *files > 0)
        .map(|(files, reason)| format!("{} {reason}", count(*files)))
        .collect();

    (total, reasons)
}

fn print_summary(summary: &Summary) {
    let (skipped, reasons) = skip_breakdown(summary);

    let rate = if summary.elapsed > 0.0 {
        summary.scanned as f64 / summary.elapsed
    } else {
        0.0
    };

    // Widened from 5 to 7 to keep the column aligned once separators are in: a
    // seven-figure count still fits, and nobody has that many raws in one tree.
    println!();
    let breakdown = describe_formats(summary.by_format)
        .map(|formats| format!("   ({formats})"))
        .unwrap_or_default();
    println!(
        "Scanned  {:>7} raw file{}{breakdown}",
        count(summary.scanned),
        plural(summary.scanned)
    );
    if let Some(zones) = describe_offsets(summary.offsets) {
        println!("Timezone {:>7}   {zones}", count(summary.offsets.len()));
    }
    print_ignored(summary.ignored);
    println!(
        "Tagged   {:>7}{}",
        count(summary.tagged),
        if summary.dry_run {
            "   (dry run — nothing written)"
        } else {
            ""
        }
    );
    if skipped > 0 {
        println!("Skipped  {:>7}   {}", count(skipped), reasons.join(", "));
    }
    // Align the whole seconds, not the whole number. `{:>7.1}` right-aligns "20.3"
    // as a unit, so the decimal point and tenths consume two columns of the field
    // and the units digit sits left of every integer above it. Splitting the tenths
    // off hangs them outside the column, and lets a run over 1,000 s pick up the
    // separators that a bare `{:.1}` cannot produce.
    let tenths = (summary.elapsed * 10.0).round() as i64;
    println!(
        "Elapsed  {:>7}.{}s  ({} files/sec, {} thread{})",
        thousands(tenths / 10),
        tenths % 10,
        thousands(rate.round() as i64),
        summary.threads,
        plural(summary.threads)
    );
}

/// `thousands` for the `usize` counts the summary deals in.
fn count(value: usize) -> String {
    thousands(value as i64)
}

/// The `s` in "3 threads", absent for exactly one — so a `-j 1` run does not
/// report "1 threads".
///
/// Only the summary's prose-shaped lines take this. The skip breakdown reads
/// "3 existing sidecar" and stays that way on purpose: those are category
/// labels, and a label does not agree with a number the way a sentence does.
fn plural(value: usize) -> &'static str {
    if value == 1 {
        ""
    } else {
        "s"
    }
}

/// Format an integer with US thousands separators: `3883` → `3,883`.
///
/// Written out rather than pulled from a crate: it is a dozen lines with no
/// locale surface, and a crate that did this properly would bring localization
/// machinery this program has no use for.
fn thousands(value: i64) -> String {
    let text = value.to_string();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.as_str()),
    };

    let mut out = String::with_capacity(sign.len() + digits.len() + digits.len() / 3);
    out.push_str(sign);

    for (i, digit) in digits.char_indices() {
        // A separator precedes any digit an exact multiple of three from the
        // right — except a leading one, which would render as ",123".
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }

    out
}

/// Parse `±HHMM`, also tolerating `±HH:MM`. Shares its implementation with the
/// EXIF-side offset parser so the two can never drift apart.
fn parse_utc_offset(text: &str) -> Result<FixedOffset, String> {
    raw::parse_offset(text)
        .ok_or_else(|| format!("expected an offset like -0700 or +0430, got {text:?}"))
}

#[cfg(test)]
mod tests {
    use crate::track::TrackPoint;

    use super::*;

    #[test]
    fn utc_offsets_parse_in_both_directions() {
        assert_eq!(
            parse_utc_offset("-0700"),
            Ok(FixedOffset::west_opt(7 * 3600).unwrap())
        );
        assert_eq!(
            parse_utc_offset("+0430"),
            Ok(FixedOffset::east_opt(4 * 3600 + 30 * 60).unwrap())
        );
        assert_eq!(
            parse_utc_offset("+00:00"),
            Ok(FixedOffset::east_opt(0).unwrap())
        );
    }

    #[test]
    fn malformed_utc_offsets_are_rejected() {
        for bad in ["0700", "-7", "-07000", "-0760", "+abcd", ""] {
            assert!(parse_utc_offset(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    fn extraction(path: &str, kind: ExtractionKind) -> Extraction {
        Extraction {
            path: PathBuf::from(path),
            kind,
        }
    }

    fn resolved() -> ExtractionKind {
        ExtractionKind::Resolved {
            captured: DateTime::from_timestamp(1000, 0).expect("a valid test instant"),
            offset: FixedOffset::east_opt(0).expect("UTC"),
            conflict_warning: None,
        }
    }

    #[test]
    fn the_gate_stays_shut_when_every_file_resolved() {
        // A missing capture time or a read failure is a per-file skip, not a
        // reason to refuse the whole run. Only a missing timezone is.
        let extracted = gate(vec![
            extraction("/photos/a.cr3", resolved()),
            extraction("/photos/b.cr3", ExtractionKind::NoCaptureTime),
            extraction(
                "/photos/c.cr3",
                ExtractionKind::Failed {
                    error: "unreadable".to_string(),
                },
            ),
        ])
        .expect("only a missing timezone may refuse the run");

        assert_eq!(extracted.photos.len(), 1);
        assert_eq!(extracted.no_capture_time, 1);
        assert_eq!(extracted.failed, 1);
        // Both skips are reported, and neither is silent.
        assert_eq!(extracted.warnings.len(), 2);
    }

    #[test]
    fn the_gate_reports_every_zoneless_file_in_sorted_order() {
        // Not `expect_err`: that needs `Extracted: Debug`, and deriving it would
        // mean a failure here dumps every photo of a real run.
        let needs_offset = match gate(vec![
            extraction("/photos/c.cr3", ExtractionKind::NeedsOffset),
            extraction("/photos/z.cr3", resolved()),
            extraction("/photos/a.cr3", ExtractionKind::NeedsOffset),
            extraction("/photos/b.cr3", ExtractionKind::NeedsOffset),
        ]) {
            Err(needs_offset) => needs_offset,
            Ok(_) => panic!("a file with no timezone must refuse the run"),
        };

        // Sorted regardless of the order Phase A happened to finish in, so the
        // report is identical at any --jobs.
        assert_eq!(
            needs_offset,
            vec![
                PathBuf::from("/photos/a.cr3"),
                PathBuf::from("/photos/b.cr3"),
                PathBuf::from("/photos/c.cr3"),
            ]
        );
    }

    /// A conflict is reported *and* the photo is still tagged — the EXIF offset
    /// wins, but silently discarding the disagreement would hide a wrong clock.
    #[test]
    fn an_offset_conflict_is_warned_about_without_dropping_the_photo() {
        let extracted = gate(vec![extraction(
            "/photos/a.cr3",
            ExtractionKind::Resolved {
                captured: DateTime::from_timestamp(1000, 0).expect("a valid test instant"),
                offset: FixedOffset::east_opt(0).expect("UTC"),
                conflict_warning: Some("EXIF timezone disagrees".to_string()),
            },
        )])
        .expect("a conflict is a warning, not a gate condition");

        assert_eq!(extracted.photos.len(), 1);
        assert_eq!(extracted.warnings.len(), 1);
    }

    #[test]
    fn numbers_are_grouped_in_threes_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(3_883), "3,883");
        assert_eq!(thousands(75_728), "75,728");
        assert_eq!(thousands(102_753), "102,753");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn grouping_boundaries_do_not_produce_a_leading_separator() {
        // The off-by-one to watch for: a count whose length is an exact multiple
        // of three must not render as ",100" or ",100,000".
        assert_eq!(thousands(100), "100");
        assert_eq!(thousands(100_000), "100,000");
    }

    #[test]
    fn only_one_of_something_drops_the_plural_s() {
        // Zero takes "s" — "0 raw files" is right, "0 raw file" is not.
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }

    #[test]
    fn negative_values_keep_the_sign_outside_the_grouping() {
        // Counts are never negative, but the gap description also formats
        // differences, so the sign must not be grouped as if it were a digit.
        assert_eq!(thousands(-1_000), "-1,000");
        assert_eq!(thousands(-999), "-999");
    }

    /// The CLI speaks seconds; `track.rs` speaks `TimeDelta`. This is the seam
    /// between them, and it is only checked here.
    ///
    /// Parses real argv rather than reading the constant back, so it exercises
    /// the clap attribute itself — replacing `default_value_t =
    /// GapLimits::DEFAULT_GAP_SECONDS` with a bare literal would still compile,
    /// still pass every other test, and quietly change which photos get tagged.
    #[test]
    fn the_cli_gap_default_matches_the_shipped_limit() {
        let args = Args::parse_from(["rawgeotag", "photos", "track.gpx"]);

        assert_eq!(
            TimeDelta::seconds(args.max_gap),
            GapLimits::DEFAULT.max_gap,
            "--max-gap default drifted from GapLimits::DEFAULT"
        );
        assert_eq!(
            args.max_distance,
            GapLimits::DEFAULT.max_meters,
            "--max-distance default drifted from GapLimits::DEFAULT"
        );
    }

    /// Same seam, same failure mode as the gap defaults above: a bare literal in
    /// the clap attribute would compile, pass everything else, and silently
    /// decouple the shipped default from the constant that carries the reasoning
    /// for it. Parsing real argv is what catches that.
    #[test]
    fn the_cli_jobs_default_matches_the_constant() {
        let args = Args::parse_from(["rawgeotag", "photos", "track.gpx"]);

        assert_eq!(
            args.jobs, DEFAULT_JOBS,
            "--jobs default drifted from DEFAULT_JOBS"
        );
    }

    // ---- write_sidecar: the branches that decide whether a file is touched ----
    //
    // `write_sidecar` explains the check order and why the two flags do not compose
    // as their names suggest. What these add is coverage: `verify-fixtures.ps1`
    // passes neither flag, so before these tests nothing exercised either one.

    /// A one-point track at a known instant, so `lookup` resolves exactly.
    fn one_point_track() -> Track {
        Track::new(vec![TrackPoint {
            at: DateTime::from_timestamp(1000, 0).expect("a valid test instant"),
            lat: 47.0,
            lon: -122.0,
            ele: None,
            segment: 0,
        }])
        .expect("a single-point track is valid")
    }

    fn photo_at(dir: &Path, name: &str, seconds: i64) -> Photo {
        Photo {
            path: dir.join(name),
            captured: DateTime::from_timestamp(seconds, 0).expect("a valid test instant"),
        }
    }

    fn settings(force: bool, dry_run: bool) -> WriteSettings {
        WriteSettings {
            limits: GapLimits::DEFAULT,
            force,
            dry_run,
        }
    }

    #[test]
    fn an_existing_sidecar_is_skipped_and_left_untouched_without_force() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let photo = photo_at(dir.path(), "IMG_0001.CR3", 1000);
        let sidecar = dir.path().join("IMG_0001.xmp");
        std::fs::write(&sidecar, "someone else's sidecar").expect("seeding the sidecar");

        let kind = write_sidecar(&photo, &one_point_track(), settings(false, false));

        assert!(matches!(kind, WrittenKind::SidecarExists), "{kind:?}");
        // The point of the whole rule: the bytes that were there are still there.
        assert_eq!(
            std::fs::read_to_string(&sidecar).unwrap(),
            "someone else's sidecar"
        );
    }

    #[test]
    fn force_overwrites_an_existing_sidecar() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let photo = photo_at(dir.path(), "IMG_0002.CR3", 1000);
        let sidecar = dir.path().join("IMG_0002.xmp");
        std::fs::write(&sidecar, "someone else's sidecar").expect("seeding the sidecar");

        let kind = write_sidecar(&photo, &one_point_track(), settings(true, false));

        assert!(matches!(kind, WrittenKind::Tagged { .. }), "{kind:?}");
        assert!(std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains("exif:GPSLatitude"));
    }

    #[test]
    fn dry_run_reports_a_tag_but_creates_no_file() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let photo = photo_at(dir.path(), "IMG_0003.CR3", 1000);

        let kind = write_sidecar(&photo, &one_point_track(), settings(false, true));

        assert!(matches!(kind, WrittenKind::Tagged { .. }), "{kind:?}");
        assert!(!dir.path().join("IMG_0003.xmp").exists());
    }

    /// `--dry-run --force` must still write nothing. `force` only gets past the
    /// skip-existing check; `dry_run` returns before the write either way.
    #[test]
    fn dry_run_wins_over_force_on_an_existing_sidecar() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let photo = photo_at(dir.path(), "IMG_0004.CR3", 1000);
        let sidecar = dir.path().join("IMG_0004.xmp");
        std::fs::write(&sidecar, "untouched").expect("seeding the sidecar");

        let kind = write_sidecar(&photo, &one_point_track(), settings(true, true));

        assert!(matches!(kind, WrittenKind::Tagged { .. }), "{kind:?}");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "untouched");
    }

    /// A photo the track cannot place must leave nothing behind — no empty
    /// sidecar, no temp file. This is the accuracy mantra at the filesystem.
    #[test]
    fn a_photo_the_track_cannot_place_writes_nothing() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let track = one_point_track();

        let outside = photo_at(dir.path(), "IMG_0005.CR3", 9999);
        let kind = write_sidecar(&outside, &track, settings(false, false));
        assert!(matches!(kind, WrittenKind::OutsideTrack), "{kind:?}");

        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "an unplaceable photo must leave the directory empty"
        );
    }

    #[test]
    fn a_photo_in_a_gap_writes_nothing_and_the_gap_is_described() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        // Two points 5,000 s and ~111 km apart, in different recording runs.
        let track = Track::new(vec![
            TrackPoint {
                at: DateTime::from_timestamp(1000, 0).unwrap(),
                lat: 47.0,
                lon: -122.0,
                ele: None,
                segment: 0,
            },
            TrackPoint {
                at: DateTime::from_timestamp(6000, 0).unwrap(),
                lat: 48.0,
                lon: -122.0,
                ele: None,
                segment: 1,
            },
        ])
        .expect("a two-point track is valid");

        let photo = photo_at(dir.path(), "IMG_0006.CR3", 3000);
        let kind = write_sidecar(&photo, &track, settings(false, false));

        match kind {
            WrittenKind::InGap { description } => {
                // Separators on both numbers, and the segment break called out.
                assert!(description.contains("5,000s"), "{description}");
                assert!(description.contains("111,195 m"), "{description}");
                assert!(
                    description.contains("(different recording runs)"),
                    "{description}"
                );
            }
            other => panic!("expected InGap, got {other:?}"),
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// A swap here compiles and silently trades one destructive flag for the
    /// other, so the mapping is worth pinning rather than assuming.
    #[test]
    fn write_settings_carry_the_flags_they_were_given() {
        let args = Args::parse_from(["rawgeotag", "--force", "photos", "track.gpx"]);
        let forced = WriteSettings::from_args(&args);
        assert!(forced.force && !forced.dry_run);

        let args = Args::parse_from(["rawgeotag", "--dry-run", "photos", "track.gpx"]);
        let dry = WriteSettings::from_args(&args);
        assert!(dry.dry_run && !dry.force);

        let args = Args::parse_from(["rawgeotag", "photos", "track.gpx"]);
        let plain = WriteSettings::from_args(&args);
        assert!(!plain.force && !plain.dry_run);
        assert_eq!(plain.limits.max_gap, GapLimits::DEFAULT.max_gap);
    }

    // ---- the timezone note ---------------------------------------------------
    //
    // `describe_offsets` explains which shapes are worth naming and why there is no
    // flag to silence them. These pin the corners: silent, a single non-UTC zone, a
    // two-clock mix, a mix with no UTC in it at all, and a mix of more than two.

    fn offsets_of(pairs: &[(i32, usize)]) -> BTreeMap<i32, usize> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn an_all_utc_run_says_nothing_about_timezones() {
        assert_eq!(describe_offsets(&offsets_of(&[(0, 40)])), None);
        assert_eq!(describe_offsets(&offsets_of(&[])), None);
    }

    /// The Rockies body sat on `+01:00`. A whole shoot an hour out still tags,
    /// because the shifted times land inside the track — so nothing else in the
    /// run would say a word about it.
    #[test]
    fn a_single_non_utc_offset_is_still_worth_saying() {
        let note = describe_offsets(&offsets_of(&[(3600, 30)])).expect("non-UTC is reportable");
        assert!(note.contains("+01:00 (30 files)"), "{note}");
        assert!(note.contains("normally on UTC"), "{note}");
    }

    #[test]
    fn two_clocks_in_one_run_are_reported_together() {
        let note =
            describe_offsets(&offsets_of(&[(0, 2), (3600, 2)])).expect("a mix is reportable");
        // Ascending by offset, so the line reads the same at any --jobs.
        assert!(
            note.contains("+00:00 (2 files), +01:00 (2 files)"),
            "{note}"
        );
        assert!(note.contains("two clocks"), "{note}");
    }

    /// A mix that happens to include no UTC at all is still a mix. Also the line
    /// where a single-file zone shows, so it is the one that catches "(1 files)".
    #[test]
    fn two_non_utc_clocks_are_reported_as_a_mix() {
        let note = describe_offsets(&offsets_of(&[(3600, 1), (-25200, 1)])).expect("a mix");
        assert!(note.contains("-07:00 (1 file), +01:00 (1 file)"), "{note}");
        assert!(note.contains("two clocks"), "{note}");
    }

    /// "Two clocks in one run" is a count, not a synonym for "a mix" — three
    /// bodies on three clocks must not be reported as two.
    #[test]
    fn more_than_two_clocks_are_reported_as_several() {
        let note = describe_offsets(&offsets_of(&[(0, 4), (3600, 2), (-25200, 1)]))
            .expect("a three-zone mix is reportable");
        assert!(note.contains("several clocks in one run"), "{note}");
        assert!(!note.contains("two clocks"), "{note}");
    }

    // ---- the summary's arithmetic --------------------------------------------

    fn summary_of(
        outside: usize,
        gap: usize,
        exists: usize,
        no_time: usize,
        failed: usize,
    ) -> Summary<'static> {
        const NO_FORMATS: &[(RawFormat, usize)] = &[];
        static NOTHING_IGNORED: std::sync::LazyLock<BTreeMap<String, usize>> =
            std::sync::LazyLock::new(BTreeMap::new);
        static NO_OFFSETS: std::sync::LazyLock<BTreeMap<i32, usize>> =
            std::sync::LazyLock::new(BTreeMap::new);

        Summary {
            by_format: NO_FORMATS,
            ignored: &NOTHING_IGNORED,
            offsets: &NO_OFFSETS,
            scanned: 100,
            tagged: 7,
            outside_track: outside,
            in_gap: gap,
            sidecar_exists: exists,
            no_capture_time: no_time,
            failed,
            elapsed: 1.0,
            threads: 2,
            dry_run: false,
        }
    }

    /// Distinct powers of two, so an outcome dropped from the total is identifiable
    /// from the total alone rather than just "wrong".
    #[test]
    fn every_skip_category_is_both_counted_and_named() {
        let (skipped, reasons) = skip_breakdown(&summary_of(1, 2, 4, 8, 16));

        assert_eq!(skipped, 31);
        assert_eq!(
            reasons,
            [
                "1 outside track",
                "2 in track gap",
                "4 existing sidecar",
                "8 no capture time",
                "16 errored",
            ]
        );
    }

    #[test]
    fn skip_categories_with_no_files_are_not_named() {
        let (skipped, reasons) = skip_breakdown(&summary_of(0, 3, 0, 0, 0));

        assert_eq!(skipped, 3);
        assert_eq!(reasons, ["3 in track gap"]);
    }

    /// The summary is the only place a reader learns a file was skipped, so the
    /// counts have to carry separators like every other user-facing number.
    #[test]
    fn skip_counts_carry_thousands_separators() {
        let (_, reasons) = skip_breakdown(&summary_of(0, 1_489, 0, 0, 0));
        assert_eq!(reasons, ["1,489 in track gap"]);
    }

    // ---- resolving the GPX arguments -----------------------------------------
    //
    // A trip's tracks live in one folder and the documented workflow is to pass all
    // of them at every photo folder, so a directory argument saves enumerating four
    // or seven paths. It also fits "GPX filenames lie" better than hand-picking:
    // selecting by name is exactly what that warning says not to trust.

    fn gpx_dir(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        for name in names {
            std::fs::write(dir.path().join(name), "x").expect("creating a test file");
        }
        dir
    }

    fn names_of(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_gpx_file_argument_is_passed_through_untouched() {
        let dir = gpx_dir(&["one.gpx", "two.gpx"]);
        let one = dir.path().join("one.gpx");

        assert_eq!(
            collect_tracks(std::slice::from_ref(&one)).expect("a real file"),
            vec![one]
        );
    }

    /// **Sorted, and that is load-bearing.** Argument order decides which of several
    /// unreadable files is reported and the order of names in the overlap error, so
    /// an expansion that varied with filesystem enumeration would make those
    /// messages vary run to run.
    #[test]
    fn a_directory_expands_to_every_gpx_in_it_sorted() {
        let dir = gpx_dir(&[
            "c-third.gpx",
            "a-first.gpx",
            "b-second.GPX",
            "notes.txt",
            "photo.cr3",
        ]);

        let tracks = collect_tracks(&[dir.path().to_path_buf()]).expect("the directory has tracks");

        // Case-insensitive on the extension, but the *order* is byte-wise, and
        // only non-GPX files are dropped.
        assert_eq!(
            names_of(&tracks),
            ["a-first.gpx", "b-second.GPX", "c-third.gpx"]
        );
    }

    /// The pre-existing form, kept working: several files named explicitly stay in
    /// the order they were named, with no sorting applied across arguments.
    #[test]
    fn several_gpx_files_stay_in_the_order_they_were_named() {
        let dir = gpx_dir(&["a.gpx", "b.gpx", "c.gpx"]);
        let named = ["c.gpx", "a.gpx", "b.gpx"].map(|n| dir.path().join(n));

        let tracks = collect_tracks(&named).expect("three real files");

        assert_eq!(names_of(&tracks), ["c.gpx", "a.gpx", "b.gpx"]);
    }

    /// Mirrors the multi-file case, and pins something that one cannot: the two
    /// directories hold names that **interleave alphabetically**, so a global sort
    /// would produce `a, b, c, d`. Getting `b, d, a, c` is the proof that sorting
    /// happens *within* each directory and argument order decides the rest.
    #[test]
    fn several_directories_are_each_sorted_but_kept_in_argument_order() {
        let first = gpx_dir(&["d.gpx", "b.gpx"]);
        let second = gpx_dir(&["c.gpx", "a.gpx"]);

        let tracks = collect_tracks(&[first.path().to_path_buf(), second.path().to_path_buf()])
            .expect("both directories have tracks");

        assert_eq!(names_of(&tracks), ["b.gpx", "d.gpx", "a.gpx", "c.gpx"]);
    }

    #[test]
    fn files_and_directories_can_be_mixed_and_keep_argument_order() {
        let trip = gpx_dir(&["b.gpx", "a.gpx"]);
        let extra = gpx_dir(&["z-extra.gpx"]);
        let loose = extra.path().join("z-extra.gpx");

        let tracks = collect_tracks(&[loose.clone(), trip.path().to_path_buf()])
            .expect("both arguments resolve");

        // Sorted *within* a directory; argument order preserved *between* arguments,
        // so the loose file stays first despite sorting last by name.
        assert_eq!(names_of(&tracks), ["z-extra.gpx", "a.gpx", "b.gpx"]);
    }

    /// Otherwise this surfaces much later as "the track contains no points with
    /// timestamps", which names the wrong problem entirely.
    #[test]
    fn a_directory_with_no_gpx_is_an_error_that_says_so() {
        let dir = gpx_dir(&["notes.txt", "IMG_0001.CR3"]);

        let error = collect_tracks(&[dir.path().to_path_buf()]).expect_err("no tracks in there");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("no .gpx files"), "{rendered}");
    }

    #[test]
    fn a_path_that_is_neither_file_nor_directory_is_rejected() {
        let dir = gpx_dir(&[]);
        let missing = dir.path().join("nope.gpx");

        assert!(collect_tracks(&[missing]).is_err());
    }

    // ---- the walk: which formats a run picks up ------------------------------
    //
    // There is no extension argument any more, so the walk decides what a run
    // touches. The three cases that matter are none, one and several — the middle
    // one because it must behave exactly as the old single-format run did, and the
    // last because it is the new capability.

    fn walk_over(names: &[&str]) -> (tempfile::TempDir, Walk) {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        for name in names {
            std::fs::write(dir.path().join(name), "x").expect("creating a test file");
        }
        let walk = collect_paths(dir.path()).expect("the directory exists");
        (dir, walk)
    }

    fn found(walk: &Walk) -> Vec<String> {
        walk.files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_directory_with_no_supported_raws_finds_nothing_and_says_what_it_passed_over() {
        let (_dir, walk) = walk_over(&["a.arw", "b.ARW", "c.dng", "notes.txt"]);

        assert!(walk.files.is_empty());
        // Case-folded, so .arw and .ARW are one line rather than two.
        assert_eq!(walk.ignored.get("arw"), Some(&2));
        assert_eq!(walk.ignored.get("dng"), Some(&1));
        assert_eq!(walk.ignored.get("txt"), Some(&1));
    }

    /// **Every format alone, not just the first one.** A CR3-only directory and a
    /// NEF-only directory are different cases: they take different `read_strategy`
    /// paths, and NEF is the one whose body records no timezone, so a run of each
    /// is what the removed extension argument used to guarantee.
    ///
    /// # Adding a format
    ///
    /// This test needs no edit — it is driven off `RawFormat::ALL`, so a new variant
    /// gets its own single-format walk case automatically. **Three things do not
    /// follow automatically, and a format is not supported until all three exist:**
    ///
    /// 1. A row in `read_strategies_are_the_ones_verified_against_real_files`
    ///    (`format.rs`). That test is hand-written per format on purpose — the
    ///    compiler cannot tell a `Streaming`/`WholeFile` choice is wrong, and the
    ///    wrong one fails every file of that format at runtime.
    /// 2. A fixture of its own, and an aggregate in `verify-fixtures.ps1`. NEF
    ///    failed in a way no unit test and no crate documentation revealed; see
    ///    `docs/FIXTURES.md`.
    /// 3. A capture-tag pairing verified against a *real file of that format*, not
    ///    assumed from the spec. See the CR3 timezone trap in `CLAUDE.md`.
    ///
    /// The support matrix is `RawFormat::ALL` plus those three; this test only
    /// covers the first column of it.
    #[test]
    fn each_format_alone_behaves_as_the_old_extension_argument_did() {
        for format in RawFormat::ALL {
            let ext = format.extensions()[0];
            let names = [
                format!("IMG_0002.{}", ext.to_uppercase()),
                format!("IMG_0001.{ext}"),
                "notes.txt".to_string(),
            ];
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let (_dir, walk) = walk_over(&refs);

            assert_eq!(walk.files.len(), 2, "{format:?}");
            assert!(walk.files.iter().all(|f| f.format == *format), "{format:?}");
            assert_eq!(
                found(&walk),
                [
                    format!("IMG_0001.{ext}"),
                    format!("IMG_0002.{}", ext.to_uppercase())
                ],
                "{format:?}"
            );
            // A single-format run prints no breakdown: telling someone their two
            // CR3s were two CR3s is noise.
            assert_eq!(
                describe_formats(&tally_formats(&walk.files)),
                None,
                "{format:?}"
            );
            assert_eq!(walk.ignored.get("txt"), Some(&1), "{format:?}");
        }
    }

    #[test]
    fn a_mixed_directory_picks_up_every_supported_format_in_one_pass() {
        let (_dir, walk) = walk_over(&["DSC_0001.NEF", "IMG_0001.CR3", "IMG_0002.cr3", "x.arw"]);

        assert_eq!(walk.files.len(), 3);
        assert_eq!(
            tally_formats(&walk.files),
            [(RawFormat::Cr3, 2), (RawFormat::Nef, 1)]
        );
        // The mix is the case worth surfacing, so this one does get a breakdown.
        // Content only — the parentheses and column belong to print_summary.
        assert_eq!(
            describe_formats(&tally_formats(&walk.files)).as_deref(),
            Some("2 cr3, 1 nef")
        );
        assert_eq!(walk.ignored.get("arw"), Some(&1));
    }

    /// The tool's own file types must not be reported back as something it failed
    /// to read — a re-run would accuse itself of ignoring the sidecars it wrote,
    /// and a track staged beside its photos would be disowned as "Ignored 1 .gpx".
    #[test]
    fn our_own_sidecars_and_tracks_are_not_counted_as_ignored() {
        let (_dir, walk) = walk_over(&[
            "IMG_0001.CR3",
            "IMG_0001.xmp",
            "IMG_0002.XMP",
            "track.gpx",
            "TRACK2.GPX",
        ]);

        assert_eq!(walk.files.len(), 1);
        assert!(walk.ignored.is_empty(), "{:?}", walk.ignored);
    }

    #[test]
    fn ignored_extensions_are_reported_busiest_first_and_truncated() {
        let counts = BTreeMap::from([
            ("arw".to_string(), 418),
            ("jpg".to_string(), 42),
            ("dng".to_string(), 7),
            ("tif".to_string(), 3),
            ("psd".to_string(), 1),
        ]);
        assert_eq!(
            describe_ignored(&counts),
            ".arw 418, .jpg 42, .dng 7, +2 more"
        );
    }

    #[test]
    fn collect_paths_rejects_something_that_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let file = dir.path().join("not-a-dir.cr3");
        std::fs::write(&file, "x").expect("creating a test file");

        assert!(collect_paths(&file).is_err());
    }

    /// The sort is load-bearing twice over: it makes output order independent of
    /// filesystem enumeration order, and it hands each rayon worker a contiguous
    /// run of one directory. A `sed` once removed it silently and only the
    /// determinism check noticed — see the determinism log in `docs/TESTING.md`.
    #[test]
    fn collect_paths_finds_matching_files_recursively_and_sorts_them() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("creating a subdirectory");

        // The two names in the root differ only in case, deliberately. NTFS
        // enumerates case-insensitively, so `WalkDir` yields `img_0001` first,
        // while `PathBuf`'s `Ord` is byte-wise and puts `I` (0x49) before `i`
        // (0x69). Only the sort can produce the order asserted below — with plain
        // alphabetical names the walk already arrives sorted and the assertion
        // would hold whether or not `collect_paths` sorted anything.
        for (at, name) in [
            (dir.path(), "img_0001.cr3"),
            (dir.path(), "IMG_0002.CR3"),
            (dir.path(), "notes.txt"),
            (nested.as_path(), "IMG_0003.CR3"),
        ] {
            std::fs::write(at.join(name), "x").expect("creating a test file");
        }

        let walk = collect_paths(dir.path()).expect("the dir exists");

        assert!(walk.errors.is_empty(), "{:?}", walk.errors);
        let relative: Vec<String> = walk
            .files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            })
            .collect();
        assert_eq!(
            relative,
            ["IMG_0002.CR3", "img_0001.cr3", "nested/IMG_0003.CR3"]
        );
    }

    fn written(path: &str, kind: WrittenKind) -> Written {
        Written {
            path: PathBuf::from(path),
            kind,
        }
    }

    #[test]
    fn every_write_outcome_lands_in_exactly_one_counter() {
        let tally = tally_writes(
            vec![
                written(
                    "/photos/a.cr3",
                    WrittenKind::Tagged {
                        fix: Fix {
                            lat: 47.0,
                            lon: -122.0,
                            ele: None,
                        },
                    },
                ),
                written("/photos/b.cr3", WrittenKind::OutsideTrack),
                written(
                    "/photos/c.cr3",
                    WrittenKind::InGap {
                        description: "falls in a track gap".to_string(),
                    },
                ),
                written("/photos/d.cr3", WrittenKind::SidecarExists),
                written(
                    "/photos/e.cr3",
                    WrittenKind::Failed {
                        error: "disk full".to_string(),
                    },
                ),
            ],
            false,
        );

        assert_eq!(tally.tagged, 1);
        assert_eq!(tally.outside_track, 1);
        assert_eq!(tally.in_gap, 1);
        assert_eq!(tally.sidecar_exists, 1);
        assert_eq!(tally.failed, 1);
        // Every skip is explained; the tagged file is not a warning.
        assert_eq!(tally.warnings.len(), 4);
    }

    /// Positions are collected only under `--verbose` — the run is otherwise
    /// holding a string per tagged photo for output nobody asked for.
    #[test]
    fn per_file_positions_are_collected_only_when_verbose() {
        let tagged = || {
            vec![written(
                "/photos/a.cr3",
                WrittenKind::Tagged {
                    fix: Fix {
                        lat: 47.0,
                        lon: -122.0,
                        ele: None,
                    },
                },
            )]
        };

        assert!(tally_writes(tagged(), false).details.is_empty());
        assert_eq!(tally_writes(tagged(), true).details.len(), 1);
    }
}
