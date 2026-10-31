//! ADR-0395: the per-statement nested-evaluation budget. A nested
//! clause-engine part pass — a `CALL { … }` body part per imported row
//! (or one shared execution per part of an import-invariant body), a
//! conditional's taken branch bodies, an EXISTS/COUNT/COLLECT expression
//! subquery's body, a FOREACH clause list per list item, a NEXT stage —
//! is one tick on the statement's single counter (owned by the execution
//! view; every nested entry ticks it, so no nested context can reset
//! it). A UNION body ticks once per part, so a wide body driven by many
//! rows spends rows × parts on the budget and crosses it instead of
//! hiding its width inside one invocation tick. A statement whose
//! passes exceed the fixed 100 000 budget fails with the typed
//! `NestedEvaluationBudgetExceeded` execution error instead of running
//! the exponentially compounding fan.
//!
//! The bomb tests drive each counting site past the budget; every bomb
//! fails at the 100 001st pass, before its full theoretical fan runs.
//! The counting-rule test pins each event's exact tick count — and, by
//! running every case on one reused view, that the counter resets per
//! statement and compounds only within one.

use std::collections::BTreeMap;

use cypherlite_core::Error;

use super::test_support::{
    exec_write_unvalidated, try_exec_cypher, try_exec_cypher_on, write_scratch,
};
use crate::graph::Graph;
use crate::overlay::ExecGraph;

/// The one budget-error assertion, typed for this crate's bombs: the
/// shared `cypherlite_test_support` helper checks the exact authored
/// message (single-sourced from `NESTED_EVAL_BUDGET`) and the `Kind:`
/// prefix's parse-back to the ADR-0395 `ErrorKind` row.
fn assert_budget_error(error: Error) {
    cypherlite_test_support::nested_eval::assert_nested_eval_budget_error(&error.to_string());
}

/// The doubling nested-CALL bomb (the LABS-416 shape): each level's body
/// `UNWIND`s a two-element list feeding the next level's naked
/// `CALL { … }`, so every body above the leaf contains a nested CALL —
/// the import-invariant shared fast path never applies to them — and the
/// body passes double per level: 2^(depth+1) - 2 per-row body passes
/// plus 2^depth shared executions of the RETURN-only leaf clause,
/// 196 606 at depth 16.
fn nested_call_bomb(depth: usize) -> String {
    let mut query = String::from("UNWIND [0, 1] AS v\n");
    for k in 1..=depth {
        query.push_str(&format!("CALL {{\nUNWIND [0, 1] AS a{k}\n"));
    }
    query.push_str("CALL {\nRETURN 1 AS leaf\n}\n");
    for _ in 0..depth {
        query.push_str("}\n");
    }
    query.push_str("RETURN count(*) AS c");
    query
}

/// A store with four `L0` and four `L1` nodes and two `(:L0)-[:R]->(:L1)`
/// relationships for the expression-subquery bombs.
fn label_graph() -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let mut l0 = Vec::new();
    let mut l1 = Vec::new();
    for _ in 0..4 {
        l0.push(g.create_node(vec!["L0".into()], BTreeMap::new()).unwrap());
        l1.push(g.create_node(vec!["L1".into()], BTreeMap::new()).unwrap());
    }
    for k in 0..2 {
        g.create_rel(l0[k], l1[k], "R".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    g
}

/// The EXISTS-chain bomb: every level carries a node-pattern `WHERE`, so
/// no level is probe-eligible and each runs the clause engine — one pass
/// per part per candidate row. Each level binds its own variable (`a1`,
/// `a2`, …), so an inner `MATCH` never sees its variable pre-bound from
/// the outer row: every level's label scan runs fresh over the four
/// candidates and the passes compound 4× per level — (4^(d+1) - 1) / 3 at
/// depth d, 349 525 at depth 9.
fn exists_chain_bomb(depth: usize) -> String {
    let mut body = "MATCH (z:Z)".to_string();
    for k in (1..=depth).rev() {
        let label = if k % 2 == 1 { "L1" } else { "L0" };
        body = format!("MATCH (a{k}:{label} WHERE EXISTS {{ {body} }})");
    }
    format!("RETURN EXISTS {{ {body} }} AS bomb")
}

/// The nested CALL counting site: depth 16 (196 606 body passes) crosses
/// the budget, and the error arrives at the 100 001st pass — before the
/// fan completes — with the typed kind and the exact message.
#[test]
fn nested_call_bomb_exceeds_the_budget() {
    let g = Graph::default();
    assert_budget_error(try_exec_cypher(&g, &nested_call_bomb(16)).err().unwrap());
}

/// The EXISTS clause-engine counting site (`subquery_eval`): the
/// predicate-bearing chain never takes the probes, and its passes cross
/// the budget with the same typed error.
#[test]
fn exists_chain_bomb_exceeds_the_budget() {
    let g = label_graph();
    assert_budget_error(try_exec_cypher(&g, &exists_chain_bomb(9)).err().unwrap());
}

/// The wide-UNION counting rule (the granularity the security review
/// pinned): a body's UNION parts each tick, so a two-part body driven
/// by 60 000 rows spends 120 000 part passes on the budget and crosses
/// it at the 100 001st — the rows × parts product lands on the
/// counter, not just the row axis.
#[test]
fn wide_union_body_driven_by_rows_exceeds_the_budget() {
    let g = Graph::default();
    assert_budget_error(
        try_exec_cypher(
            &g,
            "UNWIND range(1, 60000) AS i \
             CALL (i) { RETURN i AS x UNION ALL RETURN i + 1 AS x } \
             RETURN count(*) AS c",
        )
        .err()
        .unwrap(),
    );
}

/// The omitted-RETURN COUNT counting site (`subquery_eval`): a COUNT
/// body with no RETURN runs the clause engine directly, once per
/// subquery per row — two per row over 60 000 rows crosses the budget.
#[test]
fn omitted_return_count_bomb_exceeds_the_budget() {
    let g = label_graph();
    assert_budget_error(
        try_exec_cypher(
            &g,
            "UNWIND range(1, 60000) AS i \
             RETURN COUNT { MATCH (n:L0) } + COUNT { MATCH (n:L1) } AS x",
        )
        .err()
        .unwrap(),
    );
}

/// The FOREACH counting site (`dispatch`): the clause list runs once per
/// list item per row, so a 100 001-item list crosses the budget before
/// its last pass.
#[test]
fn foreach_bomb_exceeds_the_budget() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    assert_budget_error(
        exec_write_unvalidated(&g, &scratch, "FOREACH (i IN range(1, 100001) | CREATE ())")
            .err()
            .unwrap(),
    );
}

/// The counting rule (ADR-0395), pinned per event on one reused view: each
/// nested statement-engine pass ticks exactly once, the counter compounds
/// across nesting levels within a statement, each statement starts from
/// zero (the counts stay exact in sequence on the same view), and the
/// pattern-probe fast path answers without ticking.
#[test]
fn the_counting_rule_ticks_each_event_once() {
    let g = label_graph();
    let view = ExecGraph::read(&g);
    let cases: &[(&str, u64)] = &[
        // A pattern-probe shape (one plain 2-node-1-rel MATCH from the
        // row-bound endpoint `a` to an anonymous endpoint, no WHERE)
        // answers through the LABS-193 anchored probe: zero engine
        // passes, zero ticks.
        (
            "MATCH (a:L0) RETURN EXISTS { MATCH (a)-[:R]->() } AS b",
            0,
        ),
        // An import-invariant CALL body shares one execution across its
        // two driving rows: one tick for the clause.
        (
            "UNWIND [0, 1] AS v CALL { RETURN 1 AS leaf } RETURN count(*) AS c",
            1,
        ),
        // A body computing from its import runs per imported row.
        (
            "UNWIND [0, 1] AS v CALL (v) { RETURN v AS out } RETURN count(*) AS c",
            2,
        ),
        // A UNION body ticks once per part per row: the wide-body
        // granularity (the rows × parts product lands on the budget,
        // not just the row axis).
        (
            "UNWIND [0, 1] AS v CALL (v) { RETURN v AS x \
             UNION ALL RETURN v + 1 AS x } RETURN count(*) AS c",
            4,
        ),
        // A conditional's taken branch body is one nested pass.
        ("WHEN true THEN { RETURN 1 AS x } ELSE { RETURN 2 AS x }", 1),
        // A nested body plus its NEXT-stage recursion: two passes.
        (
            "CALL { RETURN 1 AS a NEXT RETURN a + 1 AS b } RETURN b",
            2,
        ),
        // The COLLECT expression-subquery engine plus its NEXT stage.
        (
            "RETURN COLLECT { RETURN 1 AS a NEXT RETURN a + 1 AS b } AS c",
            2,
        ),
        // The EXISTS clause-engine path compounds across nesting levels:
        // 1 + 4 + 16 + 64 = 85 passes at depth 3 (four candidates per
        // level), one tick per part per row — the shared counter never
        // resets at a nested context.
        ("RETURN EXISTS { MATCH (a1:L1 WHERE EXISTS { MATCH (a2:L0 WHERE EXISTS { MATCH (a3:L1 WHERE EXISTS { MATCH (z:Z) }) }) }) } AS b", 85),
    ];
    for (query, expected) in cases {
        try_exec_cypher_on(&view, query).unwrap();
        assert_eq!(view.nested_evals().count(), *expected, "{query}");
    }
    // The nested-CALL doubling bomb below the budget, both CALL events of
    // the counting rule: the per-row bodies (a nested CALL in every one,
    // so the import-invariant fast path never applies) double per level —
    // 2^(depth+1) - 2 = 16 382 passes at depth 13 — plus one shared
    // execution per import-invariant leaf clause per body pass — 2^depth
    // = 8 192 more: 24 574 on the one statement's counter.
    try_exec_cypher_on(&view, &nested_call_bomb(13)).unwrap();
    assert_eq!(view.nested_evals().count(), 24_574);
    // The FOREACH arm: one pass per list item per row, on the write view.
    let scratch = write_scratch(&g);
    let wview = ExecGraph::write(&g, &scratch);
    try_exec_cypher_on(&wview, "FOREACH (i IN range(1, 5) | CREATE ())").unwrap();
    assert_eq!(wview.nested_evals().count(), 5);
}
