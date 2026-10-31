//! Engine-level `LOAD CSV` integration tests for the CALL-clause
//! executor (`super::exec_load_csv`), kept in the exec layer beside the
//! code they drive.

use std::collections::BTreeMap;
use std::fs;

use cypherlite_core::{Error, Value};
use cypherlite_ir::Expr;

use super::super::test_support::{exec_cypher_on, try_exec_cypher_on};
use super::exec_load_csv;
use crate::csv::{local_file_url, test_root, CsvPolicy};
use crate::ctx::ExecCtx;
use crate::eval::Row;
use crate::{ExecGraph, Graph};

#[test]
fn load_csv_rejects_multi_char_field_terminator() {
    let graph = Graph::default();
    let view = ExecGraph::read(&graph);
    let params = BTreeMap::new();
    let url = Expr::String("rows.csv".into());
    let err = exec_load_csv(
        ExecCtx::new(&view, &params, None),
        vec![Row::new()],
        false,
        &url,
        "row",
        Some("||"),
    )
    .unwrap_err();
    assert_eq!(
        err,
        Error::ty("LOAD CSV FIELDTERMINATOR must be exactly one character")
    );
}

fn csv_rooted_graph(root: &std::path::Path) -> Graph {
    let mut graph = Graph::default();
    graph.configure_load_csv(CsvPolicy::new(Some(root.to_path_buf()), None).unwrap());
    graph
}

fn csv_row(value: &str) -> Vec<Value> {
    vec![Value::List(vec![Value::String(value.into())].into())]
}

#[test]
fn one_physical_read_per_statement() {
    let dir = test_root();
    let root = dir.path();
    fs::write(root.join("rows.csv"), "a\nb\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("rows.csv", root.join("alias.csv")).unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let result = exec_cypher_on(
        &view,
        "UNWIND [1,2,3] AS n LOAD CSV FROM 'rows.csv' AS row RETURN n, row",
    );
    assert_eq!(result.rows.len(), 6);
    assert_eq!(
        result.rows[0],
        vec![
            Value::Int(1),
            Value::List(vec![Value::String("a".into())].into())
        ]
    );
    assert_eq!(view.csv.read_count(), 1);
    #[cfg(unix)]
    assert_eq!(view.csv.load("alias.csv").unwrap().as_ref(), "a\nb\n");
    assert_eq!(view.csv.read_count(), 1);
    assert_eq!(
        ExecGraph::read(&graph)
            .csv
            .load("rows.csv")
            .unwrap()
            .as_ref(),
        "a\nb\n"
    );
    assert_eq!(view.csv.read_count(), 1);
    // The CIP-202 fixture resolves from canned data, never a physical
    // read, even with a local root configured.
    assert_eq!(
        view.csv
            .load("https://data.neo4j.com/bands/artists.csv")
            .unwrap()
            .as_ref(),
        "1,ABBA,1992\n2,Roxette,1986\n3,Europe,1979\n"
    );
    assert_eq!(view.csv.read_count(), 1);
}

#[test]
fn load_csv_honors_rfc4180_quoting_across_lines() {
    let dir = test_root();
    let root = dir.path();
    fs::write(
        root.join("rows.csv"),
        "id,title,owner\n1,\"Neo4j, graph\nand \"\"Cypher\"\"\",42\n2,plain,7\n",
    )
    .unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let result = exec_cypher_on(
        &view,
        "LOAD CSV WITH HEADERS FROM 'rows.csv' AS r RETURN r.id, r.title, r.owner",
    );
    let row = |id: &str, title: &str, owner: &str| {
        vec![id, title, owner]
            .into_iter()
            .map(|s| Value::String(s.into()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        result.rows,
        vec![
            row("1", "Neo4j, graph\nand \"Cypher\"", "42"),
            row("2", "plain", "7"),
        ]
    );
}

/// Neo4j-parity ragged-row rule: under WITH HEADERS a short record maps
/// the missing names to null (an absent field is distinguishable from an
/// empty one); extra cells past the header width are dropped; list rows
/// keep their natural width.
#[test]
fn load_csv_with_headers_maps_missing_cells_to_null() {
    let dir = test_root();
    let root = dir.path();
    fs::write(root.join("ragged.csv"), "a,b,c\n1,2\n3,4,5,6\n9,\n").unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let result = exec_cypher_on(
        &view,
        "LOAD CSV WITH HEADERS FROM 'ragged.csv' AS r RETURN r.a, r.b, r.c",
    );
    let s = |v: &str| Value::String(v.into());
    assert_eq!(
        result.rows,
        vec![
            vec![s("1"), s("2"), Value::Null],
            vec![s("3"), s("4"), s("5")],
            vec![s("9"), s(""), Value::Null],
        ]
    );
    let lists = exec_cypher_on(&view, "LOAD CSV FROM 'ragged.csv' AS row RETURN row");
    assert_eq!(
        lists.rows,
        vec![
            vec![Value::List(vec![s("a"), s("b"), s("c")].into())],
            vec![Value::List(vec![s("1"), s("2")].into())],
            vec![Value::List(vec![s("3"), s("4"), s("5"), s("6")].into())],
            vec![Value::List(vec![s("9"), s("")].into())],
        ]
    );
}

#[test]
fn header_record_is_extracted_before_the_blank_line_policy() {
    let dir = test_root();
    let root = dir.path();
    // A blank line after the header yields no row; the header is the
    // file's first record.
    fs::write(root.join("rows.csv"), "id,name\n\n1,abba\n").unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let result = exec_cypher_on(
        &view,
        "LOAD CSV WITH HEADERS FROM 'rows.csv' AS r RETURN r.id, r.name",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("1".into()),
            Value::String("abba".into()),
        ]]
    );
    // A leading blank line is the header record (one empty name): the
    // blank-line policy never runs ahead of header extraction, so map
    // rows cannot silently degrade to list rows.
    fs::write(root.join("leading.csv"), "\n1,2\n").unwrap();
    let result = exec_cypher_on(
        &view,
        "LOAD CSV WITH HEADERS FROM 'leading.csv' AS r RETURN keys(r), r['']",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::List(vec![Value::String("".into())].into()),
            Value::String("1".into()),
        ]]
    );
}

#[test]
fn reused_view_reads_edited_source_on_each_execute() {
    let dir = test_root();
    let root = dir.path();
    let path = root.join("rows.csv");
    fs::write(&path, "first\n").unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let query = "UNWIND [1,2,3] AS n LOAD CSV FROM 'rows.csv' AS row RETURN row";

    assert_eq!(exec_cypher_on(&view, query).rows, vec![csv_row("first"); 3]);
    assert_eq!(view.csv.read_count(), 1);
    fs::write(&path, "second\n").unwrap();
    assert_eq!(
        exec_cypher_on(&view, query).rows,
        vec![csv_row("second"); 3]
    );
    assert_eq!(view.csv.read_count(), 2);
}

#[test]
fn reused_view_clears_canonical_memo_for_distinct_source_spellings() {
    let dir = test_root();
    let root = dir.path();
    let path = root.join("rows.csv");
    fs::write(&path, "first\n").unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);

    let relative = "LOAD CSV FROM 'rows.csv' AS row RETURN row";
    assert_eq!(exec_cypher_on(&view, relative).rows, vec![csv_row("first")]);
    fs::write(&path, "second\n").unwrap();
    let absolute = format!(
        "LOAD CSV FROM '{}' AS row RETURN row",
        local_file_url(&path)
    );
    assert_eq!(
        exec_cypher_on(&view, &absolute).rows,
        vec![csv_row("second")]
    );
    assert_eq!(view.csv.read_count(), 2);
}

#[test]
fn reused_view_reports_removed_source_on_next_execute() {
    let dir = test_root();
    let root = dir.path();
    let path = root.join("rows.csv");
    fs::write(&path, "first\n").unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let query = "LOAD CSV FROM 'rows.csv' AS row RETURN row";

    assert_eq!(exec_cypher_on(&view, query).rows, vec![csv_row("first")]);
    fs::remove_file(path).unwrap();
    assert_eq!(
        try_exec_cypher_on(&view, query).err().unwrap(),
        Error::execution("LOAD CSV file unavailable")
    );
    assert_eq!(view.csv.read_count(), 1);
}

#[cfg(unix)]
#[test]
fn reused_view_observes_in_root_symlink_retarget_next_execute() {
    use std::os::unix::fs::symlink;

    let dir = test_root();
    let root = dir.path();
    fs::write(root.join("first.csv"), "first\n").unwrap();
    fs::write(root.join("second.csv"), "second\n").unwrap();
    let alias = root.join("alias.csv");
    symlink("first.csv", &alias).unwrap();
    let graph = csv_rooted_graph(root);
    let view = ExecGraph::read(&graph);
    let first =
        "UNWIND ['alias.csv','first.csv','alias.csv'] AS src LOAD CSV FROM src AS row RETURN row";
    assert_eq!(exec_cypher_on(&view, first).rows, vec![csv_row("first"); 3]);
    assert_eq!(view.csv.read_count(), 1);

    fs::remove_file(&alias).unwrap();
    symlink("second.csv", &alias).unwrap();
    let second = "UNWIND ['alias.csv','second.csv'] AS src LOAD CSV FROM src AS row RETURN row";
    assert_eq!(
        exec_cypher_on(&view, second).rows,
        vec![csv_row("second"); 2]
    );
    assert_eq!(view.csv.read_count(), 2);
}

#[test]
fn transaction_error_message_keeps_the_pinned_uniqueness_wrapper() {
    // ADR-0172 pins the REPORT STATUS text byte-identical: the typed-detail
    // hand-over must not intercept the uniqueness violation's wrapper.
    let error = Error::uniqueness_violation("p_id");
    assert_eq!(
        super::transaction_error_message(&error),
        "A value already exists and violates a uniqueness constraint: \
         semantic error: ConstraintValidationFailed: `p_id` uniqueness"
    );
    // The other typed detail variants hand over the legacy bare inner text.
    let error = Error::type_error("schema name must be STRING");
    assert_eq!(
        super::transaction_error_message(&error),
        "TypeError: schema name must be STRING"
    );
    let error = Error::ty("anything");
    assert_eq!(super::transaction_error_message(&error), "anything");
}

/// LABS-198: shared-execution fast path for import-invariant `CALL { }`
/// bodies — the detector's admit/refuse line plus row-equivalence of the
/// shared merge against per-row semantics.
mod shared_subquery {
    use cypherlite_ir::{Clause, Query};

    use super::super::shared::{import_invariant_body, shared_execution_plan};
    use super::*;

    fn call_body(cypher: &str) -> (Query, cypherlite_ir::CallScope) {
        let query = cypherlite_front::parse_and_lower(cypher).unwrap();
        for part in &query.parts {
            for clause in &part.clauses {
                if let Clause::CallSubquery { query, scope, .. } = clause {
                    return ((**query).clone(), scope.clone());
                }
            }
        }
        panic!("test query must contain a CALL subquery: {cypher}");
    }

    fn invariant(cypher: &str) -> bool {
        let (body, scope) = call_body(cypher);
        let rows = vec![crate::eval::Row::from([
            ("i".to_string(), Value::Int(1)),
            ("p".to_string(), Value::Int(1)),
        ])];
        let plan = shared_execution_plan(&scope, &rows)
            .expect("the fixture's imports are all in the outer row");
        let Some(plan) = plan else {
            panic!("fixture must admit the fast path: {cypher}");
        };
        import_invariant_body(&body, &plan.check)
    }

    #[test]
    fn admits_chained_aggregation_with_pass_through_import() {
        // The LABS-198 shape: the import threads the chain as a bare
        // pass-through; nothing computes from it.
        assert!(invariant(
            "UNWIND range(1,5) AS i \
             CALL { WITH i MATCH (n) WITH i, count(*) AS c WITH sum(c) AS s RETURN s } \
             RETURN s"
        ));
        // Explicit scope form, body ignoring the import entirely.
        assert!(invariant(
            "UNWIND range(1,5) AS i CALL (i) { UNWIND [1,2] AS j RETURN j } RETURN j"
        ));
        // A constant positional window shares fine (the import-invariant
        // row stream is identical per outer row).
        assert!(invariant(
            "UNWIND range(1,5) AS i CALL { WITH i UNWIND [1,2,3] AS j RETURN j LIMIT 2 } RETURN j"
        ));
        // A fresh relationship binding over an import-free node pattern
        // stays shareable.
        assert!(invariant(
            "UNWIND range(1,5) AS i CALL (i) { MATCH (a)-[r]->(b) RETURN count(r) AS c } RETURN c"
        ));
    }

    #[test]
    fn refuses_bodies_computing_from_the_import() {
        // Correlated MATCH property map.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i MATCH (n {v: i}) RETURN count(*) AS c } RETURN c"
        ));
        // Correlated WHERE.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i MATCH (n) WHERE n.v = i RETURN count(*) AS c } \
             RETURN c"
        ));
        // Correlated pattern variable (CIP-71 scenario 17 shape): a
        // pattern binding that names the import is a join on it, not a
        // fresh binding.
        assert!(!invariant(
            "MATCH (p) CALL (p) { MATCH (p)-[:FRIEND]->(f) RETURN count(f) AS d } RETURN d"
        ));
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { MATCH (n)-[i]->(m) RETURN count(*) AS c } RETURN c"
        ));
        // Import inside an aggregate argument.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i MATCH (n) RETURN sum(i) AS s } RETURN s"
        ));
        // Renamed pass-through would need per-row value patching.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i AS j RETURN j } RETURN j"
        ));
        // Nondeterministic functions sample once under sharing — at any
        // spelling: the IR keeps the user's case and the temporal suffixes
        // verbatim while dispatch lowercases and strips them.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i RETURN rand() AS r } RETURN r"
        ));
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i RETURN UUID() AS u } RETURN u"
        ));
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i RETURN datetime.realtime() AS t } RETURN t"
        ));
        // A scope-bearing expression (EXISTS here) is refused wholesale:
        // its interior executes per row with its own scope semantics that
        // sharing does not analyze — and the nested clock call is itself
        // seen by the walk's function scan. The body is refused either way.
        assert!(!invariant(
            "UNWIND range(1,5) AS i \
             CALL { WITH i MATCH (n) WHERE EXISTS { MATCH (m) WHERE rand() < 0.5 } RETURN count(*) AS c } \
             RETURN c"
        ));
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i RETURN COLLECT { MATCH (n) RETURN n } AS ns } \
             RETURN ns"
        ));
        // Writes keep per-row side effects.
        assert!(!invariant(
            "UNWIND range(1,5) AS i CALL { WITH i CREATE (n:N) RETURN 1 AS one } RETURN one"
        ));
    }

    fn rows_of(cypher: &str) -> Vec<Vec<Value>> {
        let graph = Graph::default();
        let view = ExecGraph::read(&graph);
        exec_cypher_on(&view, cypher).rows
    }

    #[test]
    fn shared_execution_matches_per_row_results() {
        // Chained aggregation (LABS-198 repro shape, UNWIND-based body).
        assert_eq!(
            rows_of(
                "UNWIND range(1,4) AS i \
                 CALL { WITH i UNWIND range(1,3) AS j WITH i, count(*) AS c WITH sum(c) AS s RETURN s } \
                 RETURN i, s ORDER BY i"
            ),
            vec![
                vec![Value::Int(1), Value::Int(3)],
                vec![Value::Int(2), Value::Int(3)],
                vec![Value::Int(3), Value::Int(3)],
                vec![Value::Int(4), Value::Int(3)],
            ]
        );
        // Same-name pass-through keeps the outer row's value, and the
        // outer row order (including duplicates) is preserved.
        assert_eq!(
            rows_of("UNWIND [2,1,2] AS i CALL { WITH i UNWIND [10,20] AS j RETURN j } RETURN i, j"),
            vec![
                vec![Value::Int(2), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(1), Value::Int(20)],
                vec![Value::Int(2), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
        // Empty body result drops the outer row under CALL and null-pads
        // under OPTIONAL CALL.
        assert_eq!(
            rows_of(
                "UNWIND [1,2] AS i OPTIONAL CALL { WITH i WHERE false RETURN 1 AS x } RETURN i, x"
            ),
            vec![
                vec![Value::Int(1), Value::Null],
                vec![Value::Int(2), Value::Null],
            ]
        );
        assert_eq!(
            rows_of("UNWIND [1,2] AS i CALL { WITH i WHERE false RETURN 1 AS x } RETURN i"),
            Vec::<Vec<Value>>::new()
        );
        // Renamed pass-through stays on the per-row path and still tracks
        // the outer value.
        assert_eq!(
            rows_of("UNWIND [1,2] AS i CALL { WITH i AS j RETURN j } RETURN i, j ORDER BY i"),
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(2), Value::Int(2)]
            ]
        );
        // Rule 7 reinjection: an import referenced after a WITH dropped it
        // still resolves to the outer binding (this shape computes from the
        // import, so it runs per-row — the pin is the semantics, not the
        // path taken).
        assert_eq!(
            rows_of(
                "UNWIND [1,2] AS i CALL (i) { UNWIND [10,20] AS j WITH j RETURN i + j AS k } \
                 RETURN i, k ORDER BY i, k"
            ),
            vec![
                vec![Value::Int(1), Value::Int(11)],
                vec![Value::Int(1), Value::Int(21)],
                vec![Value::Int(2), Value::Int(12)],
                vec![Value::Int(2), Value::Int(22)],
            ]
        );
        // Per-row entropy: uuid() stays per-row, drawing a fresh value per
        // outer row.
        let drawn = rows_of("UNWIND range(1,8) AS i CALL { WITH i RETURN uuid() AS u } RETURN u");
        let mut as_text: Vec<String> = drawn.iter().map(|r| format!("{:?}", r[0])).collect();
        as_text.sort();
        as_text.dedup();
        assert_eq!(as_text.len(), drawn.len());
        // Correlated bodies keep per-row semantics.
        // Per-row order is outer-row order; no sort needed.
        let correlated = rows_of(
            "UNWIND [1,2,3] AS i CALL { WITH i UNWIND range(1, i) AS j RETURN count(*) AS c } \
             RETURN i, c",
        );
        assert_eq!(
            correlated,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Int(3), Value::Int(3)],
            ]
        );
    }
}

#[test]
fn transaction_error_message_keeps_the_legacy_text_of_adr_0307_variants() {
    // ADR-0307's typed variants report exactly what their stringly
    // predecessors did (CIP-198 pins `'/ by zero'` in REPORT STATUS).
    let cases = [
        (
            Error::arithmetic(cypherlite_core::ArithmeticFailure::DivisionByZero),
            Error::ty("/ by zero"),
        ),
        (
            Error::arithmetic(cypherlite_core::ArithmeticFailure::ModuloByZero),
            Error::ty("% by zero"),
        ),
        (
            Error::arithmetic(cypherlite_core::ArithmeticFailure::RoundingNecessary),
            Error::execution("ArithmeticError: rounding necessary"),
        ),
        (
            Error::database_not_found("nosuchdb"),
            Error::semantic("DatabaseNotFound: Graph not found: nosuchdb"),
        ),
        (Error::cancelled(), Error::execution("query cancelled")),
    ];
    for (typed, legacy) in cases {
        assert_eq!(
            super::transaction_error_message(&typed),
            super::transaction_error_message(&legacy),
            "{typed:?}"
        );
    }
    assert_eq!(
        super::transaction_error_message(&Error::arithmetic(
            cypherlite_core::ArithmeticFailure::DivisionByZero
        )),
        "/ by zero"
    );
}
