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
use chrono::{DateTime, TimeDelta, Utc};
use gpx::Waypoint;
use rayon::prelude::*;
use time::OffsetDateTime;

/// One timestamped position from the track.
///
/// Instants are `chrono::DateTime<Utc>` throughout. Two upstream crates disagree
/// about time types — `gpx` returns `time::OffsetDateTime`, `nom-exif` returns
/// `chrono` — and chrono wins because it is the one *we* already depend on
/// directly and the one nom-exif hands us for free. `gpx`'s type is converted at
/// exactly one place, `track_point` below.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackPoint {
    pub at: DateTime<Utc>,
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
    pub max_gap: TimeDelta,
    pub max_meters: f64,
}

impl GapLimits {
    /// The shipped defaults, and the single place they are written down.
    ///
    /// `--max-gap` and `--max-distance` take their `default_value_t` from
    /// `DEFAULT_GAP_SECONDS` and `DEFAULT.max_meters`, and the tests below
    /// exercise the same values, so the tool and its tests cannot drift apart the
    /// way a hand-copied constant would.
    ///
    /// Both limits are load-bearing and neither implies the other — see the gap
    /// rule in CLAUDE.md before changing either.
    pub const DEFAULT: Self = Self {
        max_gap: TimeDelta::seconds(Self::DEFAULT_GAP_SECONDS),
        max_meters: 100.0,
    };

    /// The gap limit in whole seconds, for the CLI.
    ///
    /// `--max-gap` is a number of seconds to the user and a `TimeDelta` to the
    /// rest of the code; this is the one place the two representations meet.
    pub const DEFAULT_GAP_SECONDS: i64 = 60;
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
    pub duration: TimeDelta,
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
        // Parsing runs in parallel because the files are wholly independent and
        // the XML is slow: seven tracks of a real trip (15 MB, 76k points) cost
        // ~700 ms here, all of it before a single photo is touched.
        //
        // Everything after the parse runs in argument order, so nothing a user
        // sees depends on which worker finished first — not the segment ids, not
        // which of several bad files gets reported, not the overlap message.
        let parsed: Vec<Result<ParsedFile>> =
            paths.par_iter().map(|path| read_file(path)).collect();

        let mut points = Vec::new();
        let mut spans: Vec<(&Path, DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        let mut next_segment = 0u32;

        for (path, parsed) in paths.iter().zip(parsed) {
            // `?` here rather than on the collect above: when several files are
            // unreadable, the one reported is the first on the command line
            // instead of whichever worker failed first.
            let ParsedFile {
                mut points_in_file,
                segment_count,
            } = parsed?;

            // Segment ids arrive numbered from zero within each file; sliding them
            // onto a running total is what makes a seam between two files a
            // segment break rather than a contiguous pair.
            for point in &mut points_in_file {
                point.segment += next_segment;
            }
            next_segment += segment_count;

            if let Some((first, last)) = span_of(&points_in_file) {
                spans.push((path.as_path(), first, last));
            }

            points.append(&mut points_in_file);
        }

        ensure_no_overlap(&spans)?;

        Self::new(points).with_context(|| {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            format!("building track index from {}", names.join(", "))
        })
    }

    /// Sort and deduplicate into a lookup-ready index.
    pub fn new(mut points: Vec<TrackPoint>) -> Result<Self> {
        points.sort_by_key(|point| point.at);
        points.dedup_by_key(|point| point.at);

        ensure!(
            !points.is_empty(),
            "the track contains no points with timestamps"
        );

        Ok(Self { points })
    }

    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// The track's time span, inclusive of both ends.
    pub fn span(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        // `new` rejects an empty track, so both ends exist.
        let ends = (self.points.first(), self.points.last());
        match ends {
            (Some(first), Some(last)) => (first.at, last.at),
            _ => unreachable!("Track::new rejects an empty track"),
        }
    }

    /// Position at `at`, if the track genuinely supports one there.
    pub fn lookup(&self, at: DateTime<Utc>, limits: GapLimits) -> Lookup {
        match self.points.binary_search_by_key(&at, |point| point.at) {
            // An exact hit is a recorded observation, not an interpolation, so no
            // gap test applies.
            Ok(i) => Lookup::Found(fix(self.points[i])),
            Err(i) if i == 0 || i == self.points.len() => Lookup::OutsideTrack,
            Err(i) => {
                let (before, after) = (self.points[i - 1], self.points[i]);

                let gap = Gap {
                    duration: after.at - before.at,
                    meters: distance_meters(before, after),
                    across_segments: before.segment != after.segment,
                };

                if gap.across_segments
                    || gap.duration > limits.max_gap
                    || gap.meters > limits.max_meters
                {
                    Lookup::InGap(gap)
                } else {
                    Lookup::Found(interpolate(before, after, at))
                }
            }
        }
    }
}

/// The one place `gpx`'s time type crosses into ours.
///
/// `time::OffsetDateTime` in, `chrono::DateTime<Utc>` out, via the Unix epoch —
/// which both crates agree on, so nothing is lost. Keeping the conversion to this
/// single function is what stops the two time crates from spreading.
fn track_point(waypoint: Waypoint, segment: u32) -> Option<TrackPoint> {
    // Read the borrowing accessors before moving `time` out of the waypoint.
    let point = waypoint.point();
    let ele = waypoint.elevation;
    let at = DateTime::from_timestamp(OffsetDateTime::from(waypoint.time?).unix_timestamp(), 0)?;

    Some(TrackPoint {
        at,
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

/// One GPX file's points, numbered as if it were the only file given.
struct ParsedFile {
    points_in_file: Vec<TrackPoint>,
    /// How far `load` must advance the running segment id before the next file.
    ///
    /// Not the same as the highest id actually used: a file whose standalone-
    /// waypoint run is empty still consumes an id for it. Keeping it that way is
    /// what stops two files from ever sharing a segment id.
    segment_count: u32,
}

/// Read every timestamped point from one GPX file.
///
/// Segments are numbered from zero, local to this file, so that files can be
/// parsed in any order; `load` rebases them onto a running total afterwards.
fn read_file(path: &Path) -> Result<ParsedFile> {
    let file = File::open(path).with_context(|| format!("opening GPX file {}", path.display()))?;
    let gpx = gpx::read(BufReader::new(file))
        .with_context(|| format!("parsing GPX file {}", path.display()))?;

    let mut points_in_file = Vec::new();
    let mut segment = 0u32;

    for track in gpx.tracks {
        for track_segment in track.segments {
            points_in_file.extend(
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
    points_in_file.extend(
        gpx.waypoints
            .into_iter()
            .filter_map(|waypoint| track_point(waypoint, segment)),
    );

    Ok(ParsedFile {
        points_in_file,
        segment_count: segment + 1,
    })
}

/// Earliest and latest instant among `points`, or `None` if there are none.
fn span_of(points: &[TrackPoint]) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let first = points.iter().map(|p| p.at).min()?;
    let last = points.iter().map(|p| p.at).max()?;
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
fn ensure_no_overlap(spans: &[(&Path, DateTime<Utc>, DateTime<Utc>)]) -> Result<()> {
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
///
/// Deliberately not `geo::Haversine`, despite `geo-types` already arriving with
/// `gpx`: that crate brings a whole computational-geometry stack — spade, rstar,
/// i_overlay, earcut — for this one function. The result is only ever compared
/// against a threshold and is never written to a sidecar, so the exactness a
/// geodesic solver would add has nothing to buy here. If that ever changes,
/// `geographiclib-rs` is exact and costs far less than `geo`.
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
fn interpolate(a: TrackPoint, b: TrackPoint, at: DateTime<Utc>) -> Fix {
    // `Track::new` deduplicates by instant, so `b.at > a.at` and the denominator
    // is never zero.
    let fraction = (at - a.at).as_seconds_f64() / (b.at - a.at).as_seconds_f64();

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
        max_gap: TimeDelta::MAX,
        max_meters: f64::INFINITY,
    };

    /// Test data stays written in plain Unix seconds — terse and easy to eyeball —
    /// and becomes a real instant here, at the one place it enters the code.
    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("a representable test instant")
    }

    /// The shipped defaults, read from the one place that defines them, so these
    /// tests always exercise what the CLI actually hands to `lookup`.
    const DEFAULT: GapLimits = GapLimits::DEFAULT;

    fn point(ts: i64, lat: f64, lon: f64, ele: Option<f64>) -> TrackPoint {
        TrackPoint {
            at: at(ts),
            lat,
            lon,
            ele,
            segment: 0,
        }
    }

    fn in_segment(ts: i64, lat: f64, lon: f64, segment: u32) -> TrackPoint {
        TrackPoint {
            at: at(ts),
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
            track.lookup(at(2000), LENIENT),
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

        let fix = found(track.lookup(at(1500), LENIENT));
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

        let fix = found(track.lookup(at(100), LENIENT));
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

        assert_eq!(track.lookup(at(999), LENIENT), Lookup::OutsideTrack);
        assert_eq!(track.lookup(at(2001), LENIENT), Lookup::OutsideTrack);
    }

    #[test]
    fn antimeridian_crossing_takes_the_shortest_arc() {
        let track = track(vec![
            point(1000, 0.0, 179.0, None),
            point(2000, 0.0, -179.0, None),
        ]);

        let fix = found(track.lookup(at(1500), LENIENT));
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

        let fix = found(track.lookup(at(1500), LENIENT));
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

        let fix = found(track.lookup(at(1500), LENIENT));
        assert!((fix.lon - 0.0).abs() < 1e-9, "lon was {}", fix.lon);
    }

    #[test]
    fn missing_elevation_on_one_end_suppresses_altitude() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -123.0, None),
        ]);

        assert_eq!(found(track.lookup(at(1500), LENIENT)).ele, None);
    }

    #[test]
    fn points_are_sorted_and_deduplicated() {
        let track = track(vec![
            point(2000, 48.0, -123.0, None),
            point(1000, 47.0, -122.0, None),
            point(2000, 99.0, -99.0, None),
        ]);

        assert_eq!(track.point_count(), 2);
        assert_eq!(track.span(), (at(1000), at(2000)));
    }

    #[test]
    fn an_empty_track_is_rejected() {
        assert!(Track::new(Vec::new()).is_err());
    }

    #[test]
    fn a_single_point_track_only_matches_exactly() {
        let track = track(vec![point(1000, 47.0, -122.0, None)]);

        assert!(matches!(track.lookup(at(1000), LENIENT), Lookup::Found(_)));
        assert_eq!(track.lookup(at(1001), LENIENT), Lookup::OutsideTrack);
        assert_eq!(track.lookup(at(999), LENIENT), Lookup::OutsideTrack);
    }

    // ---- gap rejection ------------------------------------------------------

    #[test]
    fn a_time_gap_beyond_the_limit_is_not_bridged() {
        // 61s apart but only ~1 m, so only the time limit is exceeded.
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1061, 47.00001, -122.0, None),
        ]);

        match track.lookup(at(1030), DEFAULT) {
            Lookup::InGap(gap) => {
                assert_eq!(gap.duration.num_seconds(), 61);
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

        match track.lookup(at(1005), DEFAULT) {
            Lookup::InGap(gap) => {
                assert_eq!(gap.duration.num_seconds(), 10);
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

        let fix = found(track.lookup(at(1005), DEFAULT));
        assert!((fix.lat - 47.00005).abs() < 1e-9, "lat was {}", fix.lat);
    }

    #[test]
    fn a_gap_exactly_at_the_time_limit_is_still_bridged() {
        // The limit is a maximum, not a threshold to fall short of: the check is
        // `>`, so exactly 60 s interpolates and 61 s does not. Worth pinning
        // because the comparison changed type — it was `i64 > i64` and is now
        // `TimeDelta > TimeDelta` — and an off-by-one here silently changes which
        // photos get tagged rather than failing loudly.
        let seconds = DEFAULT.max_gap.num_seconds();
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1000 + seconds, 47.00001, -122.0, None),
        ]);

        match track.lookup(at(1000 + seconds / 2), DEFAULT) {
            Lookup::Found(_) => {}
            other => panic!("a gap of exactly {seconds}s must still bridge, got {other:?}"),
        }
    }

    #[test]
    fn one_second_past_the_time_limit_is_not_bridged() {
        // The other side of the same boundary, so the pair brackets it exactly.
        let seconds = DEFAULT.max_gap.num_seconds();
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(1000 + seconds + 1, 47.00001, -122.0, None),
        ]);

        match track.lookup(at(1000 + seconds / 2), DEFAULT) {
            Lookup::InGap(gap) => assert_eq!(gap.duration.num_seconds(), seconds + 1),
            other => panic!("a gap of {}s must be refused, got {other:?}", seconds + 1),
        }
    }

    #[test]
    fn a_segment_boundary_is_never_bridged_however_close_the_points_are() {
        // 1 second and centimeters apart, but from different recording runs.
        let adjacent = track(vec![
            in_segment(1000, 47.0, -122.0, 0),
            in_segment(1001, 47.0000001, -122.0, 1),
        ]);

        match adjacent.lookup(at(1000), DEFAULT) {
            // 1000 is an exact hit, so it resolves.
            Lookup::Found(_) => {}
            other => panic!("exact hit should resolve, got {other:?}"),
        }

        // Nothing strictly between them can be, though.
        let spanning = track(vec![
            in_segment(1000, 47.0, -122.0, 0),
            in_segment(1010, 47.0000001, -122.0, 1),
        ]);
        match spanning.lookup(at(1005), DEFAULT) {
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

        assert!(matches!(track.lookup(at(5000), DEFAULT), Lookup::Found(_)));
        assert!(matches!(track.lookup(at(4999), DEFAULT), Lookup::InGap(_)));
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

    fn span(name: &str, start: i64, end: i64) -> (&Path, DateTime<Utc>, DateTime<Utc>) {
        (Path::new(name), at(start), at(end))
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
            let dir = std::env::temp_dir().join(format!(
                "rawgeotag-track-{}-{test_name}",
                std::process::id()
            ));
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

        /// A GPX holding a track segment *and* standalone waypoints, so the file
        /// consumes two segment ids rather than one.
        fn gpx_with_waypoints(&self, name: &str, trk: &[&str], wpt: &[&str]) -> PathBuf {
            let point = |tag: &str, times: &[&str]| -> String {
                times
                    .iter()
                    .map(|t| {
                        format!(
                            r#"<{tag} lat="47.0" lon="-122.0"><time>2022-01-01T{t}Z</time></{tag}>"#
                        )
                    })
                    .collect()
            };
            let path = self.0.join(name);
            std::fs::write(
                &path,
                format!(
                    r#"<?xml version="1.0"?><gpx version="1.1" creator="test">{}<trk><trkseg>{}</trkseg></trk></gpx>"#,
                    point("wpt", wpt),
                    point("trkpt", trk)
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
        assert_eq!(track.span(), (at(1_640_995_200), at(1_640_995_270)));
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
        match track.lookup(at(1_640_995_230), DEFAULT) {
            Lookup::InGap(gap) => {
                assert!(
                    gap.across_segments,
                    "the seam must read as a segment break, got {gap:?}"
                );
                assert!(
                    gap.duration.num_seconds() <= DEFAULT.max_gap.num_seconds(),
                    "{gap:?}"
                );
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

    /// The rebasing arithmetic, on the one shape that can actually expose it.
    ///
    /// A file's standalone waypoints occupy a segment id of their own, so a file
    /// with both a track and waypoints consumes *two*. If `segment_count` counted
    /// only the ids visibly used, the next file would be rebased one too low and
    /// land on top of this file's waypoint run — making the seam between two
    /// separate recordings look like one contiguous segment.
    #[test]
    fn a_files_waypoints_consume_a_segment_id_of_their_own() {
        let dir = ScratchDir::new("waypoint-segment");
        let first = dir.gpx_with_waypoints(
            "first.gpx",
            &["00:00:00", "00:00:10"],
            &["00:00:20", "00:00:30"],
        );
        let second = dir.gpx("second.gpx", &["00:01:00", "00:01:10"]);

        let track = Track::load(&[first, second]).expect("both files are valid and disjoint");
        assert_eq!(track.point_count(), 6);

        // Both holes below are 0 m apart and within `max_gap`, so nothing but the
        // segment ids can reject them.
        for (ts, what) in [
            (
                1_640_995_215,
                "the track-to-waypoint break inside the first file",
            ),
            (
                1_640_995_245,
                "the seam between the first file's waypoints and the second file",
            ),
        ] {
            match track.lookup(at(ts), DEFAULT) {
                Lookup::InGap(gap) => assert!(
                    gap.across_segments,
                    "{what} must read as a segment break, got {gap:?}"
                ),
                other => panic!("expected InGap at {what}, got {other:?}"),
            }
        }
    }

    /// Parsing runs in parallel, so which worker fails first is a race. The file
    /// reported must still be the first one on the command line.
    #[test]
    fn the_first_unreadable_file_is_reported_whichever_worker_fails_first() {
        let dir = ScratchDir::new("bad-files");
        let missing = [dir.0.join("aaa-missing.gpx"), dir.0.join("zzz-missing.gpx")];

        // Repeated because this is a scheduling property: one pass could pick the
        // right file by luck.
        for _ in 0..20 {
            let error = match Track::load(&missing) {
                Err(error) => error,
                Ok(_) => panic!("neither file exists"),
            };

            let rendered = format!("{error:#}");
            assert!(rendered.contains("aaa-missing"), "{rendered}");
            assert!(!rendered.contains("zzz-missing"), "{rendered}");
        }
    }
}
