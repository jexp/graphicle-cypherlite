//! Unit-level validation invariants.
//!
//! Public behavior is covered by the acceptance suite
//! (`tests/acceptance/`); these tests pin the internal shape acceptance
//! cannot observe directly.

use indexmap::IndexMap;

use super::expected_constraint_types;
use super::ext::{validate_index_option, INDEX_OPTION_TYPES};
use super::read::{
    expected_neo4j_types, neo4j_type_list, opt_nullable_bool, opt_nullable_int,
    opt_nullable_number, opt_nullable_string,
};
use crate::error::OntologyError;
use crate::model::{CONSTRAINTS, NEO4J_TYPES};
use crate::tests::doc;
use crate::value::Value;

/// The `label` shorthand is desugared into the one `labels` form at
/// validation, so every reader sees `labels.identifier` directly and the
/// implied/optional arrays stay untouched.
#[test]
fn the_label_shorthand_desugars_into_the_labels_form() {
    let doc = doc(r#","nodes":{"N":{"label":"N"}}"#);
    assert_eq!(doc.nodes()[0].labels.identifier.as_deref(), Some("N"));
    assert!(doc.nodes()[0].labels.implied.is_empty());
    assert!(doc.nodes()[0].labels.optional.is_empty());
}

fn assert_validation_error(err: OntologyError, path: &str, message: &str) {
    let OntologyError::Validation {
        path: at,
        message: got,
    } = err
    else {
        panic!("expected a validation error, got {err}");
    };
    assert_eq!(at, path);
    assert_eq!(got, message);
}

/// Every nullable scalar accessor derives from the one nullable shape: one
/// sentence, one `or null` suffix, one path.
#[test]
fn nullable_scalar_reads_share_the_or_null_message() {
    let mut obj = IndexMap::new();
    obj.insert("string".to_owned(), Value::Integer(1));
    obj.insert("boolean".to_owned(), Value::String("yes".to_owned()));
    obj.insert("integer".to_owned(), Value::Float(1.5));
    obj.insert("number".to_owned(), Value::String("1".to_owned()));
    obj.insert("absent".to_owned(), Value::Null);

    assert_validation_error(
        opt_nullable_string(&obj, "string", "/p").unwrap_err(),
        "/p/string",
        "'string' must be a string or null, found an integer",
    );
    assert_validation_error(
        opt_nullable_bool(&obj, "boolean", "/p").unwrap_err(),
        "/p/boolean",
        "'boolean' must be a boolean or null, found a string",
    );
    assert_validation_error(
        opt_nullable_int(&obj, "integer", "/p").unwrap_err(),
        "/p/integer",
        "'integer' must be an integer or null, found a float",
    );
    assert_validation_error(
        opt_nullable_number(&obj, "number", "/p").unwrap_err(),
        "/p/number",
        "'number' must be a number or null, found a string",
    );

    assert_eq!(opt_nullable_string(&obj, "absent", "/p").unwrap(), None);
    assert_eq!(opt_nullable_bool(&obj, "absent", "/p").unwrap(), None);
    assert_eq!(opt_nullable_int(&obj, "absent", "/p").unwrap(), None);
    assert_eq!(opt_nullable_number(&obj, "absent", "/p").unwrap(), None);
}

/// A `Neo4jType` list is an array reader like the string and number
/// lists: one array sentence and one item sentence, the item reported at
/// the element path under the field.
#[test]
fn neo4j_type_lists_share_the_array_shape() {
    let value = Value::Array(vec![Value::String("STRING".to_owned()), Value::Integer(1)]);
    let err = neo4j_type_list(&value, "supported", "/p").unwrap_err();
    assert_validation_error(
        err,
        "/p/1",
        "'supported' items must be Neo4jType strings, found an integer",
    );

    assert_validation_error(
        neo4j_type_list(&Value::String("STRING".to_owned()), "supported", "/p").unwrap_err(),
        "/p",
        "'supported' must be an array, found a string",
    );
}

/// Both expected-token messages walk the model's canonical vocabulary
/// tables, so the prose can never drift from the parsers.
#[test]
fn token_messages_derive_from_the_canonical_tables() {
    let expected_types = expected_neo4j_types();
    assert!(
        expected_types.contains(&NEO4J_TYPES.len().to_string()),
        "{expected_types}"
    );
    for (token, _) in NEO4J_TYPES {
        assert!(expected_types.contains(token), "{token}: {expected_types}");
    }

    let expected_constraints = expected_constraint_types();
    for (token, _) in CONSTRAINTS {
        assert!(
            expected_constraints.contains(&format!("'{token}'")),
            "{token}: {expected_constraints}"
        );
    }
    assert_eq!(
        expected_constraints.matches(" or ").count(),
        CONSTRAINTS.len() - 1
    );
}

/// The index `options.type` dispatch and its expected-list error read the
/// same vocabulary table: a tag is accepted exactly when it has a row, each
/// row's validator owns its option shape, and the message names exactly the
/// listed tokens in their unchanged order and spelling.
#[test]
fn index_option_types_dispatch_from_one_vocabulary() {
    assert_eq!(
        INDEX_OPTION_TYPES
            .iter()
            .map(|(token, _)| *token)
            .collect::<Vec<_>>(),
        ["FULLTEXT", "POINT", "VECTOR"]
    );

    // Dispatch is the table in both directions: no arm can accept a tag the
    // table does not carry, and no row is dead vocabulary.
    for token in [
        "FULLTEXT", "POINT", "VECTOR", "RANGE", "TEXT", "LOOKUP", "UNKNOWN",
    ] {
        let mut options = IndexMap::new();
        options.insert("type".to_owned(), Value::String(token.to_owned()));
        assert_eq!(
            validate_index_option(&Value::Object(options), "/p").is_ok(),
            INDEX_OPTION_TYPES
                .iter()
                .any(|(listed, _)| *listed == token),
            "{token}: acceptance must match the vocabulary table"
        );
    }

    // Each row's validator owns its option shape, so the tag's own
    // `check_allowed` rejects a member outside that shape.
    for (token, _) in INDEX_OPTION_TYPES {
        let mut options = IndexMap::new();
        options.insert("type".to_owned(), Value::String((*token).to_owned()));
        options.insert("unknown".to_owned(), Value::Boolean(true));
        let err = validate_index_option(&Value::Object(options), "/p")
            .expect_err("a row's validator must reject unknown members");
        assert_validation_error(
            err,
            "/p/unknown",
            "unknown field 'unknown' (additional properties are not allowed here)",
        );
    }

    let mut options = IndexMap::new();
    options.insert("type".to_owned(), Value::String("RANGE".to_owned()));
    let err = validate_index_option(&Value::Object(options), "/p")
        .expect_err("an option type outside the table must be rejected");
    assert_validation_error(
        err,
        "/p/type",
        "invalid index options type 'RANGE': expected 'FULLTEXT', 'POINT' or 'VECTOR'",
    );
}
