//! ADR-0209 bulk-write detection and batched-chunk semantics.

use std::collections::BTreeMap;

use cypherlite_core::Value;

use super::super::test_support::{commit_scratch, exec_cypher, exec_write, write_scratch};
use super::*;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

fn analyze(cypher: &str) -> Option<BulkWrite> {
    let query = cypherlite_front::parse_and_lower(cypher)
        .unwrap_or_else(|e| panic!("parse failed for {cypher}: {e}"));
    let plan = cypherlite_planner::plan(query, &mut |_| false)
        .unwrap_or_else(|e| panic!("plan failed for {cypher}: {e}"));
    analyze_bulk_write(&plan)
}

fn seed(g: &Graph, cypher: &str) {
    let scratch = write_scratch(g);
    exec_write(g, &scratch, cypher).unwrap();
    commit_scratch(g, scratch);
}

/// Run `cypher`'s bulk write through the batched path with the given
/// budget, committing every chunk like `commit_optimistic_batched` does.
fn run_batched(g: &Graph, cypher: &str, budget: u64) -> crate::graph::QueryStats {
    let query = cypherlite_front::parse_and_lower(cypher).unwrap();
    let plan = cypherlite_planner::plan(query, &mut |_| false).unwrap();
    let bulk = analyze_bulk_write(&plan).expect("eligible bulk write");
    let pin = g.pin_now().unwrap();
    let targets = bulk
        .collect_targets(g, pin.ts())
        .unwrap()
        .expect("no constraints");
    drop(pin);
    let mut stats = crate::graph::QueryStats::default();
    let mut cursor = BulkCursor::default();
    loop {
        let scratch = write_scratch(g);
        let done = {
            let view = ExecGraph::write(g, &scratch);
            let done = bulk
                .execute_chunk(&view, &targets, &mut cursor, budget, &BTreeMap::new())
                .unwrap();
            stats.merge(&view.stats());
            done
        };
        commit_scratch(g, scratch);
        if done {
            break;
        }
    }
    stats
}

mod detection {
    use super::*;

    #[test]
    fn accepts_the_ticket_shapes() {
        assert!(analyze("MATCH (n) DETACH DELETE n").is_some());
        assert!(analyze("MATCH (n) DELETE n").is_some());
        assert!(analyze("MATCH ()-[r]->() DELETE r").is_some());
        assert!(analyze("MATCH (n:Person) DETACH DELETE n").is_some());
        assert!(analyze("MATCH (n) SET n.prop = 1").is_some());
        assert!(analyze("MATCH (n) SET n.x = n.x + 1").is_some());
        assert!(analyze("MATCH (n) SET n:Tagged").is_some());
        assert!(analyze("MATCH (n) REMOVE n:Label").is_some());
        assert!(analyze("MATCH (n) REMOVE n.prop").is_some());
        assert!(
            analyze("MATCH (n) MATCH (m) WITH n AS a, m AS b CREATE (a)-[:TAGGED]->(b)").is_some()
        );
        assert!(analyze("MATCH (a), (b) CREATE (a)-[:T]->(b)").is_some());
        assert!(analyze("MATCH ()-[r:TAGGED]->() DELETE r").is_some());
    }

    #[test]
    fn rejects_non_bulk_or_order_dependent_shapes() {
        // A selective anchor (WHERE) is out of scope.
        assert!(analyze("MATCH (n) WHERE n.x > 5 DETACH DELETE n").is_none());
        // Aggregating / paging / filtering WITH.
        assert!(analyze("MATCH (n) WITH n, count(n) AS c DELETE n").is_none());
        assert!(analyze("MATCH (n) WITH n ORDER BY n.id DETACH DELETE n").is_none());
        assert!(analyze("MATCH (n) WITH n LIMIT 10 DELETE n").is_none());
        // A RETURN after the write.
        assert!(analyze("MATCH (n) DETACH DELETE n RETURN count(*)").is_none());
        // Cross-row SET on a cross product is order-dependent.
        assert!(analyze("MATCH (n) MATCH (m) SET n.x = m.y").is_none());
        // Relationship patterns in the match are not bare factors.
        assert!(analyze("MATCH (a)-[r]->(b) DELETE r").is_none());
        // Optional matches and property-carrying patterns are out.
        assert!(analyze("OPTIONAL MATCH (n) DELETE n").is_none());
        assert!(analyze("MATCH (n {id: 1}) DELETE n").is_none());
        // CREATE reading the graph through a subquery expression.
        assert!(analyze("MATCH (n) CREATE (:X {c: COUNT { MATCH (m) RETURN m }})").is_none());
        // MERGE / FOREACH / plain reads are not bulk shapes.
        assert!(analyze("MATCH (n) MERGE (n)-[:T]->(n)").is_none());
        assert!(analyze("MATCH (n) RETURN n").is_none());
        // Two write clauses are out of scope.
        assert!(analyze("MATCH (n) SET n.x = 1 DELETE n").is_none());
        // A variable repeated across patterns is a self-join, not a
        // cross product of independent factors.
        assert!(analyze("MATCH (n), (n) SET n.x = 1").is_none());
        // `WITH *` mixed with a real rename binds two names to one
        // entity; out of scope.
        assert!(analyze("MATCH (n) WITH *, n AS m SET m.x = 1").is_none());
        // A non-DETACH DELETE mixing node and relationship targets
        // stays on the unbatched path (the connectedness preflight
        // cannot ignore the statement's own relationship targets).
        assert!(analyze("MATCH (n), ()-[r]->() DELETE r, n").is_none());
        assert!(analyze("MATCH (n), ()-[r]->() DELETE n, r").is_none());
        // The DETACH form of the same mix is fine.
        assert!(analyze("MATCH (n), ()-[r]->() DETACH DELETE n, r").is_some());
    }

    #[test]
    fn rename_swap_renames_each_factor_once() {
        // `WITH n AS m, m AS n` is a simultaneous swap; it must not
        // re-rename an already-renamed factor back to its old name.
        let bulk = analyze("MATCH (n:A), (m:B) WITH n AS m, m AS n DELETE n").unwrap();
        let mut vars: Vec<&str> = bulk.factors.iter().map(|f| f.var.as_str()).collect();
        vars.sort_unstable();
        assert_eq!(vars, ["m", "n"]);
    }

    #[test]
    fn shadowing_rename_drops_the_shadowed_factor() {
        // `WITH n AS m` (no `*`) carries only `m` = old `n`; the
        // incoming `m` factor leaves the scope instead of colliding.
        let bulk = analyze("MATCH (n:A), (m:B) WITH n AS m DELETE m").unwrap();
        assert_eq!(bulk.factors.len(), 1);
        assert_eq!(bulk.factors[0].var, "m");
        assert!(matches!(
            bulk.factors[0].source,
            FactorSource::Nodes { ref labels } if labels == &["A".to_string()]
        ));
    }
}

mod execution {
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn batched_detach_delete_matches_unbatched_state() {
        let g = Graph::default();
        seed(&g, "UNWIND range(1, 30) AS i CREATE (:N {id: i})");
        seed(
            &g,
            "MATCH (a:N), (b:N) WHERE a.id + 1 = b.id CREATE (a)-[:R {k: a.id}]->(b)",
        );
        let stats = run_batched(&g, "MATCH (n) DETACH DELETE n", 7);
        assert_eq!(stats.nodes_deleted, 30);
        assert_eq!(stats.relationships_deleted, 29);
        let r = exec_cypher(&g, "MATCH (n) RETURN count(n) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
        let r = exec_cypher(&g, "MATCH ()-[r]->() RETURN count(r) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    }

    #[test]
    fn batched_cross_create_builds_the_full_product() {
        let g = Graph::default();
        seed(&g, "UNWIND range(1, 9) AS i CREATE (:N {id: i})");
        let stats = run_batched(
            &g,
            "MATCH (n) MATCH (m) WITH n AS a, m AS b CREATE (a)-[:TAGGED]->(b)",
            10,
        );
        assert_eq!(stats.relationships_created, 81);
        let r = exec_cypher(&g, "MATCH ()-[r:TAGGED]->() RETURN count(r) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(81)]]);
    }

    #[test]
    fn batched_set_and_remove_apply_per_entity() {
        let g = Graph::default();
        seed(&g, "UNWIND range(1, 12) AS i CREATE (:N {id: i, x: 1})");
        let stats = run_batched(&g, "MATCH (n) SET n.x = n.x + 1, n:Tagged", 5);
        // labels_added counts distinct label names, not labelled nodes.
        assert_eq!(stats.properties_set, 12);
        assert_eq!(stats.labels_added, 1);
        let r = exec_cypher(&g, "MATCH (n:Tagged) RETURN sum(n.x) AS s");
        assert_eq!(r.rows, vec![vec![Value::Int(24)]]);
        let stats = run_batched(&g, "MATCH (n) REMOVE n:Tagged", 5);
        assert_eq!(stats.labels_removed, 1);
        let r = exec_cypher(&g, "MATCH (n:Tagged) RETURN count(n) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    }

    #[test]
    fn batched_rel_delete() {
        let g = Graph::default();
        seed(
            &g,
            "UNWIND range(1, 8) AS i CREATE (:A {id: i})-[:R {k: i}]->(:B {id: i})",
        );
        let stats = run_batched(&g, "MATCH ()-[r]->() DELETE r", 3);
        assert_eq!(stats.relationships_deleted, 8);
        let r = exec_cypher(&g, "MATCH ()-[r]->() RETURN count(r) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
        let r = exec_cypher(&g, "MATCH (n) RETURN count(n) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(16)]]);
    }

    #[test]
    fn rename_swap_deletes_the_swapped_set_only() {
        let g = Graph::default();
        seed(&g, "CREATE (:A {id: 1}), (:B {id: 2})");
        // Simultaneous swap: the surviving `n` binds the old `m` (:B).
        let stats = run_batched(&g, "MATCH (n:A), (m:B) WITH n AS m, m AS n DELETE n", 10);
        assert_eq!(stats.nodes_deleted, 1);
        let r = exec_cypher(&g, "MATCH (x) RETURN x.id AS id");
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    }

    #[test]
    fn delete_touches_only_the_named_factors() {
        let g = Graph::default();
        seed(&g, "CREATE (:A {id: 1}), (:B {id: 2})");
        let stats = run_batched(&g, "MATCH (n:A), (m:B) DELETE n", 10);
        assert_eq!(stats.nodes_deleted, 1);
        let r = exec_cypher(&g, "MATCH (x) RETURN x.id AS id");
        assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    }

    #[test]
    fn shadowing_rename_deletes_the_aliasing_set_only() {
        let g = Graph::default();
        seed(&g, "CREATE (:A {id: 1}), (:B {id: 2})");
        // `m` is rebound to old `n` (:A); the incoming `m` (:B) is out
        // of scope, so only the :A node is deleted.
        let stats = run_batched(&g, "MATCH (n:A), (m:B) WITH n AS m DELETE m", 10);
        assert_eq!(stats.nodes_deleted, 1);
        let r = exec_cypher(&g, "MATCH (x) RETURN x.id AS id");
        assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    }

    #[test]
    fn non_detach_delete_preflights_connected_nodes_before_any_write() {
        let g = Graph::default();
        seed(&g, "CREATE (:N {id: 1})-[:R]->(:N {id: 2}), (:N {id: 3})");
        let query = cypherlite_front::parse_and_lower("MATCH (n) DELETE n").unwrap();
        let plan = cypherlite_planner::plan(query, &mut |_| false).unwrap();
        let bulk = analyze_bulk_write(&plan).unwrap();
        let pin = g.pin_now().unwrap();
        let err = bulk.collect_targets(&g, pin.ts()).unwrap_err();
        assert!(err.to_string().contains("DeleteConnectedNode"));
        drop(pin);
        // Nothing was deleted: the store is byte-identical.
        let r = exec_cypher(&g, "MATCH (n) RETURN count(n) AS c");
        assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    }

    #[test]
    fn constrained_catalog_falls_back() {
        let g = Graph::default();
        seed(&g, "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.id IS UNIQUE");
        let query = cypherlite_front::parse_and_lower("MATCH (n) DETACH DELETE n").unwrap();
        let plan = cypherlite_planner::plan(query, &mut |_| false).unwrap();
        let bulk = analyze_bulk_write(&plan).unwrap();
        let pin = g.pin_now().unwrap();
        assert!(bulk.collect_targets(&g, pin.ts()).unwrap().is_none());
    }
}
