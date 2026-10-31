//! Ontology management through the `db.ontology.*` procedures (ADR-0121,
//! ADR-0226, ADR-0227 — Neo4j's CIP-287 surface): `set` stores a validated
//! JSON or YAML document verbatim, `get` returns it in the requested format
//! (`yaml` by default), `apply` turns it into the database's graph type
//! (failing on violating data), and `drop` removes it. The stored document
//! is the sole source of truth: the `system_ontologies` namespace is
//! listed by `SHOW DATABASES` but not queryable — every other statement
//! routed to it fails with 42N17 (ADR-0229).
//!
//! Run with `cargo run -p cypherlite --example ontology`.

use std::collections::BTreeMap;

use cypherlite::{Database, Value};

/// A small ontology in YAML: one node type with a key property.
const ONTOLOGY: &str = "\
schema: https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json
id: example
version: \"1\"
nodes:
  Person:
    label: Person
    properties:
      id:
        type: STRING
        key: true
";

fn main() -> cypherlite::Result<()> {
    // The `db.ontology.*` family is gated behind the preview opt-in
    // (ADR-0378). This in-process mutation is example-only: a library must
    // not set the process environment. Library callers instead have the
    // process owner set CYPHERLITE_PREVIEW=ontology before open — the
    // documented deployment path. The variable is read once, at open, so
    // it must be set before the call below.
    std::env::set_var("CYPHERLITE_PREVIEW", "ontology");
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;

    // set validates the document (pinned spec version, schema shape) and
    // stores the submitted bytes verbatim.
    conn.query_with_params(
        "CALL db.ontology.set($ontology)",
        BTreeMap::from([("ontology".to_string(), Value::String(ONTOLOGY.into()))]),
    )?;

    // get returns the stored bytes unchanged when the requested format
    // matches the stored one (YAML here); 'json' converts canonically.
    let result = conn.query("CALL db.ontology.get() YIELD ontology RETURN ontology")?;
    assert_eq!(result.rows[0][0], Value::String(ONTOLOGY.into()));
    let result = conn.query("CALL db.ontology.get('json') YIELD ontology RETURN ontology")?;
    assert!(matches!(&result.rows[0][0], Value::String(json) if json.starts_with('{')));

    // system_ontologies is listed but not queryable: the stored document
    // is reachable only through the procedures, so an ordinary read routed
    // there fails with the 42N17 text (ADR-0229 §1/§2).
    let result = conn.query("SHOW DATABASES YIELD name RETURN name")?;
    assert_eq!(result.rows[2][0], Value::String("system_ontologies".into()));
    let err = conn
        .query("USE system_ontologies MATCH (n) RETURN count(n) AS c")
        .unwrap_err();
    assert!(err.to_string().ends_with(
        "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
    ));

    // apply sets the current graph type from the ontology; data violating
    // it (a Person without the key property id) would fail the call.
    conn.query("CALL db.ontology.apply()")?;
    let result = conn.query("SHOW CURRENT GRAPH TYPE")?;
    assert!(result.rows[0][0].as_string().unwrap().contains("Person"));
    // The derived key constraint now gates writes.
    assert!(conn.query("CREATE (:Person)").is_err());
    conn.query("CREATE (:Person {id: 'p1'})")?;

    // drop removes the document; get then yields null.
    conn.query("CALL db.ontology.drop()")?;
    let result = conn.query("CALL db.ontology.get() YIELD ontology RETURN ontology")?;
    assert_eq!(result.rows[0][0], Value::Null);
    Ok(())
}
