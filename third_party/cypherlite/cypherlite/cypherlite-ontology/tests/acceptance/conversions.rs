//! JSON/YAML equivalence and the `db.ontology.get` format conversions with
//! key order preserved (ADR-0227 §3, ADR-0364 §2).

use cypherlite_ontology::{detect_format, parse_ontology_document, DocumentFormat, Value};

// ---------------------------------------------------------------------------
// JSON / YAML equivalence
// ---------------------------------------------------------------------------

const TWIN_JSON: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"twin\",\"version\":\"1\",\"name\":\"Twin\",",
    "\"nodes\":{\"Person\":{\"label\":\"Person\",\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"key\":true},",
    "\"tags\":{\"type\":\"LIST<STRING>\"}}}},",
    "\"relationships\":{\"KNOWS\":{\"type\":\"KNOWS\",",
    "\"from\":{\"node\":\"Person\"},",
    "\"to\":{\"node\":\"Person\",\"max_count\":5}}}}"
);

const TWIN_YAML: &str = "\
schema: https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json
id: twin
version: \"1\"
name: Twin
nodes:
  Person:
    label: Person
    properties:
      id:
        type: STRING
        key: true
      tags:
        type: LIST<STRING>
relationships:
  KNOWS:
    type: KNOWS
    from:
      node: Person
    to:
      node: Person
      max_count: 5
";

#[test]
fn json_and_yaml_forms_validate_to_the_same_model() {
    let from_json = parse_ontology_document(TWIN_JSON.as_bytes()).unwrap();
    let from_yaml = parse_ontology_document(TWIN_YAML.as_bytes()).unwrap();
    assert_eq!(from_json, from_yaml);
    assert_eq!(from_yaml.version(), "1");
}

#[test]
fn stored_format_sniff() {
    assert_eq!(detect_format(TWIN_JSON.as_bytes()), DocumentFormat::Json);
    assert_eq!(detect_format(TWIN_YAML.as_bytes()), DocumentFormat::Yaml);
    assert_eq!(DocumentFormat::Json.as_str(), "json");
    assert_eq!(DocumentFormat::Yaml.as_str(), "yaml");
}

// ---------------------------------------------------------------------------
// Round trips and format conversion
// ---------------------------------------------------------------------------

/// A main-shape document exercising floats, nested payloads, empty
/// containers and strings that need quoting, in compact canonical JSON.
const ROUNDTRIP_JSON: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"roundtrip\",\"version\":\"2.10\",\"name\":\"Round Trip\",",
    "\"description\":\"colon: and newline\\ninside\",",
    "\"nodes\":{\"N\":{\"label\":\"N\",\"properties\":{",
    "\"p\":{\"type\":\"VECTOR<FLOAT>\",\"dimension\":3,\"one_of\":[\"x\",\"y\"],",
    "\"extensions\":{\"custom\":{\"list\":[],\"obj\":{},\"score\":0.5}}}}}},",
    "\"relationships\":{\"R\":{\"type\":\"R\",\"from\":{\"node\":\"N\",\"count\":0},",
    "\"to\":{\"node\":\"N\",\"min_count\":1,\"max_count\":2}}}}"
);

#[test]
fn json_round_trip_is_byte_identical_for_canonical_input() {
    let doc = parse_ontology_document(ROUNDTRIP_JSON.as_bytes()).unwrap();
    assert_eq!(doc.to_json().unwrap(), ROUNDTRIP_JSON);
    // And the emitted JSON re-parses to the same model.
    let again = parse_ontology_document(doc.to_json().unwrap().as_bytes()).unwrap();
    assert_eq!(doc, again);
}

#[test]
fn yaml_round_trip_is_a_fixpoint() {
    let doc = parse_ontology_document(TWIN_YAML.as_bytes()).unwrap();
    let yaml = doc.to_yaml().unwrap();
    let reparsed = parse_ontology_document(yaml.as_bytes()).unwrap();
    assert_eq!(doc, reparsed);
    assert_eq!(reparsed.to_yaml().unwrap(), yaml);
}

fn root_keys(doc: &cypherlite_ontology::OntologyDocument) -> Vec<String> {
    match doc.root() {
        Value::Object(map) => map.keys().cloned().collect(),
        _ => panic!("root is an object"),
    }
}

#[test]
fn json_to_yaml_conversion_preserves_the_model_and_key_order() {
    let doc = parse_ontology_document(ROUNDTRIP_JSON.as_bytes()).unwrap();
    let yaml = doc.to_yaml().unwrap();
    let reparsed = parse_ontology_document(yaml.as_bytes()).unwrap();
    assert_eq!(doc, reparsed);
    // Root key order is preserved: source order, not sorted order.
    assert_eq!(
        root_keys(&reparsed),
        [
            "schema",
            "id",
            "version",
            "name",
            "description",
            "nodes",
            "relationships"
        ]
    );
    // Both `get` directions reproduce the canonical JSON byte-for-byte, so
    // a JSON → YAML → JSON conversion is lossless (key order included).
    assert_eq!(reparsed.to_json().unwrap(), ROUNDTRIP_JSON);
}

#[test]
fn yaml_to_json_conversion_preserves_the_model_and_key_order() {
    let doc = parse_ontology_document(TWIN_YAML.as_bytes()).unwrap();
    let json = doc.to_json().unwrap();
    let reparsed = parse_ontology_document(json.as_bytes()).unwrap();
    assert_eq!(doc, reparsed);
    assert_eq!(
        root_keys(&reparsed),
        ["schema", "id", "version", "name", "nodes", "relationships"]
    );
    // And the YAML written back from the JSON form is a fixpoint.
    let yaml = reparsed.to_yaml().unwrap();
    assert_eq!(
        parse_ontology_document(yaml.as_bytes())
            .unwrap()
            .to_yaml()
            .unwrap(),
        yaml
    );
}

#[test]
fn sorted_canonical_json_for_payloads() {
    let value = Value::Object(
        [
            ("b".to_owned(), Value::Integer(1)),
            ("a".to_owned(), Value::Boolean(true)),
        ]
        .into_iter()
        .collect(),
    );
    assert_eq!(
        cypherlite_ontology::to_json_sorted_string(&value).unwrap(),
        "{\"a\":true,\"b\":1}"
    );
}
