//! Capture-time extraction.
//!
//! `MediaParser` pools internal buffers and is worth reusing, but sharing one
//! across threads behind a mutex would serialize the whole run. Callers pass one
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

    let Some((datetime, paired_offset)) = format.capture_tags().iter().find_map(|tag| {
        let datetime = exif
            .get(tag.datetime)
            .and_then(|value| value.as_datetime())?;
        let offset = exif
            .get(tag.offset)
            .and_then(|value| value.as_str())
            .and_then(parse_offset);
        Some((datetime, offset))
    }) else {
        return Ok(Capture::NoCaptureTime);
    };

    // The zone the camera recorded. Two shapes have to be handled because nom-exif
    // only merges the offset into the datetime for some containers: a JPEG yields
    // an `Aware` value, while a CR3 yields a `Naive` one plus a separate
    // `OffsetTimeOriginal` text entry. Checking both keeps "EXIF wins" true for
    // every format rather than only the ones that happen to pre-merge.
    let exif_offset = datetime
        .aware()
        .map(|aware| *aware.offset())
        .or(paired_offset);

    let conflict = match (exif_offset, utc_offset) {
        (Some(exif), Some(cli)) if exif != cli => Some(OffsetConflict { exif, cli }),
        _ => None,
    };

    let resolved = match exif_offset.or(utc_offset) {
        // `or_offset` attaches the offset only to naive values and returns aware
        // ones untouched, so an already-aware datetime keeps its own zone.
        Some(offset) => datetime.or_offset(offset),
        None => return Ok(Capture::NeedsOffset),
    };

    Ok(Capture::Resolved {
        ts: resolved.timestamp(),
        conflict,
    })
}

/// Parse an EXIF/CLI UTC offset: `±HH:MM` as EXIF writes it, or `±HHMM` as the
/// command line takes it.
///
/// Hand-rolled because neither time crate already in the tree does this one job:
/// chrono has no `FromStr` for `FixedOffset`, and `time::UtcOffset::parse` wants
/// a format description per shape, so accepting both forms would mean two parse
/// attempts and two descriptions to hold correct.
pub fn parse_offset(text: &str) -> Option<FixedOffset> {
    let (sign, rest) = match text.trim().strip_prefix('+') {
        Some(rest) => (1, rest),
        None => (-1, text.trim().strip_prefix('-')?),
    };

    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    if minutes > 59 {
        return None;
    }

    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_style_offsets_parse() {
        assert_eq!(parse_offset("+00:00"), FixedOffset::east_opt(0));
        assert_eq!(parse_offset("-07:00"), FixedOffset::west_opt(7 * 3600));
        assert_eq!(
            parse_offset("+04:30"),
            FixedOffset::east_opt(4 * 3600 + 30 * 60)
        );
    }

    #[test]
    fn command_line_style_offsets_parse() {
        assert_eq!(parse_offset("-0700"), FixedOffset::west_opt(7 * 3600));
        assert_eq!(
            parse_offset("+0430"),
            FixedOffset::east_opt(4 * 3600 + 30 * 60)
        );
    }

    #[test]
    fn malformed_offsets_are_rejected() {
        for bad in ["", "0700", "-7", "-07000", "-0760", "+abcd", "Z"] {
            assert_eq!(parse_offset(bad), None, "{bad:?} should be rejected");
        }
    }
}
