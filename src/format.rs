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

    /// Does `ext` select this format? Case-insensitive, and `ext` carries no
    /// leading dot.
    ///
    /// The single home of the matching rule. The directory walk asks every format
    /// in `ALL` through this, so a format declaring two extensions finds files
    /// under both, and no other code is entitled to its own idea of what matches.
    pub fn matches_extension(self, ext: &str) -> bool {
        self.extensions()
            .iter()
            .any(|known| known.eq_ignore_ascii_case(ext))
    }

    /// Every supported extension, joined for the summary's `Ignored` line — since
    /// the extension argument was removed, the one place a user whose files were
    /// passed over learns what would have been read.
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
    use std::fs;
    use std::path::Path;

    use super::*;

    /// Every declared extension must be claimed by its own format and by no other,
    /// in any case. This is the one gap the compiler cannot catch: a new variant
    /// with no table entry, or one that collides with an existing extension, still
    /// builds.
    #[test]
    fn every_declared_extension_is_claimed_by_exactly_one_format() {
        for format in RawFormat::ALL {
            assert!(
                !format.extensions().is_empty(),
                "{format:?} declares no extensions"
            );

            for ext in format.extensions() {
                let claimants: Vec<RawFormat> = RawFormat::ALL
                    .iter()
                    .copied()
                    .filter(|f| f.matches_extension(ext))
                    .collect();
                assert_eq!(claimants, [*format], "{ext:?} is not uniquely {format:?}");

                assert!(format.matches_extension(&ext.to_uppercase()));
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

    /// Pinned rather than derived, because **no fixture reaches the second entry**:
    /// every fixture file carries `DateTimeOriginal`, so the `CreateDate` fallback
    /// can be deleted and the whole suite plus all three aggregates still pass. It
    /// exists for bodies that write only `CreateDate`, and the order is part of the
    /// contract — `capture_time` takes the first tag that resolves.
    ///
    /// If a format ever legitimately diverges here, split this into per-format
    /// expectations rather than loosening it.
    #[test]
    fn capture_tags_are_the_spec_pairs_in_priority_order() {
        for format in RawFormat::ALL {
            assert_eq!(
                format.capture_tags(),
                &[
                    CaptureTag {
                        datetime: ExifTag::DateTimeOriginal,
                        offset: ExifTag::OffsetTimeOriginal,
                    },
                    CaptureTag {
                        datetime: ExifTag::CreateDate,
                        offset: ExifTag::OffsetTimeDigitized,
                    },
                ],
                "{format:?} capture tags drifted"
            );
        }
    }

    /// The rule the directory walk filters on. Pinned here rather than only
    /// through the walk, so a break in the matching rule itself is reported as
    /// such rather than as some directory finding the wrong files.
    #[test]
    fn extension_matching_is_per_format_and_case_insensitive() {
        assert!(RawFormat::Cr3.matches_extension("cr3"));
        assert!(RawFormat::Cr3.matches_extension("CR3"));
        assert!(!RawFormat::Cr3.matches_extension("nef"));
        assert!(!RawFormat::Cr3.matches_extension(".cr3"));
    }

    /// Pinned rather than derived: each of these was established against real
    /// files of that format, and the compiler cannot tell that one is wrong. A
    /// flipped value fails every file of that format at runtime.
    #[test]
    fn read_strategies_are_the_ones_verified_against_real_files() {
        for format in RawFormat::ALL {
            // Exhaustive on purpose. Adding a `RawFormat` without a line here is a
            // **build error**, not a test someone forgot to extend — which is the
            // only way this stays in lockstep without relying on diligence. Do not
            // add a `_ =>` arm; that would trade the guarantee for nothing.
            let verified_against_a_real_file_of_that_format = match format {
                RawFormat::Cr3 => ReadStrategy::Streaming,
                RawFormat::Nef => ReadStrategy::WholeFile,
            };

            assert_eq!(
                format.read_strategy(),
                verified_against_a_real_file_of_that_format,
                "{format:?}"
            );
        }
    }

    /// **A format is not supported until it has a fixture of its own.**
    ///
    /// This does not consult a list that someone has to remember to update — it
    /// reads the per-file manifests actually committed under
    /// `scripts/fixture-manifests/` and checks that some real file carries each
    /// format's extension. Add a `RawFormat` without adding a fixture and this
    /// fails, naming what is missing.
    ///
    /// NEF is why: it parses through one `MediaSource` constructor and not the
    /// other, which no unit test and no crate documentation would have revealed.
    #[test]
    fn every_format_has_a_fixture_of_its_own() {
        let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/fixture-manifests");
        let mut fixture_extensions: Vec<String> = Vec::new();

        for entry in fs::read_dir(&manifests).expect("the manifest directory is committed") {
            let path = entry.expect("a manifest entry").path();
            let listing = fs::read_to_string(&path).expect("a readable manifest");
            for line in listing.lines() {
                // "<sha256>  <filename>"
                let Some(name) = line.split_whitespace().nth(1) else {
                    continue;
                };
                if let Some(extension) = Path::new(name).extension().and_then(|e| e.to_str()) {
                    fixture_extensions.push(extension.to_ascii_lowercase());
                }
            }
        }

        for format in RawFormat::ALL {
            assert!(
                format
                    .extensions()
                    .iter()
                    .any(|wanted| fixture_extensions.iter().any(|have| have == wanted)),
                "{format:?} has no fixture: nothing under scripts/fixture-manifests/ carries \
                 any of {:?}. A format is not supported until a real file of it is in the \
                 verification set — see docs/FIXTURES.md.",
                format.extensions()
            );
        }
    }
}
