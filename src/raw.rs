//! Capture-time extraction.
//!
//! `MediaParser` pools internal buffers and is worth reusing, but sharing one
//! across threads behind a mutex would serialise the whole run. Callers pass one
//! in; `main` builds a parser per rayon worker with `map_init`.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::FixedOffset;
use nom_exif::{Error as ExifError, Exif, MediaParser, MediaSource};

use crate::format::RawFormat;

/// What reading one file's capture time produced.
pub enum Capture {
    /// Resolved to an absolute instant, in Unix seconds.
    Resolved {
        ts: i64,
        /// Set when EXIF and `--utc-offset` disagree. The EXIF value is used
        /// regardless; this is only reported.
        conflict: Option<OffsetConflict>,
    },
    /// The timestamp carries no zone and no `--utc-offset` was given. This is the
    /// gate condition: guessing here would misplace every photo by the offset.
    NeedsOffset,
    /// No capture tag this format knows about was present.
    NoCaptureTime,
}

/// EXIF recorded a timezone that disagrees with `--utc-offset`.
pub struct OffsetConflict {
    pub exif: FixedOffset,
    pub cli: FixedOffset,
}

/// Read the capture instant from `path`.
///
/// Timezone precedence is **EXIF wins, warn on conflict**: the camera recorded
/// its own zone and is the better authority, so `--utc-offset` only ever fills in
/// for files that have none.
pub fn capture_time(
    parser: &mut MediaParser,
    path: &Path,
    format: RawFormat,
    utc_offset: Option<FixedOffset>,
) -> Result<Capture> {
    let source = MediaSource::open(path).with_context(|| format!("opening {}", path.display()))?;
    let iter = match parser.parse_exif(source) {
        Ok(iter) => iter,
        // No EXIF at all is the same situation for the user as EXIF without a date
        // tag — there is no capture time to correlate — so report it the same way
        // rather than as a hard error that would also change the exit code.
        Err(ExifError::ExifNotFound) => return Ok(Capture::NoCaptureTime),
        Err(error) => {
            return Err(error).with_context(|| format!("reading EXIF from {}", path.display()))
        }
    };
    let exif: Exif = iter.into();

    let Some(datetime) = format
        .capture_tags()
        .iter()
        .find_map(|tag| exif.get(*tag).and_then(|value| value.as_datetime()))
    else {
        return Ok(Capture::NoCaptureTime);
    };

    let conflict = match (datetime.aware(), utc_offset) {
        (Some(aware), Some(cli)) if *aware.offset() != cli => Some(OffsetConflict {
            exif: *aware.offset(),
            cli,
        }),
        _ => None,
    };

    let resolved = match utc_offset {
        // `or_offset` attaches the fallback only to naive values and returns aware
        // ones untouched — the whole precedence rule in one call.
        Some(offset) => datetime.or_offset(offset),
        None => match datetime.aware() {
            Some(aware) => aware,
            None => return Ok(Capture::NeedsOffset),
        },
    };

    Ok(Capture::Resolved {
        ts: resolved.timestamp(),
        conflict,
    })
}
