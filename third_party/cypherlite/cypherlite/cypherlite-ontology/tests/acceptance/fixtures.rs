//! The vendored fixtures: the authored main-shape example and the schema it
//! validates against (ADR-0364 §2/§6).

use cypherlite_ontology::{parse_ontology_document, ConstraintType, Neo4jType, PINNED_SCHEMA_URI};

use crate::support::{at, neo4j_type_tokens, EXAMPLE_JSON, VENDORED_SCHEMA};

// ---------------------------------------------------------------------------
// Vendored fixtures
// ---------------------------------------------------------------------------

#[test]
fn vendored_example_document_validates() {
    let doc = parse_ontology_document(EXAMPLE_JSON.as_bytes()).expect("example validates");
    assert_eq!(doc.schema(), PINNED_SCHEMA_URI);
    assert_eq!(doc.id(), "cypherlite-ontology-example");
    assert_eq!(doc.version(), "1");
    assert_eq!(doc.name(), Some("CypherLite main-shape validator example"));
    assert!(doc.description().is_some());

    // Nodes in document order; the map key is the element identity.
    let node_keys: Vec<&str> = doc.nodes().iter().map(|n| n.key.as_str()).collect();
    assert_eq!(
        node_keys,
        ["person", "movie", "no-identifier", "type-showcase"]
    );
    let person = &doc.nodes()[0];
    assert_eq!(person.identifying_label(), Some("Person"));
    assert_eq!(person.aliases, ["Individual"]);
    let prop_ids: Vec<&str> = person.properties.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        prop_ids,
        [
            "id",
            "name",
            "email",
            "score",
            "location",
            "embedding",
            "tags",
            ""
        ]
    );
    assert_eq!(person.properties[0].property_type, Some(Neo4jType::String));
    assert_eq!(person.properties[0].key, Some(true));
    assert_eq!(
        person.properties[1].one_of.as_deref(),
        Some(&["Ada".to_owned(), "Grace".to_owned(), "Linus".to_owned()][..])
    );
    // Nullable flags and dimensions are carried as `None`, not defaults.
    assert_eq!(person.properties[3].dimension, None);
    assert_eq!(person.properties[3].must_exist, None);

    // Every constraint type of main's enum appears in the authored example.
    let kinds: Vec<ConstraintType> = person
        .constraints
        .iter()
        .map(|c| c.constraint_type)
        .collect();
    assert_eq!(
        kinds,
        [
            ConstraintType::Key,
            ConstraintType::Exists,
            ConstraintType::Unique,
            ConstraintType::PropertyType,
            ConstraintType::Exists,
        ]
    );
    assert_eq!(person.constraints[0].id, "person-id-key");
    assert_eq!(person.constraints[1].name, None);
    assert!(person.constraints[4].properties.is_empty());

    // A node with a null identifier: main-valid, `apply` is the loud gate.
    assert_eq!(doc.nodes()[2].identifying_label(), None);

    // The labels-object form and the display `name`.
    let movie = &doc.nodes()[1];
    assert_eq!(movie.identifying_label(), Some("Movie"));
    assert_eq!(movie.labels.implied, ["Media"]);
    assert_eq!(movie.labels.optional, ["Classic"]);
    assert_eq!(movie.name, None);

    // Every Neo4jType token is pinned by the type-showcase node.
    let showcase = &doc.nodes()[3];
    assert_eq!(showcase.properties.len(), 39);

    // Relationships and `RelationshipTarget` forms, in document order.
    let rel_keys: Vec<&str> = doc.relationships().iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        rel_keys,
        ["acted-in", "label-only", "empty-targets", "produced"]
    );
    let acted_in = &doc.relationships()[0];
    assert_eq!(acted_in.rel_type, "ACTED_IN");
    assert_eq!(acted_in.from.node.as_deref(), Some("person"));
    assert_eq!(acted_in.from.min_count, Some(0));
    assert_eq!(acted_in.to.count, None);
    let label_only = &doc.relationships()[1];
    assert_eq!(label_only.from.node, None);
    assert_eq!(label_only.from.label.as_deref(), Some("Person"));
    let empty = &doc.relationships()[2];
    assert_eq!(empty.from.node, None);
    assert_eq!(empty.to.min_count, Some(-1));
    let produced = &doc.relationships()[3];
    assert_eq!(produced.from.count, Some(2));
    assert_eq!(produced.to.min_count, Some(1));
    assert_eq!(produced.to.max_count, Some(5));

    // Extension payloads stay on the retained root value tree, in source
    // order; there is no second typed carrier (ADR-0364 §5.1).
    let root = doc.root();
    let node_extensions = at(root, &["nodes", "person", "extensions"]);
    let indexes = at(node_extensions, &["indexes"])
        .as_object()
        .expect("indexes is an object");
    let index_ids: Vec<&str> = indexes.keys().map(String::as_str).collect();
    assert_eq!(
        index_ids,
        [
            "person-range",
            "person-text",
            "person-lookup",
            "person-fulltext",
            "person-point",
            "person-vector"
        ]
    );
    assert_eq!(
        at(
            root,
            &[
                "nodes",
                "person",
                "extensions",
                "indexes",
                "person-range",
                "type"
            ]
        )
        .as_str(),
        Some("RANGE")
    );
    assert!(!at(node_extensions, &["display"]).is_null());

    let document_extensions = at(root, &["extensions"]);
    let tables = at(document_extensions, &["tables"])
        .as_object()
        .expect("tables is an object");
    assert_eq!(
        tables.keys().map(String::as_str).collect::<Vec<_>>(),
        ["people", "movies"]
    );
    let mappings = at(document_extensions, &["mappings"])
        .as_object()
        .expect("mappings is an object");
    assert_eq!(
        mappings.keys().map(String::as_str).collect::<Vec<_>>(),
        ["import"]
    );
    assert!(!at(document_extensions, &["custom"]).is_null());
}

#[test]
fn vendored_schema_is_the_main_1_0_0_schema() {
    let schema: serde_json::Value = serde_json::from_str(VENDORED_SCHEMA).unwrap();
    assert_eq!(schema["$id"], PINNED_SCHEMA_URI);
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, ["schema", "version", "id"]);
    assert_eq!(schema["properties"]["version"]["type"], "string");
    let tokens = neo4j_type_tokens();
    assert_eq!(tokens.len(), 39);
    assert_eq!(
        schema["$defs"]["RelationshipTarget"]["required"],
        serde_json::json!([])
    );
}
