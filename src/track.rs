//! The GPX track: load it once, then look positions up by timestamp.
//!
//! The track is built before any sidecar is written and is immutable afterwards,
//! so workers share it as `&Track` with no lock and no contention.

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

        // Standalone waypoints count as track points too — some loggers write the
        // whole session that way.
        let points = gpx
            .tracks
            .into_iter()
            .flat_map(|track| track.segments)
            .flat_map(|segment| segment.points)
            .chain(gpx.waypoints)
            .filter_map(track_point)
            .collect();

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

    /// Position at `ts`, or `None` if `ts` falls outside the track.
    ///
    /// Outside the track is deliberately not an error and not clamped: a photo
    /// taken before the logger started has no known location, and inventing one
    /// from the nearest endpoint would be a silent lie.
    pub fn lookup(&self, ts: i64) -> Option<Fix> {
        match self.points.binary_search_by_key(&ts, |point| point.ts) {
            Ok(i) => {
                let point = self.points[i];
                Some(Fix {
                    lat: point.lat,
                    lon: point.lon,
                    ele: point.ele,
                })
            }
            // Before the first point or after the last.
            Err(i) if i == 0 || i == self.points.len() => None,
            Err(i) => Some(interpolate(self.points[i - 1], self.points[i], ts)),
        }
    }
}

fn track_point(waypoint: Waypoint) -> Option<TrackPoint> {
    // Read the borrowing accessors before moving `time` out of the waypoint.
    let point = waypoint.point();
    let ele = waypoint.elevation;
    let ts = OffsetDateTime::from(waypoint.time?).unix_timestamp();

    Some(TrackPoint {
        ts,
        lat: point.y(),
        lon: point.x(),
        ele,
    })
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

    fn point(ts: i64, lat: f64, lon: f64, ele: Option<f64>) -> TrackPoint {
        TrackPoint { ts, lat, lon, ele }
    }

    fn track(points: Vec<TrackPoint>) -> Track {
        Track::new(points).expect("test track should be valid")
    }

    #[test]
    fn exact_timestamp_uses_that_point() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -123.0, Some(200.0)),
        ]);

        assert_eq!(
            track.lookup(2000),
            Some(Fix {
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

        let fix = track.lookup(1500).expect("1500 is inside the track");
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

        let fix = track.lookup(100).expect("100 is inside the track");
        assert!((fix.lat - 2.5).abs() < 1e-9, "lat was {}", fix.lat);
        assert!((fix.lon - 5.0).abs() < 1e-9, "lon was {}", fix.lon);
        assert!((fix.ele.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn before_first_and_after_last_are_skipped() {
        let track = track(vec![
            point(1000, 47.0, -122.0, None),
            point(2000, 48.0, -123.0, None),
        ]);

        assert_eq!(track.lookup(999), None);
        assert_eq!(track.lookup(2001), None);
    }

    #[test]
    fn antimeridian_crossing_takes_the_shortest_arc() {
        let track = track(vec![
            point(1000, 0.0, 179.0, None),
            point(2000, 0.0, -179.0, None),
        ]);

        let fix = track.lookup(1500).expect("1500 is inside the track");
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

        let fix = track.lookup(1500).expect("1500 is inside the track");
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

        let fix = track.lookup(1500).expect("1500 is inside the track");
        assert!((fix.lon - 0.0).abs() < 1e-9, "lon was {}", fix.lon);
    }

    #[test]
    fn missing_elevation_on_one_end_suppresses_altitude() {
        let track = track(vec![
            point(1000, 47.0, -122.0, Some(100.0)),
            point(2000, 48.0, -123.0, None),
        ]);

        assert_eq!(track.lookup(1500).map(|fix| fix.ele), Some(None));
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

        assert!(track.lookup(1000).is_some());
        assert_eq!(track.lookup(1001), None);
        assert_eq!(track.lookup(999), None);
    }
}
