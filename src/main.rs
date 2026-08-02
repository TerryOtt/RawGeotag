//! Geotag camera raw files from one or more GPX tracks by writing XMP sidecars.
//!
//! Raw files are never modified; the whole operation is undone by deleting the
//! `.xmp` files.
//!
//! Several GPX files may be given for one directory, since a day's shooting is
//! often split across separate recordings. `track.rs` documents what keeps that
//! merge honest.
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
/// Counter-intuitively this is far below the core count. On local storage the
/// EXIF read is nearly free (~0.3 s for 3,883 CR3s) and the run is dominated by
/// *creating* sidecars, which are NTFS directory-metadata operations that NTFS
/// serializes within a directory. Extra threads there only add contention, so
/// throughput peaks at 2 and degrades above it — measured both warm and cold.
///
/// High-latency storage inverts this completely: over SMB the read dominates
/// and parallelizes ~12x, so `-j 16` or more is the right call there. That case
/// is rare enough to be worth a flag rather than a worse default.
const DEFAULT_JOBS: usize = 2;

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
                  is written — run those as separate passes instead."
)]
struct Args {
    /// Parent directory, searched recursively
    dir: PathBuf,

    /// Raw extension: "cr3" or "nef" (case-insensitive, leading "." tolerated)
    ext: String,

    /// Path to the GPX track file. Repeat for a day split across several tracks
    #[arg(required = true, num_args = 1..)]
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

    /// Do all the work, write nothing
    #[arg(long)]
    dry_run: bool,

    /// Worker threads. Tuned for local SSD; raise it for network storage
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

    let format = RawFormat::from_extension(&args.ext).with_context(|| {
        format!(
            "unsupported raw extension {:?}; supported: {}",
            args.ext,
            RawFormat::supported_extensions()
        )
    })?;

    if args.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.jobs)
        .build_global()
        .context("configuring the worker thread pool")?;

    let started = Instant::now();

    let track = Track::load(&args.gpx)?;
    if args.verbose {
        let (start, end) = track.span();
        // The span is the union of every file given. It is not a claim of
        // continuous coverage — holes between the tracks are still holes, and a
        // photo falling in one is skipped like any other gap.
        println!(
            "Track: {} points from {} file(s), {} to {}",
            count(track.point_count()),
            args.gpx.len(),
            format_utc(start),
            format_utc(end)
        );
    }

    // Named from the format rather than from whatever the user typed, because the
    // walk matches every extension the format declares, not just that one.
    let extensions = format.extensions().join(", ");
    let (paths, walk_errors) = collect_paths(&args.dir, format)?;
    if paths.is_empty() {
        println!("Scanned 0 .{extensions} files under {}", args.dir.display());
        return Ok(Outcome::Clean);
    }
    let scanned = paths.len();

    // ---- Phase A: extract capture times -------------------------------------
    let progress = progress_bar(paths.len(), args.no_progress, "reading capture times");
    let extractions: Vec<Extraction> = paths
        .par_iter()
        .map_init(MediaParser::new, |parser, path| {
            let extraction = extract(parser, path, format, args.utc_offset);
            progress.inc(1);
            extraction
        })
        .collect();
    progress.finish_and_clear();

    // ---- Gate ---------------------------------------------------------------
    let Extracted {
        photos,
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

    for error in &walk_errors {
        eprintln!("warning: {error}");
    }

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
        extension: &extensions,
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
        Ok(Capture::Resolved { at, conflict }) => ExtractionKind::Resolved {
            captured: at,
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
                conflict_warning,
            } => {
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

/// Walk the tree, collecting matching files.
///
/// Materializing into a `Vec` before parallelizing gives rayon contiguous slices
/// to split, which load-balances far better than bridging a sequential iterator.
/// It also yields an exact denominator for the progress bar.
fn collect_paths(dir: &Path, format: RawFormat) -> Result<(Vec<PathBuf>, Vec<String>)> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut paths = Vec::new();
    let mut errors = Vec::new();

    for entry in WalkDir::new(dir) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                if matches_format(entry.path(), format) {
                    paths.push(entry.into_path());
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

    // Load-bearing twice, and neither reason is visible from here: it makes every
    // report independent of filesystem enumeration order, and it hands each rayon
    // worker a contiguous run of one directory, which is what lets a recursive run
    // parallelize its writes across NTFS directory locks. Deleting it breaks
    // nothing that fails loudly.
    paths.sort_unstable();
    Ok((paths, errors))
}

/// Filtered against the format's own extension table rather than against the
/// string the user typed, so a format declaring more than one extension finds
/// files under all of them.
fn matches_format(path: &Path, format: RawFormat) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| format.matches_extension(ext))
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
    extension: &'a str,
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
    println!(
        "Scanned  {:>7} .{} files",
        count(summary.scanned),
        summary.extension
    );
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
    println!(
        "Elapsed  {:>7.1}s  ({} files/sec, {} threads)",
        summary.elapsed,
        thousands(rate.round() as i64),
        summary.threads
    );
}

/// `thousands` for the `usize` counts the summary deals in.
fn count(value: usize) -> String {
    thousands(value as i64)
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
        let args = Args::parse_from(["rawgeotag", "photos", "cr3", "track.gpx"]);

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

    #[test]
    fn extension_matching_ignores_case() {
        assert!(matches_format(
            Path::new("/photos/IMG_1234.CR3"),
            RawFormat::Cr3
        ));
        assert!(matches_format(
            Path::new("/photos/IMG_1234.cr3"),
            RawFormat::Cr3
        ));
        assert!(!matches_format(
            Path::new("/photos/IMG_1234.jpg"),
            RawFormat::Cr3
        ));
        assert!(!matches_format(
            Path::new("/photos/IMG_1234"),
            RawFormat::Cr3
        ));
    }

    /// One format's files must not be picked up by a run for another, which is
    /// the property the walk gets from filtering on the format's own table.
    #[test]
    fn a_run_for_one_format_does_not_collect_another() {
        assert!(!matches_format(
            Path::new("/photos/DSC_0001.NEF"),
            RawFormat::Cr3
        ));
        assert!(matches_format(
            Path::new("/photos/DSC_0001.NEF"),
            RawFormat::Nef
        ));
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
        let args = Args::parse_from(["rawgeotag", "--force", "photos", "cr3", "track.gpx"]);
        let forced = WriteSettings::from_args(&args);
        assert!(forced.force && !forced.dry_run);

        let args = Args::parse_from(["rawgeotag", "--dry-run", "photos", "cr3", "track.gpx"]);
        let dry = WriteSettings::from_args(&args);
        assert!(dry.dry_run && !dry.force);

        let args = Args::parse_from(["rawgeotag", "photos", "cr3", "track.gpx"]);
        let plain = WriteSettings::from_args(&args);
        assert!(!plain.force && !plain.dry_run);
        assert_eq!(plain.limits.max_gap, GapLimits::DEFAULT.max_gap);
    }

    // ---- the summary's arithmetic --------------------------------------------

    fn summary_of(
        outside: usize,
        gap: usize,
        exists: usize,
        no_time: usize,
        failed: usize,
    ) -> Summary<'static> {
        Summary {
            extension: "cr3",
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

    // ---- collect_paths -------------------------------------------------------

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
            (dir.path(), "DSC_0001.NEF"),
            (nested.as_path(), "IMG_0003.CR3"),
        ] {
            std::fs::write(at.join(name), "x").expect("creating a test file");
        }

        let (paths, errors) = collect_paths(dir.path(), RawFormat::Cr3).expect("the dir exists");

        assert!(errors.is_empty(), "{errors:?}");
        let relative: Vec<String> = paths
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        // `notes.txt` and the NEF are excluded, the nested CR3 is found, and the
        // uppercase name sorts ahead of the lowercase one.
        assert_eq!(
            relative,
            ["IMG_0002.CR3", "img_0001.cr3", "nested/IMG_0003.CR3"]
        );
    }

    #[test]
    fn collect_paths_rejects_something_that_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("creating the scratch directory");
        let file = dir.path().join("not-a-dir.cr3");
        std::fs::write(&file, "x").expect("creating a test file");

        assert!(collect_paths(&file, RawFormat::Cr3).is_err());
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
