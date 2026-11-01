//! ADR-0171: the per-label snapshot probe keeps `+labels` / `-labels` equal
//! to the old census while checking a bounded number of snapshot nodes.

use std::collections::BTreeMap;

use crate::exec::test_support::{exec_autocommit, exec_write, write_scratch, write_scratch_at};
use crate::graph::{take_label_probe_checks, Graph, QueryStats};
use crate::redo::RedoOp;

/// Run `q` on a fresh scratch at `ts` (nothing is committed). Returns its
/// stats and how many snapshot ids label accounting checked.
fn probe_at(g: &Graph, ts: u64, q: &str) -> (QueryStats, u64) {
    let scratch = write_scratch_at(g, ts);
    take_label_probe_checks();
    let stats = exec_write(g, &scratch, q).unwrap().stats;
    (stats, take_label_probe_checks())
}

fn probe(g: &Graph, q: &str) -> (QueryStats, u64) {
    probe_at(g, g.commit_ts(), q)
}

/// Run `q` at the current commit and apply its redo as the next commit. No
/// GC runs, so ids the commit made stale stay in `label_index`.
fn commit_query(g: &Graph, q: &str) -> QueryStats {
    exec_autocommit(g, q).unwrap().stats
}

fn seed(g: &Graph, label: &str, n: usize) {
    for _ in 0..n {
        g.create_node(vec![label.into()], BTreeMap::new()).unwrap();
    }
}

fn labels(stats: &QueryStats) -> (i64, i64) {
    (stats.labels_added, stats.labels_removed)
}

#[test]
fn label_probe_inspections_independent_of_graph_size() {
    for n in [1_000, 10_000] {
        let g = Graph::default();
        seed(&g, "Seed", n);
        let (create_seed, seed_checks) = probe(&g, "CREATE (:Seed)");
        assert_eq!(labels(&create_seed), (0, 0));
        let (create_fresh, fresh_checks) = probe(&g, "CREATE (:Fresh)");
        assert_eq!(labels(&create_fresh), (1, 0));
        let (delete_one, delete_checks) = probe(&g, "MATCH (n:Seed) WHERE id(n) = 0 DELETE n");
        assert_eq!(delete_one.nodes_deleted, 1);
        assert_eq!(labels(&delete_one), (0, 0));
        // A gain stops at the first visible carrier, an uninterned label is
        // not probed, and a loss needs one other carrier besides the deleted
        // node.
        assert_eq!(
            [seed_checks, fresh_checks, delete_checks],
            [1, 0, 2],
            "[CREATE (:Seed), CREATE (:Fresh), DELETE one :Seed] checks at {n} :Seed nodes"
        );
    }
}

#[test]
fn deleting_all_of_a_label_checks_at_most_its_set() {
    let g = Graph::default();
    seed(&g, "Other", 2_000);
    seed(&g, "L", 300);
    let (stats, checks) = probe(&g, "MATCH (n:L) DELETE n");
    assert_eq!(stats.nodes_deleted, 300);
    assert_eq!(labels(&stats), (0, 1));
    // The last carrier's loss must rule out every other id, each once.
    assert_eq!(checks, 300);
}

#[test]
fn create_after_committed_delete_skips_the_stale_id() {
    let g = Graph::default();
    commit_query(&g, "CREATE (:A)");
    let deleted = commit_query(&g, "MATCH (n:A) DELETE n");
    assert_eq!(labels(&deleted), (0, 1));
    // No GC ran: the deleted node's id is still in `label_index` for `A`,
    // and the probe checks it and skips it.
    let (stats, checks) = probe(&g, "CREATE (:A)");
    assert_eq!(stats.nodes_created, 1);
    assert_eq!(labels(&stats), (1, 0));
    assert_eq!(checks, 1);
}

#[test]
fn relabeled_stale_id_does_not_count() {
    let g = Graph::default();
    commit_query(&g, "CREATE (:A:B)");
    assert_eq!(labels(&commit_query(&g, "MATCH (n:A) REMOVE n:A")), (0, 1));
    let (stats, _) = probe(&g, "CREATE (:A)");
    assert_eq!(labels(&stats), (1, 0));
    let (stats, _) = probe(&g, "MATCH (n:B) SET n:A");
    assert_eq!(labels(&stats), (1, 0));
}

#[test]
fn parity_with_the_census() {
    let g = Graph::default();
    seed(&g, "A", 2);
    seed(&g, "B", 1);
    let cases: [(&str, (i64, i64)); 11] = [
        ("CREATE (:New)", (1, 0)),
        ("CREATE (:A)", (0, 0)),
        ("CREATE (:New), (:New)", (1, 0)),
        ("CREATE (n:New) DELETE n", (0, 0)),
        ("MATCH (n:A) WITH n LIMIT 1 DELETE n", (0, 0)),
        ("MATCH (n:A) DELETE n", (0, 1)),
        ("MATCH (n:B) DELETE n", (0, 1)),
        ("MATCH (n:B) SET n:C", (1, 0)),
        ("MATCH (n:B) REMOVE n:B", (0, 1)),
        ("MATCH (n:A) DELETE n CREATE (:A)", (0, 0)),
        ("MATCH (n:B) DELETE n CREATE (:B)", (0, 0)),
    ];
    for (q, want) in cases {
        assert_eq!(labels(&probe(&g, q).0), want, "{q}");
    }
}

/// A scratch keeps answering at its own `snapshot_ts`, as an explicit
/// transaction does while other commits land (ADR-0171 rule 8).
#[test]
fn old_snapshot_ignores_later_commits() {
    let g = Graph::default();
    seed(&g, "Q", 1);
    let ts0 = g.commit_ts();
    commit_query(&g, "CREATE (:P)");
    commit_query(&g, "MATCH (n:Q) DELETE n");
    // `P` exists now but not at ts0; `Q` is gone now but was there at ts0.
    assert_eq!(labels(&probe_at(&g, ts0, "CREATE (:P)").0), (1, 0));
    assert_eq!(labels(&probe_at(&g, ts0, "CREATE (:Q)").0), (0, 0));
    assert_eq!(labels(&probe(&g, "CREATE (:P)").0), (0, 0));
    assert_eq!(labels(&probe(&g, "CREATE (:Q)").0), (1, 0));
}

/// `restore_scratch` (CALL … IN TRANSACTIONS) shares the memo, so a
/// restore keeps what the failed batch learned about the snapshot.
#[test]
fn restored_scratch_keeps_the_memo() {
    let g = Graph::default();
    seed(&g, "Seed", 100);
    let scratch = write_scratch(&g);
    let before = scratch.borrow().clone();
    take_label_probe_checks();
    let stats = exec_write(&g, &scratch, "CREATE (:Seed)").unwrap().stats;
    assert_eq!(labels(&stats), (0, 0));
    assert_eq!(take_label_probe_checks(), 1);
    *scratch.borrow_mut() = before;
    let stats = exec_write(&g, &scratch, "CREATE (:Seed)").unwrap().stats;
    assert_eq!(labels(&stats), (0, 0));
    assert_eq!(take_label_probe_checks(), 0);
}

/// Probes on an old snapshot run while another thread applies commits that
/// add the same label (`index_add` holds `nodes` then takes `label_index`).
/// The probe never nests the two guards, so this cannot deadlock, and every
/// answer stays at the snapshot.
#[test]
fn probes_overlap_concurrent_applies() {
    let g = Graph::default();
    seed(&g, "L", 2);
    let ts0 = g.commit_ts();
    let first_new = g.next_node_id();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..300 {
                let op = RedoOp::create_node(first_new + i, vec!["L".into()], BTreeMap::new());
                g.apply_redo(vec![op], g.commit_ts() + 1).unwrap();
            }
        });
        for _ in 0..300 {
            let (stats, _) = probe_at(&g, ts0, "MATCH (n:L) WHERE id(n) = 0 DELETE n");
            assert_eq!(labels(&stats), (0, 0));
            let (stats, _) = probe_at(&g, ts0, "MATCH (n:L) DELETE n");
            assert_eq!((stats.nodes_deleted, labels(&stats)), (2, (0, 1)));
        }
    });
}
