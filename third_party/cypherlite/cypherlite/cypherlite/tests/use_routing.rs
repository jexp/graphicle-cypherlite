//! `USE` routing (ADR-0116 §3, §5): a statement targets the namespace named
//! by its `USE` clause, resolved eagerly before execution, else the session
//! namespace (ADR-0117 §2).

use cypherlite::{Database, Namespace, Value};

mod common;

use common::{connection, err_text, NOT_QUERYABLE_SUFFIX, SYSTEM_CLAUSES_SUFFIX};

/// CE 42NA5 text (ADR-0116 §3); CE names the attempted graph on a second
/// line (testdata/ce-errors/neo4j-ce.json `multi_graph_*`).
const MULTIPLE_GRAPHS_PREFIX: &str = "Multiple graphs in the same query not allowed here. This feature is only available on composite databases.\nAttempted to access graph ";
/// CE 42N72 text (ADR-0116 §3); CE names the attempted graph reference on a
/// second line (testdata/ce-errors/neo4j-ce.json `use_graph_byname`).
const COMPOSITE_ONLY_PREFIX: &str = "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph ";
/// CE 42NA7 text (ADR-0116 §3, §7; testdata/ce-errors/neo4j-ce.json
/// `byelementid_unknown`).
fn element_id_no_namespace_suffix(id: &str) -> String {
    format!(
        "No database is corresponding to `graph.byElementId({id})`. Verify that the elementId is correct."
    )
}

#[test]
fn use_routes_reads_and_writes_to_the_target_namespace() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("neo4j").unwrap();

    // ADR-0229 §1: the ontology namespace is listed and routable but not
    // queryable — every statement routed there, reads included, fails
    // with the authored 42N17 text.
    let err = err_text(conn.query("USE system_ontologies MATCH (n) RETURN count(n) AS c"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
    let err = err_text(conn.query("USE system_ontologies CREATE ()"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");

    // `USE neo4j` reads and writes the user data graph.
    conn.query("USE neo4j CREATE (:T)").unwrap();
    let result = conn
        .query("USE neo4j MATCH (n:T) RETURN count(n) AS c")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::Int(1));
    assert_eq!(result.header.namespace, Namespace::Neo4j);

    // `USE system` runs under the system-namespace rules (42N17).
    let err = err_text(conn.query("USE system MATCH (n) RETURN count(n)"));
    assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{err}");
    let result = conn
        .query("USE system SHOW DATABASES YIELD name RETURN name")
        .unwrap();
    assert_eq!(result.header.namespace, Namespace::System);
}

#[test]
fn use_unknown_graph_fails_before_execution() {
    let mut conn = connection();

    let err = err_text(conn.query("USE foo MATCH (n) RETURN count(n)"));
    assert!(err.ends_with("Graph not found: foo"), "{err}");

    // No lazy resolution: a statement that touches no graph data fails the
    // same way.
    let err = err_text(conn.query("USE foo RETURN 1"));
    assert!(err.ends_with("Graph not found: foo"), "{err}");
    let err = err_text(conn.query("EXPLAIN USE foo RETURN 1"));
    assert!(err.ends_with("Graph not found: foo"), "{err}");

    // No side effects: the graph is untouched and the connection usable.
    let result = conn.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(result.rows[0][0], Value::Int(0));
}

#[test]
fn composite_only_graph_functions_are_42n72_as_use_target_and_as_calls() {
    let mut conn = connection();

    for (function, use_form, call_form) in [
        (
            "graph.byName('neo4j')",
            "USE graph.byName('neo4j') RETURN 1",
            "RETURN graph.byName('neo4j') AS g",
        ),
        (
            "graph.names()",
            "USE graph.names() RETURN 1",
            "RETURN graph.names() AS g",
        ),
        (
            "graph.propertiesByName('neo4j')",
            "USE graph.propertiesByName('neo4j') RETURN 1",
            "RETURN graph.propertiesByName('neo4j') AS g",
        ),
    ] {
        let suffix = format!("{COMPOSITE_ONLY_PREFIX}{function}");
        let err = err_text(conn.query(use_form));
        assert!(err.ends_with(&suffix), "{use_form}: {err}");
        let err = err_text(conn.query(call_form));
        assert!(err.ends_with(&suffix), "{call_form}: {err}");
    }

    // All three stay listed by SHOW FUNCTIONS.
    let result = conn.query("SHOW FUNCTIONS YIELD name RETURN name").unwrap();
    let names: Vec<&str> = result
        .rows
        .iter()
        .filter_map(|row| row[0].as_string())
        .collect();
    for function in ["graph.byName", "graph.names", "graph.propertiesByName"] {
        assert!(names.contains(&function), "{function} not in {names:?}");
    }
}

#[test]
fn graph_by_element_id_is_42n72_in_expression_position() {
    let mut conn = connection();

    // In an arbitrary expression position `graph.byElementId` fails with
    // the same CE 42N72 composite-databases text as the other selectors
    // (ADR-0116 §3) — not as an unknown function.
    let function = "graph.byElementId('4:cypherlite:n1')";
    let err = err_text(conn.query(&format!("RETURN {function} AS g")));
    assert!(
        err.ends_with(&format!("{COMPOSITE_ONLY_PREFIX}{function}")),
        "{err}"
    );

    // Its `USE`-target path keeps the element-id resolution: the id
    // resolves to `system_ontologies`, and the statement then fails that
    // namespace's gate (ADR-0229 §5).
    let err = err_text(
        conn.query("USE graph.byElementId('4:cypherlite.system_ontologies:n1') RETURN 1 AS one"),
    );
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
}

#[test]
fn use_graph_by_element_id_resolves_the_id_namespace() {
    let mut conn = connection();

    // ADR-0229 §5: the `system_ontologies` segment still decodes and routes
    // the statement to the namespace, which then rejects it (§1/§2).
    let err = err_text(conn.query(
        "USE graph.byElementId('4:cypherlite.system_ontologies:n1') MATCH (n) RETURN count(n) AS c",
    ));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");

    // The bare `cypherlite` segment is `neo4j` (ADR-0116 §7).
    let result = conn
        .query("USE graph.byElementId('4:cypherlite:n1') RETURN 1 AS one")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::Int(1));
    assert_eq!(result.header.namespace, Namespace::Neo4j);

    // An id naming no namespace fails with CE's 42NA7 text.
    let id = "4:00000000-0000-0000-0000-000000000000:n1";
    let err = err_text(conn.query(&format!("USE graph.byElementId('{id}') RETURN 1")));
    assert!(err.ends_with(&element_id_no_namespace_suffix(id)), "{err}");
}

#[test]
fn distinct_targets_in_one_statement_are_42na5() {
    let mut conn = connection();

    for (cypher, attempted) in [
        // UNION arms.
        (
            "USE neo4j RETURN 1 AS x UNION USE system RETURN 2 AS x",
            "system",
        ),
        // CALL subquery: `USE g` targets `g`, different from the outer
        // `USE neo4j`.
        (
            "USE neo4j WITH 'system' AS g CALL (g) { USE g RETURN 1 AS x } RETURN x",
            "g",
        ),
        // NEXT stages.
        (
            "USE neo4j RETURN 1 AS x NEXT USE system RETURN x AS y",
            "system",
        ),
    ] {
        let err = err_text(conn.query(cypher));
        assert!(
            err.ends_with(&format!("{MULTIPLE_GRAPHS_PREFIX}{attempted}")),
            "{cypher}: {err}"
        );
    }

    // The same target twice is fine.
    let result = conn
        .query("USE neo4j CALL { USE neo4j RETURN 1 AS x } RETURN x")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::Int(1));
    let result = conn
        .query("USE neo4j RETURN 1 AS x UNION USE NEO4J RETURN 1 AS x")
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    // One explicit target repeated across UNION arms is one target, even
    // outside `neo4j`: the statement below is NOT 42NA5 — it fails the
    // routed namespace's own gate instead (ADR-0229 §1).
    let err = err_text(conn.query(
        "USE system_ontologies MATCH (n) RETURN count(n) AS x \
         UNION USE system_ontologies MATCH (n) RETURN count(n) AS x",
    ));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
}

#[test]
fn use_routing_inside_an_explicit_transaction() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("neo4j").unwrap();

    conn.begin().unwrap();
    // A USE-routed read of `system_ontologies` fails the ADR-0229 §1 gate
    // inside the transaction too, and fails the transaction (ADR-0028).
    let err = err_text(conn.query("USE system_ontologies MATCH (n) RETURN count(n) AS c"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
    let err = err_text(conn.query("RETURN 1 AS x"));
    assert!(
        err.contains("transaction failed; rollback required"),
        "{err}"
    );
    conn.rollback().unwrap();
    // Writes go to `neo4j`, the only writable namespace.
    conn.begin().unwrap();
    conn.query("USE neo4j CREATE (:Txn)").unwrap();
    conn.commit().unwrap();

    let result = conn.query("MATCH (n:Txn) RETURN count(n) AS c").unwrap();
    assert_eq!(result.rows[0][0], Value::Int(1));
    let err = err_text(conn.query("USE system_ontologies MATCH (n) RETURN count(n) AS c"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
}

#[test]
fn use_system_show_composition_rule_holds_for_any_casing() {
    let mut conn = connection();
    for spelling in ["system", "SYSTEM"] {
        let err = err_text(conn.query(&format!(
            "USE {spelling} SHOW INDEXES YIELD name SHOW FUNCTIONS YIELD name RETURN name"
        )));
        assert!(
            err.contains("NotAllowedOnSystemDatabase"),
            "{spelling}: {err}"
        );
    }
}

#[test]
fn use_nested_in_dynamic_label_expressions_is_resolved() {
    // Regression: the clause-tree walker skipped the label payloads of
    // SET/REMOVE `n:$(…)` items and the label_expr/type_expr of pattern
    // elements (the surviving repr once lowering clears the compact
    // nodes/rels for quantified patterns), so a `USE` nested in a dynamic
    // label expression escaped resolution and the statement silently ran
    // on the session namespace.
    let mut conn = connection();
    for cypher in [
        "MATCH (n) SET n:$(COUNT { USE foo MATCH (m) }) RETURN n",
        "MATCH (n) REMOVE n:$(COUNT { USE foo MATCH (m) }) RETURN n",
        "MATCH (n)-[:$(COUNT { USE foo MATCH (m) })]->(x) RETURN n",
        "MATCH ((n:$(COUNT { USE foo MATCH (m) }))-->(x))+ RETURN n",
    ] {
        let err = err_text(conn.query(cypher));
        assert!(err.ends_with("Graph not found: foo"), "{cypher}: {err}");
    }
}
