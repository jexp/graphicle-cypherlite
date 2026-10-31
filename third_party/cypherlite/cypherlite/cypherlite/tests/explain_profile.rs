//! `EXPLAIN` / `PROFILE` plan introspection on the merged engine
//! (GitHub issue #14): `EXPLAIN` returns the logical plan tree without
//! executing anything (no columns, no rows — ADR-0359); `PROFILE` executes
//! and returns the data plus the stamped plan tree. No statement class may
//! come back as a silently empty table. The rendered text form is the
//! boundary's (the formatter's `plan_text_lines`) — these tests assert on
//! the tree.

use std::collections::BTreeMap;

mod common;

use cypherlite::{QueryType, Value};

use common::{connection, find_node, find_nodes, plan};

#[test]
fn explain_match_returns_the_plan() {
    // The issue #14 repro: this used to print an empty `_explain` table.
    // ADR-0359: EXPLAIN carries the tree on `result.plan` with no columns
    // and no rows.
    let result = connection()
        .query("EXPLAIN MATCH (n:Person) RETURN n.name")
        .unwrap();
    assert!(
        result.header.columns.is_empty(),
        "EXPLAIN returns no data columns"
    );
    assert!(result.rows.is_empty(), "EXPLAIN returns no data rows");
    let tree = plan(&result);
    assert_eq!(tree.operator, "ProduceResults");
    // ADR-0344 phase 3: the operator names its subject — a single-label
    // node-only MATCH is a scan that names its label.
    let scan = find_node(tree, "NodeByLabelScan").expect("the label scan");
    assert!(scan.details.contains("(n:Person)"), "{tree:?}");
}

#[test]
fn explain_executes_nothing() {
    let mut conn = connection();
    let result = conn.query("EXPLAIN CREATE (p:Person {name: 'A'})").unwrap();
    let tree = plan(&result);
    let create = find_node(tree, "Create").expect("the planned write");
    assert!(create.details.contains("(p:Person)"), "{tree:?}");
    // The write was planned, not executed: the graph is still empty.
    let count = conn.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(count.rows[0][0], Value::Int(0));
}

#[test]
fn explain_with_parameters_plans() {
    let mut conn = connection();
    let params = BTreeMap::from([("name".to_string(), Value::String("Ada".into()))]);
    let result = conn
        .query_with_params(
            "EXPLAIN MATCH (n:Person {name: $name}) RETURN n.name",
            params,
        )
        .unwrap();
    assert!(
        find_node(plan(&result), "NodeByLabelScan").is_some(),
        "the parameterized MATCH plans a label scan"
    );
}

#[test]
fn explain_next_chain_plans_every_stage() {
    let result = connection()
        .query("EXPLAIN RETURN 1 AS x NEXT MATCH (n) RETURN x, n")
        .unwrap();
    let tree = plan(&result);
    // Every stage plans: the root pipeline plus the chained NEXT stage
    // (the `Next` node names it), nothing dropped by the tree.
    assert_eq!(tree.operator, "ProduceResults");
    let next = find_node(tree, "Next").expect("the stage link");
    assert!(next.details.contains("stage 2"), "{tree:?}");
}

#[test]
fn explain_union_plans_every_part() {
    let result = connection()
        .query("EXPLAIN RETURN 1 AS x UNION RETURN 2 AS x")
        .unwrap();
    let tree = plan(&result);
    // The Union node carries both parts' pipelines as children.
    let union = find_node(tree, "Union").expect("the union link");
    assert!(union.details.contains("2 parts"), "{tree:?}");
    let parts = find_nodes(tree, "ProduceResults");
    assert_eq!(parts.len(), 2, "both parts' projections: {tree:?}");
}

/// The build-pass id arithmetic: one pre-order numbering spans every union
/// part and the `NEXT` stage, so an operator id names exactly one node —
/// unique and dense from the root (ADR-0363 decision 4 reads those ids).
#[test]
fn explain_union_parts_and_next_stage_share_one_pre_order_numbering() {
    let result = connection()
        .query(
            "EXPLAIN MATCH (n:Person) WITH n WHERE n.name = 'Ada' RETURN n.name AS x
             UNION RETURN 2 AS x
             NEXT RETURN x",
        )
        .unwrap();
    let tree = plan(&result);
    // A multi-part statement whose first part has three clauses, a second
    // part, and a chained stage: every repeated-node shape is here.
    let union = find_node(tree, "Union").expect("the union link");
    assert_eq!(
        union.children.len(),
        3,
        "two part pipelines plus the NEXT stage: {tree:?}"
    );
    assert_eq!(
        find_nodes(tree, "ProduceResults").len(),
        3,
        "both parts' projections and the NEXT stage's: {tree:?}"
    );
    assert!(
        find_node(tree, "Next").is_some(),
        "the stage link: {tree:?}"
    );

    let mut ids = Vec::new();
    collect_ids(tree, &mut ids);
    assert_eq!(
        ids,
        (0..ids.len()).collect::<Vec<_>>(),
        "pre-order ids are unique and dense: {tree:?}"
    );
}

/// Collect a tree's node ids in pre-order — the order
/// [`cypherlite::PlanNode::assign_ids`] stamps and every consumer reads.
fn collect_ids(node: &cypherlite::PlanNode, ids: &mut Vec<usize>) {
    ids.push(node.id);
    for child in &node.children {
        collect_ids(child, ids);
    }
}

#[test]
fn explain_call_subquery_plans() {
    let result = connection()
        .query("EXPLAIN MATCH (n) CALL (n) { RETURN n AS m } RETURN m")
        .unwrap();
    assert!(
        find_node(plan(&result), "CallSubquery").is_some(),
        "the subquery call plans"
    );
}

#[test]
fn explain_catalog_command_plans() {
    let result = connection().query("EXPLAIN SHOW INDEXES").unwrap();
    let catalog = find_node(plan(&result), "Catalog").expect("the catalog op");
    assert!(catalog.details.contains("SHOW INDEXES"), "{catalog:?}");
}

#[test]
fn explain_of_unplannable_statement_is_a_typed_error() {
    // No silent empty table: a statement that cannot be planned fails.
    let err = connection()
        .query("EXPLAIN MATCH (n)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("query must conclude with RETURN"),
        "wrong error: {err}"
    );
    let err = connection()
        .query("EXPLAIN CALL no.such.procedure()")
        .unwrap_err()
        .to_string();
    assert!(err.contains("ProcedureNotFound"), "wrong error: {err}");
}

#[test]
fn explain_inside_transaction_writes_nothing() {
    let mut conn = connection();
    conn.begin().unwrap();
    let result = conn.query("EXPLAIN CREATE (n:Marker)").unwrap();
    assert!(
        find_node(plan(&result), "Create").is_some(),
        "the planned write carries a Create operator"
    );
    assert!(result.rows.is_empty());
    let count = conn.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(count.rows[0][0], Value::Int(0));
    conn.rollback().unwrap();
}

#[test]
fn profile_returns_data_plus_statistics() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();
    let result = conn
        .query("PROFILE MATCH (n:Person) RETURN n.name")
        .unwrap();
    // ADR-0344/0345 phase 2: the statement's own columns and rows come
    // back unchanged; the profile facts ride the structured plan tree —
    // the deprecated `_profile_*` columns are gone.
    assert_eq!(result.header.columns, ["n.name"]);
    assert_eq!(result.rows.len(), 2, "PROFILE keeps the query's data rows");
    for row in &result.rows {
        assert!(matches!(row[0], Value::String(_)), "data cell: {row:?}");
    }
    let tree = plan(&result);
    assert_eq!(tree.operator, "ProduceResults");
    let scan = &tree.children[0];
    assert_eq!(scan.operator, "NodeByLabelScan");
    assert!(scan.details.contains("(n:Person)"), "{tree:?}");
    // The per-operator metrics: rows produced and elapsed time, > 0 for
    // an operator that did work.
    let metrics = scan.metrics.expect("the scan operator's metrics");
    assert_eq!(metrics.rows, 2, "the scan produced the two Person rows");
    assert!(metrics.time_ns > 0, "{metrics:?}");
}

#[test]
fn profile_write_executes_and_reports_mutations() {
    let mut conn = connection();
    let result = conn.query("PROFILE CREATE (p:Person {name: 'A'})").unwrap();
    // The statement's own result — no data rows, its own columns — with
    // the plan tree carrying the operator metrics; the mutation counts
    // ride `QueryResult::stats` (the engine-facing fact) as always.
    assert!(result.rows.is_empty());
    let tree = plan(&result);
    // A bare `CREATE` with no RETURN: the plan's own shape — the Create
    // operator is the root (the projection is synthetic).
    assert_eq!(tree.operator, "Create");
    let metrics = tree.metrics.expect("the Create operator's metrics");
    assert!(metrics.time_ns > 0, "{metrics:?}");
    assert_eq!(result.stats.nodes_created, 1);
}

#[test]
fn profile_read_reports_no_mutations() {
    let mut conn = connection();
    let result = conn.query("PROFILE RETURN 1 AS n").unwrap();
    // The statement's own rows and columns; the profile facts ride the
    // tree (`_profile_*` is gone, ADR-0345), the mutation counters stay
    // on `stats`.
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int(1));
    assert_eq!(result.header.columns, ["n"]);
    let tree = plan(&result);
    let metrics = tree.metrics.expect("the projection operator's metrics");
    assert_eq!(metrics.rows, 1);
    assert_eq!(result.stats.nodes_created, 0);
}

#[test]
fn explain_reports_the_wrapped_statements_query_type() {
    // ADR-0162: EXPLAIN classifies the wrapped statement from its plan and
    // executes nothing — the graph stays empty and stats stay default.
    let mut conn = connection();
    for (src, expected) in [
        ("EXPLAIN MATCH (n:Person) RETURN n.name", QueryType::Read),
        ("EXPLAIN CREATE (p:Person {name: 'A'})", QueryType::Write),
        // ADR-0332: a write with no returned columns is `w` — a MATCH that
        // only feeds a SET returns no columns (latest Neo4j 2026.09.0).
        ("EXPLAIN MATCH (n) SET n.x = 1", QueryType::Write),
        (
            "EXPLAIN CREATE INDEX idx FOR (n:Person) ON (n.name)",
            QueryType::Schema,
        ),
    ] {
        let result = conn.query(src).unwrap();
        assert_eq!(result.header.query_type, expected, "{src}");
        assert_eq!(
            result.header.query_type.as_str(),
            expected.as_str(),
            "{src}"
        );
    }
    // Nothing executed: no nodes, no index.
    let count = conn.query("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(count.rows[0][0], Value::Int(0));
    let indexes = conn.query("SHOW INDEXES").unwrap();
    assert!(indexes.rows.is_empty(), "EXPLAIN created no index");
}

#[test]
fn profile_reports_the_same_query_type_as_explain() {
    // ADR-0162: PROFILE runs the statement and keeps its plan-derived type.
    let mut conn = connection();
    for (wrapped, expected) in [
        ("MATCH (n:Person) RETURN n.name", QueryType::Read),
        ("CREATE (p:Person {name: 'A'})", QueryType::Write),
        ("MATCH (n) SET n.x = 1 RETURN n", QueryType::ReadWrite),
        (
            "CREATE INDEX pidx FOR (n:Person) ON (n.name)",
            QueryType::Schema,
        ),
    ] {
        let explain = conn.query(&format!("EXPLAIN {wrapped}")).unwrap();
        let profile = conn.query(&format!("PROFILE {wrapped}")).unwrap();
        assert_eq!(explain.header.query_type, expected, "EXPLAIN {wrapped}");
        assert_eq!(profile.header.query_type, expected, "PROFILE {wrapped}");
    }
}

#[test]
fn plain_runs_carry_the_query_type_too() {
    // The field is stamped for every statement, not only EXPLAIN/PROFILE.
    let mut conn = connection();
    assert_eq!(
        conn.query("MATCH (n) RETURN n").unwrap().header.query_type,
        QueryType::Read
    );
    assert_eq!(
        conn.query("CREATE (n:N)").unwrap().header.query_type,
        QueryType::Write
    );
    assert_eq!(
        conn.query("MATCH (n) SET n.x = 1 RETURN n")
            .unwrap()
            .header
            .query_type,
        QueryType::ReadWrite
    );
    assert_eq!(
        conn.query("CREATE INDEX i FOR (n:N) ON (n.p)")
            .unwrap()
            .header
            .query_type,
        QueryType::Schema
    );
    // The wire codes are Neo4j Query API v2's (ADR-0162).
    assert_eq!(QueryType::Read.as_str(), "r");
    assert_eq!(QueryType::Write.as_str(), "w");
    assert_eq!(QueryType::ReadWrite.as_str(), "rw");
    assert_eq!(QueryType::Schema.as_str(), "s");
}

#[test]
fn no_statement_class_comes_back_silently_empty() {
    // The issue's core complaint was a zero-row `_explain` table. Every
    // lowered statement class yields the stamped plan tree (or a typed
    // error, covered above).
    let mut conn = connection();
    for src in [
        "EXPLAIN RETURN 1 AS n",
        "EXPLAIN MATCH (n) RETURN n",
        "EXPLAIN CREATE (n:N)",
        "EXPLAIN MATCH (n) SET n.x = 1",
        "EXPLAIN UNWIND [1, 2] AS i RETURN i",
        "EXPLAIN CALL db.labels()",
        "EXPLAIN SHOW CONSTRAINTS",
        "EXPLAIN RETURN 1 AS x NEXT RETURN x",
        "EXPLAIN FOREACH (i IN [1] | CREATE (n:N))",
        "PROFILE RETURN 1 AS n",
        "PROFILE MATCH (n) RETURN n",
        "PROFILE SHOW INDEXES",
    ] {
        let result = conn.query(src).unwrap();
        // Per statement class, actually falsifiable (ADR-0344/0345):
        // a PROFILE statement always carries the stamped plan tree —
        // and its own columns/rows, empty or not, come from the
        // statement; it never comes back silently tree-less.
        assert!(
            result.plan.is_some(),
            "PROFILE {src} must carry the stamped plan tree"
        );
        assert!(
            !result.header.columns.iter().any(|c| c == "_explain"),
            "{src} must not use the old stub column"
        );
    }
}

#[test]
fn explain_plan_returns_the_tree_without_executing() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Ada'})").unwrap();
    // The structured tree (ADR-0344): ProduceResults outermost, the Expand
    // beneath it; a planned write leaves the graph untouched.
    let tree = conn
        .explain_plan("CREATE (p:Marker {id: 1}) RETURN p")
        .unwrap();
    assert_eq!(tree.operator, "ProduceResults");
    assert_eq!(tree.children[0].operator, "Create");
    let count = conn.query("MATCH (n:Marker) RETURN count(n) AS c").unwrap();
    assert_eq!(count.rows[0][0], Value::Int(0));

    // The EXPLAIN statement stamps the same tree `explain_plan` returns —
    // one producer, one plan (ADR-0359: no second rendering path).
    let explained = conn
        .query("EXPLAIN CREATE (p:Marker {id: 1}) RETURN p")
        .unwrap();
    assert_eq!(
        plan(&explained).to_value(),
        tree.to_value(),
        "EXPLAIN's tree is explain_plan's tree"
    );

    // A statement that cannot be planned fails with the same typed error
    // as EXPLAIN — no silent empty tree.
    let err = conn.explain_plan("MATCH (n)").unwrap_err().to_string();
    assert!(err.contains("query must conclude with RETURN"), "{err}");
}

#[test]
fn explain_result_carries_the_structured_plan() {
    let result = connection()
        .query("EXPLAIN MATCH (n:Person) RETURN n.name")
        .unwrap();
    // ADR-0344/0345 + ADR-0359: the structured tree rides the result —
    // `None` only on plain runs.
    let tree = plan(&result);
    assert_eq!(tree.operator, "ProduceResults");
    assert_eq!(tree.children[0].operator, "NodeByLabelScan");
    // The machine-native form: Neo4j field shape, pre-order global ids.
    let Value::Map(map) = tree.to_value() else {
        panic!("to_value is a map");
    };
    assert_eq!(
        map.get("operatorType"),
        Some(&Value::String("ProduceResults".into()))
    );
    let Value::Map(args) = map.get("args").expect("args") else {
        panic!("args is a map");
    };
    assert_eq!(args.get("Id"), Some(&Value::Int(0)));
    let Value::List(children) = map.get("children").expect("children") else {
        panic!("children is a list");
    };
    let Value::Map(child) = &children[0] else {
        panic!("child is a map");
    };
    assert_eq!(
        child.get("operatorType"),
        Some(&Value::String("NodeByLabelScan".into()))
    );
    let Value::Map(child_args) = child.get("args").expect("child args") else {
        panic!("child args is a map");
    };
    assert_eq!(child_args.get("Id"), Some(&Value::Int(1)));

    let plain = connection().query("RETURN 1 AS n").unwrap();
    assert!(plain.plan.is_none(), "plain runs carry no plan");
}

#[test]
fn is_profiled_classifies_explain_from_profile() {
    // The one classification fact the surfaces read to pick their plan
    // summary key (`plan` vs `profile`, `queryPlan` vs `profiledQueryPlan`):
    // EXPLAIN never stamps metrics, PROFILE stamps them on every executed
    // clause operator — so a tree carrying metrics anywhere is profiled.
    let explain = connection()
        .query("EXPLAIN MATCH (n:Person) RETURN n.name")
        .unwrap();
    assert!(
        !plan(&explain).is_profiled(),
        "EXPLAIN trees carry no metrics"
    );
    let profile = connection()
        .query("PROFILE MATCH (n:Person) RETURN n.name")
        .unwrap();
    assert!(
        plan(&profile).is_profiled(),
        "PROFILE trees carry the metrics pair"
    );
}

/// The shape pins (ADR-0344 phase 3): every pattern shape's operator names
/// its subject — an Expand carries the pattern text with its quantifier
/// bounds, an EagerAggregation names its grouping keys — and the estimates
/// (ADR-0218) fill what the store's stats offer honestly.
#[test]
fn explain_expand_subject_carries_the_pattern_text_with_bounds() {
    let mut conn = connection();
    conn.query(
        "CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'}),
         (:Person {name: 'Cara'})",
    )
    .unwrap();
    conn.query(
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
    )
    .unwrap();
    let result = conn
        .query("EXPLAIN MATCH (:Person)-[:KNOWS*1..2]->(:Person) RETURN 1")
        .unwrap();
    let tree = plan(&result);
    // The pattern text, quantifier bounds included, is the Details subject.
    let expand = find_node(tree, "Expand").expect("the traversal");
    assert!(
        expand
            .details
            .contains("(:Person)-[:KNOWS*1..2]->(:Person)"),
        "{tree:?}"
    );
    // Labeled endpoints narrow past the type count: no node carries an
    // estimate (the estimate column's gate reads exactly this fact).
    assert!(expand.estimated_rows.is_none(), "{tree:?}");
    assert!(tree.estimated_rows.is_none(), "{tree:?}");
}

#[test]
fn explain_bare_relationship_expand_carries_the_type_count_estimate() {
    let mut conn = connection();
    conn.query(
        "CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'}),
         (:Person {name: 'Cara'})",
    )
    .unwrap();
    conn.query(
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
    )
    .unwrap();
    let result = conn
        .query("EXPLAIN MATCH ()-[k:KNOWS]->() RETURN k")
        .unwrap();
    let tree = plan(&result);
    // A single typed relationship between unfiltered endpoints: the type's
    // row count is the estimate.
    let expand = find_node(tree, "Expand").expect("the traversal");
    assert!(expand.details.contains("()-[k:KNOWS]->()"), "{tree:?}");
    assert_eq!(expand.estimated_rows, Some(1), "{tree:?}");
    // The wire form carries the estimate beside the id and details.
    let Value::Map(map) = expand.to_value() else {
        panic!("to_value is a map");
    };
    let Value::Map(args) = map.get("args").expect("args") else {
        panic!("args is a map");
    };
    assert_eq!(args.get("EstimatedRows"), Some(&Value::Int(1)));
}

#[test]
fn explain_scan_estimate_is_the_label_count() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();
    let result = conn
        .query("EXPLAIN MATCH (n:Person) RETURN n.name")
        .unwrap();
    let tree = plan(&result);
    let scan = &tree.children[0];
    assert_eq!(scan.operator, "NodeByLabelScan");
    assert_eq!(scan.estimated_rows, Some(2), "{tree:?}");
}

#[test]
fn explain_aggregation_names_its_grouping_keys() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'})").unwrap();
    let result = conn
        .query("EXPLAIN MATCH (n:Person) RETURN n.name AS name, count(*) AS c")
        .unwrap();
    let tree = plan(&result);
    let aggregation = find_node(tree, "EagerAggregation").expect("the aggregation");
    assert!(aggregation.details.contains("keys: name"), "{tree:?}");
    // The grouped projection is the aggregation node itself, the tree's
    // root — not a ProduceResults projection above it.
    assert!(
        find_node(tree, "ProduceResults").is_none(),
        "an aggregating RETURN plans no projection above the aggregation: {tree:?}"
    );
}

#[test]
fn explain_merge_and_create_subjects_carry_the_pattern_text() {
    let result = connection()
        .query("EXPLAIN MERGE (a:Person)-[:KNOWS]->(b:Person) RETURN a")
        .unwrap();
    let merge = find_node(plan(&result), "Merge").expect("the merge");
    assert!(
        merge.details.contains("(a:Person)-[:KNOWS]->(b:Person)"),
        "{merge:?}"
    );
    let result = connection()
        .query("EXPLAIN CREATE (a:Person), (b:Person) RETURN a")
        .unwrap();
    let create = find_node(plan(&result), "Create").expect("the create");
    assert!(
        create.details.contains("(a:Person), (b:Person)"),
        "{create:?}"
    );
}

#[test]
fn explain_scan_estimate_folds_equality_predicates_when_indexed() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();
    conn.query("CREATE RANGE INDEX person_name FOR (p:Person) ON (p.name)")
        .unwrap();
    // A point lookup on the indexed property: the estimate is the
    // equality hit count (1), never the whole label count — the ratio
    // stays 1.0x and no EstimationMiss fires on an ordinary lookup.
    let result = conn
        .query("EXPLAIN MATCH (n:Person {name: 'Alice'}) RETURN n.name")
        .unwrap();
    let tree = plan(&result);
    let scan = &tree.children[0];
    assert_eq!(scan.operator, "NodeByLabelScan");
    assert_eq!(scan.estimated_rows, Some(1), "{tree:?}");
    assert!(scan.warnings.is_empty(), "{tree:?}");
}

#[test]
fn explain_scan_estimate_declines_when_filtered_without_stats() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();
    // No index serves the equality predicate: the fused scan-then-filter
    // operator honestly scans the whole label before filtering, so the
    // estimate IS the label count — the est-vs-actual ⚠ it fires on the
    // point lookup is the missing-index diagnosis (the LABS-328 class),
    // not a false positive.
    let result = conn
        .query("EXPLAIN MATCH (n:Person {name: 'Alice'}) RETURN n.name")
        .unwrap();
    let tree = plan(&result);
    let scan = &tree.children[0];
    assert_eq!(scan.estimated_rows, Some(2), "{tree:?}");
}
