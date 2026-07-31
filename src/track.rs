//! The GPX track index: load it once, then look positions up by timestamp.
//!
//! One or more GPX files are flattened into a single index — a day is often split
//! across several tracks. The merge is deliberately conservative: the seam between
//! two files is a segment break like any other and is never interpolated across,
//! and files whose time ranges overlap are rejected outright rather than letting
//! argument order decide which recording wins.
//!
//! The index is built before any sidecar is written and is immutable afterwards,
//! so workers share it as `&Track` with no lock and no contention.
//!
//! Interpolation is refused wherever the data does not actually support it. A
//! wrong geotag looks authoritative and silently corrupts a photo's provenance,
//! whereas a missing one is visibly missing and can be fixed later — so the
//! bracketing points must be close in *both* time and distance, and must come
//! from the same recording run, or the photo is skipped.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use gpx::Waypoint;
use time::OffsetDateTime;

/// One timestamped position from the track.
///
/// Times are Unix seconds: `gpx` speaks `time::OffsetDateTime` and `nom-exif`
/// speaks `chrono::DateTime`, so both sides normalize to this one scalar domain
/// at the boundary rather than converting between the two time crates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackPoint {
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    /// Which contiguous recording run this came from. Points from different runs
    /// are never interpolated between: a `<trkseg>` break means the logger stopped,
    /// so nothing is known about the path in between.
    pub segment: u32,
}

/// A position resolved from the track for some instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    /// `None` when the track has no usable elevation for this instant; the
    /// sidecar then omits altitude entirely rather than inventing a value.
    pub ele: Option<f64>,
}

/// How far apart bracketing points may be and still be interpolated between.
#[derive(Debug, Clone, Copy)]
pub struct GapLimits {
    pub max_seconds: i64,
    pub max_meters: f64,
}

impl GapLimits {
    /// The shipped defaults, and the single place they are written down.
    ///
    /// `--max-gap` and `--max-distance` take their `default_value_t` from these
    /// fields, and the tests below exercise the same value, so the tool and its
    /// tests cannot drift apart the way a hand-copied constant would.
    ///
    /// Both limits are load-bearing and neither implies the other — see the gap
    /// rule in CLAUDE.md before changing either.
    pub const DEFAULT: Self = Self {
        max_seconds: 60,
        max_meters: 100.0,
    };
}

/// Why a lookup did not produce a position, or the position it produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lookup {
    Found(Fix),
    /// Before the first point or after the last. No clamping, no extrapolation.
    OutsideTrack,
    /// Inside the track's span but between two points too far apart to bridge.
    InGap(Gap),
}

/// The hole a photo fell into, kept for reporting so the skip can be explained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    pub seconds: i64,
    pub meters: f64,
    /// The bracketing points come from different recording runs.
    pub across_segments: bool,
}

/// Every timestamped point in the track, sorted by time and deduplicated.
pub struct Track {
    points: Vec<TrackPoint>,
}

impl Track {
    /// Load one or more GPX files into a single index.
    ///
    /// A day's shooting is often split across several tracks — a driving log and
    /// a separate evening walk, say — and a photo is matched against whichever
    /// one actually covers its capture time.
    ///
    /// **Segment numbering continues across files.** Two files are no more
    /// bridgeable than two `<trkseg>` runs within one file: a separate track is a
    /// separate recording session by definition, and nothing is known about the
    /// path between them. Restarting the counter per file would silently make
    /// the last point of one file and the first of the next look contiguous.
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut points = Vec::new();
        let mut spans: Vec<(&Path, i64, i64)> = Vec::new();
        let mut segment = 0u32;

        for path in paths {
            let before = points.len();
            segment = read_into(path, &mut points, segment)?;

            if let Some(span) = span_of(&points[before..]) {
                spans.push((path.as_path(), span.0, span.1));
            }
        }

        ensure_no_overlap(&spans)?;

        Self::new(points).with_context(|| {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            format!("building track index from {}", names.join(", "))
        })
    }

    /// Sort and deduplicate into a lookup-ready index.
    pub fn new(mut points: Vec<TrackPoint>) -> Result<Self> {
        points.sort_by_key(|point| point.ts);
        points.dedup_by_key(|point| point.ts);

        ensure!(
            !points.is_empty(),
            "the track contains no points with timestamps"
        );

        Ok(Self { points })
    }

    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// The track's time span, as inclusive Unix seconds.
    pub fn span(&self) -> (i64, i64) {
        // `new` rejects an empty track, so both ends exist.
        let ends = (self.points.first(), self.points.last());
        match ends {
            (Some(first), Some(last)) => (first.ts, last.ts),
            _ => unreachable!("Track::new rejects an empty track"),
        }
    }

    /// Position at `ts`, if the track genuinely supports one there.
    pub fn lookup(&self, ts: i64, limits: GapLimits) -> Lookup {
        match self.points.binary_search_by_key(&ts, |point| point.ts) {
            // An exact hit is a recorded observation, not an interpolation, so no
            // gap test applies.
            Ok(i) => Lookup::Found(fix(self.points[i])),
            Err(i) if i == 0 || i == self.points.len() => Lookup::OutsideTrack,
            Err(i) => {
                let (before, after) = (self.points[i - 1], self.points[i]);

                let gap = Gap {
                    seconds: after.ts - before.ts,
                    meters: distance_meters(before, after),
                    across_segments: before.segment != after.segment,
                };

                if gap.across_segments
                    || gap.seconds > limits.max_seconds
                    || gap.meters > limits.max_meters
                {
                    Lookup::InGap(gap)
                } else {
                    Lookup::Found(interpolate(before, after, ts))
                }
            }
        }
    }
}

fn track_point(waypoint: Waypoint, segment: u32) -> Option<TrackPoint> {
    // Read the borrowing accessors before moving `time` out of the waypoint.
    let point = waypoint.point();
    let ele = waypoint.elevation;
    let ts = OffsetDateTime::from(waypoint.time?).unix_timestamp();

    Some(TrackPoint {
        ts,
        lat: point.y(),
        lon: point.x(),
        ele,
        segment,
    })
}

fn fix(point: TrackPoint) -> Fix {
    Fix {
        lat: point.lat,
        lon: point.lon,
        ele: point.ele,
    }
}

/// Append every timestamped point from one GPX file, returning the next free
/// segment id so the caller can keep numbering unique across files.
fn read_into(path: &Path, points: &mut Vec<TrackPoint>, first_segment: u32) -> Result<u32> {
    let file = File::open(path).with_context(|| format!("opening GPX file {}", path.display()))?;
    let gpx = gpx::read(BufReader::new(file))
        .with_context(|| format!("parsing GPX file {}", path.display()))?;

    let mut segment = first_segment;

    for track in gpx.tracks {
        for track_segment in track.segments {
            points.extend(
                track_segment
                    .points
                    .into_iter()
                    .filter_map(|waypoint| track_point(waypoint, segment)),
            );
            segment += 1;
        }
    }

    // Standalone waypoints count as track points too — some loggers write the
    // whole session that way. They form one more run of their own.
    points.extend(
        gpx.waypoints
            .into_iter()
            .filter_map(|waypoint| track_point(waypoint, segment)),
    );

    Ok(segment + 1)
}

/// Earliest and latest timestamp among `points`, or `None` if there are none.
fn span_of(points: &[TrackPoint]) -> Option<(i64, i64)> {
    let first = points.iter().map(|p| p.ts).min()?;
    let last = points.iter().map(|p| p.ts).max()?;
    Some((first, last))
}

/// Refuse to merge GPX files whose time ranges overlap.
///
/// This is a hard error, not a warning. Two loggers covering the same instant
/// can disagree about where the subject was, and the index keeps only one point
/// per timestamp — so which observation survives would come down to the order
/// the files were listed on the command line. A geotag decided by argument order
/// is exactly the authoritative-looking wrong answer the project refuses to
/// produce, and refusing coverage is always the cheaper mistake.
///
/// Fires while the track is being built, before any file is scanned or any
/// sidecar written, so a rejected run leaves nothing behind.
///
/// The bound is inclusive: sharing even a single second means two recorded
/// positions for one instant.
fn ensure_no_overlap(spans: &[(&Path, i64, i64)]) -> Result<()> {
    for (i, (path_a, start_a, end_a)) in spans.iter().enumerate() {
        for (path_b, start_b, end_b) in &spans[i + 1..] {
            if start_a <= end_b && start_b <= end_a {
                bail!(
                    "these GPX files cover overlapping times, so a photo in the overlap \
                     could resolve to either track:\n  \
                     {} spans {} to {}\n  \
                     {} spans {} to {}\n\n\
                     No sidecars were written. Pass only tracks that do not overlap, or \
                     run them as separate passes — photos outside a track are skipped, so \
                     a later pass tags only what the earlier one left alone.",
                    path_a.display(),
                    crate::format_utc(*start_a),
                    crate::format_utc(*end_a),
                    path_b.display(),
                    crate::format_utc(*start_b),
                    crate::format_utc(*end_b),
                );
            }
        }
    }

    Ok(())
}

/// Great-circle distance in meters.
///
/// Haversine is accurate to well under a meter at the scales this is tested
/// against, which is far more than a 100 m threshold needs. It is naturally
/// periodic in longitude, so an antimeridian-crossing pair needs no special case.
fn distance_meters(a: TrackPoint, b: TrackPoint) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_008.8;

    let (lat_a, lat_b) = (a.lat.to_radians(), b.lat.to_radians());
    let delta_lat = lat_b - lat_a;
    let delta_lon = (b.lon - a.lon).to_radians();

    let h = (delta_lat / 2.0).sin().powi(2)
        + lat_a.cos() * lat_b.cos() * (delta_lon / 2.0).sin().powi(2);

    2.0 * EARTH_RADIUS_M * h.sqrt().clamp(-1.0, 1.0).asin()
}

/// Linear interpolation between two bracketing points.
fn interpolate(a: TrackPoint, b: TrackPoint, ts: i64) -> Fix {
    // `Track::new` deduplicates by timestamp, so `b.ts > a.ts` here.
    let fraction = (ts - a.ts) as f64 / (b.ts - a.ts) as f64;

    // Longitude must take the shortest arc. A track crossing the antimeridian has
    // neighboring longitudes ~360 apart in raw value; interpolating those
    // directly puts the result on the opposite side of the planet.
    let b_lon = if (b.lon - a.lon).abs() > 180.0 {
        if b.lon < a.lon {
            b.lon + 360.0
        } else {
            b.lon - 360.0
        }
    } else {
        b.lon
    };

    let ele = match (a.ele, b.ele) {
        (Some(a_ele), Some(b_ele)) => Some(a_ele + (b_ele - a_ele) * fraction),
        // One end has no elevation, so there is nothing honest to interpolate.
        _ => None,
    };

    Fix {
        lat: a.lat + (b.lat - a.lat) * fraction,
        lon: normalize_lon(a.lon + (b_lon - a.lon) * fraction),
        ele,
    }
}

/// Fold a longitude back into -180..180.
fn normalize_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generous enough that tests exercising other behavior are not gated.
    const LENIENT: GapLimits = GapLimits {
        max_seconds: i64::MAX,
        max_meters: f64::INFINITY,
    };

    /// The shipped defaults, read from the one place that defines them, so these
    /// tests always exercise what the CLI actually hands to `lookup`.
    const DEFAULT: GapLimits = GapLimits::DEFAULT;

    fn point(ts: i64, lat: f64, lon: f64, ele: Option<f64>) -> TrackPoint {
        TrackPoint {
            ts,
            lat,
            lon,
            ele,
            segment: 0,
        }
    }

    fn in_segment(ts: i64, lat: f64, lon: f64, segment: u32) -> TrackPoint {
        TrackPoint {
            ts,
            lat,
            lon,
            ele: None,
            segment,
        }
    }

    fn track(points: Vec<TrackPoint>) -> Track {
        Track::new(points).expect("test track should be valid")
    }

    fn found(lookup: Lookup) -> Fix {
        match lookup {
            Lookup::Found(fix) => fix,
            other => panic!("expected a fix, got {other:?}"),
        }
    }

    #[test]
    fn exact_timestamp_uses_that_point() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -123.0, Some(200.0)),
        ]);

        assert_eq!(
            track.lookup(2000, LENIENT),
            Lookup::Found(Fix {
                lat: 48.0,
                lon: -123.0,
                ele: Some(200.0)
            })
        );
    }

    #[test]
    fn midpoint_interpolates_halfway() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -120.0, Some(200.0)),
        ]);

        let fix = found(track.lookup(1500, LENIENT));
        assert!((fix.lat - 47.5).abs() < 1e-9, "lat was {}", fix.lat);
        assert!((fix.lon - -121.0).abs() < 1e-9, "lon was {}", fix.lon);
        assert!((fix.ele.unwrap() - 150.0).abs() < 1e-9);
    }

    #[test]
    fn quarter_point_interpolates_proportionally() {
        let track = track(vec![
            point(0, 0.0, 0.0, Some(0.0)),
            point(400, 10.0, 20.0, Some(80.0)),
        ]);

        let fix = found(track.lookup(100, LENIENT));
        assert!((fix.lat - 2.5).abs() < 1e-9, "lat was {}", fix.lat);
        assert!((fix.lon - 5.0).abs() < 1e-9, "lon was {}", fix.lon);
        assert!((fix.ele.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn before_first_and_after_last_are_outside_the_track() {
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(2000, 48.0, -123.0, None),
        ]);

        assert_eq!(track.lookup(999, LENIENT), Lookup::OutsideTrack);
        assert_eq!(track.lookup(2001, LENIENT), Lookup::OutsideTrack);
    }

    #[test]
    fn antimeridian_crossing_takes_the_shortest_arc() {
        let track = track(vec![
            point(1000, 0.0, 179.0, None),
            point(2000, 0.0, -179.0, None),
        ]);

        let fix = found(track.lookup(1500, LENIENT));
        // The shortest arc runs 179 -> 180 -> -179, so the midpoint sits on the
        // antimeridian itself, not at longitude 0 on the far side of the planet.
        assert!(
            fix.lon.abs() > 179.9,
            "expected a longitude near +/-180, got {}",
            fix.lon
        );
    }

    #[test]
    fn antimeridian_crossing_westward_also_takes_the_shortest_arc() {
        let track = track(vec![
            point(1000, 0.0, -179.0, None),
            point(2000, 0.0, 179.0, None),
        ]);

        let fix = found(track.lookup(1500, LENIENT));
        assert!(
            fix.lon.abs() > 179.9,
            "expected a longitude near +/-180, got {}",
            fix.lon
        );
    }

    #[test]
    fn ordinary_longitudes_are_not_wrapped() {
        let track = track(vec![
            point(1000, 0.0, -10.0, None),
            point(2000, 0.0, 10.0, None),
        ]);

        let fix = found(track.lookup(1500, LENIENT));
        assert!((fix.lon - 0.0).abs() < 1e-9, "lon was {}", fix.lon);
    }

    #[test]
    fn missing_elevation_on_one_end_suppresses_altitude() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -123.0, None),
        ]);

        assert_eq!(found(track.lookup(1500, LENIENT)).ele, None);
    }

    #[test]
    fn points_are_sorted_and_deduplicated() {
        let track = track(vec![
            point(2000, 48.0, -123.0, None),
            point(1000, 47.0, -122.0, None),
            point(2000, 99.0, -99.0, None),
        ]);

        assert_eq!(track.point_count(), 2);
        assert_eq!(track.span(), (1000, 2000));
    }

    #[test]
    fn an_empty_track_is_rejected() {
        assert!(Track::new(Vec::new()).is_err());
    }

    #[test]
    fn a_single_point_track_only_matches_exactly() {
        let track = track(vec![point(1000, 47.0, -122.0, None)]);

        assert!(matches!(track.lookup(1000, LENIENT), Lookup::Found(_)));
        assert_eq!(track.lookup(1001, LENIENT), Lookup::OutsideTrack);
        assert_eq!(track.lookup(999, LENIENT), Lookup::OutsideTrack);
    }

    // ---- gap rejection ------------------------------------------------------

    #[test]
    fn a_time_gap_beyond_the_limit_is_not_bridged() {
        // 61s apart but only ~1 m, so only the time limit is exceeded.
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1061, 47.00001, -122.0, None),
        ]);

        match track.lookup(1030, DEFAULT) {
            Lookup::InGap(gap) => {
                assert_eq!(gap.seconds, 61);
                assert!(!gap.across_segments);
                assert!(gap.meters < 100.0, "distance was {}", gap.meters);
            }
            other => panic!("expected InGap, got {other:?}"),
        }
    }

    #[test]
    fn a_distance_gap_beyond_the_limit_is_not_bridged() {
        // 10s apart but ~1.1 km, so only the distance limit is exceeded.
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1010, 47.01, -122.0, None),
        ]);

        match track.lookup(1005, DEFAULT) {
            Lookup::InGap(gap) => {
                assert_eq!(gap.seconds, 10);
                assert!(gap.meters > 100.0, "distance was {}", gap.meters);
            }
            other => panic!("expected InGap, got {other:?}"),
        }
    }

    #[test]
    fn within_both_limits_still_interpolates() {
        // 10s and ~11 m apart: comfortably inside both limits.
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1010, 47.0001, -122.0, None),
        ]);

        let fix = found(track.lookup(1005, DEFAULT));
        assert!((fix.lat - 47.00005).abs() < 1e-9, "lat was {}", fix.lat);
    }

    #[test]
    fn a_segment_boundary_is_never_bridged_however_close_the_points_are() {
        // 1 second and centimeters apart, but from different recording runs.
        let adjacent = track(vec![
            in_segment(1000, 47.0, -122.0, 0),
            in_segment(1001, 47.0000001, -122.0, 1),
        ]);

        match adjacent.lookup(1000, DEFAULT) {
            // 1000 is an exact hit, so it resolves.
            Lookup::Found(_) => {}
            other => panic!("exact hit should resolve, got {other:?}"),
        }

        // Nothing strictly between them can be, though.
        let spanning = track(vec![
            in_segment(1000, 47.0, -122.0, 0),
            in_segment(1010, 47.0000001, -122.0, 1),
        ]);
        match spanning.lookup(1005, DEFAULT) {
            Lookup::InGap(gap) => assert!(gap.across_segments),
            other => panic!("expected InGap across segments, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_hit_inside_a_gap_still_resolves() {
        // The middle point is an observation, not a guess, even though its
        // neighbors are far away in both time and distance.
        let track = track(vec![
            point(0, 47.0, -122.0, None),
            point(5000, 48.0, -122.0, None),
            point(10000, 49.0, -122.0, None),
        ]);

        assert!(matches!(track.lookup(5000, DEFAULT), Lookup::Found(_)));
        assert!(matches!(track.lookup(4999, DEFAULT), Lookup::InGap(_)));
    }

    // ---- distance ------------------------------------------------------------

    #[test]
    fn haversine_matches_known_distances() {
        // One degree of latitude is about 111.2 km anywhere on the globe.
        let a = point(0, 47.0, -122.0, None);
        let b = point(1, 48.0, -122.0, None);
        let d = distance_meters(a, b);
        assert!(
            (d - 111_195.0).abs() < 200.0,
            "one degree of latitude measured {d} m"
        );
    }

    #[test]
    fn haversine_handles_the_antimeridian() {
        // 0.2 degrees of longitude at the equator, straddling the antimeridian.
        let a = point(0, 0.0, 179.9, None);
        let b = point(1, 0.0, -179.9, None);
        let d = distance_meters(a, b);
        assert!(
            (d - 22_239.0).abs() < 200.0,
            "expected ~22 km across the antimeridian, got {d} m"
        );
    }

    #[test]
    fn identical_points_are_zero_meters_apart() {
        let a = point(0, 47.0, -122.0, None);
        assert!(distance_meters(a, a) < 1e-6);
    }

    // ---- multiple GPX files -------------------------------------------------

    fn span(name: &str, start: i64, end: i64) -> (&Path, i64, i64) {
        (Path::new(name), start, end)
    }

    #[test]
    fn tracks_that_do_not_overlap_are_accepted() {
        let spans = [
            span("morning.gpx", 1000, 2000),
            span("evening.gpx", 2001, 3000),
        ];
        assert!(ensure_no_overlap(&spans).is_ok());
    }

    #[test]
    fn tracks_sharing_even_one_second_are_rejected() {
        // Touching endpoints still means two recorded positions for one instant,
        // and nothing here can say which logger was right.
        let spans = [span("a.gpx", 1000, 2000), span("b.gpx", 2000, 3000)];

        let error = ensure_no_overlap(&spans).expect_err("2000 belongs to both files");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("a.gpx"), "{rendered}");
        assert!(rendered.contains("b.gpx"), "{rendered}");
        assert!(
            rendered.contains("No sidecars were written"),
            "the error should say nothing was written, got {rendered}"
        );
    }

    #[test]
    fn a_contained_track_is_rejected() {
        // Full containment is not caught by comparing starts or ends alone.
        let spans = [span("outer.gpx", 1000, 5000), span("inner.gpx", 2000, 3000)];
        assert!(ensure_no_overlap(&spans).is_err());
    }

    #[test]
    fn overlap_is_detected_whatever_order_the_files_are_given_in() {
        let forward = [span("a.gpx", 1000, 2500), span("b.gpx", 2000, 3000)];
        let reversed = [span("b.gpx", 2000, 3000), span("a.gpx", 1000, 2500)];

        assert!(ensure_no_overlap(&forward).is_err());
        assert!(ensure_no_overlap(&reversed).is_err());
    }

    #[test]
    fn overlap_is_checked_across_every_pair_not_just_neighbours() {
        // The first and last overlap while neither touches the middle one, so a
        // check that only compared adjacent files would miss this.
        let spans = [
            span("a.gpx", 1000, 9000),
            span("b.gpx", 20_000, 21_000),
            span("c.gpx", 5000, 6000),
        ];
        assert!(ensure_no_overlap(&spans).is_err());
    }

    /// A scratch directory of its own, removed when the test ends.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(test_name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rawgeotag-track-{}-{test_name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("creating the scratch directory");
            Self(dir)
        }

        /// Write a one-segment GPX at the given `HH:MM:SS` times, all at the same
        /// spot so distance never decides anything the test is asking about.
        fn gpx(&self, name: &str, times: &[&str]) -> PathBuf {
            let points: String = times
                .iter()
                .map(|t| {
                    format!(
                        r#"<trkpt lat="47.0" lon="-122.0"><time>2022-01-01T{t}Z</time></trkpt>"#
                    )
                })
                .collect();
            let path = self.0.join(name);
            std::fs::write(
                &path,
                format!(
                    r#"<?xml version="1.0"?><gpx version="1.1" creator="test"><trk><trkseg>{points}</trkseg></trk></gpx>"#
                ),
            )
            .expect("writing the test GPX");
            path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn several_files_load_into_one_index() {
        let dir = ScratchDir::new("load-many");
        let morning = dir.gpx("morning.gpx", &["00:00:00", "00:00:10"]);
        let evening = dir.gpx("evening.gpx", &["00:01:00", "00:01:10"]);

        let track = Track::load(&[morning, evening]).expect("both files are valid and disjoint");

        assert_eq!(track.point_count(), 4);
        // The span is the union, from the first file's start to the last's end.
        assert_eq!(track.span(), (1_640_995_200, 1_640_995_270));
    }

    #[test]
    fn the_seam_between_two_files_is_not_interpolated_across() {
        let dir = ScratchDir::new("seam");
        let morning = dir.gpx("morning.gpx", &["00:00:00", "00:00:10"]);
        let evening = dir.gpx("evening.gpx", &["00:01:00", "00:01:10"]);

        let track = Track::load(&[morning, evening]).expect("both files are valid and disjoint");

        // 00:00:30 sits between the two files: 50 s and 0 m apart, so both the
        // time and distance limits would allow it. Only the segment break stops
        // it — which is the whole point of continuing segment ids across files.
        match track.lookup(1_640_995_230, DEFAULT) {
            Lookup::InGap(gap) => {
                assert!(
                    gap.across_segments,
                    "the seam must read as a segment break, got {gap:?}"
                );
                assert!(gap.seconds <= DEFAULT.max_seconds, "{gap:?}");
                assert!(gap.meters <= DEFAULT.max_meters, "{gap:?}");
            }
            other => panic!("expected InGap across the file seam, got {other:?}"),
        }
    }

    #[test]
    fn loading_overlapping_files_fails_before_a_track_is_built() {
        let dir = ScratchDir::new("overlap");
        let first = dir.gpx("first.gpx", &["00:00:00", "00:01:00"]);
        let second = dir.gpx("second.gpx", &["00:00:30", "00:02:00"]);

        // Not `expect_err`: that needs `Track: Debug`, and deriving it would mean
        // any failure elsewhere dumps every point in a real track.
        let error = match Track::load(&[first, second]) {
            Err(error) => error,
            Ok(_) => panic!("the two files overlap and must be rejected"),
        };

        let rendered = format!("{error:#}");
        assert!(rendered.contains("overlapping"), "{rendered}");
        assert!(rendered.contains("No sidecars were written"), "{rendered}");
    }
}
