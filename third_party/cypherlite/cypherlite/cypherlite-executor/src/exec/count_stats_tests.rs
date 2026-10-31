//! The count-only statistics short-circuit (LABS-370 v1): the shapes it
//! answers from the maintained counts, byte-identical to the scan's
//! answer, and the shapes it declines (the scan stays).

use super::count_stats::count_only_stats_result;
use super::test_support::{
    commit_scratch, exec_autocommit, exec_cypher, exec_cypher_on, exec_write, write_scratch,
};
use crate::eval::Row;
use crate::graph::Graph;
use crate::overlay::ExecGraph;
use cypherlite_core::Value;
use cypherlite_ir::Clause;
use std::collections::BTreeMap;

/// Parse a statement into the flat clause list the recognizer takes,
/// against the statement's opening stage (one unbound seed row).
fn clauses_of(cypher: &str) -> Vec<Clause> {
    let q = cypherlite_front::parse_and_lower(cypher).unwrap();
    q.parts.into_iter().flat_map(|p| p.clauses).collect()
}

/// Pin the decline itself, not a coincidentally-equal accept: parse each
/// declined statement and assert the recognizer returns `Ok(None)` (the
/// scan answers it), so a wrong accept diverges even where the scan's
/// answer happens to match.
fn declines_the_shortcircuit(g: &Graph, cypher: &str) {
    let clauses = clauses_of(cypher);
    let view = ExecGraph::read(g);
    assert!(
        count_only_stats_result(&view, &clauses, &[Row::new()]).is_none(),
        "must decline to the scan: {cypher}"
    );
}

fn count_graph() -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g
        .create_node(vec!["B".into(), "C".into()], BTreeMap::new())
        .unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(a, a, "SELF".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    g
}

#[test]
fn count_only_shapes_answer_from_the_stats() {
    let g = count_graph();
    // Whole-label count.
    let r = exec_cypher(&g, "MATCH (a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // Pair count, both pattern spellings of the same direction.
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    let r = exec_cypher(&g, "MATCH (b:B)<-[:R]-(a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // count(var) equals count(*): the pattern binds the var every row.
    let r = exec_cypher(&g, "MATCH (a:A)-[r:R]->(b:B) RETURN count(r) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // A never-written pair is a count of zero, not a decline.
    let r = exec_cypher(&g, "MATCH (a:A)-[:NOPE]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
    // The un-aliased column keeps the projection's own name.
    let r = exec_cypher(&g, "MATCH (a:A) RETURN count(*)");
    assert_eq!(r.columns[0], "count(*)");
}

#[test]
fn count_only_declines_the_shapes_the_stats_cannot_answer() {
    let g = count_graph();
    // The multi-label node: the per-label counts cannot answer a
    // label-set intersection — the scan stays. A :B-only node beside a
    // :C-only node makes a wrong accept diverge (the scan answers 0 for
    // `MATCH (b:B:C)`; the single-label stats would answer nonzero).
    g.begin_write().unwrap();
    g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    declines_the_shortcircuit(&g, "MATCH (b:B:C) RETURN count(*) AS n");
    assert_eq!(
        exec_cypher(&g, "MATCH (b:B:C) RETURN count(*) AS n").rows[0][0],
        Value::Int(1)
    );
    // A WHERE, DISTINCT, grouping, SKIP or LIMIT disqualifies.
    declines_the_shortcircuit(&g, "MATCH (a:A) WHERE 1 = 1 RETURN count(*) AS n");
    declines_the_shortcircuit(&g, "MATCH (a:A) RETURN count(DISTINCT a) AS n");
    declines_the_shortcircuit(&g, "MATCH (a:A) RETURN count(*) AS n SKIP 1");
    // An undirected rel counts both orientations, which the
    // one-directional pair counts cannot answer; the scan stays (here
    // the reversed orientation's start would need :A, so the scan's
    // answer is 1 either way — the point is the short-circuit declines).
    declines_the_shortcircuit(&g, "MATCH (a:A)-[:R]-(b:B) RETURN count(*) AS n");
}

/// LABS-370 (v1 follow-up, F-1-1): the single-node shape answers from the
/// exact per-label live counts, not the label index's plan-time upper
/// bound — after a delete and a label removal the exact value is the
/// scan's answer and the index overcounts.
#[test]
fn count_only_label_count_is_exact_after_deletes_and_label_removal() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let doomed = g.create_node(vec!["L".into()], BTreeMap::new()).unwrap();
    g.create_node(vec!["L".into(), "M".into()], BTreeMap::new())
        .unwrap();
    g.create_node(vec!["L".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    // One :L deleted, one :L loses no :L (M removed from the multi-label
    // node): the exact live count of :L is 2. The label index keeps the
    // deleted id until checkpoint GC — the upper bound stays 3.
    g.begin_write().unwrap();
    g.delete_node(doomed, true).unwrap();
    g.commit_write();
    exec_autocommit(&g, "MATCH (n:L:M) REMOVE n:M").unwrap();
    let r = exec_cypher(&g, "MATCH (n:L) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
    // The drained :M count is a hard 0, exact.
    let r = exec_cypher(&g, "MATCH (n:M) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
}

/// LABS-370 (v1 follow-up, F-1-2): the base stats cannot see an
/// in-flight transaction's writes — inside the transaction the
/// short-circuit declines and the scan reads its own writes; after the
/// commit the stats answer again.
#[test]
fn count_only_declines_inside_a_transaction_and_reads_its_own_writes() {
    let g = count_graph();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (a:A)-[:R]->(b:B)").unwrap();
    // The in-tx view: the scan must see the scratch write (a wrong
    // short-circuit would answer the base's stale 1).
    let view = ExecGraph::write(&g, &scratch);
    let r = exec_cypher_on(&view, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
    let r = exec_cypher_on(&view, "MATCH (a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
    // Commit: the stats pick the write up and answer again.
    commit_scratch(&g, scratch);
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
    let r = exec_cypher(&g, "MATCH (a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
}

/// LABS-370 (v1 follow-up, F-1-2): a pinned older snapshot declines too —
/// the base stats are only exact for the latest published state (a
/// concurrent `apply_redo` advances them past an older pin).
#[test]
fn count_only_declines_on_a_pinned_older_snapshot() {
    let g = count_graph();
    let stale = ExecGraph::read_at(&g, 0);
    // Nothing is visible at ts 0 — the scan's answer; the stats would
    // answer the current populations.
    let r = exec_cypher_on(&stale, "MATCH (a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
    let r = exec_cypher_on(&stale, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn count_only_stats_follow_deletes_and_label_mutations() {
    let g = count_graph();
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // A delete leaves the pair count exactly.
    exec_autocommit(&g, "MATCH (a:A)-[r:R]->(b:B) DELETE r").unwrap();
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
    // Recreate and move the end node's labels: the counts follow both
    // endpoint sides.
    exec_autocommit(&g, "CREATE (a:A)-[:R]->(b:B)").unwrap();
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    exec_autocommit(&g, "MATCH (b:B:C) REMOVE b:C").unwrap();
    let r = exec_cypher(&g, "MATCH (a:A)-[:R]->(b:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
}

/// LABS-370 round 3 (F-1-3): the short-circuit answers a
/// whole-population count — one unbound seed row, the statement's
/// opening stage. `run_clauses_mode` runs per part with the previous
/// stage's rows (walk_plan_stages → next_seeds → result_seed_rows), so a
/// NEXT/WHEN/ELSE body seeded with N rows must answer N × population
/// (0 when N = 0) — the recognizer declines on anything but the one
/// unbound row, and the normal pipeline answers through the scan.
#[test]
fn count_only_declines_seeded_rows_beyond_one_unbound_seed() {
    let g = count_graph();
    // Round 4, F-4-3: the probe feeds through the real pipeline — front
    // lowering plus `cypherlite_planner::plan` — so the clauses are the
    // shape production sends (whatever the planner does or does not
    // normalize away, the raw front lowering's static `label_expr` mirror
    // included), never a hand-stripped shape no pipeline stage produces.
    let q = cypherlite_front::parse_and_lower("MATCH (a:A) RETURN count(*) AS n").unwrap();
    let planned = cypherlite_planner::plan(q, &mut |_| false).unwrap();
    let clauses: Vec<Clause> = planned.parts.iter().flatten().cloned().collect();
    let view = ExecGraph::read(&g);
    // The statement's opening stage: exactly one row, no bindings — the
    // one shape the short-circuit answers.
    assert!(count_only_stats_result(&view, &clauses, &[Row::new()]).is_some());
    // N = 0: a seeded stage with no rows must answer 0, not the
    // population — decline.
    assert!(count_only_stats_result(&view, &clauses, &[]).is_none());
    // N = 2: the answer would be 2 × population — decline.
    let two = vec![Row::new(), Row::new()];
    assert!(count_only_stats_result(&view, &clauses, &two).is_none());
    // One row that already carries bindings: the count is not
    // whole-population over an unbound scan — decline.
    let bound = vec![Row::from([("x".to_string(), Value::Int(1))])];
    assert!(count_only_stats_result(&view, &clauses, &bound).is_none());
}

/// LABS-370 round 3 (F-1-3): inline pattern predicates the counts never
/// checked — node-level and rel-level WHERE change the scan's row set;
/// the recognizer declines and the scan answers them.
#[test]
fn count_only_declines_inline_pattern_predicates() {
    let g = count_graph();
    // Node-level inline WHERE.
    declines_the_shortcircuit(&g, "MATCH (a:A WHERE a.x > 1) RETURN count(*) AS n");
    let r = exec_cypher(&g, "MATCH (a:A WHERE a.x > 1) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
    // Rel-level inline WHERE.
    declines_the_shortcircuit(
        &g,
        "MATCH (a:A)-[r:R WHERE r.x > 1]->(b:B) RETURN count(*) AS n",
    );
    let r = exec_cypher(
        &g,
        "MATCH (a:A)-[r:R WHERE r.x > 1]->(b:B) RETURN count(*) AS n",
    );
    assert_eq!(r.rows[0][0], Value::Int(0));
}

/// LABS-370 round 5 (the rust review's self-loop finding): a self-loop
/// rides both adjacency sides of its own node, so the label mutation's
/// incident walk sees it twice — the (mutated, type, mutated) diagonal
/// pair must be charged once, not once per side, or the pair counts
/// diverge from the scan on the node's label add and remove.
#[test]
fn count_only_self_loop_label_mutation_charges_the_diagonal_once() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, a, "SELF".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    // Add: the one self-loop realizes the (B, SELF, B) diagonal once —
    // the walk charged it once per adjacency side before the fix (2).
    exec_autocommit(&g, "MATCH (a:A) SET a:B").unwrap();
    let r = exec_cypher(&g, "MATCH (x:B)-[:SELF]->(y:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // The off-diagonal crosses are one each, not doubled.
    let r = exec_cypher(&g, "MATCH (x:A)-[:SELF]->(y:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    let r = exec_cypher(&g, "MATCH (x:B)-[:SELF]->(y:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(1));
    // Remove: the same diagonal, drained once on the way down.
    exec_autocommit(&g, "MATCH (a:A:B) REMOVE a:B").unwrap();
    let r = exec_cypher(&g, "MATCH (x:B)-[:SELF]->(y:B) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
    let r = exec_cypher(&g, "MATCH (x:B)-[:SELF]->(y:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(0));
}

/// LABS-370 round 3 (F-1-4): a same-variable (self-loop) pattern
/// `(a:A)-[:SELF]->(a:A)` answered from the (A,SELF,A) pair count would
/// count distinct-endpoint rels the scan's same-variable shape excludes
/// — the recognizer declines and the scan answers.
#[test]
fn count_only_declines_same_variable_self_loop_shapes() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let a2 = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, a, "SELF".into(), BTreeMap::new()).unwrap();
    // The pair count (A,SELF,A) counts this distinct-endpoint rel too;
    // the scan's same-variable shape does not — the divergence the
    // decline must pin.
    g.create_rel(a, a2, "SELF".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    declines_the_shortcircuit(&g, "MATCH (a:A)-[:SELF]->(a:A) RETURN count(*) AS n");
    // The scan's answer today: the compact walker does not enforce
    // variable identity on a repeated name, so it counts both rels (2) —
    // the pair count (A,SELF,A) agrees today. The decline stays
    // regardless: the day the walker enforces identity (the openCypher
    // semantics — only the true self-loop), the scan answers 1 while
    // the pair count still answers 2, and the short-circuit must not be
    // the path that answers the wrong one.
    let r = exec_cypher(&g, "MATCH (a:A)-[:SELF]->(a:A) RETURN count(*) AS n");
    assert_eq!(r.rows[0][0], Value::Int(2));
}
