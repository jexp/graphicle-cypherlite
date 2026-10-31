//! Test-only helpers shared by the crate's unit-test modules.
//!
//! Integration tests compile as separate crates and cannot reach these; the
//! acceptance suite keeps its own local header helper in
//! `tests/acceptance/support.rs` (the crate boundary requires it).

use crate::error::PINNED_SCHEMA_URI;
use crate::model::OntologyDocument;
use crate::parse_ontology_document;

/// Parse a main-shape document whose root body is `rest` (a leading-comma
/// JSON fragment), with the three required header fields filled in.
pub(crate) fn doc(rest: &str) -> OntologyDocument {
    let text = format!(r#"{{"schema":"{PINNED_SCHEMA_URI}","version":"1","id":"test"{rest}}}"#);
    parse_ontology_document(text.as_bytes())
        .unwrap_or_else(|e| panic!("test document validates: {e}\n{text}"))
}
