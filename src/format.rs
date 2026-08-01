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
//! IIQ and TIFF; NEF, ARW, DNG, ORF, PEF and RW2 are not on its list, and adding
//! a row does not conjure a parser.
//!
//! NEF specifically has been tested against 150 real files and is the instructive
//! case: it parses through `MediaSource::from_memory` but *not* through
//! `MediaSource::open`, which is what `raw.rs` uses. Supporting it therefore means
//! a per-format choice of how to open the file — the first thing that would earn a
//! new column here — plus reading whole ~22 MB files. CLAUDE.md has the numbers.
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

/// A raw format we know how to read a capture time from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    Cr3,
}

impl RawFormat {
    /// Every supported format, in help-text order.
    pub const ALL: &'static [RawFormat] = &[Self::Cr3];

    /// Extensions that select this format, lowercase.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Cr3 => &["cr3"],
        }
    }

    /// Capture-time tags to try, in priority order.
    pub fn capture_tags(self) -> &'static [CaptureTag] {
        match self {
            Self::Cr3 => &[
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
}
