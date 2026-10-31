mod common;

use cypherlite::Value;

use common::connection;

#[test]
fn count_collect_and_exists_subqueries_are_scalar_expressions() {
    let mut connection = connection();
    connection
        .query("CREATE (:N {v: 1})-[:R]->(:N {v: 2})")
        .unwrap();

    let result = connection
        .query(
            "MATCH (n:N {v: 1}) RETURN \
             COUNT { MATCH (n)-[:R]->(m) } AS count, \
             COLLECT { MATCH (n)-[:R]->(m) RETURN m.v } AS values, \
             EXISTS { MATCH (n)-[:R]->() } AS exists",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int(1),
            Value::List(vec![Value::Int(2)].into()),
            Value::Bool(true),
        ]]
    );
}

#[test]
fn scoped_optional_call_and_return_star_compose() {
    let mut connection = connection();
    let result = connection
        .query(
            "WITH 1 AS a \
             CALL (a) { WITH a AS x RETURN * } \
             OPTIONAL CALL (a) { WITH a WHERE false RETURN a + 1 AS missing } \
             RETURN a, x, missing",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int(1), Value::Int(1), Value::Null]]
    );

    connection
        .query("CREATE (:Person {name: 'Alice'})")
        .unwrap();
    let result = connection
        .query("MATCH (n:Person) CALL (n) { RETURN n.name AS nm } RETURN nm")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("Alice".into())]]);

    // ADR-0116 §3: `graph.byName` is composite-only; as a `USE` target it
    // fails with CE's 42N72 text, even with the selector variable imported
    // by the scope clause.
    let err = connection
        .query(
            "WITH 'neo4j' AS g \
             CALL (g) { USE graph.byName(g) RETURN 1 AS ignored } \
             RETURN g",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.ends_with(
            "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph graph.byName(g)"
        ),
        "{err}"
    );
}

#[test]
fn next_conditional_let_filter_and_foreach_execute() {
    let mut connection = connection();
    let result = connection
        .query("UNWIND [1, 2] AS x RETURN x NEXT LET y = x + 1 FILTER y > 2 RETURN y")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(3)]]);

    let result = connection
        .query("WHEN true THEN { RETURN 1 AS x } ELSE { RETURN 2 AS x }")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);

    connection
        .query("FOREACH (x IN [1, 2] | CREATE (:Loop {v: x}))")
        .unwrap();
    let result = connection
        .query("MATCH (n:Loop) RETURN collect(n.v) AS values")
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::List(vec![Value::Int(1), Value::Int(2)].into())]]
    );

    let result = connection
        .query(
            "LET a = 1 \
             CALL (a) { RETURN a + 1 AS b NEXT RETURN a + b AS c } \
             RETURN a, c, COLLECT { RETURN a + 1 AS b NEXT RETURN a + b AS c } AS collected",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int(1),
            Value::Int(3),
            Value::List(vec![Value::Int(3)].into()),
        ]]
    );
}

#[test]
fn call_in_transactions_batches_and_reports_status() {
    let mut connection = connection();
    let result = connection
        .query(
            "UNWIND [1, 2, 3] AS x \
             CALL (x) { CREATE (:Batch {v: x}) RETURN x AS y } \
             IN TRANSACTIONS OF 2 ROWS ON ERROR CONTINUE REPORT STATUS AS status \
             RETURN y, status.committed AS committed, status.transactionId AS tx",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 3);
    assert!(result.rows.iter().all(|row| row[1] == Value::Bool(true)));
    assert_eq!(result.rows[0][2], result.rows[1][2]);
    assert_ne!(result.rows[1][2], result.rows[2][2]);
}

#[test]
fn optional_builtin_procedures_preserve_or_pad_rows() {
    let mut connection = connection();
    connection.query("CREATE (:Person)").unwrap();
    let result = connection
        .query("WITH 1 AS x OPTIONAL CALL db.labels() YIELD label RETURN x, label")
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int(1), Value::String("Person".into())]]
    );
    connection
        .query("WITH 1 AS x OPTIONAL CALL db.createLabel('Unused') RETURN x")
        .unwrap();
}

/// LABS-198: an import-invariant `CALL { }` body (imports only ever
/// pass through under their own name) is executed once and shared across
/// outer rows, not re-run per row. These tests pin row-equivalence with
/// per-row semantics plus a generous wall-clock bound for the shared path.
#[test]
fn chained_aggregation_subquery_matches_flat_query() {
    let mut connection = connection();
    connection
        .query("UNWIND range(1,1000) AS i CREATE (:N {v: i % 10})")
        .unwrap();

    let shared = connection
        .query(
            "UNWIND range(1,20) AS i \
             CALL { WITH i MATCH (n:N) WITH i, count(*) AS c WITH sum(c) AS s RETURN s } \
             RETURN i, s ORDER BY i",
        )
        .unwrap();
    assert_eq!(shared.rows.len(), 20);
    assert!(shared.rows.iter().all(|row| row[1] == Value::Int(1000)));

    // Correlated bodies keep per-row semantics.
    let correlated_rows = connection
        .query(
            "UNWIND range(0,9) AS v \
             CALL { WITH v MATCH (n:N {v: v}) RETURN count(*) AS c } \
             RETURN v, c ORDER BY v",
        )
        .unwrap();
    assert_eq!(correlated_rows.rows.len(), 10);
    assert!(correlated_rows
        .rows
        .iter()
        .all(|row| row[1] == Value::Int(100)));

    // The shared path must not take noticeably longer than one body
    // execution; per-row execution of this shape costs outer_rows x a
    // full label scan (several seconds even on fast hardware, LABS-198's
    // 18x). The bound is deliberately two orders of magnitude above the
    // shared path's measured time to stay stable on loaded CI machines.
    connection
        .query("UNWIND range(1,5000) AS i CREATE (:M {v: i % 10})")
        .unwrap();
    let start = std::time::Instant::now();
    let result = connection
        .query(
            "UNWIND range(1,2000) AS i \
             CALL { WITH i MATCH (m:M) WITH i, count(*) AS c WITH sum(c) AS s RETURN s } \
             RETURN sum(s) AS total",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(10_000_000)]]);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "import-invariant CALL subquery took {:?} — per-row re-execution regression",
        start.elapsed()
    );
}

/// LABS-198 follow-up guard: a pattern variable naming the import is a
/// correlation (CIP-71 scenario 17 shape) and must keep per-row results.
#[test]
fn pattern_variable_correlation_stays_per_row() {
    let mut connection = connection();
    connection
        .query(
            "CREATE (:Person {name: 'Ada'})-[:FRIEND]->(:Person {name: 'Bob'})-[:FRIEND]->(:Person {name: 'Carl'})",
        )
        .unwrap();
    let result = connection
        .query(
            "MATCH (p:Person) \
             CALL (p) { MATCH (p)-[:FRIEND]->(f) RETURN count(f) AS degree } \
             RETURN p.name AS name, degree ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![Value::String("Ada".into()), Value::Int(1)],
            vec![Value::String("Bob".into()), Value::Int(1)],
            vec![Value::String("Carl".into()), Value::Int(0)],
        ]
    );
}

/// The `UNWIND` source strip must keep a list that only a `CALL ( … )`
/// import list reads: the list is a row read even though no expression in
/// the suffix names it, so a stripped column would leave the clause's
/// planner scope without the binding it promises.
#[test]
fn call_import_list_keeps_the_unwind_source() {
    let mut connection = connection();
    let result = connection
        .query("WITH [1] AS xs UNWIND xs AS i CALL (xs) { RETURN 1 AS x } RETURN x")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);

    // The map-key strip (`UNWIND file.symbols AS row`) follows the same
    // rule: importing `file` needs the whole map, stripped key included.
    let result = connection
        .query(
            "WITH {symbols: [1, 2], path: '/a'} AS file \
             UNWIND file.symbols AS row \
             CALL (file) { RETURN file.path AS p } \
             RETURN row, p ORDER BY row",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(1), Value::String("/a".into())],
            vec![Value::Int(2), Value::String("/a".into())],
        ]
    );

    // The naked and `(*)` scopes take the whole row, so they keep it too.
    let result = connection
        .query("WITH [1] AS xs UNWIND xs AS i CALL { RETURN size(xs) AS n } RETURN n")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// An imported path variable seeds the body like any other binding (the
/// path is a row column, not a variable the row-shape passes may drop).
#[test]
fn imported_path_variable_seeds_the_body() {
    let mut connection = connection();
    connection.query("CREATE (:A)-[:R]->(:B)").unwrap();
    let result = connection
        .query("MATCH p = () CALL (p) { RETURN 1 AS x } RETURN x")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)], vec![Value::Int(1)]]);
}

/// An empty `OPTIONAL CALL … RETURN *` body null-pads the names its star
/// exports, so a following `CALL ( … )` import list still finds them.
#[test]
fn optional_call_return_star_null_pads_its_scope() {
    let mut connection = connection();
    let result = connection
        .query(
            "OPTIONAL CALL { MATCH (m:Missing) RETURN * } \
             CALL (m) { RETURN 1 AS x } \
             RETURN x",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// An `OPTIONAL CALL (*) { … }` empty body pads only the genuinely new
/// columns: `(*)` imports every outer name (the merge path's excluded set),
/// so the outer values survive the null-pad and only the body's own export
/// (`extra`) is null.
#[test]
fn optional_wildcard_call_null_pad_keeps_outer_values() {
    let mut connection = connection();
    let result = connection
        .query(
            "UNWIND [1] AS m \
             OPTIONAL CALL (*) { MATCH (m2:Missing) RETURN *, 42 AS extra } \
             RETURN m, extra",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1), Value::Null]]);
}

/// A concluding `CALL { … RETURN *, <expr> AS extra }` projects the star's
/// names *and* the explicit item: the export set is the RETURN's whole
/// column list, not the star scope alone (which dropped `extra`).
#[test]
fn concluding_call_return_star_keeps_its_explicit_items() {
    let mut connection = connection();
    let result = connection
        .query("UNWIND [1] AS m CALL { WITH m AS a RETURN *, a + 1 AS extra }")
        .unwrap();
    assert_eq!(
        result.header.columns,
        vec!["a".to_string(), "extra".to_string()]
    );
    assert_eq!(result.rows, vec![vec![Value::Int(1), Value::Int(2)]]);
}

/// The `OPTIONAL CALL` null-pad takes the same export set: a body that
/// produced no rows still binds `extra`, so a following `CALL (extra) { … }`
/// import list finds the name its planner scope promises instead of failing
/// the import invariant.
#[test]
fn optional_call_return_star_null_pads_its_explicit_items() {
    let mut connection = connection();
    let result = connection
        .query(
            "OPTIONAL CALL { MATCH (m:Missing) RETURN *, 1 AS extra } \
             CALL (extra) { RETURN 2 AS y } \
             RETURN y",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
}

/// A `WHERE` filter of a sink-projected `WITH` evaluates its expression
/// subqueries on rows the match sink already projected. The subquery body
/// must start from its own scope, not inherit the sink's parked internal
/// columns (which made the body's projection skip items it never
/// evaluated).
#[test]
fn count_subquery_in_a_sink_projected_where_runs() {
    let mut connection = connection();
    connection
        .query("CREATE (:A)-[:R]->(:B), (:A)-[:R]->(:B)")
        .unwrap();
    let result = connection
        .query(
            "MATCH p = () \
             WITH DISTINCT p WHERE COUNT { WITH p AS a } >= 0 \
             RETURN p",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 4);
}
