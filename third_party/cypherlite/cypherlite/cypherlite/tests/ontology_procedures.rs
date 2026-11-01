//! ADR-0121 as amended by ADR-0227 (CIP-287) and ADR-0364 (main's
//! document shape): the `db.ontology.set` /
//! `get` / `drop` / `apply` procedures, end to end through the facade —
//! signatures and `SHOW PROCEDURES` listing, validation, format
//! conversion, transaction and namespace rules, and `apply`'s
//! validate-first data check (ADR-0226 §3).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite::{Database, Value};
use cypherlite_storage::{MemoryVfs, Vfs};

use common::{err_text, get_doc, set_doc};

/// Main's Ontology Graph Spec 1.0.0 `$id` — the only accepted document
/// `schema` URI (ADR-0364 §1).
const SCHEMA_URI: &str = "https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json";

/// A small valid document as canonical compact JSON (same shape as the
/// YAML twin below — the cross-format conversion test depends on the two
/// denoting the same document in the same key order).
const DOC_JSON: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"twin\",\"version\":\"1\",\"name\":\"Twin\",",
    "\"nodes\":{\"Person\":{\"label\":\"Person\",\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"key\":true},",
    "\"tags\":{\"type\":\"LIST<STRING>\"}}}},",
    "\"relationships\":{\"KNOWS\":{\"type\":\"KNOWS\",",
    "\"from\":{\"node\":\"Person\"},",
    "\"to\":{\"node\":\"Person\",\"max_count\":5}}}}"
);

/// The same document in YAML, with a comment and free formatting — the
/// verbatim-storage proof (ADR-0227 §2).
const DOC_YAML: &str = "\
# leading comment, kept verbatim
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

fn memory_db() -> (Arc<dyn Vfs>, Database) {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let db = Database::open_vfs(Arc::clone(&vfs), Default::default()).unwrap();
    (vfs, db)
}

/// (1) set → get round-trips: stored bytes unchanged when the requested
/// format matches the stored one (either syntax), canonical conversion
/// otherwise; the document survives a reopen.
#[test]
fn set_get_round_trip_yaml_and_json() {
    common::preview_ontology();
    let (vfs, db) = memory_db();
    let mut conn = db.connect().unwrap();

    // YAML in: default get returns the identical bytes, comments included.
    set_doc(&mut conn, DOC_YAML).unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_YAML.into())
    );
    // Converted to JSON it equals the canonical JSON twin byte-for-byte.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
    // An explicit 'yaml' request still returns the stored bytes.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('yaml') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_YAML.into())
    );

    // JSON in: get('json') returns the identical bytes, get() converts.
    set_doc(&mut conn, DOC_JSON).unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
    let yaml = get_doc(
        &mut conn,
        "CALL db.ontology.get() YIELD ontology RETURN ontology",
    );
    let Value::String(yaml) = yaml else {
        panic!("expected a string, got {yaml:?}")
    };
    // The canonical YAML conversion (the stored document is JSON, the
    // default format is YAML): block style, document order kept.
    assert!(
        yaml.contains("neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json"),
        "{yaml}"
    );
    assert!(yaml.contains("id: twin"), "{yaml}");
    assert!(yaml.contains("key: true"), "{yaml}");

    // An unknown format is a typed argument error naming the two.
    let err = err_text(conn.query("CALL db.ontology.get('xml') YIELD ontology RETURN ontology"));
    assert!(
        err.contains("unknown db.ontology.get format 'xml'; expected 'yaml' or 'json'"),
        "{err}"
    );

    // The document survives a reopen (WAL replay on the shared VFS; a
    // graceful drop discards a non-durable store, ADR-0105, so leak it).
    drop(conn);
    db.leak_without_checkpoint();
    let db = Database::open_vfs(vfs, Default::default()).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
    // The stored document is JSON: the default (YAML) get converts, and
    // the same bytes survive the reopen unchanged under a matching get.
    let yaml = get_doc(
        &mut conn,
        "CALL db.ontology.get() YIELD ontology RETURN ontology",
    );
    assert!(
        matches!(&yaml, Value::String(y) if y.contains("id: twin")),
        "{yaml:?}"
    );
}

/// (2) `set` validates before any commit: a schema violation names the
/// field, a newer spec version names 1.0.0 as the highest supported, and
/// an oversized document is refused — none of them touch the store.
#[test]
fn set_validates_before_committing() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    set_doc(&mut conn, DOC_JSON).unwrap();

    // A missing required field: the typed error names it.
    let err = err_text(set_doc(
        &mut conn,
        &format!("{{\"schema\":\"{SCHEMA_URI}\",\"id\":\"x\"}}"),
    ));
    assert!(err.contains("missing required field 'version'"), "{err}");

    // A newer spec version fails, naming 1.0.0 as the highest supported.
    let err = err_text(set_doc(
        &mut conn,
        "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/2.0.0/schema.json\",\"id\":\"x\",\"version\":\"1\"}",
    ));
    assert!(err.contains("Unsupported ontology spec version: 'https://neo4j.com/docs/ontology-graph-spec/2.0.0/schema.json'"), "{err}");
    assert!(
        err.contains("supports up to Ontology Graph Spec 1.0.0"),
        "{err}"
    );

    // Over the 16 MiB envelope (ADR-0120 §3): refused before any commit.
    let huge = format!(
        "{{\"schema\":\"{SCHEMA_URI}\",\"id\":\"x\",\"version\":\"1\",\"description\":\"{}\"}}",
        "a".repeat(16 * 1024 * 1024)
    );
    let err = err_text(set_doc(&mut conn, &huge));
    assert!(err.contains("16 MiB"), "{err}");

    // Nothing was replaced.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
}

/// (2b) The hard cutover at the procedure boundary (ADR-0364 §1): the old
/// pinned URI fails at the gate, and a main-URI document carrying the old
/// shape fails structural validation on its first old member.
#[test]
fn set_rejects_pre_bump_documents() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();

    // The old URI is a different dialect: the gate refuses it before
    // structure is looked at.
    let err = err_text(set_doc(
        &mut conn,
        "{\"schema\":\"https://neo4j.com/ontology-graph-spec/1.0.0/schema.json\",\
         \"id\":\"x\",\"version\":\"1\"}",
    ));
    assert!(err.contains("Unsupported ontology spec version"), "{err}");
    assert!(err.contains(SCHEMA_URI), "{err}");

    // The new URI with an integer version: structural validation names the
    // field.
    let err = err_text(set_doc(
        &mut conn,
        &format!("{{\"schema\":\"{SCHEMA_URI}\",\"id\":\"x\",\"version\":1}}"),
    ));
    assert!(err.contains("/version"), "{err}");
    assert!(err.contains("must be a string"), "{err}");

    // The renamed property flag is an unknown member now.
    let err = err_text(set_doc(
        &mut conn,
        &format!(
            "{{\"schema\":\"{SCHEMA_URI}\",\"id\":\"x\",\"version\":\"1\",\
             \"nodes\":{{\"N\":{{\"label\":\"N\",\"properties\":{{\"p\":{{\"mustExist\":true}}}}}}}}}}"
        ),
    ));
    assert!(err.contains("mustExist"), "{err}");
    assert!(err.contains("unknown field"), "{err}");

    // None of the rejected sets touched the store.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null
    );
}

/// (3) `drop` removes the document, succeeds when none is set, and the
/// empty state survives a reopen.
#[test]
fn drop_removes_the_document_and_is_idempotent() {
    common::preview_ontology();
    let (vfs, db) = memory_db();
    let mut conn = db.connect().unwrap();

    // Drop with none set succeeds.
    conn.query("CALL db.ontology.drop()").unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null
    );

    set_doc(&mut conn, DOC_JSON).unwrap();
    conn.query("CALL db.ontology.drop()").unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null
    );

    drop(conn);
    db.leak_without_checkpoint();
    let db = Database::open_vfs(vfs, Default::default()).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null
    );
}

/// (4) Transaction rules (ADR-0121 §3): set/drop are implicit-only with
/// Neo4j's exact text; a statement calling them may not write anything
/// else (42N03); `get` reads the calling transaction's captured document.
#[test]
fn transaction_rules() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();

    // set/drop in an explicit transaction: the exact implicit-only text.
    conn.begin().unwrap();
    let err = err_text(set_doc(&mut conn, DOC_JSON));
    assert!(
        err.contains(
            "'db.ontology.set' can only be executed in an implicit transaction, \
             but tried to execute in an explicit transaction."
        ),
        "{err}"
    );
    conn.rollback().unwrap();
    conn.begin().unwrap();
    let err = err_text(conn.query("CALL db.ontology.drop()"));
    assert!(
        err.contains(
            "'db.ontology.drop' can only be executed in an implicit transaction, \
             but tried to execute in an explicit transaction."
        ),
        "{err}"
    );
    conn.rollback().unwrap();

    // A statement calling set may not write anything else (42N03).
    let err = err_text(conn.query_with_params(
        "CREATE (n) CALL db.ontology.set($ontology)",
        BTreeMap::from([("ontology".to_string(), Value::String(DOC_JSON.into()))]),
    ));
    assert!(
        err.contains("Writing to more than one database per transaction is not allowed."),
        "{err}"
    );
    let err = err_text(conn.query("CREATE (n) CALL db.ontology.drop()"));
    assert!(
        err.contains("Writing to more than one database per transaction is not allowed."),
        "{err}"
    );
    // The failed statements wrote nothing.
    let result = conn.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(result.rows[0][0], Value::Int(0));
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null
    );

    // get reads the calling transaction's captured document: a transaction
    // that began before the set sees the pre-set state (none), and after
    // commit sees the committed document (ADR-0121 §3, begin-time capture).
    conn.begin().unwrap();
    let mut writer = db.connect().unwrap();
    set_doc(&mut writer, DOC_JSON).unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get() YIELD ontology RETURN ontology"
        ),
        Value::Null,
        "the open transaction keeps its begin-time view of the document"
    );
    conn.commit().unwrap();
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
}

/// (5) `apply`: fails when no ontology is set; fails naming the constraint
/// when existing data violates the resulting graph type (ADR-0226 §3);
/// applies the type on conforming data; runs in explicit transactions.
#[test]
fn apply_semantics() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();

    // No ontology set.
    let err = err_text(conn.query("CALL db.ontology.apply()"));
    assert!(
        err.contains("no ontology is set for database 'neo4j'"),
        "{err}"
    );

    // Violating data: a Person without the key property `id`.
    conn.query("CREATE (:Person {name: 'noid'})").unwrap();
    set_doc(&mut conn, DOC_JSON).unwrap();
    let err = err_text(conn.query("CALL db.ontology.apply()"));
    assert!(err.contains("ConstraintValidationFailed"), "{err}");
    assert!(err.contains("key_Person_id"), "{err}");
    assert!(err.contains("4:cypherlite:n0"), "{err}");
    // The failed apply changed nothing: the graph type is still empty.
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::String("{}".into()));

    // Fix the data; apply now succeeds and sets the current graph type.
    conn.query("MATCH (n:Person) SET n.id = 'p0'").unwrap();
    conn.query("CALL db.ontology.apply()").unwrap();
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    let graph_type = result.rows[0][0].as_string().unwrap().to_string();
    assert!(graph_type.contains("Person"), "{graph_type}");
    assert!(graph_type.contains("KNOWS"), "{graph_type}");
    // And the derived key constraint now gates writes.
    let err = err_text(conn.query("CREATE (:Person)"));
    assert!(err.contains("ConstraintValidationFailed"), "{err}");

    // apply inside an explicit transaction (ADR-0121 §3), then rollback
    // leaves the committed graph type alone.
    conn.query("CREATE (:Movie {title: 'm'})").unwrap();
    conn.begin().unwrap();
    conn.query("CALL db.ontology.apply()").unwrap();
    conn.rollback().unwrap();
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    assert_eq!(
        result.rows[0][0].as_string().unwrap(),
        graph_type.as_str(),
        "rollback discards the re-applied graph type"
    );
    conn.begin().unwrap();
    conn.query("CALL db.ontology.apply()").unwrap();
    conn.commit().unwrap();
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    assert_eq!(result.rows[0][0].as_string().unwrap(), graph_type.as_str());
}

/// (5b) Main-valid documents the graph-type body cannot express are stored
/// and readable, but `apply` fails loudly naming the construct — never a
/// silent drop (ADR-0364 §3/§5.4).
#[test]
fn apply_rejects_main_valid_but_unconvertible_documents() {
    common::preview_ontology();
    let header = |id: &str| {
        serde_json::json!({
            "schema": SCHEMA_URI,
            "id": id,
            "version": "1",
        })
    };
    let mut property_type = header("pt");
    property_type["nodes"] = serde_json::json!({
        "N": {
            "label": "N",
            "properties": {"p": {"type": "STRING"}},
            "constraints": {
                "c": {"type": "PROPERTY_TYPE", "properties": ["p"]},
            },
        },
    });
    let mut label_only = header("lo");
    label_only["nodes"] = serde_json::json!({"N": {"label": "N"}});
    label_only["relationships"] = serde_json::json!({
        "R": {"type": "R", "from": {"label": "N"}, "to": {"node": "N"}},
    });
    let mut unsupported_scalar = header("f32");
    unsupported_scalar["nodes"] =
        serde_json::json!({"N": {"label": "N", "properties": {"p": {"type": "FLOAT32"}}}});
    let property_type = property_type.to_string();
    let label_only = label_only.to_string();
    let unsupported_scalar = unsupported_scalar.to_string();
    for (doc, needle) in [
        (property_type, "PROPERTY_TYPE"),
        (label_only, "no node"),
        (unsupported_scalar, "FLOAT32"),
    ] {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        // `set` accepts every one of these: they are main-valid.
        set_doc(&mut conn, &doc).unwrap();
        let err = err_text(conn.query("CALL db.ontology.apply()"));
        assert!(err.contains(needle), "{doc}: {err}");
        // The failed apply changed nothing.
        let result = conn
            .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
            .unwrap();
        assert_eq!(result.rows[0][0], Value::String("{}".into()), "{doc}");
    }
}

/// (6) `SHOW PROCEDURES` lists all four with their CIP-287 signatures and
/// modes (ADR-0121 §7).
#[test]
fn show_procedures_lists_the_ontology_procedures() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query(
            "SHOW PROCEDURES YIELD name, signature, mode \
             WHERE name STARTS WITH 'db.ontology.' \
             RETURN name, signature, mode ORDER BY name",
        )
        .unwrap();
    let rows: Vec<(&str, &str, &str)> = result
        .rows
        .iter()
        .map(|row| {
            (
                row[0].as_string().unwrap(),
                row[1].as_string().unwrap(),
                row[2].as_string().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("db.ontology.apply", "db.ontology.apply()", "WRITE"),
            ("db.ontology.drop", "db.ontology.drop()", "WRITE"),
            (
                "db.ontology.get",
                "db.ontology.get(format = \"yaml\" :: STRING) :: (ontology :: STRING)",
                "READ"
            ),
            (
                "db.ontology.set",
                "db.ontology.set(ontology :: STRING)",
                "WRITE"
            ),
        ]
    );
}

/// (7) Namespace gating (ADR-0227 §1, ADR-0229 §1): routed to
/// `system_ontologies` every procedure — `get` included — fails the
/// namespace's not-queryable gate with the 42N17 text, BEFORE the
/// neo4j-only text of ADR-0227 §1, which now fires only on `system`.
#[test]
fn namespace_gating() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    set_doc(&mut db.connect().unwrap(), DOC_JSON).unwrap();

    let mut ontologies = db.connect_namespace("system_ontologies").unwrap();
    for cypher in [
        "CALL db.ontology.set('x')",
        "CALL db.ontology.drop()",
        "CALL db.ontology.apply()",
        "CALL db.ontology.get() YIELD ontology RETURN ontology",
    ] {
        let err = err_text(ontologies.query(cypher));
        assert!(
            err.ends_with(
                "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
            ),
            "{cypher}: {err}"
        );
    }
    // Administration commands still run from there (ADR-0116 §2).
    ontologies.query("SHOW DATABASES").unwrap();

    let mut system = db.connect_namespace("system").unwrap();
    // The existing system gate (42N17) still rejects data clauses.
    let err = err_text(system.query("MATCH (n) RETURN n"));
    assert!(
        err.contains("The following unsupported clauses were used: MATCH."),
        "{err}"
    );
    // The ontology procedures (standalone calls pass the 42N17 shape gate)
    // then fail at the target-namespace check.
    for cypher in [
        "CALL db.ontology.get() YIELD ontology RETURN ontology",
        "CALL db.ontology.drop()",
    ] {
        let err = err_text(system.query(cypher));
        assert!(
            err.contains("Ontologies can only be stored for the user database 'neo4j'."),
            "{cypher}: {err}"
        );
    }

    // The same gate fires through USE routing on a neo4j session — before
    // the ontology target text there too.
    let mut conn = db.connect().unwrap();
    let err = err_text(conn.query("USE system_ontologies CALL db.ontology.drop()"));
    assert!(
        err.ends_with(
            "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
        ),
        "{err}"
    );
    let err = err_text(
        conn.query("USE system_ontologies CALL db.ontology.get() YIELD ontology RETURN ontology"),
    );
    assert!(
        err.ends_with(
            "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
        ),
        "{err}"
    );
}

/// (8) ADR-0229 §1/§3: after `set` the `system_ontologies` namespace stays
/// not queryable — no graph is derived from the stored document — while the
/// procedures against `neo4j` keep working on it.
#[test]
fn system_ontologies_is_not_queryable_after_set() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    set_doc(&mut conn, DOC_JSON).unwrap();

    // The stored ontology is reachable only through the procedures.
    for cypher in [
        "USE system_ontologies MATCH (n:Ontology) RETURN n.ontology AS db, n.id AS id",
        "USE system_ontologies MATCH (n) RETURN count(n) AS c",
        "USE system_ontologies CREATE (:Nope)",
    ] {
        let err = err_text(conn.query(cypher));
        assert!(
            err.ends_with(
                "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
            ),
            "{cypher}: {err}"
        );
    }

    // The procedures against `neo4j` are unaffected: get returns the
    // stored document and apply still works on it.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(DOC_JSON.into())
    );
    conn.query("CALL db.ontology.apply()").unwrap();
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    assert!(
        result.rows[0][0].as_string().unwrap().contains("Person"),
        "{}",
        result.rows[0][0].as_string().unwrap()
    );
}
