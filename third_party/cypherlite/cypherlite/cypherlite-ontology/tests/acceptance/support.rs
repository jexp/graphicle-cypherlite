//! Shared fixtures and helpers for the acceptance-test modules.

use cypherlite_ontology::{Value, PINNED_SCHEMA_URI};

/// The CypherLite-authored, main-shape, validation-only example
/// (`neo4j/graph-spec` @ `066515593ae59317d758bcf2cef54195e74bde39`,
/// ADR-0364 §6).
pub const EXAMPLE_JSON: &str = include_str!("../../schema/example.json");

/// The vendored main schema itself.
pub const VENDORED_SCHEMA: &str = include_str!("../../schema/ontology-graph-spec.schema.json");

/// The `$defs.Neo4jType.enum` token vocabulary from the vendored main
/// schema. The schema is the canonical source this compilation unit can
/// read: the crate's `NEO4J_TYPES` table is `pub(crate)`, out of reach from
/// an integration test.
pub fn neo4j_type_tokens() -> Vec<String> {
    let schema: serde_json::Value =
        serde_json::from_str(VENDORED_SCHEMA).expect("vendored schema is valid JSON");
    let tokens = schema["$defs"]["Neo4jType"]["enum"]
        .as_array()
        .expect("vendored schema has a `$defs.Neo4jType.enum` array");
    tokens
        .iter()
        .map(|token| {
            token
                .as_str()
                .expect("every `Neo4jType` enum token is a string")
                .to_owned()
        })
        .collect()
}

/// Wrap a JSON body in a valid main-shape document header.
pub fn json_doc(rest: &str) -> String {
    format!("{{\"schema\":\"{PINNED_SCHEMA_URI}\",\"id\":\"test\",\"version\":\"1\"{rest}}}")
}

/// Walk `keys` down a value tree, panicking on the first non-object or
/// missing member. Extension payloads have no typed view; assertions read
/// the retained root value tree (ADR-0364 §5.1).
pub fn at<'a>(value: &'a Value, keys: &[&str]) -> &'a Value {
    let mut current = value;
    for (depth, key) in keys.iter().enumerate() {
        current = current
            .as_object()
            .and_then(|obj| obj.get(*key))
            .unwrap_or_else(|| {
                panic!(
                    "no value at '/{}' (not an object at '/{}')",
                    keys[..=depth].join("/"),
                    keys[..depth].join("/")
                )
            });
    }
    current
}
