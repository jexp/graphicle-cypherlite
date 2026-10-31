//! The spec-version gate and the root clause (ADR-0364 §1), plus the
//! rejection of every pre-bump shape whichever URI it carries.

use cypherlite_ontology::{parse_ontology_document, OntologyError, PINNED_SCHEMA_URI};

use crate::support::json_doc;

/// The retired draft's schema URI: a different dialect from main's
/// `.../docs/ontology-graph-spec/...` (ADR-0364 §1).
const OLD_DRAFT_URI: &str = "https://neo4j.com/ontology-graph-spec/1.0.0/schema.json";

// ---------------------------------------------------------------------------
// Spec-version gate (ADR-0364 §1)
// ---------------------------------------------------------------------------

#[test]
fn spec_version_gate_accepts_exactly_the_main_uri() {
    assert!(parse_ontology_document(json_doc("").as_bytes()).is_ok());

    // A newer version fails, naming 1.0.0 as the highest supported version,
    // even when the document is otherwise structurally broken (the gate runs
    // before structural validation).
    let newer = "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/2.0.0/schema.json\"}";
    let err = parse_ontology_document(newer.as_bytes()).unwrap_err();
    let OntologyError::UnsupportedSpecVersion { uri } = &err else {
        panic!("expected UnsupportedSpecVersion, got {err}");
    };
    assert_eq!(
        uri,
        "https://neo4j.com/docs/ontology-graph-spec/2.0.0/schema.json"
    );
    let text = err.to_string();
    assert_eq!(
        text,
        "Unsupported ontology spec version: \
         'https://neo4j.com/docs/ontology-graph-spec/2.0.0/schema.json'. \
         This version of CypherLite supports up to Ontology Graph Spec 1.0.0 \
         ('https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json')."
    );

    // A 1.0.0-ish but different URI fails too (the match is exact).
    let other = "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.1/schema.json\",\"id\":\"x\",\"version\":\"1\"}";
    let err = parse_ontology_document(other.as_bytes()).unwrap_err();
    assert!(
        matches!(err, OntologyError::UnsupportedSpecVersion { .. }),
        "{err}"
    );
}

#[test]
fn old_pinned_uri_fails_at_the_gate() {
    // The pinned draft's URI is a different dialect: the gate refuses it
    // before structure is looked at, even when the rest of the document is
    // main-shaped.
    let old = format!("{{\"schema\":\"{OLD_DRAFT_URI}\",\"id\":\"x\",\"version\":\"1\"}}");
    let err = parse_ontology_document(old.as_bytes()).unwrap_err();
    let OntologyError::UnsupportedSpecVersion { uri } = &err else {
        panic!("expected UnsupportedSpecVersion, got {err}");
    };
    assert_eq!(uri, OLD_DRAFT_URI);
    assert!(err.to_string().contains(PINNED_SCHEMA_URI), "{err}");
}

#[test]
fn non_string_schema_is_a_type_error_not_a_gate_error() {
    let err =
        parse_ontology_document(b"{\"schema\":1,\"id\":\"x\",\"version\":\"1\"}").unwrap_err();
    assert!(
        matches!(err, OntologyError::Validation { ref path, .. } if path == "/schema"),
        "{err}"
    );
    assert!(err.to_string().contains("must be a string"), "{err}");
}

// ---------------------------------------------------------------------------
// Root clause
// ---------------------------------------------------------------------------

#[test]
fn root_clause_fixtures() {
    // The minimal document: the three required fields, nothing else.
    let doc = parse_ontology_document(json_doc("").as_bytes()).expect("minimal root validates");
    assert!(doc.nodes().is_empty());
    assert!(doc.relationships().is_empty());

    // Optional metadata, in both syntaxes.
    let doc = parse_ontology_document(json_doc(",\"name\":\"N\",\"description\":\"D\"").as_bytes())
        .unwrap();
    assert_eq!(doc.name(), Some("N"));
    assert_eq!(doc.description(), Some("D"));

    // `name` is a plain string, not nullable in main.
    let err = parse_ontology_document(json_doc(",\"name\":null").as_bytes()).unwrap_err();
    assert!(err.to_string().contains("/name"), "{err}");

    // The root is closed: `$schema`/`$id` document members are rejected.
    for member in ["\"$schema\":\"x\"", "\"$id\":\"x\""] {
        let err = parse_ontology_document(json_doc(&format!(",{member}")).as_bytes()).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{member}: {err}");
    }

    // `nodes`/`relationships` are maps.
    for field in ["nodes", "relationships"] {
        let err =
            parse_ontology_document(json_doc(&format!(",\"{field}\":[]")).as_bytes()).unwrap_err();
        assert!(err.to_string().contains(field), "{field}: {err}");
    }
}

#[test]
fn missing_top_level_required_fields_fail() {
    // Each of schema / id / version absent.
    let no_schema = "{\"id\":\"test\",\"version\":\"1\"}";
    let err = parse_ontology_document(no_schema.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("'schema'"), "{err}");

    let no_id = json_doc("").replace("\"id\":\"test\",", "");
    let err = parse_ontology_document(no_id.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("'id'"), "{err}");

    let no_version = json_doc("").replace(",\"version\":\"1\"", "");
    let err = parse_ontology_document(no_version.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("'version'"), "{err}");
}

#[test]
fn version_is_a_string_carried_verbatim() {
    // Any string is accepted and carried unchanged; there is no numeric
    // comparison anywhere (ADR-0364 §2/§5.1).
    let doc = parse_ontology_document(
        format!("{{\"schema\":\"{PINNED_SCHEMA_URI}\",\"id\":\"x\",\"version\":\"2026.10\"}}")
            .as_bytes(),
    )
    .expect("any string version validates");
    assert_eq!(doc.version(), "2026.10");

    // The pinned draft's integer version is the first old-shape member a
    // main-URI document hits.
    for integer in ["1", "-1"] {
        let err = parse_ontology_document(
            format!("{{\"schema\":\"{PINNED_SCHEMA_URI}\",\"id\":\"x\",\"version\":{integer}}}")
                .as_bytes(),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("/version"), "{integer}: {text}");
        assert!(text.contains("must be a string"), "{integer}: {text}");
        assert!(text.contains("an integer"), "{integer}: {text}");
    }
}

// ---------------------------------------------------------------------------
// The old pinned shape is rejected by the raised gate and the closed shapes
// (ADR-0364 §1)
// ---------------------------------------------------------------------------

#[test]
fn old_pinned_shape_is_rejected_whichever_uri_it_carries() {
    // The old URI: the gate fires before structure is looked at.
    let old_uri = format!("{{\"schema\":\"{OLD_DRAFT_URI}\",\"id\":\"old\",\"version\":1}}");
    let err = parse_ontology_document(old_uri.as_bytes()).unwrap_err();
    assert!(
        matches!(err, OntologyError::UnsupportedSpecVersion { .. }),
        "{err}"
    );

    // The new URI with the old shape: structural validation rejects the
    // first old member. Each table case carries the document it builds.
    let integer_version = json_doc("").replace("\"version\":\"1\"", "\"version\":1");
    for (doc, path) in [
        (integer_version, "/version"),
        (
            json_doc(
                ",\"nodes\":{\"N\":{\"label\":\"N\",\"properties\":{\"p\":{\"mustExist\":true}}}}",
            ),
            "/nodes/N/properties/p/mustExist",
        ),
        (
            json_doc(
                ",\"nodes\":{\"N\":{\"label\":\"N\",\"constraints\":[{\"constraint_type\":\"key\",\"properties\":[\"p\"]}]}}",
            ),
            "/nodes/N/constraints",
        ),
    ] {
        let err = parse_ontology_document(doc.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains(path),
            "{doc}: expected {path}, got {err}"
        );
    }
}
