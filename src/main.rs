//! Geotag camera raw files from a GPX track by writing XMP sidecars.
//!
//! Raw files are never modified; the whole operation is undone by deleting the
//! `.xmp` files.
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
use chrono::FixedOffset;
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
/// EXIF read is nearly free (~0.3 s for 3883 CR3s) and the run is dominated by
/// *creating* sidecars, which are NTFS directory-metadata operations that NTFS
/// serialises within a directory. Extra threads there only add contention, so
/// throughput peaks at 2 and degrades above it — measured both warm and cold.
///
/// High-latency storage inverts this completely: over SMB the read dominates
/// and parallelises ~12x, so `-j 16` or more is the right call there. That case
/// is rare enough to be worth a flag rather than a worse default.
const DEFAULT_JOBS: usize = 2;

#[derive(Parser)]
#[command(
    name = "rawgeotag",
    version,
    about = "Geotag camera raw files from a GPX track by writing XMP sidecars",
    after_help = "Raw files are never modified. Existing sidecars are skipped unless --force \
                  is given, which overwrites them wholesale — discarding any develop settings \
                  or keywords another tool stored there.\n\n\
                  A photo is only tagged when the two track points bracketing its capture time \
                  are close in BOTH time and distance, and come from the same recording run. \
                  A geotag that is wrong is worse than one that is missing, so anything the \
                  track does not actually support is skipped and reported."
)]
struct Args {
    /// Parent directory, searched recursively
    dir: PathBuf,

    /// Raw extension, e.g. "cr3" (case-insensitive, leading "." tolerated)
    ext: String,

    /// Path to the GPX track file
    gpx: PathBuf,

    /// Offset for files with no EXIF timezone, e.g. -0700, +0430
    #[arg(long, value_name = "±HHMM", allow_hyphen_values = true, value_parser = parse_utc_offset)]
    utc_offset: Option<FixedOffset>,

    /// Refuse to interpolate across a hole longer than this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    max_gap: i64,

    /// Refuse to interpolate across a hole wider than this many meters
    #[arg(long, value_name = "METERS", default_value_t = 100.0)]
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
        println!(
            "Track: {} points, {} to {}",
            track.point_count(),
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
    let mut needs_offset: Vec<&Path> = extractions
        .iter()
        .filter_map(|extraction| match extraction {
            Extraction::NeedsOffset { path } => Some(path.as_path()),
            _ => None,
        })
        .collect();

    if !needs_offset.is_empty() {
        needs_offset.sort_unstable();
        eprintln!(
            "error: {} file(s) have a capture time with no timezone, and no --utc-offset was given:",
            needs_offset.len()
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
                ts,
                conflict_warning,
            } => {
                if let Some(warning) = conflict_warning {
                    warnings.push((path.clone(), warning));
                }
                photos.push(Photo { path, ts });
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
    ts: i64,
}

/// Phase A's per-file result. Diagnostics travel in the value rather than being
/// printed, so workers stay silent and output stays deterministic.
enum Extraction {
    Resolved {
        path: PathBuf,
        ts: i64,
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
        Ok(Capture::Resolved { ts, conflict }) => Extraction::Resolved {
            path: path.to_path_buf(),
            ts,
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
    let limits = GapLimits {
        max_seconds: args.max_gap,
        max_meters: args.max_distance,
    };

    let fix = match track.lookup(photo.ts, limits) {
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
                    "falls in a track gap of {}s / {:.0} m{reason}",
                    gap.seconds, gap.meters
                ),
            };
        }
    };

    let sidecar = xmp::sidecar_path(&photo.path);
    if !args.force && sidecar.exists() {
        return WrittenKind::SidecarExists;
    }

    let packet = match xmp::render(&fix, photo.ts) {
        Ok(packet) => packet,
        Err(error) => {
            return WrittenKind::Failed {
                error: format!("{error:#}"),
            }
        }
    };

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
        reasons.push(format!("{} outside track", summary.outside_track));
    }
    if summary.in_gap > 0 {
        reasons.push(format!("{} in track gap", summary.in_gap));
    }
    if summary.sidecar_exists > 0 {
        reasons.push(format!("{} existing sidecar", summary.sidecar_exists));
    }
    if summary.no_capture_time > 0 {
        reasons.push(format!("{} no capture time", summary.no_capture_time));
    }
    if summary.failed > 0 {
        reasons.push(format!("{} errored", summary.failed));
    }

    let rate = if summary.elapsed > 0.0 {
        summary.scanned as f64 / summary.elapsed
    } else {
        0.0
    };

    println!();
    println!(
        "Scanned  {:>5} .{} files",
        summary.scanned, summary.extension
    );
    println!(
        "Tagged   {:>5}{}",
        summary.tagged,
        if summary.dry_run {
            "   (dry run — nothing written)"
        } else {
            ""
        }
    );
    if skipped > 0 {
        println!("Skipped  {:>5}   {}", skipped, reasons.join(", "));
    }
    println!(
        "Elapsed  {:>5.1}s  ({rate:.0} files/sec, {} threads)",
        summary.elapsed, summary.threads
    );
}

fn format_utc(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{ts} (unrepresentable)"))
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

    #[test]
    fn extension_matching_ignores_case() {
        assert!(has_extension(Path::new("/photos/IMG_1234.CR3"), "cr3"));
        assert!(has_extension(Path::new("/photos/IMG_1234.cr3"), "cr3"));
        assert!(!has_extension(Path::new("/photos/IMG_1234.jpg"), "cr3"));
        assert!(!has_extension(Path::new("/photos/IMG_1234"), "cr3"));
    }
}
