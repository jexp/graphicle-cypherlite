//! Implicit column-name source-text extraction: regression guard for the
//! byte/char column mismatch after non-ASCII input (LABS-81). Public
//! parser API only.

use cypherlite_parser::{parse, Clause};

#[test]
fn test_source_text_after_non_ascii_is_correct() {
    // Byte/char mismatch regression guard (LABS-81): the lexer counts
    // columns in bytes but token_byte_offset used to count them in
    // chars, so after any non-ASCII char the offset scan missed (running
    // to EOF and returning None) or matched the wrong byte — corrupting
    // the implicit column names derived from source_text.
    let stmt = parse("RETURN '💥' AS a,\n  n.name, 'héllo'").unwrap();
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected return");
    };
    assert_eq!(r.items.len(), 3);
    assert_eq!(r.items[0].source_text.as_deref(), Some("'💥'"));
    // These two follow the non-ASCII text (same line and next line):
    assert_eq!(r.items[1].source_text.as_deref(), Some("n.name"));
    assert_eq!(r.items[2].source_text.as_deref(), Some("'héllo'"));
}
