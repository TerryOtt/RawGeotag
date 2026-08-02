//! The binary end to end, over synthetic input.
//!
//! `run()` is not unit-testable as a `fn main` binary, and for a long time its
//! only process-level coverage was `verify-fixtures.ps1` — which exercises the
//! happy path and the gate, and can never reach the orchestration branches that
//! fire *without* real camera files: the early returns, the refusals, and the
//! exit codes. Those live here. The 2026-08-02 walk-error bug is why this file
//! exists: it sat in exactly that blind spot, reporting a clean zero over a
//! tree it could not read.
//!
//! `env!("CARGO_BIN_EXE_rawgeotag")` is Cargo's own mechanism for this — it
//! builds the binary before the test runs and hands over its path. `assert_cmd`
//! offers the same with more API; a path and `std::process::Command` are enough
//! here, so no dependency was added.
//!
//! Anything that needs a real raw — tagging, the timezone gate, the two read
//! strategies — belongs to the fixture harness, not here. See docs/TESTING.md.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The built binary, with the progress bar off so output is stable to assert on.
fn rawgeotag() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rawgeotag"));
    command.arg("--no-progress");
    command
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A one-segment GPX at the given `HH:MM:SS` times, all at one spot — the same
/// minimal shape the unit tests in `track.rs` build.
fn write_gpx(dir: &Path, name: &str, times: &[&str]) -> PathBuf {
    let points: String = times
        .iter()
        .map(|t| format!(r#"<trkpt lat="47.0" lon="-122.0"><time>2022-01-01T{t}Z</time></trkpt>"#))
        .collect();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            r#"<?xml version="1.0"?><gpx version="1.1" creator="test"><trk><trkseg>{points}</trkseg></trk></gpx>"#
        ),
    )
    .expect("writing the test GPX");
    path
}

/// A tree holding no supported raws exits clean and volunteers what it passed
/// over — the only signal that a whole shoot was invisible rather than absent.
#[test]
fn a_tree_with_no_raws_exits_clean_and_names_what_it_passed_over() {
    let dir = tempfile::tempdir().expect("creating the scratch directory");
    let photos = dir.path().join("photos");
    std::fs::create_dir(&photos).expect("creating the photo directory");
    for name in ["DSC_0001.arw", "notes.txt"] {
        std::fs::write(photos.join(name), "x").expect("creating a decoy file");
    }
    let track = write_gpx(dir.path(), "track.gpx", &["00:00:00", "00:00:10"]);

    let output = rawgeotag()
        .arg(&photos)
        .arg(&track)
        .output()
        .expect("running rawgeotag");

    assert!(output.status.success(), "{output:?}");
    let report = stdout_of(&output);
    assert!(report.contains("0 raw files"), "{report}");
    assert!(report.contains(".arw 1"), "{report}");
    assert!(report.contains("(supported:"), "{report}");
}

#[cfg(windows)]
fn icacls(path: &Path, args: &[&str]) {
    // `output()` rather than `status()`, so icacls' per-file chatter is captured
    // instead of leaking into the test output.
    let output = Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .expect("running icacls");
    assert!(
        output.status.success(),
        "icacls {args:?} failed on {}",
        path.display()
    );
}

/// The regression test for the bug this file exists because of: a tree whose
/// subdirectory cannot be read must fail the run, not report a clean
/// "Scanned 0 raw files" — which is what it did until 2026-08-02.
#[cfg(windows)]
#[test]
fn an_unreadable_subdirectory_fails_the_run_rather_than_exiting_clean() {
    let dir = tempfile::tempdir().expect("creating the scratch directory");
    let photos = dir.path().join("photos");
    let locked = photos.join("locked");
    std::fs::create_dir_all(&locked).expect("creating the locked subdirectory");
    let track = write_gpx(dir.path(), "track.gpx", &["00:00:00", "00:00:10"]);

    // Deny ourselves the right to list it, so the walk yields an error entry.
    // An explicit deny beats the allows, even for an elevated CI runner.
    let user = std::env::var("USERNAME").expect("USERNAME is always set on Windows");
    icacls(&locked, &["/deny", &format!("{user}:(RD)")]);
    let output = rawgeotag()
        .arg(&photos)
        .arg(&track)
        .output()
        .expect("running rawgeotag");
    // Re-grant before asserting anything, or a failing assert below would also
    // leave the TempDir unable to clean up after itself.
    icacls(&locked, &["/remove:d", &user]);

    assert!(!output.status.success(), "{output:?}");
    assert!(stdout_of(&output).contains("0 raw files"), "{output:?}");
    assert!(stderr_of(&output).contains("warning:"), "{output:?}");
}

/// A stray file wearing a raw extension is named on stderr, counted as errored
/// in the summary, and fails the run — the unit tests pin the per-file error,
/// but only the process shows the exit code carrying it.
#[test]
fn a_file_that_is_not_a_raw_is_named_counted_and_fails_the_run() {
    let dir = tempfile::tempdir().expect("creating the scratch directory");
    let photos = dir.path().join("photos");
    std::fs::create_dir(&photos).expect("creating the photo directory");
    std::fs::write(photos.join("IMG_0001.CR3"), "not a raw file at all")
        .expect("creating the decoy raw");
    let track = write_gpx(dir.path(), "track.gpx", &["00:00:00", "00:00:10"]);

    let output = rawgeotag()
        .arg(&photos)
        .arg(&track)
        .output()
        .expect("running rawgeotag");

    assert!(!output.status.success(), "{output:?}");
    assert!(stderr_of(&output).contains("IMG_0001"), "{output:?}");
    assert!(stdout_of(&output).contains("1 errored"), "{output:?}");
}

/// Two tracks covering one instant are refused before anything is scanned:
/// a named error, a non-zero exit, and the promise that nothing was written.
#[test]
fn overlapping_tracks_refuse_the_run_with_a_named_error() {
    let dir = tempfile::tempdir().expect("creating the scratch directory");
    let photos = dir.path().join("photos");
    std::fs::create_dir(&photos).expect("creating the photo directory");
    let first = write_gpx(dir.path(), "first.gpx", &["00:00:00", "00:01:00"]);
    let second = write_gpx(dir.path(), "second.gpx", &["00:00:30", "00:02:00"]);

    let output = rawgeotag()
        .arg(&photos)
        .arg(&first)
        .arg(&second)
        .output()
        .expect("running rawgeotag");

    assert!(!output.status.success(), "{output:?}");
    let report = stderr_of(&output);
    assert!(report.contains("error:"), "{report}");
    assert!(report.contains("overlapping"), "{report}");
    assert!(report.contains("No sidecars were written"), "{report}");
}

/// The one validation `run()` performs before touching anything.
#[test]
fn zero_jobs_is_refused() {
    let dir = tempfile::tempdir().expect("creating the scratch directory");
    let photos = dir.path().join("photos");
    std::fs::create_dir(&photos).expect("creating the photo directory");
    let track = write_gpx(dir.path(), "track.gpx", &["00:00:00", "00:00:10"]);

    let output = rawgeotag()
        .args(["--jobs", "0"])
        .arg(&photos)
        .arg(&track)
        .output()
        .expect("running rawgeotag");

    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr_of(&output).contains("--jobs must be at least 1"),
        "{output:?}"
    );
}
