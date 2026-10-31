//! Canonical-registry regression tests (audit C17): one table per concern —
//! SHOW columns (front), procedure signatures (executor), known functions
//! (front), aggregate names (ir, re-exported by `front::agg`) — consumed by
//! every former duplicate site, so user-visible behavior cannot drift
//! between validation, binding, execution, and the catalog.

mod common;

use cypherlite::Value;

use common::connection;

#[test]
fn show_indexes_where_state_uses_the_default_columns() {
    let mut c = connection();
    c.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
        .unwrap();
    // `state`/`id`/`populationPercent` are default columns: visible to a
    // bare WHERE and to the projection after the clause.
    let r = c
        .query("SHOW INDEXES WHERE state = 'ONLINE' RETURN name")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_string(), Some("person_name"));
    let r = c
        .query("SHOW INDEXES WHERE id IS NOT NULL RETURN name")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn show_constraints_where_id_uses_the_default_columns() {
    let mut c = connection();
    let r = c
        .query("SHOW CONSTRAINTS WHERE id IS NOT NULL RETURN name")
        .unwrap();
    assert_eq!(r.rows.len(), 0);
}

#[test]
fn show_indexes_where_options_is_yield_only() {
    let mut c = connection();
    let err = c
        .query("SHOW INDEXES WHERE options IS NULL RETURN name")
        .unwrap_err()
        .to_string();
    assert!(err.contains("UndefinedVariable"), "{err}");
    // The same column is reachable with an explicit YIELD.
    c.query("SHOW INDEXES YIELD options RETURN options")
        .unwrap();
}

#[test]
fn vector_query_argument_is_list_of_float_everywhere() {
    let mut c = connection();
    // The SHOW PROCEDURES signature comes from the executor's canonical
    // procedure registry — the same table compile-time validation reads.
    let r = c
        .query(
            "SHOW PROCEDURES YIELD name, signature \
             WHERE name = 'db.index.vector.queryNodes' RETURN signature",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    let signature = r.rows[0][0].as_string().unwrap();
    assert!(
        signature.contains("query :: LIST<FLOAT>"),
        "signature: {signature}"
    );
    // And a non-list static argument is rejected, not silently accepted.
    let err = c
        .query("CALL db.index.vector.queryNodes('idx', 5, 'notalist') YIELD node RETURN node")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("InvalidArgumentType") || err.contains("InvalidValueType"),
        "{err}"
    );
}

#[test]
fn builtin_procedure_defaults_come_from_the_registry() {
    let mut c = connection();
    // `db.awaitIndexes`'s only parameter has a default in the registry, so
    // the call may omit it — validation and dispatch agree on the arity.
    c.query("CALL db.awaitIndexes()").unwrap();
    c.query("CALL db.awaitIndexes(5)").unwrap();
}

#[test]
fn show_functions_matches_the_known_function_registry() {
    let mut c = connection();
    // Advertised by SHOW FUNCTIONS and accepted by the binder (the
    // planner-level test proves the binder side; here the catalog side).
    let r = c
        .query("SHOW FUNCTIONS YIELD name WHERE name = 'linenumber' RETURN name")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    // Aggregates are marked from the same single aggregate set.
    let r = c
        .query("SHOW FUNCTIONS YIELD name, aggregating WHERE aggregating RETURN name")
        .unwrap();
    let names: Vec<&str> = r.rows.iter().filter_map(|row| row[0].as_string()).collect();
    for expected in [
        "count",
        "sum",
        "stdev",
        "stdevp",
        "percentilecont",
        "percentiledisc",
    ] {
        assert!(names.contains(&expected), "SHOW FUNCTIONS lost {expected}");
    }
}

#[test]
fn aggregate_aliases_lower_to_the_canonical_display_name() {
    let mut c = connection();
    // The CIP-128 snake_case spelling executes as the aggregate and keeps
    // the Cypher-canonical column spelling from the single alias table.
    let r = c.query("RETURN percentile_cont([1.0], 0.5)").unwrap();
    assert!(
        r.header.columns[0].starts_with("percentileCont("),
        "column: {}",
        r.header.columns[0]
    );
    let r = c
        .query("UNWIND [1.0, 2.0, 3.0] AS x RETURN stdev_samp(x) AS s")
        .unwrap();
    assert!(matches!(r.rows[0][0], Value::Float(_)));
}

#[test]
fn zero_row_yield_star_keeps_the_full_column_header() {
    // Pins the zero-row `YIELD *` header contract (the deliberate delta
    // the catalog projection introduced): an always-false WHERE yields no
    // rows, but the result still carries the command's full column table
    // as its header — a zero-row SHOW keeps its column header instead of
    // returning zero columns.
    let mut c = connection();
    let r = c.query("SHOW SETTINGS YIELD * WHERE 1 = 0").unwrap();
    assert_eq!(r.rows.len(), 0);
    assert_eq!(r.header.columns, ["name", "value", "isDeprecated"]);
}
