//! Unicode identifier character classes for unquoted Cypher identifiers.
//!
//! The range tables are generated data (ADR-0182), kept in the
//! `unicode_identifier/tables.rs` submodule and regenerated with:
//!
//! ```text
//! python3 scripts/gen-unicode-identifier-tables.py
//! ```
//!
//! The predicates and the range lookup below are hand-written. An unquoted
//! identifier character is a BMP code point (`<= U+FFFF`) whose Unicode
//! 13.0.0 `General_Category` is in the start or part set and which is not
//! `Pattern_Syntax`; there are no exceptions. The tables start at U+0080 --
//! ASCII is decided by the `is_ascii` branch of the predicates below, the
//! fast path and the source of truth for ASCII.

pub(crate) mod tables;

/// True when `c` is an unquoted identifier-start character (ADR-0182).
pub fn is_identifier_start(c: char) -> bool {
    if c.is_ascii() {
        return c == '_' || c.is_ascii_alphabetic();
    }
    let cp = c as u32;
    cp <= 0xFFFF && in_ranges(tables::IDENTIFIER_START_RANGES, cp as u16)
}

/// True when `c` is an unquoted identifier-part character (ADR-0182).
pub fn is_identifier_part(c: char) -> bool {
    if c.is_ascii() {
        return c == '_' || c.is_ascii_alphanumeric();
    }
    let cp = c as u32;
    cp <= 0xFFFF && in_ranges(tables::IDENTIFIER_PART_RANGES, cp as u16)
}

/// True when `cp` falls inside one of the sorted, disjoint, inclusive
/// `ranges`.
fn in_ranges(ranges: &[(u16, u16)], cp: u16) -> bool {
    let idx = ranges.partition_point(|&(_, last)| last < cp);
    idx < ranges.len() && ranges[idx].0 <= cp
}

#[cfg(test)]
mod tests {
    use super::tables::{IDENTIFIER_PART_RANGES, IDENTIFIER_START_RANGES, UNICODE_VERSION};
    use super::{is_identifier_part, is_identifier_start};
    use sha2::Digest;

    #[test]
    fn test_generated_unicode_identifier_tables_shape_and_boundaries() {
        assert_eq!(UNICODE_VERSION, (13, 0, 0));
        assert_eq!(IDENTIFIER_START_RANGES.len(), 383, "ADR-0182 start ranges");
        assert_eq!(IDENTIFIER_PART_RANGES.len(), 436, "ADR-0182 part ranges");
        for ranges in [IDENTIFIER_START_RANGES, IDENTIFIER_PART_RANGES] {
            let mut previous_last = 0x007F;
            for &(first, last) in ranges {
                assert!(first <= last, "range {first:#06X}..{last:#06X} is empty");
                assert!(
                    first > previous_last,
                    "ranges must be sorted, disjoint and ASCII-free"
                );
                previous_last = last;
            }
        }

        // The probe's boundary code points: the Unicode 13 additions are in,
        // the Unicode 14 additions are out; `Pc` starts and `Sc` parts are in
        // while `Other_ID_Start`/`Other_ID_Continue`, `Pattern_Syntax` and the
        // supplementary plane are out.
        for (c, start, part) in [
            ('\u{08BE}', true, true),    // Unicode 13 addition
            ('\u{9FFC}', true, true),    // Unicode 13 addition
            ('\u{A7BA}', true, true),    // Unicode 13 addition
            ('\u{0870}', false, false),  // Unicode 14 addition
            ('\u{08C8}', false, false),  // Unicode 14 addition
            ('\u{203F}', true, true),    // Pc start (XID_Start excludes it)
            ('\u{20AC}', false, true),   // Sc part, never a start
            ('\u{00B7}', false, false),  // Other_ID_Continue
            ('\u{2118}', false, false),  // Other_ID_Start
            ('\u{212E}', false, false),  // Other_ID_Start
            ('\u{2E2F}', false, false),  // Lm ∩ Pattern_Syntax
            ('\u{00B2}', false, false),  // No
            ('\u{10400}', false, false), // supplementary plane
            ('\u{1F600}', false, false), // astral emoji
            ('\u{0301}', false, true),   // Mn part
            ('\u{1885}', false, true),   // Mn part (Other_ID_Start in UAX #31)
            ('_', true, true),
            ('0', false, true),
        ] {
            assert_eq!(is_identifier_start(c), start, "is_identifier_start({c:?})");
            assert_eq!(is_identifier_part(c), part, "is_identifier_part({c:?})");
        }
    }

    #[test]
    fn test_generated_unicode_identifier_table_matches_the_generator() {
        // ADR-0182's mechanism: `scripts/gen-unicode-identifier-tables.py`
        // deterministically renders the data module
        // `unicode_identifier/tables.rs` from the SHA-256 pinned Unicode 13.0.0
        // inputs. CI does not fetch the UCD, so instead of re-fetching it this
        // test pins the SHA-256 of the generated file: any hand edit, partial
        // regeneration or generator-output change fails here until the file and
        // this pin are updated together (run the generator; it prints the
        // digest).
        assert_eq!(
            format!(
                "{:x}",
                sha2::Sha256::digest(include_bytes!("unicode_identifier/tables.rs"))
            ),
            "69f67498e36039faf967554111cd95cad385fdbb5727ceaaf71655bdad7ff2eb",
            "unicode_identifier/tables.rs drifted from the generator output; regenerate with \
             `python3 scripts/gen-unicode-identifier-tables.py` and update this pin"
        );
    }
}
