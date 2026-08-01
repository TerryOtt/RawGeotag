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
use crate::track::{Fix, GapLimits, Lookup, Track};

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
    let wanted_ext = args.ext.trim_start_matches('.').to_ascii_lowercase();

    if args.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.jobs)
        .build_global()
        .context("configuring the worker thread pool")?;

    let started = Instant::now();

    let track = Track::load(&args.gpx)?;
    let (track_start, track_end) = track.span();
    if args.verbose {
        // The span is the union of every file given. It is not a claim of
        // continuous coverage — holes between the tracks are still holes, and a
        // photo falling in one is skipped like any other gap.
        println!(
            "Track: {} points from {} file(s), {} to {}",
            count(track.point_count()),
            args.gpx.len(),
            format_utc(track_start),
            format_utc(track_end)
        );
    }

    let (paths, walk_errors) = collect_paths(&args.dir, &wanted_ext)?;
    if paths.is_empty() {
        println!("Scanned 0 .{wanted_ext} files under {}", args.dir.display());
        return Ok(Outcome::Clean);
    }

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
    let needs_offset = files_needing_offset(&extractions);

    if !needs_offset.is_empty() {
        eprintln!(
            "error: {} file(s) have a capture time with no timezone, and no --utc-offset was given:",
            count(needs_offset.len())
        );
        for path in &needs_offset {
            eprintln!("  {}", path.display());
        }
        eprintln!("\nRe-run with --utc-offset <±HHMM>. No sidecars were written.");
        return Ok(Outcome::HadFailures);
    }

    // ---- Phase B: interpolate and write -------------------------------------
    let mut photos = Vec::new();
    let mut warnings = Vec::new();
    let mut no_capture_time = 0usize;
    let mut failed = 0usize;

    for extraction in extractions {
        match extraction {
            Extraction::Resolved {
                path,
                captured,
                conflict_warning,
            } => {
                if let Some(warning) = conflict_warning {
                    warnings.push((path.clone(), warning));
                }
                photos.push(Photo { path, captured });
            }
            Extraction::NoCaptureTime { path } => {
                no_capture_time += 1;
                warnings.push((path, "no capture time in EXIF".to_string()));
            }
            Extraction::Failed { path, error } => {
                failed += 1;
                warnings.push((path, error));
            }
            // Ruled out by the gate above.
            Extraction::NeedsOffset { .. } => unreachable!("the gate returns early"),
        }
    }

    let scanned = paths.len();
    let progress = progress_bar(photos.len(), args.no_progress, "writing sidecars");
    let results: Vec<Written> = photos
        .into_par_iter()
        .map(|photo| {
            let written = write_one(&photo, &track, &args);
            progress.inc(1);
            written
        })
        .collect();
    progress.finish_and_clear();

    // ---- Report -------------------------------------------------------------
    let mut tagged = 0usize;
    let mut outside_track = 0usize;
    let mut in_gap = 0usize;
    let mut sidecar_exists = 0usize;
    let mut details = Vec::new();

    for written in results {
        match written.kind {
            WrittenKind::Tagged { fix } => {
                tagged += 1;
                if args.verbose {
                    details.push((written.path, format!("{:.6}, {:.6}", fix.lat, fix.lon)));
                }
            }
            WrittenKind::OutsideTrack => {
                outside_track += 1;
                warnings.push((
                    written.path,
                    "capture time is outside the track".to_string(),
                ));
            }
            WrittenKind::InGap { description } => {
                in_gap += 1;
                warnings.push((written.path, description));
            }
            WrittenKind::SidecarExists => {
                sidecar_exists += 1;
                warnings.push((
                    written.path,
                    "sidecar already exists (use --force to overwrite)".to_string(),
                ));
            }
            WrittenKind::Failed { error } => {
                failed += 1;
                warnings.push((written.path, error));
            }
        }
    }

    for error in &walk_errors {
        eprintln!("warning: {error}");
    }

    details.sort_unstable();
    for (path, detail) in details {
        println!("{}  {}", path.display(), detail);
    }

    warnings.sort_unstable();
    for (path, warning) in &warnings {
        eprintln!("warning: {}: {warning}", path.display());
    }

    print_summary(&Summary {
        extension: &wanted_ext,
        scanned,
        tagged,
        outside_track,
        in_gap,
        sidecar_exists,
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
enum Extraction {
    Resolved {
        path: PathBuf,
        captured: DateTime<Utc>,
        conflict_warning: Option<String>,
    },
    NeedsOffset {
        path: PathBuf,
    },
    NoCaptureTime {
        path: PathBuf,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

fn extract(
    parser: &mut MediaParser,
    path: &Path,
    format: RawFormat,
    utc_offset: Option<FixedOffset>,
) -> Extraction {
    match raw::capture_time(parser, path, format, utc_offset) {
        Ok(Capture::Resolved { at, conflict }) => Extraction::Resolved {
            path: path.to_path_buf(),
            captured: at,
            conflict_warning: conflict.map(|conflict| {
                format!(
                    "EXIF timezone {} disagrees with --utc-offset {}; using the EXIF value",
                    conflict.exif, conflict.cli
                )
            }),
        },
        Ok(Capture::NeedsOffset) => Extraction::NeedsOffset {
            path: path.to_path_buf(),
        },
        Ok(Capture::NoCaptureTime) => Extraction::NoCaptureTime {
            path: path.to_path_buf(),
        },
        Err(error) => Extraction::Failed {
            path: path.to_path_buf(),
            error: format!("{error:#}"),
        },
    }
}

/// Phase B's per-file result.
struct Written {
    path: PathBuf,
    kind: WrittenKind,
}

enum WrittenKind {
    Tagged { fix: Fix },
    OutsideTrack,
    InGap { description: String },
    SidecarExists,
    Failed { error: String },
}

fn write_one(photo: &Photo, track: &Track, args: &Args) -> Written {
    let kind = write_sidecar(photo, track, args);
    Written {
        path: photo.path.clone(),
        kind,
    }
}

fn write_sidecar(photo: &Photo, track: &Track, args: &Args) -> WrittenKind {
    // `--max-gap` is seconds on the command line and a `TimeDelta` everywhere
    // inside; this is the only place the two meet.
    let limits = GapLimits {
        max_gap: TimeDelta::seconds(args.max_gap),
        max_meters: args.max_distance,
    };

    let fix = match track.lookup(photo.captured, limits) {
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
    if !args.force && sidecar.exists() {
        return WrittenKind::SidecarExists;
    }

    let packet = xmp::render(&fix, photo.captured);

    // --dry-run still does every bit of work above, so it exercises the same code
    // paths and reports the same counts; it just stops before touching the disk.
    if args.dry_run {
        return WrittenKind::Tagged { fix };
    }

    match xmp::write_atomic(&sidecar, &packet) {
        Ok(()) => WrittenKind::Tagged { fix },
        Err(error) => WrittenKind::Failed {
            error: format!("{error:#}"),
        },
    }
}

/// The files the gate must refuse the run over: a capture time with no zone and
/// no `--utc-offset` to resolve it.
///
/// Returned sorted, because this list is printed and the run must read the same
/// at any `--jobs`. Non-empty means no sidecar is written at all — guessing an
/// offset would misplace every photo by that amount.
fn files_needing_offset(extractions: &[Extraction]) -> Vec<&Path> {
    let mut paths: Vec<&Path> = extractions
        .iter()
        .filter_map(|extraction| match extraction {
            Extraction::NeedsOffset { path } => Some(path.as_path()),
            _ => None,
        })
        .collect();
    paths.sort_unstable();
    paths
}

/// Walk the tree, collecting matching files.
///
/// Materializing into a `Vec` before parallelizing gives rayon contiguous slices
/// to split, which load-balances far better than bridging a sequential iterator.
/// It also yields an exact denominator for the progress bar.
fn collect_paths(dir: &Path, wanted_ext: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut paths = Vec::new();
    let mut errors = Vec::new();

    for entry in WalkDir::new(dir) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                if has_extension(entry.path(), wanted_ext) {
                    paths.push(entry.into_path());
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

    paths.sort_unstable();
    Ok((paths, errors))
}

fn has_extension(path: &Path, wanted: &str) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(wanted))
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

fn print_summary(summary: &Summary) {
    let skipped = summary.outside_track
        + summary.in_gap
        + summary.sidecar_exists
        + summary.no_capture_time
        + summary.failed;

    let mut reasons = Vec::new();
    if summary.outside_track > 0 {
        reasons.push(format!("{} outside track", count(summary.outside_track)));
    }
    if summary.in_gap > 0 {
        reasons.push(format!("{} in track gap", count(summary.in_gap)));
    }
    if summary.sidecar_exists > 0 {
        reasons.push(format!(
            "{} existing sidecar",
            count(summary.sidecar_exists)
        ));
    }
    if summary.no_capture_time > 0 {
        reasons.push(format!(
            "{} no capture time",
            count(summary.no_capture_time)
        ));
    }
    if summary.failed > 0 {
        reasons.push(format!("{} errored", count(summary.failed)));
    }

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

/// The one timestamp format the tool prints, so every report reads the same.
///
/// Infallible now that instants are `DateTime<Utc>`; it previously needed a
/// fallback branch for a Unix second that could not be converted back.
fn format_utc(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parse `±HHMM`, also tolerating `±HH:MM`. Shares its implementation with the
/// EXIF-side offset parser so the two can never drift apart.
fn parse_utc_offset(text: &str) -> Result<FixedOffset, String> {
    raw::parse_offset(text)
        .ok_or_else(|| format!("expected an offset like -0700 or +0430, got {text:?}"))
}

#[cfg(test)]
mod tests {
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

    fn needs_offset(path: &str) -> Extraction {
        Extraction::NeedsOffset {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn the_gate_stays_shut_when_every_file_resolved() {
        let extractions = vec![
            Extraction::Resolved {
                path: PathBuf::from("/photos/a.cr3"),
                captured: DateTime::from_timestamp(1000, 0).expect("a valid test instant"),
                conflict_warning: None,
            },
            Extraction::NoCaptureTime {
                path: PathBuf::from("/photos/b.cr3"),
            },
            Extraction::Failed {
                path: PathBuf::from("/photos/c.cr3"),
                error: "unreadable".to_string(),
            },
        ];

        // A missing capture time or a read failure is a per-file skip, not a
        // reason to refuse the whole run. Only a missing timezone is.
        assert!(files_needing_offset(&extractions).is_empty());
    }

    #[test]
    fn the_gate_reports_every_zoneless_file_in_sorted_order() {
        let extractions = vec![
            needs_offset("/photos/c.cr3"),
            Extraction::Resolved {
                path: PathBuf::from("/photos/z.cr3"),
                captured: DateTime::from_timestamp(1000, 0).expect("a valid test instant"),
                conflict_warning: None,
            },
            needs_offset("/photos/a.cr3"),
            needs_offset("/photos/b.cr3"),
        ];

        // Sorted regardless of the order Phase A happened to finish in, so the
        // report is identical at any --jobs.
        assert_eq!(
            files_needing_offset(&extractions),
            vec![
                Path::new("/photos/a.cr3"),
                Path::new("/photos/b.cr3"),
                Path::new("/photos/c.cr3"),
            ]
        );
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

    #[test]
    fn extension_matching_ignores_case() {
        assert!(has_extension(Path::new("/photos/IMG_1234.CR3"), "cr3"));
        assert!(has_extension(Path::new("/photos/IMG_1234.cr3"), "cr3"));
        assert!(!has_extension(Path::new("/photos/IMG_1234.jpg"), "cr3"));
        assert!(!has_extension(Path::new("/photos/IMG_1234"), "cr3"));
    }
}
