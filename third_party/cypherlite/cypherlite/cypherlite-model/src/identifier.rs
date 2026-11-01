//! Shape-only Cypher identifier quoting for display text.
//!
//! This is the lowest shared home of the policy: the parser's generated
//! Cypher and the engine's value rendering both consult it rather than each
//! spelling the backtick escape themselves. It sits in `cypherlite-model`
//! because that is the only crate both `cypherlite-parser` and
//! `cypherlite-core` depend on.

/// True when `name` has the plain ASCII identifier shape
/// `[A-Za-z_][A-Za-z0-9_]*` — the shape-only half of the quoting policy.
///
/// ASCII-only by design, and deliberately narrower than the lexer: unquoted
/// identifiers may use the Unicode 13.0.0 classes (so `café` parses bare,
/// ADR-0182), but rendered text quotes every non-ASCII name. Backticks keep
/// rendered Cypher ASCII — readable by older CypherLite versions that
/// predate Unicode identifiers, and independent of the pinned Unicode table
/// version — and the quoted form round-trips exactly in every name position,
/// so the narrower gate is a rendering choice, not a dialect claim.
pub fn has_plain_identifier_shape(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii() && (c == '_' || c.is_ascii_alphabetic()))
        && chars.all(|c| c.is_ascii() && (c == '_' || c.is_ascii_alphanumeric()))
}

/// Renders `name` in the always-backtick-quoted form: wrapped in backticks
/// with every embedded backtick doubled (the Cypher escape rule), whatever
/// [`has_plain_identifier_shape`] would say. The one spelling of the quoting
/// escape: every caller that needs a forced-quoted name calls it instead of
/// re-spelling the `format!`/`replace`.
pub fn quote_identifier_always(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Renders `name` for display text: bare when [`has_plain_identifier_shape`]
/// holds, otherwise through [`quote_identifier_always`].
///
/// Display text is not re-parsed as a statement, so it owes no
/// keyword-position guarantee: a keyword spelling such as `when` stays bare
/// here, while generated Cypher that must round-trip as a statement uses the
/// parser's keyword-aware `token::quote_identifier` instead.
pub fn quote_identifier_shaped(name: &str) -> String {
    if has_plain_identifier_shape(name) {
        name.to_string()
    } else {
        quote_identifier_always(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_identifier_shape_is_ascii_only() {
        for bare in ["a", "Person", "_x", "a1", "ship_date"] {
            assert!(has_plain_identifier_shape(bare), "{bare}");
        }
        for quoted in ["", "1a", "a b", "a-b", "café", "we`ird", "a.b"] {
            assert!(!has_plain_identifier_shape(quoted), "{quoted}");
        }
    }

    #[test]
    fn shaped_quoting_leaves_plain_names_bare_and_escapes_the_rest() {
        assert_eq!(quote_identifier_shaped("Person"), "Person");
        assert_eq!(quote_identifier_shaped("first name"), "`first name`");
        assert_eq!(quote_identifier_shaped("we`ird"), "`we``ird`");
        assert_eq!(quote_identifier_always("Person"), "`Person`");
    }
}
