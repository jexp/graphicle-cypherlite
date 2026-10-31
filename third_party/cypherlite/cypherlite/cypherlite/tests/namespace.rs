//! Namespace sessions (ADR-0116 §1-§5, §10, ADR-0117 §2, §7, ADR-0229):
//! connecting with a session namespace, switching between transactions,
//! and the per-namespace statement rules for `system` and
//! `system_ontologies` — the former restricted to SHOW/administration
//! commands and standalone procedure calls (42N17), the latter listed but
//! not queryable (ADR-0229 §1/§2: every non-administration statement
//! routed there — reads included — fails with the authored 42N17 text).

use std::collections::BTreeMap;

use cypherlite::{Database, Namespace, Procedure, Value};

mod common;

use common::{err_text, NOT_QUERYABLE_SUFFIX, SYSTEM_CLAUSES_SUFFIX};
use std::sync::Arc;

#[test]
fn connect_defaults_to_neo4j_and_namespace_is_case_insensitive() {
    let db = Database::open(":memory:").unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(conn.namespace(), Namespace::Neo4j);

    let conn = db.connect_namespace("neo4j").unwrap();
    assert_eq!(conn.namespace(), Namespace::Neo4j);
    let conn = db.connect_namespace("SYSTEM").unwrap();
    assert_eq!(conn.namespace(), Namespace::System);
    // ADR-0229 §1: the name stays accepted everywhere a namespace name is
    // parsed — selection works; the queries fail in the gate below.
    let conn = db.connect_namespace("System_Ontologies").unwrap();
    assert_eq!(conn.namespace(), Namespace::SystemOntologies);
}

#[test]
fn connect_namespace_rejects_unknown_names_with_ce_text() {
    let db = Database::open(":memory:").unwrap();
    let err = err_text(db.connect_namespace("movies"));
    assert!(err.ends_with("Graph not found: movies"), "{err}");
}

#[test]
fn set_namespace_switches_between_transactions_only() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.set_namespace("SYSTEM").unwrap();
    assert_eq!(conn.namespace(), Namespace::System);
    conn.set_namespace("neo4j").unwrap();
    assert_eq!(conn.namespace(), Namespace::Neo4j);

    let err = err_text(conn.set_namespace("movies"));
    assert!(err.ends_with("Graph not found: movies"), "{err}");

    // Inside an explicit transaction the namespace is fixed (ADR-0116 §5).
    conn.begin().unwrap();
    let err = err_text(conn.set_namespace("system"));
    assert!(
        err.ends_with("Cannot change the namespace while a transaction is open."),
        "{err}"
    );
    assert_eq!(conn.namespace(), Namespace::Neo4j);
    conn.rollback().unwrap();
    conn.set_namespace("system").unwrap();
    assert_eq!(conn.namespace(), Namespace::System);
}

#[test]
fn system_session_rejects_data_clauses_with_42n17() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system").unwrap();

    let err = err_text(conn.query("MATCH (n) RETURN count(n)"));
    // CE lists only the clause kinds outside its CALL/YIELD/RETURN
    // structure: `MATCH`, not `MATCH, RETURN`.
    assert!(
        err.contains("The following unsupported clauses were used: MATCH."),
        "{err}"
    );
    assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{err}");

    let err = err_text(conn.query("RETURN 1"));
    assert!(
        err.contains("The following unsupported clauses were used: RETURN."),
        "{err}"
    );
    assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{err}");

    let err = err_text(conn.query("CREATE (n)"));
    assert!(
        err.contains("The following unsupported clauses were used: CREATE."),
        "{err}"
    );
    assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{err}");
}

#[test]
fn system_session_allows_show_commands_and_procedure_calls() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system").unwrap();

    for show in [
        "SHOW DATABASES",
        "SHOW PROCEDURES",
        "SHOW FUNCTIONS",
        "SHOW SETTINGS",
    ] {
        let result = conn.query(show).unwrap_or_else(|e| panic!("{show}: {e}"));
        assert_eq!(result.header.namespace, Namespace::System, "{show}");
    }
    let result = conn.query("CALL db.labels()").unwrap();
    assert_eq!(result.header.namespace, Namespace::System);
    // A standalone call composes with YIELD/RETURN (ADR-0116 §4).
    conn.query("CALL db.labels() YIELD label RETURN label")
        .unwrap();
    // Supported administration commands run from any namespace (§2).
    conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 25")
        .unwrap();
    // Deliberate corner: the 42N17 offender collection is per-part
    // top-level, so a read nested in an expression subquery attached to a
    // standalone CALL stays allowed.
    conn.query("CALL db.labels() YIELD label WHERE EXISTS { MATCH (n) } RETURN label")
        .unwrap();
}

#[test]
fn explain_is_gated_by_the_namespace_rules_like_profile() {
    // Regression: EXPLAIN's early-return bypassed the ADR-0116 §4 namespace
    // gates in dispatch, so EXPLAIN of a statement that plain execution and
    // PROFILE reject with 42N17 succeeded. CE raises the gates at
    // semantic-check time, which EXPLAIN performs.
    let db = Database::open(":memory:").unwrap();

    // 42N17 on `system`: EXPLAIN and PROFILE of the same clause fail alike.
    let mut conn = db.connect_namespace("system").unwrap();
    for statement in ["EXPLAIN MATCH (n) RETURN n", "PROFILE MATCH (n) RETURN n"] {
        let err = err_text(conn.query(statement));
        assert!(
            err.contains("The following unsupported clauses were used: MATCH."),
            "{statement}: {err}"
        );
        assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{statement}: {err}");
    }

    // ADR-0229 §2 on `system_ontologies`: reads fail too, EXPLAIN-wrapped
    // included.
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    for statement in [
        "EXPLAIN MATCH (n) RETURN n",
        "PROFILE MATCH (n) RETURN n",
        "EXPLAIN CREATE (n)",
        "PROFILE CREATE (n)",
    ] {
        let err = err_text(conn.query(statement));
        assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{statement}: {err}");
    }

    // Administration commands are exempt (ADR-0116 §2) and still EXPLAIN
    // fine on both namespaces.
    let mut conn = db.connect_namespace("system").unwrap();
    conn.query("EXPLAIN SHOW DATABASES").unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    conn.query("EXPLAIN SHOW DATABASES").unwrap();
}

#[test]
fn explain_matches_execute_error_precedence_and_gates_in_transactions() {
    // EXPLAIN validates procedure calls before gating, exactly like execution
    // and PROFILE: a doubly-invalid statement reports the procedure error on
    // every path.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system").unwrap();
    for statement in [
        "CALL no.such.proc() YIELD x MATCH (n) RETURN x",
        "EXPLAIN CALL no.such.proc() YIELD x MATCH (n) RETURN x",
        "PROFILE CALL no.such.proc() YIELD x MATCH (n) RETURN x",
    ] {
        let err = err_text(conn.query(statement));
        assert!(err.contains("ProcedureNotFound"), "{statement}: {err}");
    }

    // The gates apply inside explicit transactions too — on `system`
    // (42N17) and on `system_ontologies` (ADR-0229 §2, reads included).
    let mut conn = db.connect_namespace("system").unwrap();
    conn.begin().unwrap();
    let err = err_text(conn.query("EXPLAIN MATCH (n) RETURN n"));
    assert!(err.ends_with(SYSTEM_CLAUSES_SUFFIX), "{err}");
    conn.rollback().unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    conn.begin().unwrap();
    // Administration commands still run inside the transaction (§2)…
    conn.query("EXPLAIN SHOW DATABASES").unwrap();
    // …and the failing read fails the transaction, so a rollback follows
    // (ADR-0028's failed-transaction rule).
    let err = err_text(conn.query("EXPLAIN MATCH (n) RETURN n"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
    let err = err_text(conn.query("EXPLAIN SHOW DATABASES"));
    assert!(
        err.contains("transaction failed; rollback required"),
        "{err}"
    );
    conn.rollback().unwrap();
}

#[test]
fn use_system_show_composition_rule_still_holds() {
    // The front-end's `USE system` SHOW-composition rule is unchanged by
    // session namespaces.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let err = err_text(
        conn.query("USE system SHOW INDEXES YIELD name SHOW FUNCTIONS YIELD name RETURN name"),
    );
    assert!(err.contains("NotAllowedOnSystemDatabase"), "{err}");
}

#[test]
fn system_ontologies_session_rejects_every_non_administration_statement() {
    // ADR-0229 §1/§2: the namespace is listed and selectable, but every
    // statement routed to it — reads and writes, data clauses, DDL,
    // procedure and function calls, EXPLAIN/PROFILE-wrapped — fails with
    // the authored 42N17 text. Only the ADR-0116 §2
    // database-administration commands still run from there.
    let db = Database::open(":memory:").unwrap();
    db.register_procedure(Procedure {
        name: "custom.touch".to_string(),
        arg_names: vec![],
        arg_types: vec![],
        yield_names: vec!["ok".to_string()],
        rows: vec![BTreeMap::from([("ok".to_string(), Value::Bool(true))])],
    })
    .unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();

    for statement in [
        // Reads.
        "MATCH (n) RETURN count(n) AS c",
        "RETURN 1 AS one",
        "CALL db.info()",
        "EXPLAIN MATCH (n) RETURN n",
        "PROFILE RETURN 1 AS one",
        // Writes: data clauses, DDL, write procedures — user-registered
        // procedures included (ADR-0116 §4).
        "CREATE ()",
        "CREATE INDEX i FOR (n:N) ON (n.p)",
        "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.id IS UNIQUE",
        "CALL db.createLabel('X')",
        "CALL custom.touch()",
    ] {
        let err = err_text(conn.query(statement));
        assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{statement}: {err}");
    }

    // Administration commands still run (ADR-0116 §2's carve-out).
    conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 25")
        .unwrap();
    let result = conn.query("SHOW DATABASES").unwrap();
    assert_eq!(result.header.namespace, Namespace::SystemOntologies);
}

#[test]
fn system_ontologies_rejects_writes_nested_in_subqueries_and_label_exprs() {
    // The gate is unconditional (ADR-0229 §1), so nothing can nest its way
    // past it: write procedures in WHEN/ELSE branches, CALL subqueries,
    // FOREACH bodies, NEXT stages and dynamic label expressions all fail
    // with the same 42N17 text.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();

    for statement in [
        "WHEN true THEN { CALL db.createLabel('X') }",
        "WHEN false THEN { RETURN 1 AS x } ELSE { CALL db.createLabel('X') RETURN 1 AS x }",
        "CALL { CALL db.createLabel('X') }",
        "FOREACH (x IN [1] | CALL db.createLabel('X'))",
        "RETURN 1 AS x NEXT CALL db.createLabel('X')",
        "CALL { RETURN 1 AS x }",
        "CALL { MATCH (n) RETURN n }",
        "CALL { CREATE (n) }",
        "MATCH (n) REMOVE n:$(COUNT { CALL db.createLabel('X') }) RETURN n",
        "MATCH ((n:$(COUNT { CALL db.createLabel('X') }))-->(x))+ RETURN n",
    ] {
        let err = err_text(conn.query(statement));
        assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{statement}: {err}");
    }

    // The same statements still succeed on a writable namespace.
    let mut conn = db.connect_namespace("neo4j").unwrap();
    for statement in [
        "WHEN true THEN { CALL db.createLabel('X') }",
        "WHEN false THEN { RETURN 1 AS x } ELSE { CALL db.createLabel('Y') RETURN 1 AS x }",
        "CALL { CALL db.createLabel('Z') }",
        "FOREACH (x IN [1] | CALL db.createLabel('W'))",
        "RETURN 1 AS x NEXT CALL db.createLabel('V')",
    ] {
        conn.query(statement)
            .unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
}

#[test]
fn read_only_call_in_expression_subqueries_stays_on_the_read_path_on_neo4j() {
    // Regression: `is_write` must not descend into expression subqueries —
    // a read-only `CALL { … }` inside EXISTS/COUNT/COLLECT is a pure read
    // and must stay on the read path (no write stats). Routed to
    // `system_ontologies` it now fails the 42N17 gate like everything else
    // (ADR-0229), so the read-path pin lives on `neo4j`.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    let err = err_text(conn.query("RETURN EXISTS { CALL { RETURN 1 AS x } } AS x"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");

    let mut conn = db.connect_namespace("neo4j").unwrap();
    let result = conn
        .query("RETURN EXISTS { CALL { RETURN 1 AS x } } AS x")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::Bool(true));
    assert_eq!(result.header.namespace, Namespace::Neo4j);
    assert_eq!(result.stats.nodes_created, 0);
    assert_eq!(result.stats.labels_added, 0);
}

#[test]
fn write_procedure_nested_in_expression_subquery_is_rejected_at_plan_time() {
    // Reverse-hole pin: a write procedure inside a `CALL { … }` nested in
    // an expression subquery never reaches a namespace gate — plan-time
    // validation rejects writes in subquery expressions first
    // (`expr_check` → `clause_is_write` → `query_contains_write`), on every
    // namespace, before the ADR-0229 gate could fire.
    let db = Database::open(":memory:").unwrap();
    for namespace in ["neo4j", "system_ontologies"] {
        let mut conn = db.connect_namespace(namespace).unwrap();
        let err = err_text(conn.query("RETURN EXISTS { CALL { CALL db.createLabel('X') } } AS x"));
        assert!(
            err.contains("writes are not allowed in subquery expressions"),
            "{namespace}: {err}"
        );
    }
}

#[test]
fn catalog_write_nested_in_expression_subquery_is_rejected_at_plan_time() {
    // The same reverse-hole pin for a catalog write: `ALTER CURRENT GRAPH
    // TYPE` persists a change while moving no ADR-0309 counter, and the
    // plan-time composition rule counts it as a write (`expr_check` ->
    // `clause_is_write` -> the one per-clause answer), so no namespace gate
    // ever sees the statement.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("neo4j").unwrap();
    let err = err_text(
        conn.query("RETURN EXISTS { ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph } AS x"),
    );
    assert!(
        err.contains("writes are not allowed in subquery expressions"),
        "{err}"
    );
}

#[test]
fn neo4j_writes_are_invisible_to_system_ontologies_sessions() {
    // ADR-0229 §1: the namespace rejects every read routed to it, so
    // `neo4j`'s data is invisible there by construction — and `system`
    // holds no graph data at all (ADR-0116 §1).
    let db = Database::open(":memory:").unwrap();
    let mut neo4j = db.connect().unwrap();
    neo4j.query("CREATE (:Thing {name: 'data'})").unwrap();

    let mut ontologies = db.connect_namespace("system_ontologies").unwrap();
    let err = err_text(ontologies.query("MATCH (n) RETURN count(n) AS c"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");

    let mut system = db.connect_namespace("system").unwrap();
    system.query("SHOW DATABASES").unwrap();
    let result = neo4j.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(result.rows[0][0], Value::Int(1));
}

#[test]
fn registered_procedures_are_callable_from_any_writable_namespace_session() {
    let db = Database::open(":memory:").unwrap();
    db.register_procedure(Procedure {
        name: "custom.ping".to_string(),
        arg_names: vec![],
        arg_types: vec![],
        yield_names: vec!["ok".to_string()],
        rows: vec![BTreeMap::from([("ok".to_string(), Value::Bool(true))])],
    })
    .unwrap();

    for namespace in ["neo4j", "system"] {
        let mut conn = db.connect_namespace(namespace).unwrap();
        let result = conn
            .query("CALL custom.ping()")
            .unwrap_or_else(|e| panic!("{namespace}: {e}"));
        assert_eq!(result.rows[0][0], Value::Bool(true));
        assert_eq!(result.header.namespace.as_str(), namespace);
    }
}

#[test]
fn system_session_element_id_on_procedure_node_is_a_typed_error_not_a_panic() {
    // Regression: a `system` session allows standalone procedure calls, and
    // a registered procedure can yield a node, so `elementId()` can see an
    // element on a namespace that holds no element ids (ADR-0116 §7). That
    // is a typed execution error, not a worker panic.
    let db = Database::open(":memory:").unwrap();
    db.register_procedure(Procedure {
        name: "custom.node".to_string(),
        arg_names: vec![],
        arg_types: vec![],
        yield_names: vec!["n".to_string()],
        rows: vec![BTreeMap::from([(
            "n".to_string(),
            Value::from_node(cypherlite::Node {
                id: 42,
                labels: Arc::new(vec!["Thing".to_string()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            }),
        )])],
    })
    .unwrap();

    let mut conn = db.connect_namespace("system").unwrap();
    let err = err_text(conn.query("CALL custom.node() YIELD n RETURN elementId(n)"));
    assert!(
        err.contains("elements of this namespace have no element ids"),
        "{err}"
    );
}

#[test]
fn query_result_reports_the_session_namespace() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.header.namespace, Namespace::Neo4j);

    // A `system_ontologies` session reports its namespace on the one
    // statement kind that still runs there (ADR-0116 §2), and the
    // selection itself keeps working (ADR-0229 §1).
    conn.set_namespace("system_ontologies").unwrap();
    let result = conn.query("SHOW DATABASES").unwrap();
    assert_eq!(result.header.namespace, Namespace::SystemOntologies);

    // EXPLAIN and PROFILE results carry the namespace too.
    conn.set_namespace("neo4j").unwrap();
    let result = conn.query("EXPLAIN RETURN 1").unwrap();
    assert_eq!(result.header.namespace, Namespace::Neo4j);
    let result = conn.query("PROFILE RETURN 1 AS one").unwrap();
    assert_eq!(result.header.namespace, Namespace::Neo4j);
}

#[test]
fn db_info_reports_the_session_namespace() {
    // ADR-0116 §7: db.info() id/name is the namespace the statement targets.
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.query("CALL db.info()").unwrap();
    assert_eq!(result.rows[0][0], Value::String("neo4j".into()));
    assert_eq!(result.rows[0][1], Value::String("neo4j".into()));

    // Routed to `system_ontologies`, db.info() is a procedure call, not an
    // administration command, so ADR-0229 §1's gate rejects it.
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    let err = err_text(conn.query("CALL db.info()"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
}

#[test]
fn namespace_survives_explicit_transactions() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("system_ontologies").unwrap();
    conn.begin().unwrap();
    // The transaction keeps its BEGIN-time namespace; the one statement
    // kind that runs there reports it (ADR-0116 §2, §5).
    let result = conn.query("SHOW DATABASES").unwrap();
    assert_eq!(result.header.namespace, Namespace::SystemOntologies);
    conn.commit().unwrap();
    assert_eq!(conn.namespace(), Namespace::SystemOntologies);
    // Reads fail the ADR-0229 gate in and out of transactions alike.
    let err = err_text(conn.query("MATCH (n) RETURN count(n) AS c"));
    assert!(err.ends_with(NOT_QUERYABLE_SUFFIX), "{err}");
}
