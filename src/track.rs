//! The GPX track: load it once, then look positions up by timestamp.
//!
//! The track is built before any sidecar is written and is immutable afterwards,
//! so workers share it as `&Track` with no lock and no contention.
//!
//! Interpolation is refused wherever the data does not actually support it. A
//! wrong geotag looks authoritative and silently corrupts a photo's provenance,
//! whereas a missing one is visibly missing and can be fixed later — so the
//! bracketing points must be close in *both* time and distance, and must come
//! from the same recording run, or the photo is skipped.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use gpx::Waypoint;
use time::OffsetDateTime;

/// One timestamped position from the track.
///
/// Times are Unix seconds: `gpx` speaks `time::OffsetDateTime` and `nom-exif`
/// speaks `chrono::DateTime`, so both sides normalise to this one scalar domain
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
    pub max_metres: f64,
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
    pub metres: f64,
    /// The bracketing points come from different recording runs.
    pub across_segments: bool,
}

/// Every timestamped point in the track, sorted by time and deduplicated.
pub struct Track {
    points: Vec<TrackPoint>,
}

impl Track {
    pub fn load(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening GPX file {}", path.display()))?;
        let gpx = gpx::read(BufReader::new(file))
            .with_context(|| format!("parsing GPX file {}", path.display()))?;

        let mut points = Vec::new();
        let mut segment = 0u32;

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

        Self::new(points).with_context(|| format!("building track index from {}", path.display()))
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
                    metres: distance_metres(before, after),
                    across_segments: before.segment != after.segment,
                };

                if gap.across_segments
                    || gap.seconds > limits.max_seconds
                    || gap.metres > limits.max_metres
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

/// Great-circle distance in metres.
///
/// Haversine is accurate to well under a metre at the scales this is tested
/// against, which is far more than a 100 m threshold needs. It is naturally
/// periodic in longitude, so an antimeridian-crossing pair needs no special case.
fn distance_metres(a: TrackPoint, b: TrackPoint) -> f64 {
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
    // neighbouring longitudes ~360 apart in raw value; interpolating those
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

    /// Generous enough that tests exercising other behaviour are not gated.
    const LENIENT: GapLimits = GapLimits {
        max_seconds: i64::MAX,
        max_metres: f64::INFINITY,
    };

    /// The shipped defaults.
    const DEFAULT: GapLimits = GapLimits {
        max_seconds: 60,
        max_metres: 100.0,
    };

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
                assert!(gap.metres < 100.0, "distance was {}", gap.metres);
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
                assert!(gap.metres > 100.0, "distance was {}", gap.metres);
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
        // 1 second and centimetres apart, but from different recording runs.
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
        // neighbours are far away in both time and distance.
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
        let d = distance_metres(a, b);
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
        let d = distance_metres(a, b);
        assert!(
            (d - 22_239.0).abs() < 200.0,
            "expected ~22 km across the antimeridian, got {d} m"
        );
    }

    #[test]
    fn identical_points_are_zero_metres_apart() {
        let a = point(0, 47.0, -122.0, None);
        assert!(distance_metres(a, a) < 1e-6);
    }
}
