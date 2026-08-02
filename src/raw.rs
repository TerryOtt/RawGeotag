//! Capture-time extraction.
//!
//! `MediaParser` pools internal buffers and is worth reusing, but sharing one
//! across threads behind a mutex would serialize the whole run. Callers pass one
//! in; `main` builds a parser per rayon worker with `map_init`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use nom_exif::{Error as ExifError, Exif, ExifDateTime, MediaParser, MediaSource};

use crate::format::{RawFormat, ReadStrategy};

/// What reading one file's capture time produced.
pub enum Capture {
    /// Resolved to an absolute instant.
    Resolved {
        at: DateTime<Utc>,
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
#[derive(Debug, PartialEq, Eq)]
pub struct OffsetConflict {
    pub exif: FixedOffset,
    pub cli: FixedOffset,
}

/// Which offset to attach to a capture time, and what to report about it.
#[derive(Debug, PartialEq, Eq)]
enum OffsetChoice {
    /// Attach this offset. `conflict` is set when EXIF and `--utc-offset`
    /// disagreed; the EXIF value is the one applied either way.
    Apply {
        offset: FixedOffset,
        conflict: Option<OffsetConflict>,
    },
    /// Refuse the whole run. Nothing states what zone this timestamp is in, and
    /// guessing could misplace the photo by a whole day of travel.
    Gate,
}

/// The timezone policy in one place: **EXIF wins, `--utc-offset` fills in, and
/// neither one means the run is refused.**
///
/// Split out of `capture_time` so the rule can be tested exhaustively without a
/// fixture file per format — most of all the `Gate` branch, where the correct
/// behavior is that not one sidecar gets written.
fn choose_offset(exif: Option<FixedOffset>, cli: Option<FixedOffset>) -> OffsetChoice {
    match (exif, cli) {
        // The camera recorded its own zone and is the better authority, so the
        // CLI value never overrides it — it is only reported as a disagreement.
        (Some(exif), Some(cli)) => OffsetChoice::Apply {
            offset: exif,
            conflict: (exif != cli).then_some(OffsetConflict { exif, cli }),
        },
        (Some(exif), None) => OffsetChoice::Apply {
            offset: exif,
            conflict: None,
        },
        (None, Some(cli)) => OffsetChoice::Apply {
            offset: cli,
            conflict: None,
        },
        (None, None) => OffsetChoice::Gate,
    }
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
    // The two arms build different `MediaSource` types, so they cannot be joined
    // before the parse — they converge on its result instead.
    let parsed = match format.read_strategy() {
        ReadStrategy::Streaming => {
            let source =
                MediaSource::open(path).with_context(|| format!("opening {}", path.display()))?;
            parser.parse_exif(source)
        }
        ReadStrategy::WholeFile => {
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let source = MediaSource::from_memory(bytes)
                .with_context(|| format!("reading {}", path.display()))?;
            parser.parse_exif(source)
        }
    };

    let iter = match parsed {
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

    let (offset, conflict) = match choose_offset(exif_offset(&datetime, paired_offset), utc_offset)
    {
        OffsetChoice::Apply { offset, conflict } => (offset, conflict),
        OffsetChoice::Gate => return Ok(Capture::NeedsOffset),
    };

    Ok(Capture::Resolved {
        // `or_offset` attaches the offset only to naive values and returns aware
        // ones untouched, so an already-aware datetime keeps its own zone. The
        // shift to UTC is then a pure change of representation, not of instant.
        at: datetime.or_offset(offset).with_timezone(&Utc),
        conflict,
    })
}

/// The zone the camera recorded, from whichever shape nom-exif produced.
///
/// Two shapes have to be handled because nom-exif only merges the offset into the
/// datetime for some containers: a JPEG yields an `Aware` value, while a CR3 yields
/// a `Naive` one plus a separate `OffsetTimeOriginal` text entry. Checking both
/// keeps "EXIF wins" true for every format rather than only the ones that happen to
/// pre-merge. That is the CR3 timezone trap; see CLAUDE.md before touching it.
///
/// **Split out of `capture_time` because no fixture can reach the `Aware` arm.**
/// Every CR3 and NEF this tool reads comes back `Naive`, so that half is exercised
/// by nothing on disk — deleting it passes all three fixtures. The tests below are
/// the only thing holding it.
fn exif_offset(datetime: &ExifDateTime, paired: Option<FixedOffset>) -> Option<FixedOffset> {
    datetime.aware().map(|aware| *aware.offset()).or(paired)
}

/// Parse an EXIF/CLI UTC offset: `±HH:MM` as EXIF writes it, or `±HHMM` as the
/// command line takes it.
///
/// Hand-rolled because neither time crate already in the tree does this one job:
/// chrono has no `FromStr` for `FixedOffset`, and `time::UtcOffset::parse` wants
/// a format description per shape, so accepting both forms would mean two parse
/// attempts and two descriptions to hold correct.
pub fn parse_offset(text: &str) -> Option<FixedOffset> {
    let text = text.trim();
    let (sign, rest) = match text.strip_prefix('+') {
        Some(rest) => (1, rest),
        None => (-1, text.strip_prefix('-')?),
    };

    // A colon is tolerated only where EXIF actually puts one. Filtering colons out
    // wherever they appear — the previous shape — accepted `+0:0:00` and `+::0700`
    // as valid offsets, and a bad offset is a whole shoot in the wrong place.
    let (hours, minutes) = match rest.split_once(':') {
        Some(halves) => halves,
        None if rest.len() == 4 => rest.split_at(2),
        None => return None,
    };

    let hours = two_digits(hours)?;
    let minutes = two_digits(minutes)?;
    // Both bounds stated here rather than leaving hours to be caught incidentally
    // by `east_opt` rejecting anything past 23:59:59.
    if hours > 23 || minutes > 59 {
        return None;
    }

    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

/// Exactly two ASCII digits as a number: `"07"` is 7, while `"7"`, `"7a"` and
/// `"+7"` are all rejected.
fn two_digits(text: &str) -> Option<i32> {
    if text.len() == 2 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone};

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

    /// A colon belongs between the hours and the minutes, nowhere else. These all
    /// parsed before, because the old shape stripped every colon first and then
    /// looked at whatever four digits were left.
    #[test]
    fn colons_are_only_accepted_between_the_hours_and_the_minutes() {
        for bad in ["+0:0:00", "+::0700", "+07:0:0", "+:0700", "+0700:"] {
            assert_eq!(parse_offset(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn out_of_range_hours_and_minutes_are_rejected() {
        assert_eq!(parse_offset("+2400"), None);
        assert_eq!(parse_offset("+9900"), None);
        assert_eq!(parse_offset("+0060"), None);
        // The extremes either side are still valid.
        assert_eq!(
            parse_offset("+23:59"),
            FixedOffset::east_opt(23 * 3600 + 59 * 60)
        );
        assert_eq!(
            parse_offset("-23:59"),
            FixedOffset::west_opt(23 * 3600 + 59 * 60)
        );
    }

    // ---- the CR3 timezone trap ----------------------------------------------
    //
    // These are the only thing holding the `Aware` arm of `exif_offset`. Every CR3
    // and NEF nom-exif hands back is `Naive`, so no fixture reaches it: a version
    // of the function that ignored `datetime` entirely and returned the paired tag
    // was measured to pass all 81 tests and all three fixture aggregates. JPEG is
    // the format that yields `Aware`, and this tool does not read JPEG.

    fn aware(hours: i32) -> ExifDateTime {
        ExifDateTime::Aware(
            east(hours)
                .with_ymd_and_hms(2025, 9, 18, 6, 52, 3)
                .single()
                .expect("a valid test instant"),
        )
    }

    fn naive() -> ExifDateTime {
        ExifDateTime::Naive(
            NaiveDate::from_ymd_opt(2025, 9, 18)
                .expect("a valid test date")
                .and_hms_opt(6, 52, 3)
                .expect("a valid test time"),
        )
    }

    #[test]
    fn a_merged_aware_timestamp_supplies_its_own_offset() {
        assert_eq!(exif_offset(&aware(2), None), Some(east(2)));
    }

    /// The case nothing else can reach: both shapes present and disagreeing. The
    /// merged value is the camera's own and must win over the separate tag.
    #[test]
    fn an_aware_timestamp_wins_over_a_paired_offset_tag() {
        assert_eq!(exif_offset(&aware(2), Some(east(-7))), Some(east(2)));
    }

    /// The CR3 and NEF shape: naive timestamp, offset in a separate entry.
    #[test]
    fn a_naive_timestamp_falls_back_to_the_paired_offset_tag() {
        assert_eq!(exif_offset(&naive(), Some(east(1))), Some(east(1)));
    }

    /// The D3300 shape: naive and no offset tag at all, so nothing states the
    /// zone and `choose_offset` must gate.
    #[test]
    fn a_naive_timestamp_with_no_paired_tag_has_no_offset() {
        assert_eq!(exif_offset(&naive(), None), None);
        assert_eq!(
            choose_offset(exif_offset(&naive(), None), None),
            OffsetChoice::Gate
        );
    }

    /// `capture_time` relies on `or_offset` leaving an already-aware value alone —
    /// that is what makes "EXIF wins" true rather than just claimed. It is a
    /// dependency's contract, unreachable by any fixture, so a nom-exif upgrade
    /// that changed it would otherwise be silent.
    #[test]
    fn or_offset_does_not_override_a_timestamp_that_already_has_a_zone() {
        let resolved = aware(2).or_offset(east(-7));
        assert_eq!(*resolved.offset(), east(2));
        assert_eq!(
            resolved.with_timezone(&Utc).to_string(),
            "2025-09-18 04:52:03 UTC"
        );
    }

    // ---- the gate rule ------------------------------------------------------
    //
    // These four cases are the whole timezone policy. The first is the one that
    // matters most: a Nikon D3300 writes no `OffsetTimeOriginal` at all, so every
    // file from that body lands there, and getting it wrong would silently tag a
    // whole shoot with positions off by the missing offset.

    fn east(hours: i32) -> FixedOffset {
        FixedOffset::east_opt(hours * 3600).expect("a valid offset")
    }

    #[test]
    fn no_exif_zone_and_no_cli_offset_gates_the_run() {
        // Nothing on earth says what zone this timestamp is in. Refusing is the
        // only honest answer — a guess could misplace the photo by a day of
        // travel, and `main` turns this into "no sidecars were written".
        assert_eq!(choose_offset(None, None), OffsetChoice::Gate);
    }

    #[test]
    fn the_cli_offset_fills_in_when_exif_has_no_zone() {
        assert_eq!(
            choose_offset(None, Some(east(-7))),
            OffsetChoice::Apply {
                offset: east(-7),
                conflict: None,
            }
        );
    }

    #[test]
    fn exif_alone_is_enough_and_needs_no_cli_offset() {
        assert_eq!(
            choose_offset(Some(east(1)), None),
            OffsetChoice::Apply {
                offset: east(1),
                conflict: None,
            }
        );
    }

    #[test]
    fn exif_beats_the_cli_offset_and_the_disagreement_is_reported() {
        // EXIF wins — the camera recorded its own zone — but the user is told,
        // because one of the two is wrong and it is not ours to decide which.
        assert_eq!(
            choose_offset(Some(east(1)), Some(east(-7))),
            OffsetChoice::Apply {
                offset: east(1),
                conflict: Some(OffsetConflict {
                    exif: east(1),
                    cli: east(-7),
                }),
            }
        );
    }

    #[test]
    fn agreeing_offsets_are_not_reported_as_a_conflict() {
        assert_eq!(
            choose_offset(Some(east(1)), Some(east(1))),
            OffsetChoice::Apply {
                offset: east(1),
                conflict: None,
            }
        );
    }
}
