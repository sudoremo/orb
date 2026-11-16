//! Search: the query language and how it becomes SQL.
//!
//! Parsing and compilation are separate on purpose. The parser knows nothing
//! about the schema, so it can be tested exhaustively without a database, and
//! the compiler decides which conditions the full-text index answers and which
//! are plain indexed columns.

pub mod compile;
pub mod embed;
pub mod query;

pub use query::{Clause, Field, Flag, ParseError, Query, Term, parse};

/// Text as a search compares it: lower case, accents taken off, so that
/// `muller` finds "Müller" as the full-text index already does.
pub fn fold(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `haystack` holds `needle`, which [`fold`] has already been
/// applied to. The SQL function `orb_contains` is this.
pub fn contains_folded(haystack: &str, needle: &str) -> bool {
    if haystack.is_ascii() {
        let needle = needle.as_bytes();
        return needle.is_empty()
            || haystack
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle));
    }
    fold(haystack).contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_ignores_case_and_accents() {
        assert_eq!(fold("Thomas MÜLLER"), "thomas muller");
        assert!(contains_folded("Thomas Müller <t@x.ch>", "mull"));
        assert!(contains_folded("Sam <sam@fritzmail.example>", "fritz"));
        assert!(contains_folded("THOMAS", "thom"));
        assert!(!contains_folded("Thomas", "tomas"));
    }
}
