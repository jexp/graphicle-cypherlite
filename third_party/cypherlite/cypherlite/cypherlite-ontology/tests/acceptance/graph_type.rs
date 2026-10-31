//! Graph-type derivation on main-shape documents (ADR-0226 §2,
//! ADR-0364 §5): a convertible example derives a body, and every
//! main-valid construct the body cannot express fails loudly.

use cypherlite_ontology::{parse_ontology_document, to_graph_type_body, OntologyError};
use cypherlite_parser::{PredefinedKind, TypeExpr};

use crate::support::{json_doc, EXAMPLE_JSON};

// ---------------------------------------------------------------------------
// Graph-type derivation (ADR-0226 §2, ADR-0364 §5)
// ---------------------------------------------------------------------------

/// A convertible main-shape example: keys, a `must_exist` declaration, a
/// `UUID`, implied labels and a node-referenced relationship. The
/// validation-only `schema/example.json` deliberately is not convertible,
/// so `apply` coverage uses this separate document (ADR-0364 §6).
const APPLY_EXAMPLE_JSON: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"people\",\"version\":\"1\",",
    "\"nodes\":{",
    "\"person\":{\"label\":\"Person\",\"labels\":{\"identifier\":\"Person\",",
    "\"implied\":[\"Human\"]},\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"key\":true},",
    "\"name\":{\"type\":\"STRING\",\"must_exist\":true}},",
    "\"constraints\":{\"person-id\":{\"type\":\"KEY\",\"properties\":[\"id\"],",
    "\"name\":\"person_id_key\"}}},",
    "\"company\":{\"labels\":{\"identifier\":\"Company\"},\"properties\":{",
    "\"id\":{\"type\":\"UUID\",\"key\":true}}}},",
    "\"relationships\":{\"works-at\":{\"type\":\"WORKS_AT\",",
    "\"from\":{\"node\":\"person\"},\"to\":{\"node\":\"company\"},",
    "\"properties\":{\"since\":{\"type\":\"DATE\"}}}}}"
);

#[test]
fn graph_type_derivation_from_a_main_shape_example() {
    let doc = parse_ontology_document(APPLY_EXAMPLE_JSON.as_bytes()).expect("example validates");
    let body = to_graph_type_body(&doc).expect("convertible example derives a graph type");

    // Node type specs, in document order, with the map key as the alias and
    // the identifying label as the node label.
    assert_eq!(body.node_types.len(), 2);
    let person = &body.node_types[0];
    assert_eq!(person.alias.as_deref(), Some("person"));
    assert_eq!(person.identifying_label, "Person");
    assert_eq!(person.implied_labels, ["Human"]);
    assert_eq!(person.properties.len(), 2);
    assert_eq!(person.properties[0].name, "id");
    assert_eq!(
        person.properties[0].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: false
        }
    );
    assert_eq!(
        person.properties[1].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: true
        }
    );
    let company = &body.node_types[1];
    assert_eq!(company.identifying_label, "Company");
    assert_eq!(
        company.properties[0].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::Uuid,
            not_null: false
        }
    );

    // The edge type resolves both endpoints through the node aliases.
    assert_eq!(body.edge_types.len(), 1);
    let works_at = &body.edge_types[0];
    assert_eq!(works_at.alias.as_deref(), Some("works-at"));
    assert_eq!(works_at.identifying_label, "WORKS_AT");
    assert_eq!(
        works_at.source,
        cypherlite_parser::GraphTypeElementRef::AliasRef("person".to_owned())
    );
    assert_eq!(
        works_at.target,
        cypherlite_parser::GraphTypeElementRef::AliasRef("company".to_owned())
    );

    // The explicit constraint object survives with its name and replaces
    // the `key` flag it restates; the company's shorthand `key` is the
    // second, flag-derived constraint.
    assert_eq!(body.constraints.len(), 2);
    assert_eq!(body.constraints[0].name.as_deref(), Some("person_id_key"));
    assert_eq!(body.constraints[0].properties, ["id"]);
    assert_eq!(
        body.constraints[0].kind,
        cypherlite_parser::GraphTypeConstraintVerb::Key
    );
    assert_eq!(body.constraints[1].name, None);
    assert_eq!(body.constraints[1].properties, ["id"]);
    assert!(body.drop_constraint_names.is_empty());
}

#[test]
fn the_validation_only_example_is_not_silently_converted() {
    let doc = parse_ontology_document(EXAMPLE_JSON.as_bytes()).unwrap();
    let err = to_graph_type_body(&doc).expect_err("the validation-only example is not convertible");
    assert!(matches!(err, OntologyError::Mapping(_)), "{err}");
    assert!(err.to_string().contains("node 'person'"), "{err}");
    assert!(err.to_string().contains("non-empty"), "{err}");
}

#[test]
fn property_type_constraints_fail_loudly_at_apply() {
    let doc = parse_ontology_document(
        json_doc(
            ",\"nodes\":{\"N\":{\"label\":\"N\",\"properties\":{\"p\":{\"type\":\"STRING\"}},\
             \"constraints\":{\"c\":{\"type\":\"PROPERTY_TYPE\",\"properties\":[\"p\"]}}}}",
        )
        .as_bytes(),
    )
    .expect("PROPERTY_TYPE is main-valid");
    let err = to_graph_type_body(&doc).expect_err("PROPERTY_TYPE is not convertible");
    assert!(matches!(err, OntologyError::Mapping(_)), "{err}");
    assert!(err.to_string().contains("constraint 'c'"), "{err}");
    assert!(err.to_string().contains("PROPERTY_TYPE"), "{err}");
}

#[test]
fn unsupported_neo4j_scalars_fail_loudly_at_apply() {
    // `FLOAT32`, `INTEGER8`, `INTEGER16`, `INTEGER32` and their list forms
    // validate but have no plain CIP-176 property type: silent widening is
    // forbidden (ADR-0364 §5.2).
    for token in [
        "FLOAT32",
        "LIST<FLOAT32>",
        "INTEGER8",
        "LIST<INTEGER8>",
        "INTEGER16",
        "LIST<INTEGER16>",
        "INTEGER32",
        "LIST<INTEGER32>",
    ] {
        let doc = parse_ontology_document(
            json_doc(&format!(
                ",\"nodes\":{{\"N\":{{\"label\":\"N\",\"properties\":{{\"p\":{{\"type\":\"{token}\"}}}}}}}}"
            ))
            .as_bytes(),
        )
        .unwrap_or_else(|e| panic!("{token} is main-valid: {e}"));
        let err = to_graph_type_body(&doc).expect_err("no plain CIP-176 counterpart");
        assert!(matches!(err, OntologyError::Mapping(_)), "{token}: {err}");
        assert!(
            err.to_string().contains(token),
            "{token}: the error names the token: {err}"
        );
        assert!(err.to_string().contains("'p'"), "{token}: {err}");
    }

    // The vector coordinate forms do convert (CIP-216), including the
    // 8/16/32-bit element types that fail as plain scalars.
    for token in [
        "VECTOR<FLOAT32>",
        "VECTOR<INTEGER8>",
        "VECTOR<INTEGER16>",
        "VECTOR<INTEGER32>",
    ] {
        let doc = parse_ontology_document(
            json_doc(&format!(
                ",\"nodes\":{{\"N\":{{\"label\":\"N\",\"properties\":{{\"p\":{{\"type\":\"{token}\",\"dimension\":3}}}}}}}}"
            ))
            .as_bytes(),
        )
        .unwrap();
        let body = to_graph_type_body(&doc).unwrap_or_else(|e| panic!("{token}: {e}"));
        assert!(matches!(
            body.node_types[0].properties[0].value_type,
            TypeExpr::Vector {
                dimension: Some(3),
                ..
            }
        ));
    }
}

#[test]
fn label_only_and_empty_endpoints_fail_loudly_at_apply() {
    let doc = parse_ontology_document(
        json_doc(
            ",\"nodes\":{\"N\":{\"label\":\"N\"}},\"relationships\":{\
             \"label-only\":{\"type\":\"L\",\"from\":{\"label\":\"N\"},\"to\":{\"node\":\"N\"}},\
             \"empty\":{\"type\":\"E\",\"from\":{\"node\":\"N\"},\"to\":{}}}",
        )
        .as_bytes(),
    )
    .expect("label-only and empty targets are main-valid");
    let err = to_graph_type_body(&doc).expect_err("the label-only target cannot resolve");
    assert!(matches!(err, OntologyError::Mapping(_)), "{err}");
    assert!(
        err.to_string().contains("relationship 'label-only'"),
        "{err}"
    );
    assert!(err.to_string().contains("'from'"), "{err}");
    assert!(err.to_string().contains("no node"), "{err}");

    let doc = parse_ontology_document(
        json_doc(
            ",\"nodes\":{\"N\":{\"label\":\"N\"}},\"relationships\":{\
             \"empty\":{\"type\":\"E\",\"from\":{\"node\":\"N\"},\"to\":{}}}",
        )
        .as_bytes(),
    )
    .unwrap();
    let err = to_graph_type_body(&doc).expect_err("the empty target cannot resolve");
    assert!(err.to_string().contains("relationship 'empty'"), "{err}");
    assert!(err.to_string().contains("'to'"), "{err}");
    assert!(err.to_string().contains("no node"), "{err}");
}

#[test]
fn nullable_and_empty_main_forms_validate_but_fail_loudly_at_apply() {
    // No identifying label: accepted at validation, loud at apply. Both the
    // null and the absent form are main-valid; the error names the entry.
    let doc = parse_ontology_document(json_doc(",\"nodes\":{\"N\":{\"label\":null}}").as_bytes())
        .expect("a null label is main-valid");
    let err = to_graph_type_body(&doc).expect_err("a node needs a label to be convertible");
    assert!(
        err.to_string()
            .contains("node 'N' has no identifying label"),
        "{err}"
    );
    let doc = parse_ontology_document(
        json_doc(",\"nodes\":{\"meaningless\":{\"description\":\"no label\"}}").as_bytes(),
    )
    .expect("a label-less node entry is main-valid");
    let err = to_graph_type_body(&doc).expect_err("a node needs a label to be convertible");
    assert!(
        err.to_string()
            .contains("node 'meaningless' has no identifying label"),
        "{err}"
    );

    // An empty constraint property list: accepted at validation, loud at
    // apply.
    let doc = parse_ontology_document(
        json_doc(
            ",\"nodes\":{\"N\":{\"label\":\"N\",\"constraints\":{\"c\":{\"type\":\"EXISTS\",\"properties\":[]}}}}",
        )
        .as_bytes(),
    )
    .expect("an empty constraint property list is main-valid");
    let err = to_graph_type_body(&doc).expect_err("a graph-type constraint needs a property");
    assert!(err.to_string().contains("constraint 'c'"), "{err}");
    assert!(err.to_string().contains("no properties"), "{err}");

    // A purely descriptive empty-named property is carried and compiles to
    // nothing; one that declares a type fails loudly.
    let doc = parse_ontology_document(
        json_doc(",\"nodes\":{\"N\":{\"label\":\"N\",\"properties\":{\"\":{}}}}").as_bytes(),
    )
    .expect("a descriptive empty-named property is main-valid");
    assert!(to_graph_type_body(&doc).unwrap().node_types[0]
        .properties
        .is_empty());
    let doc = parse_ontology_document(
        json_doc(
            ",\"nodes\":{\"N\":{\"label\":\"N\",\"properties\":{\"\":{\"type\":\"STRING\"}}}}",
        )
        .as_bytes(),
    )
    .unwrap();
    let err = to_graph_type_body(&doc).expect_err("an empty property name cannot be declared");
    assert!(err.to_string().contains("non-empty"), "{err}");
}
