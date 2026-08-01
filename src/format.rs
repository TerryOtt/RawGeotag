//! The set of raw formats we know how to read a capture time from.
//!
//! This is the extension point for new formats. What actually varies between raw
//! formats is much less than it appears: `nom-exif` dispatches on file *content*
//! rather than extension, so adding a format usually means adding a row to the
//! tables below and nothing else.
//!
//! Adding a format is: add the variant, let the compiler flag every `match` that
//! stopped compiling, fill in those arms, add a fixture test. Forgetting a spot is
//! a build error rather than a runtime surprise — which is exactly the property a
//! trait-object registry or a `HashMap` of handlers would throw away.
//!
//! When two formats end up with identical arms that is not duplication to factor
//! away; it is the code stating plainly that the formats do not differ. Collapse
//! them with `Self::Cr3 | Self::Raf => ...` and split the arm when one diverges.
//!
//! **What limits this table is `nom-exif`, not the table.** It reads CR3, RAF,
//! IIQ and TIFF; ARW, DNG, ORF, PEF and RW2 are not on its list, and adding a row
//! does not conjure a parser.
//!
//! NEF is the instructive case, and the reason `read_strategy` exists. It parses
//! through `MediaSource::from_memory` but *not* through `MediaSource::open` — so
//! supporting it took a genuine second column here, not just a row. That is what
//! this table is for: the variation was real, it was data, and it went in the data
//! table. Note the asymmetry it introduces — `WholeFile` reads every byte of every
//! photo, where `Streaming` reads a header — so a NEF run is I/O-shaped quite
//! differently from a CR3 one. CLAUDE.md has the measurements.
//!
//! For the formats nom-exif cannot read at all, the pure-Rust alternative is
//! `rawler` (`Decoder::raw_metadata()` reads metadata without decoding the image);
//! it costs ~106 transitive crates, including a complete JPEG-XL decoder, which is
//! why it is not here for a Canon-only tool. Reach for it when a second camera
//! system actually arrives — not before, and not as a second parser alongside
//! nom-exif.

use nom_exif::ExifTag;

/// A capture-time tag paired with the tag carrying its UTC offset.
///
/// EXIF stores the two separately, and nom-exif surfaces them that way rather than
/// merging them, so the pairing has to be stated explicitly. The spec pairs
/// `DateTimeOriginal` with `OffsetTimeOriginal` and `CreateDate` (a.k.a.
/// `DateTimeDigitized`) with `OffsetTimeDigitized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTag {
    pub datetime: ExifTag,
    pub offset: ExifTag,
}

/// How a format's bytes have to reach the parser.
///
/// This exists because nom-exif does not handle every container the same way, and
/// the difference is not cosmetic — it is the difference between reading a header
/// and reading the whole file. Guessing wrong is not subtle: the wrong strategy
/// fails outright rather than silently misreading, which is the failure mode to
/// prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStrategy {
    /// Let the parser seek within the file and read only what it needs. Cheap —
    /// a ~30 MB CR3 costs a few header reads.
    Streaming,
    /// Hand the parser the entire file.
    ///
    /// Required by the TIFF-based raws: their EXIF IFD sits past what the
    /// streaming reader buffers, and that path reports needing more bytes as
    /// malformed data rather than asking for more, so it never recovers. Costs a
    /// full-file read per photo — ~22 MB for a D3300 NEF.
    WholeFile,
}

/// A raw format we know how to read a capture time from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    Cr3,
    Nef,
}

impl RawFormat {
    /// Every supported format, in help-text order.
    pub const ALL: &'static [RawFormat] = &[Self::Cr3, Self::Nef];

    /// Extensions that select this format, lowercase.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Cr3 => &["cr3"],
            Self::Nef => &["nef"],
        }
    }

    /// How to hand this format's bytes to the parser.
    ///
    /// Verified against real files of each format, which is the only way to know:
    /// see the NEF section of CLAUDE.md for what the wrong choice looks like.
    pub fn read_strategy(self) -> ReadStrategy {
        match self {
            Self::Cr3 => ReadStrategy::Streaming,
            Self::Nef => ReadStrategy::WholeFile,
        }
    }

    /// Capture-time tags to try, in priority order.
    pub fn capture_tags(self) -> &'static [CaptureTag] {
        match self {
            // Collapsed deliberately: these two formats genuinely do not differ
            // here, and one arm says so more plainly than two identical ones.
            // Split it the moment one of them diverges.
            //
            // The pairing is the spec's, not a guess about any particular body.
            // A D3300 happens to write no `OffsetTimeOriginal` at all — those
            // files reach the `NeedsOffset` gate and need `--utc-offset` — but
            // other Nikon bodies do write it, so the tag belongs here regardless.
            Self::Cr3 | Self::Nef => &[
                CaptureTag {
                    datetime: ExifTag::DateTimeOriginal,
                    offset: ExifTag::OffsetTimeOriginal,
                },
                CaptureTag {
                    datetime: ExifTag::CreateDate,
                    offset: ExifTag::OffsetTimeDigitized,
                },
            ],
        }
    }

    /// Resolve a user-supplied extension, tolerating case and a leading dot.
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim_start_matches('.');
        Self::ALL.iter().copied().find(|format| {
            format
                .extensions()
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
    }

    /// Every supported extension, for `--help` and for the error shown when an
    /// unsupported one is given.
    pub fn supported_extensions() -> String {
        Self::ALL
            .iter()
            .flat_map(|format| format.extensions())
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared extension must round-trip, in any case. This is the one gap
    /// the compiler cannot catch: a new variant with no table entry still builds.
    #[test]
    fn every_declared_extension_round_trips() {
        for format in RawFormat::ALL {
            assert!(
                !format.extensions().is_empty(),
                "{format:?} declares no extensions"
            );

            for ext in format.extensions() {
                assert_eq!(RawFormat::from_extension(ext), Some(*format));
                assert_eq!(
                    RawFormat::from_extension(&ext.to_uppercase()),
                    Some(*format)
                );
                assert_eq!(RawFormat::from_extension(&format!(".{ext}")), Some(*format));
            }
        }
    }

    #[test]
    fn every_format_declares_a_capture_tag() {
        for format in RawFormat::ALL {
            assert!(
                !format.capture_tags().is_empty(),
                "{format:?} declares no capture tags"
            );
        }
    }

    #[test]
    fn unknown_extension_is_rejected() {
        assert_eq!(RawFormat::from_extension("jpg"), None);
        assert_eq!(RawFormat::from_extension(""), None);
    }

    /// Pinned rather than derived: each of these was established against real
    /// files of that format, and the compiler cannot tell that one is wrong. A
    /// flipped value fails every file of that format at runtime.
    #[test]
    fn read_strategies_are_the_ones_verified_against_real_files() {
        assert_eq!(RawFormat::Cr3.read_strategy(), ReadStrategy::Streaming);
        assert_eq!(RawFormat::Nef.read_strategy(), ReadStrategy::WholeFile);
    }
}
