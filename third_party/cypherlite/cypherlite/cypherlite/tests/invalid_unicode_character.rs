//! LABS-188: a non-ASCII character outside strings, comments and whitespace
//! is ONE invalid Unicode character carrying the TCK `InvalidUnicodeCharacter`
//! detail, not one mojibake error token per UTF-8 byte. Pinned end-to-end
//! through the public `cypherlite::Database` path (the retired
//! `Mathematical3 [1]` TCK skip).

mod common;

use common::{connection, err_text};

#[test]
fn invalid_unicode_hyphen_reports_decoded_character() {
    let mut c = connection();
    let err = err_text(c.query("RETURN 42 \u{2014} 41"));
    assert!(
        err.contains("InvalidUnicodeCharacter"),
        "expected the InvalidUnicodeCharacter detail in: {err}"
    );
    assert!(
        err.contains('\u{2014}'),
        "expected the decoded em dash in: {err}"
    );
    assert!(
        !err.contains('\u{e2}'), // 'â', the 0xE2 lead byte widened to char
        "byte-wise mojibake leaked into: {err}"
    );
}
