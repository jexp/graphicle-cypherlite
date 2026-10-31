use super::*;
use crate::tests::doc;
use cypherlite_parser::{PredefinedKind, VectorCoordKind};
use serde_json::json;

/// A document with one node `N` carrying `properties` and `constraints`.
fn node_doc(properties: serde_json::Value, constraints: serde_json::Value) -> OntologyDocument {
    let node = json!({
        "label": "N",
        "properties": properties,
        "constraints": constraints,
    });
    doc(&format!(",\"nodes\":{{\"N\":{node}}}"))
}

/// A document with one node `N` whose only property is `p : <token>`.
fn token_doc(token: &str) -> OntologyDocument {
    node_doc(json!({"p": {"type": token}}), json!({}))
}

/// The one typed property declaration the token tests expect — property
/// `p` with no inline constraint — so each mapping arm states only its
/// `TypeExpr`.
fn expected_props(value_type: TypeExpr) -> Vec<GraphTypePropertyType> {
    vec![GraphTypePropertyType {
        name: "p".to_owned(),
        value_type,
        default_value: None,
        inline_constraint: None,
    }]
}

/// The mapping the test expects for one token, written out independently
/// of the implementation (ADR-0364 §5.2's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    Nothing,
    Predefined(PredefinedKind),
    List(PredefinedKind),
    Vector(VectorCoordKind),
    Loud,
}

fn expected_mapping(ty: Neo4jType) -> Expected {
    use Neo4jType as T;
    match ty {
        T::Any | T::ListAny => Expected::Nothing,
        T::Boolean => Expected::Predefined(PredefinedKind::Boolean),
        T::ListBoolean => Expected::List(PredefinedKind::Boolean),
        T::Date => Expected::Predefined(PredefinedKind::Date),
        T::ListDate => Expected::List(PredefinedKind::Date),
        T::Duration => Expected::Predefined(PredefinedKind::Duration),
        T::ListDuration => Expected::List(PredefinedKind::Duration),
        T::Float32 | T::ListFloat32 => Expected::Loud,
        T::Float => Expected::Predefined(PredefinedKind::Float),
        T::ListFloat => Expected::List(PredefinedKind::Float),
        T::Integer8
        | T::ListInteger8
        | T::Integer16
        | T::ListInteger16
        | T::Integer32
        | T::ListInteger32 => Expected::Loud,
        T::Integer => Expected::Predefined(PredefinedKind::Integer),
        T::ListInteger => Expected::List(PredefinedKind::Integer),
        T::LocalDatetime => Expected::Predefined(PredefinedKind::LocalDateTime),
        T::ListLocalDatetime => Expected::List(PredefinedKind::LocalDateTime),
        T::LocalTime => Expected::Predefined(PredefinedKind::LocalTime),
        T::ListLocalTime => Expected::List(PredefinedKind::LocalTime),
        T::Point => Expected::Predefined(PredefinedKind::Point),
        T::ListPoint => Expected::List(PredefinedKind::Point),
        T::String => Expected::Predefined(PredefinedKind::String),
        T::ListString => Expected::List(PredefinedKind::String),
        T::VectorFloat => Expected::Vector(VectorCoordKind::Float64),
        T::VectorFloat32 => Expected::Vector(VectorCoordKind::Float32),
        T::VectorInteger => Expected::Vector(VectorCoordKind::Int64),
        T::VectorInteger32 => Expected::Vector(VectorCoordKind::Int32),
        T::VectorInteger16 => Expected::Vector(VectorCoordKind::Int16),
        T::VectorInteger8 => Expected::Vector(VectorCoordKind::Int8),
        T::ZonedDatetime => Expected::Predefined(PredefinedKind::ZonedDateTime),
        T::ListZonedDatetime => Expected::List(PredefinedKind::ZonedDateTime),
        T::ZonedTime => Expected::Predefined(PredefinedKind::ZonedTime),
        T::ListZonedTime => Expected::List(PredefinedKind::ZonedTime),
        T::Uuid => Expected::Predefined(PredefinedKind::Uuid),
    }
}

#[test]
fn constraint_objects_become_named_graph_type_constraints() {
    let doc = doc(r#","nodes":{"N":{"label":"N",
            "properties":{"a":{"type":"STRING"},"b":{"type":"INTEGER"}},
            "constraints":{
                "n_ab":{"type":"KEY","properties":["a","b"],"name":"n_ab"},
                "n_b":{"type":"EXISTS","properties":["b"]}
            }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert_eq!(
        body.constraints,
        vec![
            GraphTypeConstraintSpec {
                name: Some("n_ab".to_owned()),
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["a".to_owned(), "b".to_owned()],
                kind: GraphTypeConstraintVerb::Key,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["b".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
        ]
    );
}

#[test]
fn untyped_property_flags_become_standalone_constraints() {
    let doc = doc(r#","nodes":{"N":{"label":"N","properties":{
            "anything":{"type":"ANY"},
            "required_any":{"type":"ANY","must_exist":true},
            "untyped_unique":{"unique":true},
            "untyped_key":{"key":true,"unique":true},
            "untyped_required":{"must_exist":true}
        }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert!(body.node_types[0].properties.is_empty());
    assert_eq!(
        body.constraints,
        vec![
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["required_any".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["untyped_unique".to_owned()],
                kind: GraphTypeConstraintVerb::Unique,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["untyped_key".to_owned()],
                kind: GraphTypeConstraintVerb::Key,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["untyped_required".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
        ]
    );
}

#[test]
fn a_constraint_object_restating_a_flag_replaces_it() {
    let doc = doc(r#","nodes":{"N":{"label":"N",
            "properties":{"a":{"type":"STRING","unique":true}},
            "constraints":{
                "n_a":{"type":"UNIQUE","properties":["a"],"name":"n_a"}
            }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert_eq!(body.constraints.len(), 1);
    assert_eq!(body.constraints[0].name.as_deref(), Some("n_a"));
}

/// The `must_exist` shorthand gets the same object-replaces-flag rule
/// as `key`/`unique`. On a typed property the flag compiles to the
/// declaration's `NOT NULL`, a different artifact than the
/// flag-derived constraint, so both the `NOT NULL` declaration and the
/// object survive.
#[test]
fn a_must_exist_object_replaces_the_shorthand_flag() {
    let doc = doc(r#","nodes":{"N":{"label":"N",
            "properties":{
                "untyped":{"type":"ANY","must_exist":true},
                "typed":{"type":"STRING","must_exist":true}
            },
            "constraints":{
                "n_untyped":{"type":"EXISTS","properties":["untyped"],"name":"n_untyped"},
                "n_typed":{"type":"EXISTS","properties":["typed"],"name":"n_typed"}
            }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert_eq!(
        body.node_types[0].properties,
        vec![GraphTypePropertyType {
            name: "typed".to_owned(),
            value_type: TypeExpr::Predefined {
                kind: PredefinedKind::String,
                not_null: true,
            },
            default_value: None,
            inline_constraint: None,
        }]
    );
    assert_eq!(
        body.constraints,
        vec![
            GraphTypeConstraintSpec {
                name: Some("n_untyped".to_owned()),
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["untyped".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: Some("n_typed".to_owned()),
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["typed".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
        ]
    );
}

#[test]
fn relationship_constraints_target_the_edge_type() {
    let doc = doc(r#","nodes":{"A":{"label":"A"},"B":{"label":"B"}},
        "relationships":{"R":{"type":"REL",
            "from":{"node":"A"},"to":{"node":"B"},
            "properties":{"since":{"type":"DATE","must_exist":true}},
            "constraints":{
                "r_since":{"type":"UNIQUE","properties":["since"],"name":"r_since"}
            }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert_eq!(
        body.edge_types[0].properties,
        vec![GraphTypePropertyType {
            name: "since".to_owned(),
            value_type: TypeExpr::Predefined {
                kind: PredefinedKind::Date,
                not_null: true,
            },
            default_value: None,
            inline_constraint: None,
        }]
    );
    assert_eq!(
        body.constraints,
        vec![GraphTypeConstraintSpec {
            name: Some("r_since".to_owned()),
            target: in_situ("REL"),
            is_edge: true,
            properties: vec!["since".to_owned()],
            kind: GraphTypeConstraintVerb::Unique,
            property_type: None,
            options: vec![],
        }]
    );
}

#[test]
fn temporal_tokens_map_like_the_parsers_bare_leaves() {
    let doc = doc(r#","nodes":{"N":{"label":"N","properties":{
            "zt":{"type":"ZONED TIME"},
            "lt":{"type":"LOCAL TIME"},
            "zdt":{"type":"ZONED DATETIME"},
            "ldt":{"type":"LOCAL DATETIME"}
        }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    let kinds: Vec<PredefinedKind> = body.node_types[0]
        .properties
        .iter()
        .map(|p| match p.value_type {
            TypeExpr::Predefined { kind, .. } => kind,
            ref other => panic!("expected a predefined type, got {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            PredefinedKind::ZonedTime,
            PredefinedKind::LocalTime,
            PredefinedKind::ZonedDateTime,
            PredefinedKind::LocalDateTime,
        ]
    );
}

/// `LIST<ANY>` declares nothing (no CIP-176 counterpart), so
/// `must_exist: true` gets a standalone existence constraint — the
/// same treatment bare `ANY` gets.
#[test]
fn list_any_with_must_exist_gets_an_existence_constraint() {
    let doc = doc(r#","nodes":{"N":{"label":"N","properties":{
            "anything_list":{"type":"LIST<ANY>","must_exist":true}
        }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert!(body.node_types[0].properties.is_empty());
    assert_eq!(
        body.constraints,
        vec![GraphTypeConstraintSpec {
            name: None,
            target: in_situ("N"),
            is_edge: false,
            properties: vec!["anything_list".to_owned()],
            kind: GraphTypeConstraintVerb::Exists,
            property_type: None,
            options: vec![],
        }]
    );
}

#[test]
fn endpoint_to_an_unknown_key_fails() {
    let doc = doc(r#","nodes":{"N":{"label":"N"}},
        "relationships":{"R":{"type":"REL",
            "from":{"node":"N"}, "to":{"node":"MISSING"}}}"#);
    let err = to_graph_type_body(&doc).expect_err("unknown endpoint key fails");
    assert!(err.to_string().contains("unknown node key"), "{err}");
}

#[test]
fn the_conversion_is_deterministic() {
    let document = doc(
        r#","nodes":{"N":{"label":"N","properties":{"id":{"type":"STRING","key":true}}}},
        "relationships":{"R":{"type":"R","from":{"node":"N"},"to":{"node":"N"}}}"#,
    );
    assert_eq!(
        to_graph_type_body(&document).expect("converts"),
        to_graph_type_body(&document).expect("converts")
    );
}

/// ADR-0364 §5.2: every token in main's closed 39-token `Neo4jType` enum
/// either compiles to a declaration or fails loudly. The expectations are
/// written independently of `Neo4jType::mapping`, so a token cannot be
/// silently skipped or reclassified without a failing test.
#[test]
fn every_neo4j_token_maps_or_fails_loudly() {
    let tokens: Vec<Neo4jType> = crate::model::NEO4J_TYPES
        .iter()
        .map(|(_, ty)| *ty)
        .collect();
    assert_eq!(
        tokens.len(),
        39,
        "main's closed Neo4jType enum has 39 tokens"
    );
    for ty in tokens {
        assert_eq!(
            Neo4jType::parse(ty.as_str()),
            Some(ty),
            "{} is not in the closed vocabulary",
            ty.as_str()
        );
        let result = to_graph_type_body(&token_doc(ty.as_str()));
        match expected_mapping(ty) {
            Expected::Nothing => {
                let body =
                    result.unwrap_or_else(|e| panic!("{} declares nothing: {e}", ty.as_str()));
                assert!(body.node_types[0].properties.is_empty(), "{}", ty.as_str());
                assert!(body.constraints.is_empty(), "{}", ty.as_str());
            }
            Expected::Predefined(kind) => {
                let body = result.unwrap_or_else(|e| panic!("{} maps: {e}", ty.as_str()));
                assert_eq!(
                    body.node_types[0].properties,
                    expected_props(TypeExpr::Predefined {
                        kind,
                        not_null: false,
                    }),
                    "{}",
                    ty.as_str()
                );
            }
            Expected::List(kind) => {
                let body = result.unwrap_or_else(|e| panic!("{} maps: {e}", ty.as_str()));
                assert_eq!(
                    body.node_types[0].properties,
                    expected_props(TypeExpr::List {
                        element: Box::new(TypeExpr::Predefined {
                            kind,
                            not_null: true,
                        }),
                        not_null: false,
                    }),
                    "{}",
                    ty.as_str()
                );
            }
            Expected::Vector(coord) => {
                let body = result.unwrap_or_else(|e| panic!("{} maps: {e}", ty.as_str()));
                assert_eq!(
                    body.node_types[0].properties,
                    expected_props(TypeExpr::Vector {
                        coord: Some(coord),
                        dimension: None,
                        not_null: false,
                    }),
                    "{}",
                    ty.as_str()
                );
            }
            Expected::Loud => {
                let err = result.expect_err("a token with no CIP-176 counterpart fails");
                let text = err.to_string();
                assert!(
                    matches!(&err, OntologyError::Mapping(_)),
                    "{}: {text}",
                    ty.as_str()
                );
                assert!(text.contains(ty.as_str()), "{}: {text}", ty.as_str());
                assert!(text.contains("property 'p'"), "{}: {text}", ty.as_str());
                assert!(text.contains("no CIP-176"), "{}: {text}", ty.as_str());
            }
        }
    }
}

#[test]
fn explicit_exists_key_and_unique_map_over_listed_properties() {
    let doc = doc(r#","nodes":{"N":{"label":"N",
            "properties":{"a":{"type":"STRING"},"b":{"type":"INTEGER"},"c":{"type":"BOOLEAN"}},
            "constraints":{
                "n_ab":{"type":"KEY","properties":["b","a"],"name":"n_ab"},
                "n_c":{"type":"UNIQUE","properties":["c"]},
                "n_a":{"type":"EXISTS","properties":["a"],"name":"n_a"}
            }}}"#);
    let body = to_graph_type_body(&doc).expect("converts");
    assert_eq!(
        body.constraints,
        vec![
            GraphTypeConstraintSpec {
                name: Some("n_ab".to_owned()),
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["b".to_owned(), "a".to_owned()],
                kind: GraphTypeConstraintVerb::Key,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: None,
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["c".to_owned()],
                kind: GraphTypeConstraintVerb::Unique,
                property_type: None,
                options: vec![],
            },
            GraphTypeConstraintSpec {
                name: Some("n_a".to_owned()),
                target: in_situ("N"),
                is_edge: false,
                properties: vec!["a".to_owned()],
                kind: GraphTypeConstraintVerb::Exists,
                property_type: None,
                options: vec![],
            },
        ]
    );
}

/// The document schema cannot express the cross-reference, so a constraint
/// naming an undeclared property would otherwise reach the graph-type body
/// dangling (ADR-0364 §5.3).
#[test]
fn constraint_properties_must_belong_to_the_owner() {
    let doc = doc(
        r#","nodes":{"N":{"label":"N","properties":{"a":{"type":"STRING"}},
            "constraints":{"n_missing":{"type":"UNIQUE","properties":["ghost"]}}}}"#,
    );
    let err = to_graph_type_body(&doc).expect_err("a dangling constraint fails");
    let text = err.to_string();
    assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
    assert!(text.contains("node 'N'"), "{text}");
    assert!(text.contains("constraint 'n_missing'"), "{text}");
    assert!(text.contains("property 'ghost'"), "{text}");
}

/// A node constraint's `label` compiles against the owner when it names one
/// of the node's labels (identifying or implied); any other value fails
/// naming both (ADR-0364 §5.3/§5.4).
#[test]
fn node_constraint_labels_must_match_a_node_label() {
    let accepted = doc(r#","nodes":{"N":{"label":"N","labels":{"implied":["Base"]},
            "properties":{"a":{"type":"STRING"}},
            "constraints":{"n_a":{"type":"EXISTS","properties":["a"],"label":"Base"}}}}"#);
    let body = to_graph_type_body(&accepted).expect("an implied label compiles against the owner");
    assert_eq!(body.constraints.len(), 1);
    assert_eq!(body.constraints[0].target, in_situ("N"));

    let rejected = doc(r#","nodes":{"N":{"label":"N",
            "properties":{"a":{"type":"STRING"}},
            "constraints":{"n_a":{"type":"EXISTS","properties":["a"],"label":"Other"}}}}"#);
    let err = to_graph_type_body(&rejected).expect_err("an unknown constraint label fails");
    let text = err.to_string();
    assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
    assert!(text.contains("constraint 'n_a'"), "{text}");
    assert!(text.contains("label 'Other'"), "{text}");
}

#[test]
fn empty_named_properties_fail_when_declared_or_constrained() {
    let declared = node_doc(json!({"": {"type": "STRING"}}), json!({}));
    let err = to_graph_type_body(&declared).expect_err("an empty declared name fails");
    let text = err.to_string();
    assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
    assert!(text.contains("node 'N'"), "{text}");
    assert!(text.contains("property ''"), "{text}");
    assert!(text.contains("non-empty"), "{text}");

    let flagged = node_doc(json!({"": {"must_exist": true}}), json!({}));
    let err = to_graph_type_body(&flagged).expect_err("an empty constrained name fails");
    let text = err.to_string();
    assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
    assert!(text.contains("property ''"), "{text}");
    assert!(text.contains("non-empty"), "{text}");

    let listed = node_doc(
        json!({"": {}}),
        json!({"c": {"type": "EXISTS", "properties": [""]}}),
    );
    let err = to_graph_type_body(&listed).expect_err("an empty listed name fails");
    let text = err.to_string();
    assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
    assert!(text.contains("constraint 'c'"), "{text}");
    assert!(text.contains("empty property name"), "{text}");
}

#[test]
fn vector_tokens_map_coordinate_kinds_and_dimensions() {
    let doc = doc(r#","nodes":{"N":{"label":"N","properties":{
            "vf":{"type":"VECTOR<FLOAT>","dimension":3},
            "vf32":{"type":"VECTOR<FLOAT32>","dimension":4},
            "vi":{"type":"VECTOR<INTEGER>","dimension":5},
            "vi32":{"type":"VECTOR<INTEGER32>","dimension":6},
            "vi16":{"type":"VECTOR<INTEGER16>","dimension":7},
            "vi8":{"type":"VECTOR<INTEGER8>","dimension":8}
        }}}"#);
    let body = to_graph_type_body(&doc).expect("vectors convert");
    let vectors: Vec<(Option<VectorCoordKind>, Option<i64>)> = body.node_types[0]
        .properties
        .iter()
        .map(|p| match p.value_type {
            TypeExpr::Vector {
                coord, dimension, ..
            } => (coord, dimension),
            ref other => panic!("expected a vector type, got {other:?}"),
        })
        .collect();
    assert_eq!(
        vectors,
        [
            (Some(VectorCoordKind::Float64), Some(3)),
            (Some(VectorCoordKind::Float32), Some(4)),
            (Some(VectorCoordKind::Int64), Some(5)),
            (Some(VectorCoordKind::Int32), Some(6)),
            (Some(VectorCoordKind::Int16), Some(7)),
            (Some(VectorCoordKind::Int8), Some(8)),
        ]
    );
}

/// A graph-type vector needs a positive dimension; main accepts any
/// integer and leaves it nullable (ADR-0364 §3).
#[test]
fn a_non_positive_vector_dimension_fails_loudly() {
    for dimension in [0, -1] {
        let doc = node_doc(
            json!({"p": {"type": "VECTOR<FLOAT>", "dimension": dimension}}),
            json!({}),
        );
        let err = to_graph_type_body(&doc).expect_err("a non-positive dimension fails");
        let text = err.to_string();
        assert!(matches!(&err, OntologyError::Mapping(_)), "{text}");
        assert!(text.contains("property 'p'"), "{text}");
        assert!(text.contains(&format!("dimension {dimension}")), "{text}");
        assert!(text.contains("positive"), "{text}");
    }
}

#[test]
fn a_vector_without_a_dimension_stays_unconstrained() {
    let doc = token_doc("VECTOR<FLOAT>");
    let body = to_graph_type_body(&doc).expect("an absent dimension is allowed");
    assert_eq!(
        body.node_types[0].properties[0].value_type,
        TypeExpr::Vector {
            coord: Some(VectorCoordKind::Float64),
            dimension: None,
            not_null: false,
        }
    );
}

/// `dimension` only applies "when it holds a vector": on any other token it
/// is carried and has no effect (ADR-0364 §3).
#[test]
fn a_dimension_on_a_non_vector_token_has_no_effect() {
    let doc = node_doc(json!({"p": {"type": "STRING", "dimension": -5}}), json!({}));
    let body = to_graph_type_body(&doc).expect("a stray dimension does not fail");
    assert_eq!(
        body.node_types[0].properties[0].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: false,
        }
    );
}
