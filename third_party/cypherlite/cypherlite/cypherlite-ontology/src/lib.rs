//! The ontology document crate: parsing, validation and serialization for
//! Neo4j's main Ontology Graph Spec 1.0.0 document format (ADR-0364, which
//! supersedes ADR-0227's pinned draft shape).
//!
//! `db.ontology.set` accepts a document in JSON or YAML 1.2. Both syntaxes
//! are parsed into one ordered, JSON-equivalent value model ([`Value`]) —
//! a strict-JSON sniff decides the syntax (ADR-0227 §3) — and validated by
//! hand-rolled structural checks against the vendored main schema
//! (`schema/ontology-graph-spec.schema.json`, `neo4j/graph-spec` @
//! `066515593ae59317d758bcf2cef54195e74bde39`; ADR-0364 §2). The root
//! `schema` field is gated on exactly main's `$id`,
//! `https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json`, before
//! structural validation (ADR-0364 §1). Accepted shapes are main's:
//! string `version`, nullable display names, id-keyed constraint maps with
//! the uppercase `type` enum, the closed 39-token `Neo4jType` enum,
//! `must_exist`, closed per-element extension envelopes with deeply
//! validated payloads, and `RelationshipTarget` endpoints with no
//! required members.
//!
//! The validated [`OntologyDocument`] exposes nodes, relationships,
//! properties and constraints in document order and serializes
//! canonically to JSON (compact) or YAML (block style, two-space
//! indentation) with the document's key order preserved, for
//! `db.ontology.get`'s format conversion. Extension payloads have no
//! typed view: they are validated in place and carried only by the
//! document's retained root [`Value`] tree (ADR-0364 §5.1).
//!
//! Of ADR-0226's mappings, only the ontology → graph-type converter ships
//! (ADR-0229 §4): [`to_graph_type_body`] compiles a document into the
//! CIP-176 graph-type body `db.ontology.apply` sets (the
//! enforceable/descriptive split of ADR-0226 §2, re-based on main's shapes
//! by ADR-0364 §5). The document → `system_ontologies` graph mapping
//! (ADR-0226 §1) is deleted — the namespace is listed but not queryable
//! (ADR-0229), so no graph is derived from a stored document; it is
//! revived from git history when Neo4j publishes its mapping.
//!
//! ```
//! let doc = cypherlite_ontology::parse_ontology_document(
//!     br#"{"schema": "https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json",
//!         "id": "example", "version": "1",
//!         "nodes": {"person": {"label": "Person",
//!                              "properties": {"id": {"type": "STRING", "key": true}}}},
//!         "relationships": {"knows": {"type": "KNOWS",
//!                                     "from": {"node": "person"},
//!                                     "to": {"node": "person"}}}}"#,
//! )
//! .unwrap();
//! assert_eq!(doc.id(), "example");
//! assert_eq!(doc.version(), "1");
//! assert_eq!(doc.nodes()[0].identifying_label(), Some("Person"));
//!
//! let body = cypherlite_ontology::to_graph_type_body(&doc).unwrap();
//! assert_eq!(body.edge_types.len(), 1);
//! ```

mod error;
#[cfg(test)]
mod fixtures;
mod graph_type;
mod model;
mod parse;
mod serialize;
#[cfg(test)]
mod tests;
mod validate;
mod value;

pub use error::{OntologyError, MAX_SUPPORTED_SPEC_VERSION, PINNED_SCHEMA_URI};
pub use graph_type::to_graph_type_body;
pub use model::{
    Constraint, ConstraintType, Labels, Neo4jType, NodeType, OntologyDocument, Property,
    RelationshipTarget, RelationshipType,
};
pub use parse::{detect_format, parse_document_value, DocumentFormat};
pub use serialize::{to_json_sorted_string, to_json_string, to_yaml_string};
pub use value::Value;

/// Parse and validate an ontology document from JSON or YAML bytes.
///
/// The syntax is detected by a strict JSON parse (ADR-0227 §3); the
/// document is then parsed into the ordered value model, gated on the
/// pinned main 1.0.0 `schema` URI (ADR-0364 §1), and structurally
/// validated against the vendored main schema (ADR-0364 §2).
///
/// # Errors
///
/// - [`OntologyError::InvalidUtf8`] — the bytes are not UTF-8.
/// - [`OntologyError::Syntax`] — the document is not well-formed in its
///   detected syntax.
/// - [`OntologyError::UnsupportedConstruct`] — the document uses a
///   construct outside the JSON-equivalent value model (non-string mapping
///   keys, custom tags, non-finite floats, multiple YAML documents).
/// - [`OntologyError::UnsupportedSpecVersion`] — the `schema` URI is not
///   main's pinned 1.0.0 URI; the message names 1.0.0 as the highest
///   supported version and the exact URI.
/// - [`OntologyError::Validation`] — the document violates main's schema;
///   the error names the violated field/path.
pub fn parse_ontology_document(bytes: &[u8]) -> Result<OntologyDocument, OntologyError> {
    let (value, _) = parse_document_value(bytes)?;
    validate::validate_document(value)
}
