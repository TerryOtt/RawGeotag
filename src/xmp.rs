//! XMP sidecar rendering and atomic writes.
//!
//! The packet has a fixed structure and every value in it is machine-generated —
//! numbers and single cardinal letters — so there is no escaping surface for an
//! XML library to protect. A format template is the simpler correct choice here,
//! and it allocates less in a hot parallel loop. (If sidecar *merging* is ever
//! added this changes: read-modify-write of third-party XMP needs a real parser.)
//!
//! Nor `xmp-writer`, the obvious dedicated crate. Its typed API covers the `dc:`,
//! `xmp:` and `pdf:` namespaces for PDF metadata, so every exif GPS property here
//! would go through `CustomNamespace` one at a time, and it emits element-form
//! RDF where this writes attribute-form. That is the same amount of code plus a
//! dependency.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tempfile::NamedTempFile;

use crate::track::Fix;

/// `IMG_1234.CR3` -> `IMG_1234.xmp`, the Adobe/Lightroom convention.
pub fn sidecar_path(raw: &Path) -> PathBuf {
    raw.with_extension("xmp")
}

/// One photo's sidecar, rendered by its `Display` impl.
///
/// `Display` rather than a function returning `String` because the packet has one
/// genuinely conditional part — altitude, present only when the track has
/// elevation — and a format template cannot express "these two lines or nothing".
/// The previous version built that fragment as its own `String`, complete with the
/// document's newlines and four-space indent baked into a value, and spliced it
/// mid-attribute-list. The layout was smeared across two places and the indent had
/// to be kept matching by hand. Here it is an ordinary `if let`.
struct Packet<'a> {
    fix: &'a Fix,
    captured: DateTime<Utc>,
}

impl fmt::Display for Packet<'_> {
    /// One `writeln!` per line of the document, so the source reads in the same
    /// order and shape as the file it produces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>"
        )?;
        writeln!(
            f,
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="rawgeotag {}">"#,
            env!("CARGO_PKG_VERSION")
        )?;
        writeln!(
            f,
            r#" <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">"#
        )?;
        // Escaped rather than raw: the value is the empty string, so the line ends
        // with two quotes, and `r#"...about="""#` puts three in a row before the
        // delimiter. That lexes correctly but reads like a typo — and getting it
        // wrong silently drops one quote and emits malformed XML, which is exactly
        // what happened on the first attempt at this function.
        writeln!(f, "  <rdf:Description rdf:about=\"\"")?;
        writeln!(f, r#"    xmlns:exif="http://ns.adobe.com/exif/1.0/""#)?;
        writeln!(f, r#"    exif:GPSVersionID="2.2.0.0""#)?;
        writeln!(f, r#"    exif:GPSMapDatum="WGS-84""#)?;
        writeln!(
            f,
            r#"    exif:GPSLatitude="{}""#,
            encode_coordinate(self.fix.lat, 'N', 'S')
        )?;
        writeln!(
            f,
            r#"    exif:GPSLongitude="{}""#,
            encode_coordinate(self.fix.lon, 'E', 'W')
        )?;

        // Omitted entirely rather than defaulted when the track has no elevation.
        if let Some(ele) = self.fix.ele {
            writeln!(f, r#"    exif:GPSAltitude="{}""#, encode_altitude(ele))?;
            writeln!(f, r#"    exif:GPSAltitudeRef="{}""#, altitude_ref(ele))?;
        }

        writeln!(
            f,
            r#"    exif:GPSTimeStamp="{}"/>"#,
            self.captured.format("%Y-%m-%dT%H:%M:%SZ")
        )?;
        writeln!(f, " </rdf:RDF>")?;
        writeln!(f, "</x:xmpmeta>")?;
        writeln!(f, r#"<?xpacket end="w"?>"#)
    }
}

/// Render the sidecar for one photo.
///
/// Infallible: `captured` is already an absolute instant, so there is no
/// out-of-range timestamp left to reject. It used to take Unix seconds, which
/// meant carrying an error case for a value that could not be converted back.
///
/// Returns a `String` rather than handing the `Packet` to `write_atomic` to render
/// straight into the file. That would save one allocation per photo — a few hundred
/// bytes against a run that moves gigabytes — but it would make `--dry-run` stop
/// rendering anything at all, since a `Packet` is inert until displayed. `--dry-run`
/// doing every bit of work except the write is a documented property worth more
/// than the allocation.
pub fn render(fix: &Fix, captured: DateTime<Utc>) -> String {
    Packet { fix, captured }.to_string()
}

/// Write the packet so an interrupted run cannot leave a half-written sidecar.
///
/// The temp file sits in the destination directory, so the rename stays on one
/// filesystem. `tempfile` gives it a random name — two `rawgeotag` processes over
/// the same directory therefore cannot collide on it — and deletes it on drop, so
/// a failure anywhere below leaves nothing behind for a later run to trip over.
pub fn write_atomic(target: &Path, packet: &str) -> Result<()> {
    // `parent()` is the empty path for a bare file name like `IMG_1234.xmp`, which
    // is not somewhere a temp file can be created.
    let directory = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    let mut temp = NamedTempFile::new_in(directory)
        .with_context(|| format!("creating a temporary file for {}", target.display()))?;

    temp.write_all(packet.as_bytes())
        .with_context(|| format!("writing {}", target.display()))?;

    // Atomically replaces an existing sidecar, which is what `--force` wants.
    temp.persist(target)
        // The error carries the temp file back so it can be retried; nothing here
        // retries, and dropping it is what removes the file.
        .map_err(|error| error.error)
        .with_context(|| format!("renaming sidecar into place: {}", target.display()))?;

    Ok(())
}

/// Encode as the XMP spec's `DDD,MM.mmk` form: degrees, comma, decimal minutes,
/// hemisphere letter — *not* decimal degrees.
///
/// Rounding happens in integer ten-thousandths of a minute so that a value like
/// 47.99999999 carries into the degrees instead of rendering as `47,60.0000`.
fn encode_coordinate(value: f64, positive: char, negative: char) -> String {
    const TEN_THOUSANDTHS_PER_DEGREE: u64 = 60 * 10_000;

    let hemisphere = if value < 0.0 { negative } else { positive };
    let ten_thousandths = (value.abs() * TEN_THOUSANDTHS_PER_DEGREE as f64).round() as u64;

    let degrees = ten_thousandths / TEN_THOUSANDTHS_PER_DEGREE;
    let minutes = ten_thousandths % TEN_THOUSANDTHS_PER_DEGREE;

    format!(
        "{degrees},{}.{:04}{hemisphere}",
        minutes / 10_000,
        minutes % 10_000
    )
}

/// Altitude is a rational in meters; the value is absolute and the *ref* carries
/// the sign.
fn encode_altitude(ele: f64) -> String {
    format!("{}/1000", (ele.abs() * 1000.0).round() as u64)
}

/// 0 = above sea level, 1 = below.
fn altitude_ref(ele: f64) -> u8 {
    if ele < 0.0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A fixed instant for the packet tests: 2026-07-28T18:42:03Z.
    fn captured() -> DateTime<Utc> {
        DateTime::from_timestamp(1_785_264_123, 0).expect("a representable test instant")
    }

    #[test]
    fn sidecar_replaces_the_extension() {
        assert_eq!(
            sidecar_path(Path::new("/photos/IMG_1234.CR3")),
            PathBuf::from("/photos/IMG_1234.xmp")
        );
    }

    #[test]
    fn northern_and_western_coordinates_encode_to_degrees_and_decimal_minutes() {
        assert_eq!(encode_coordinate(47.4455083, 'N', 'S'), "47,26.7305N");
        assert_eq!(encode_coordinate(-122.3352833, 'E', 'W'), "122,20.1170W");
    }

    #[test]
    fn southern_and_eastern_coordinates_use_the_other_hemisphere_letters() {
        assert_eq!(encode_coordinate(-33.8688, 'N', 'S'), "33,52.1280S");
        assert_eq!(encode_coordinate(151.2093, 'E', 'W'), "151,12.5580E");
    }

    #[test]
    fn zero_encodes_as_the_positive_hemisphere() {
        assert_eq!(encode_coordinate(0.0, 'N', 'S'), "0,0.0000N");
    }

    #[test]
    fn minutes_rounding_carries_into_the_degrees() {
        // Without an integer carry this would render as the invalid "47,60.0000N".
        assert_eq!(encode_coordinate(47.99999999, 'N', 'S'), "48,0.0000N");
    }

    #[test]
    fn altitude_is_a_rational_with_the_sign_in_the_ref() {
        assert_eq!(encode_altitude(123.456), "123456/1000");
        assert_eq!(altitude_ref(123.456), 0);

        assert_eq!(encode_altitude(-12.345), "12345/1000");
        assert_eq!(altitude_ref(-12.345), 1);
    }

    /// The whole packet, byte for byte.
    ///
    /// Every other test here uses `contains`, which is why they all passed while
    /// the `Display` rewrite was emitting `rdf:about="` with one quote instead of
    /// two — malformed XML that only the fixture hashes caught. `contains` proves a
    /// value reached the output; it cannot prove the document around it is intact.
    /// If this test fails, diff it against the expected literal before assuming the
    /// literal is stale.
    #[test]
    fn the_packet_is_exactly_this() {
        let fix = Fix {
            lat: 47.4455083,
            lon: -122.3352833,
            ele: Some(123.456),
        };

        // One array entry per line of the document, joined — so leading spaces are
        // literal and there are no continuation escapes to misread.
        let xmpmeta = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="rawgeotag {}">"#,
            env!("CARGO_PKG_VERSION")
        );
        let expected = [
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>",
            &xmpmeta,
            r#" <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">"#,
            "  <rdf:Description rdf:about=\"\"",
            r#"    xmlns:exif="http://ns.adobe.com/exif/1.0/""#,
            r#"    exif:GPSVersionID="2.2.0.0""#,
            r#"    exif:GPSMapDatum="WGS-84""#,
            r#"    exif:GPSLatitude="47,26.7305N""#,
            r#"    exif:GPSLongitude="122,20.1170W""#,
            r#"    exif:GPSAltitude="123456/1000""#,
            r#"    exif:GPSAltitudeRef="0""#,
            r#"    exif:GPSTimeStamp="2026-07-28T18:42:03Z"/>"#,
            " </rdf:RDF>",
            "</x:xmpmeta>",
            r#"<?xpacket end="w"?>"#,
        ]
        .join("\n")
            + "\n";

        assert_eq!(render(&fix, captured()), expected);
    }

    // A `packet_carries_coordinates_altitude_and_utc_timestamp` test lived here and
    // was removed: it asserted seven `contains` over the same `Fix` and instant that
    // `the_packet_is_exactly_this` already compares byte for byte, so every one of
    // them was implied and none could fail on its own. Do not re-add it — the two
    // below earn their place by using *different* inputs.

    #[test]
    fn packet_omits_altitude_entirely_when_the_track_has_none() {
        let fix = Fix {
            lat: 47.4455083,
            lon: -122.3352833,
            ele: None,
        };
        let packet = render(&fix, captured());

        assert!(!packet.contains("GPSAltitude"), "{packet}");
        assert!(
            packet.contains(r#"exif:GPSLatitude="47,26.7305N""#),
            "{packet}"
        );
    }

    #[test]
    fn negative_altitude_sets_the_ref_and_keeps_the_value_absolute() {
        let fix = Fix {
            lat: 31.5,
            lon: 35.5,
            ele: Some(-420.5),
        };
        let packet = render(&fix, captured());

        assert!(
            packet.contains(r#"exif:GPSAltitude="420500/1000""#),
            "{packet}"
        );
        assert!(packet.contains(r#"exif:GPSAltitudeRef="1""#), "{packet}");
    }

    // ---- atomic write -------------------------------------------------------

    /// The file names in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("reading the scratch directory")
            .map(|entry| entry.expect("a directory entry").file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A scratch directory of its own, removed when the test ends.
    ///
    /// `tempfile` is already a dependency and already gives a unique name and
    /// deletion on drop, which is the whole of what these tests need.
    fn scratch_dir() -> TempDir {
        tempfile::tempdir().expect("creating the scratch directory")
    }

    #[test]
    fn write_atomic_lands_exactly_the_packet_and_leaves_no_temp_file() {
        let dir = scratch_dir();
        let target = dir.path().join("IMG_0001.xmp");

        write_atomic(&target, "packet contents").expect("writing the sidecar");

        assert_eq!(fs::read_to_string(&target).unwrap(), "packet contents");
        // The temp file is an implementation detail, but a leftover one would be
        // mistaken for a sidecar by anything globbing the directory.
        assert_eq!(entries(dir.path()), vec!["IMG_0001.xmp".to_string()]);
    }

    #[test]
    fn write_atomic_replaces_an_existing_sidecar_wholesale() {
        let dir = scratch_dir();
        let target = dir.path().join("IMG_0002.xmp");

        write_atomic(&target, "first").expect("writing the first sidecar");
        write_atomic(&target, "second").expect("overwriting the sidecar");

        // Wholesale replacement, not append or merge — --force discards whatever
        // another tool had stored there, and that is the documented behavior.
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
        assert_eq!(entries(dir.path()), vec!["IMG_0002.xmp".to_string()]);
    }

    #[test]
    fn write_atomic_reports_the_target_when_the_directory_is_missing() {
        let dir = scratch_dir();
        let target = dir.path().join("no-such-subdir").join("IMG_0003.xmp");

        let error = write_atomic(&target, "packet").expect_err("the directory does not exist");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("IMG_0003"),
            "the error should name the file it failed on, got {rendered:?}"
        );
    }
}
