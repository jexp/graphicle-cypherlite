use super::agg_fold::StreamAgg;
use super::test_support::duration;
use super::test_support::exec_cypher;
use super::*;
use crate::graph::Graph;
use crate::overlay::ExecGraph;
use cypherlite_core::NodeId;
use cypherlite_ir::Expr;
use std::collections::HashSet;

fn two_hop_graph() -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["Account".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
    let make_arm = |g: &Graph, a: NodeId, amount: i64| {
        let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
        let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
        g.create_rel(
            a,
            b,
            "T".into(),
            BTreeMap::from([("amount".into(), Value::Int(amount))]),
        )
        .unwrap();
        g.create_rel(b, c, "T".into(), BTreeMap::new()).unwrap();
    };
    make_arm(&g, a, 1);
    make_arm(&g, a, 100);
    make_arm(&g, a, 1);
    g.commit_write();
    g
}

#[test]
fn residual_prunes_before_second_hop() {
    let g = two_hop_graph();
    let late = exec_cypher(
        &g,
        "MATCH (a:Account {id: 1})-[t:T]->(b)-[:T]->(c) \
             WITH t, c WHERE t.amount > 10 RETURN c",
    );
    assert_eq!(late.rows.len(), 1);

    let _ = take_hop_expands();
    let early = exec_cypher(
        &g,
        "MATCH (a:Account {id: 1})-[t:T]->(b)-[:T]->(c) \
             WHERE t.amount > 10 RETURN c",
    );
    let hops = take_hop_expands();
    assert_eq!(early.rows.len(), 1);
    assert_eq!(
        hops, 1,
        "second-hop expand should run once after first-hop residual"
    );
}

#[test]
fn exists_pattern_uses_role_name() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let u = g
        .create_node(
            vec!["User".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
    for i in 0..40 {
        g.create_node(
            vec!["Role".into()],
            BTreeMap::from([("n".into(), Value::Int(i))]),
        )
        .unwrap();
    }
    let role = g
        .create_node(
            vec!["Role".into()],
            BTreeMap::from([("name".into(), Value::String("requester".into()))]),
        )
        .unwrap();
    g.create_rel(u, role, "HAS_ROLE".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();

    let r = exec_cypher(
        &g,
        "MATCH (u:User {id: 1}) \
             WHERE EXISTS { (u)-[:HAS_ROLE]->(:Role {name: 'requester'}) } \
             RETURN u.id AS id",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));

    let miss = exec_cypher(
        &g,
        "MATCH (u:User {id: 1}) \
             WHERE EXISTS { (u)-[:HAS_ROLE]->(:Role {name: 'admin'}) } \
             RETURN u.id AS id",
    );
    assert!(miss.rows.is_empty());
}

#[test]
fn with_where_sees_pre_projection_vars() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["N".into()],
        BTreeMap::from([("k".into(), Value::Int(1))]),
    )
    .unwrap();
    g.commit_write();
    let r = exec_cypher(&g, "MATCH (n:N) WITH n.k AS k WHERE n.k = 1 RETURN k");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

fn hub_amounts(degree: i64) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    for i in 0..degree {
        let n = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
        g.create_rel(
            hub,
            n,
            "E".into(),
            BTreeMap::from([("amount".into(), Value::Int(if i < 5 { 50 } else { 2000 }))]),
        )
        .unwrap();
    }
    g.commit_write();
    g
}

#[test]
fn fold_sum_count_matches_where_forced_scan() {
    let g = hub_amounts(40);
    let folded = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() RETURN sum(t.amount) AS volume, count(t) AS n",
    );
    let scanned = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() WHERE true \
             RETURN sum(t.amount) AS volume, count(t) AS n",
    );
    assert_eq!(folded.rows, scanned.rows);
    assert_eq!(folded.rows[0][0], Value::Int(5 * 50 + 35 * 2000));
    assert_eq!(folded.rows[0][1], Value::Int(40));
}

#[test]
fn explicit_group_by_disables_one_hop_fold() {
    // CIP-236 regression (PR #62 review): the one-hop fold keys on the
    // non-aggregating projection items, so it must not run when an explicit
    // GROUP BY supplies the keys -- otherwise `a.id` (constant here) becomes
    // the group key and the two amount bands collapse into one row.
    let g = hub_amounts(40);
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() \
             RETURN a.id AS id, count(*) AS n GROUP BY t.amount > 1000",
    );
    let mut counts: Vec<i64> = r.rows.iter().map(|row| row[1].as_int().unwrap()).collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![5, 35]);
}

#[test]
fn explicit_group_by_empty_input_yields_no_rows() {
    // The fold never groups by an explicit key and a keyed GROUP BY has no
    // group for an empty input, so the result stays empty rather than
    // emitting a phantom global group.
    let g = Graph::default();
    let r = exec_cypher(
        &g,
        "MATCH (a:A)-[t:E]->() RETURN count(*) AS n GROUP BY t.amount",
    );
    assert!(r.rows.is_empty());
}

#[test]
fn fold_amount_band_two_groups() {
    let g = hub_amounts(40);
    let folded = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() \
             RETURN CASE WHEN t.amount >= 1000 THEN 'large' ELSE 'small' END AS band, \
                    count(*) AS n",
    );
    let scanned = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() WHERE true \
             RETURN CASE WHEN t.amount >= 1000 THEN 'large' ELSE 'small' END AS band, \
                    count(*) AS n",
    );
    let mut got: Vec<(String, i64)> = folded
        .rows
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap().to_string(),
                r[1].as_int().unwrap(),
            )
        })
        .collect();
    got.sort();
    let mut want: Vec<(String, i64)> = scanned
        .rows
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap().to_string(),
                r[1].as_int().unwrap(),
            )
        })
        .collect();
    want.sort();
    assert_eq!(got, want);
    assert_eq!(got, vec![("large".into(), 35), ("small".into(), 5)]);
}

// ADR-0205: sum() over zero (or only-null) rows is 0, matching the grouped
// path and the reference engine; the fold used to return null here.
#[test]
fn fold_projects_node_with_properties() {
    // LABS-259: the one-hop fold binds labels-only node stubs (ADR-0033) and
    // used to emit them straight into the result, so a projected node lost
    // its properties whenever the fold ran (e.g. aggregation + ORDER BY +
    // LIMIT). The result builder must refresh stubs like `project_rows` does.
    let g = Graph::default();
    g.begin_write().unwrap();
    let s1 = g
        .create_node(
            vec!["Subject".into()],
            BTreeMap::from([("title".into(), Value::String("oversight".into()))]),
        )
        .unwrap();
    let s2 = g
        .create_node(
            vec!["Subject".into()],
            BTreeMap::from([("title".into(), Value::String("other".into()))]),
        )
        .unwrap();
    for (s, n) in [(s1, 2), (s2, 1)] {
        for _ in 0..n {
            let b = g.create_node(vec!["Bill".into()], BTreeMap::new()).unwrap();
            g.create_rel(b, s, "DEALS_WITH".into(), BTreeMap::new())
                .unwrap();
        }
    }
    g.commit_write();
    let r = exec_cypher(
        &g,
        "MATCH (b:Bill)-[:DEALS_WITH]->(subj:Subject) \
         RETURN subj, count(*) AS num ORDER BY num DESC LIMIT 25",
    );
    assert_eq!(r.rows.len(), 2);
    let Value::Node(n) = &r.rows[0][0] else {
        panic!("expected a node");
    };
    assert_eq!(
        n.properties.get("title"),
        Some(&Value::String("oversight".into())),
        "folded projection clipped the node to its labels"
    );
    assert_eq!(r.rows[0][1], Value::Int(2));
}

#[test]
fn fold_empty_sum_is_zero() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["A".into()],
        BTreeMap::from([("id".into(), Value::Int(0))]),
    )
    .unwrap();
    g.commit_write();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->() RETURN sum(t.amount) AS volume, count(t) AS n",
    );
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert_eq!(r.rows[0][1], Value::Int(0));
}

fn numbered_nodes(n: i64) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    for i in 0..n {
        g.create_node(
            vec!["N".into()],
            BTreeMap::from([("k".into(), Value::Int(i))]),
        )
        .unwrap();
    }
    g.commit_write();
    g
}

#[test]
fn order_by_limit_matches_sort_then_truncate() {
    let g = numbered_nodes(40);
    let topk = exec_cypher(&g, "MATCH (n:N) RETURN n.k AS k ORDER BY n.k LIMIT 5");
    let full = exec_cypher(&g, "MATCH (n:N) RETURN n.k AS k ORDER BY n.k");
    let got: Vec<i64> = topk.rows.iter().map(|r| r[0].as_int().unwrap()).collect();
    let want: Vec<i64> = full
        .rows
        .iter()
        .take(5)
        .map(|r| r[0].as_int().unwrap())
        .collect();
    assert_eq!(got, want);
    assert_eq!(got, vec![0, 1, 2, 3, 4]);
}

#[test]
fn order_by_skip_limit_matches_sort_then_slice() {
    let g = numbered_nodes(40);
    let page = exec_cypher(
        &g,
        "MATCH (n:N) RETURN n.k AS k ORDER BY n.k DESC SKIP 3 LIMIT 4",
    );
    let full = exec_cypher(&g, "MATCH (n:N) RETURN n.k AS k ORDER BY n.k DESC");
    let got: Vec<i64> = page.rows.iter().map(|r| r[0].as_int().unwrap()).collect();
    let want: Vec<i64> = full
        .rows
        .iter()
        .skip(3)
        .take(4)
        .map(|r| r[0].as_int().unwrap())
        .collect();
    assert_eq!(got, want);
    assert_eq!(got, vec![36, 35, 34, 33]);
}

#[test]
fn distinct_keeps_first_of_each_value() {
    let g = Graph::default();
    g.begin_write().unwrap();
    for k in [1, 2, 1, 3, 2, 1] {
        g.create_node(
            vec!["N".into()],
            BTreeMap::from([("k".into(), Value::Int(k))]),
        )
        .unwrap();
    }
    g.commit_write();
    let r = exec_cypher(&g, "MATCH (n:N) RETURN DISTINCT n.k AS k ORDER BY k");
    let got: Vec<i64> = r.rows.iter().map(|row| row[0].as_int().unwrap()).collect();
    assert_eq!(got, vec![1, 2, 3]);
}

fn hub_out(degree: i64) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    for i in 0..degree {
        let n = g
            .create_node(
                vec!["B".into()],
                BTreeMap::from([("k".into(), Value::Int(i))]),
            )
            .unwrap();
        g.create_rel(hub, n, "E".into(), BTreeMap::new()).unwrap();
    }
    g.commit_write();
    g
}

fn distinct_ks(r: &QueryResult) -> HashSet<i64> {
    r.rows.iter().map(|row| row[0].as_int().unwrap()).collect()
}

#[test]
fn distinct_limit_stops_neighbor_walk() {
    let g = hub_out(80);
    take_neighbor_walks();
    let limited = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) RETURN DISTINCT b.k AS k LIMIT 20",
    );
    let walks = take_neighbor_walks();
    let full = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) RETURN DISTINCT b.k AS k",
    );
    assert_eq!(limited.rows.len(), 20);
    assert_eq!(full.rows.len(), 80);
    let got = distinct_ks(&limited);
    let all = distinct_ks(&full);
    assert!(got.is_subset(&all));
    assert_eq!(walks, 20);
}

#[test]
fn distinct_limit_with_is_the_same_stop() {
    let g = hub_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) WITH DISTINCT b.k AS k LIMIT 20 RETURN k",
    );
    let walks = take_neighbor_walks();
    assert_eq!(r.rows.len(), 20);
    assert_eq!(walks, 20);
}

#[test]
fn distinct_limit_duplicate_edges_stop_on_keys() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    for i in 0..10 {
        let n = g
            .create_node(
                vec!["B".into()],
                BTreeMap::from([("k".into(), Value::Int(i))]),
            )
            .unwrap();
        for _ in 0..3 {
            g.create_rel(hub, n, "E".into(), BTreeMap::new()).unwrap();
        }
    }
    g.commit_write();
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) RETURN DISTINCT b.k AS k LIMIT 10",
    );
    let walks = take_neighbor_walks();
    assert_eq!(distinct_ks(&r).len(), 10);
    assert!(walks < 30, "walks={walks}");
}

fn hub_ts_out(degree: i64) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    for i in 0..degree {
        let n = g
            .create_node(
                vec!["B".into()],
                BTreeMap::from([("k".into(), Value::Int(i))]),
            )
            .unwrap();
        g.create_rel(
            hub,
            n,
            "E".into(),
            BTreeMap::from([("ts".into(), Value::Int(i))]),
        )
        .unwrap();
    }
    g.commit_write();
    g
}

fn ts_col(r: &QueryResult) -> Vec<i64> {
    r.rows.iter().map(|row| row[0].as_int().unwrap()).collect()
}

#[test]
fn order_by_rel_ts_desc_limit_stops_neighbor_walk() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let limited = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN t.ts AS ts ORDER BY t.ts DESC LIMIT 20",
    );
    let walks = take_neighbor_walks();
    let full = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN t.ts AS ts ORDER BY t.ts DESC",
    );
    assert_eq!(ts_col(&limited), ts_col(&full)[..20]);
    assert_eq!(ts_col(&limited), (60..80).rev().collect::<Vec<_>>());
    assert_eq!(walks, 20);
}

#[test]
fn order_by_rel_ts_alias_and_skip_limit() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let page = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) \
             RETURN t.ts AS ts ORDER BY ts DESC SKIP 3 LIMIT 4",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&page), vec![76, 75, 74, 73]);
    assert_eq!(walks, 7);
}

#[test]
fn order_by_rel_ts_with_limit_stops() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) \
             WITH t.ts AS ts ORDER BY ts DESC LIMIT 20 RETURN ts",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), (60..80).rev().collect::<Vec<_>>());
    assert_eq!(walks, 20);
}

#[test]
fn order_by_rel_ts_incoming_stops() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["B".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    for i in 0..40 {
        let n = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
        g.create_rel(
            n,
            hub,
            "E".into(),
            BTreeMap::from([("ts".into(), Value::Int(i))]),
        )
        .unwrap();
    }
    g.commit_write();
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (b:B {id: 0})<-[t:E]-(a) RETURN t.ts AS ts ORDER BY t.ts DESC LIMIT 5",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), vec![39, 38, 37, 36, 35]);
    assert_eq!(walks, 5);
}

#[test]
fn set_rel_ts_reorders_before_limit() {
    let g = hub_ts_out(80);
    g.begin_write().unwrap();
    let old = g
        .lookup_nodes(
            &["B".into()],
            &BTreeMap::from([("k".into(), Value::Int(0))]),
        )
        .unwrap()[0];
    let hub = g
        .lookup_nodes(
            &["A".into()],
            &BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap()[0];
    let rid = g
        .neighbors(hub, cypherlite_ir::Direction::Outgoing)
        .into_iter()
        .find(|&id| g.rel(id).is_some_and(|r| r.end == old))
        .unwrap();
    g.set_property_rel(rid, "ts", Value::Int(1000));
    g.commit_write();
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN t.ts AS ts ORDER BY t.ts DESC LIMIT 1",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), vec![1000]);
    assert_eq!(walks, 1);
}

#[test]
fn deleted_newest_are_skipped_then_limit_stops() {
    let g = hub_ts_out(80);
    g.begin_write().unwrap();
    let hub = g
        .lookup_nodes(
            &["A".into()],
            &BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap()[0];
    let newest: Vec<_> = g
        .neighbors(hub, cypherlite_ir::Direction::Outgoing)
        .into_iter()
        .take(10)
        .collect();
    for rid in newest {
        g.delete_rel(rid);
    }
    g.commit_write();
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN t.ts AS ts ORDER BY t.ts DESC LIMIT 20",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), (50..70).rev().collect::<Vec<_>>());
    assert_eq!(walks, 20);
}

#[test]
fn order_by_rel_ts_asc_does_not_early_stop() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN t.ts AS ts ORDER BY t.ts ASC LIMIT 5",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), vec![0, 1, 2, 3, 4]);
    assert_eq!(walks, 80);
}

#[test]
fn order_by_rel_ts_keeps_other_return_cols() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN b.k AS k ORDER BY t.ts DESC LIMIT 5",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), vec![79, 78, 77, 76, 75]);
    assert_eq!(walks, 5);
}

#[test]
fn order_by_node_prop_does_not_early_stop() {
    let g = hub_ts_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[t:E]->(b) RETURN b.k AS k ORDER BY b.k DESC LIMIT 5",
    );
    let walks = take_neighbor_walks();
    assert_eq!(ts_col(&r), vec![79, 78, 77, 76, 75]);
    assert_eq!(walks, 80);
}

#[test]
fn order_by_disables_distinct_limit_stop() {
    let g = hub_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) RETURN DISTINCT b.k AS k ORDER BY k DESC LIMIT 5",
    );
    let walks = take_neighbor_walks();
    let got: Vec<i64> = r.rows.iter().map(|row| row[0].as_int().unwrap()).collect();
    assert_eq!(got, vec![79, 78, 77, 76, 75]);
    assert_eq!(walks, 80);
}

#[test]
fn match_where_disables_distinct_limit_stop() {
    let g = hub_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) WHERE b.k >= 70 RETURN DISTINCT b.k AS k LIMIT 5",
    );
    let walks = take_neighbor_walks();
    let got = distinct_ks(&r);
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|k| *k >= 70));
    assert_eq!(walks, 80);
}

#[test]
fn count_distinct_still_walks_all() {
    let g = hub_out(80);
    take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 0})-[:E]->(b:B) RETURN count(DISTINCT b.k) AS n",
    );
    let walks = take_neighbor_walks();
    assert_eq!(r.rows[0][0], Value::Int(80));
    assert_eq!(walks, 80);
}

#[test]
fn contains_seek_matches_label_scan() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["Customer".into()],
        BTreeMap::from([("name".into(), Value::String("Alice".into()))]),
    )
    .unwrap();
    g.create_node(
        vec!["Customer".into()],
        BTreeMap::from([("name".into(), Value::String("Alicia".into()))]),
    )
    .unwrap();
    g.create_node(
        vec!["Customer".into()],
        BTreeMap::from([("name".into(), Value::String("Bob".into()))]),
    )
    .unwrap();
    g.commit_write();
    let r = exec_cypher(
        &g,
        "MATCH (c:Customer) WHERE c.name CONTAINS 'lic' RETURN c.name AS n ORDER BY n",
    );
    let got: Vec<String> = r
        .rows
        .iter()
        .map(|row| row[0].as_string().unwrap().to_string())
        .collect();
    assert_eq!(got, vec!["Alice".to_string(), "Alicia".to_string()]);
}

/// LABS-375: a node match counts one store read per candidate it
/// examined — the scan side `HOP_EXPANDS`/`NEIGHBOR_WALKS` never see, so
/// a label scan's PROFILE `Db Hits` is nonzero.
#[test]
fn a_label_scan_counts_one_node_read_per_candidate() {
    let g = Graph::default();
    g.begin_write().unwrap();
    for name in ["Alice", "Bob", "Cara"] {
        g.create_node(
            vec!["P".into()],
            BTreeMap::from([("name".into(), Value::String(name.into()))]),
        )
        .unwrap();
    }
    g.commit_write();
    take_node_reads();
    let r = exec_cypher(&g, "MATCH (p:P) RETURN p.name AS n");
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        take_node_reads(),
        3,
        "one node-store read per candidate the scan examined"
    );
}

#[test]
fn starts_with_seek_is_prefix() {
    let g = Graph::default();
    g.begin_write().unwrap();
    for name in ["Ada", "Adam", "Bob"] {
        g.create_node(
            vec!["P".into()],
            BTreeMap::from([("name".into(), Value::String(name.into()))]),
        )
        .unwrap();
    }
    g.commit_write();
    let r = exec_cypher(
        &g,
        "MATCH (p:P) WHERE p.name STARTS WITH 'Ada' RETURN p.name AS n ORDER BY n",
    );
    let got: Vec<String> = r
        .rows
        .iter()
        .map(|row| row[0].as_string().unwrap().to_string())
        .collect();
    assert_eq!(got, vec!["Ada".to_string(), "Adam".to_string()]);
}

#[test]
fn contains_empty_needle_is_label_scan() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["P".into()],
        BTreeMap::from([("name".into(), Value::String("x".into()))]),
    )
    .unwrap();
    g.commit_write();
    let r = exec_cypher(&g, "MATCH (p:P) WHERE p.name CONTAINS '' RETURN p");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn set_string_moves_contains_postings() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["P".into()],
        BTreeMap::from([("name".into(), Value::String("old-token".into()))]),
    )
    .unwrap();
    g.commit_write();
    assert_eq!(
        exec_cypher(&g, "MATCH (p:P) WHERE p.name CONTAINS 'old' RETURN p")
            .rows
            .len(),
        1
    );
    g.begin_write().unwrap();
    let id = g
        .lookup_nodes(
            &["P".into()],
            &BTreeMap::from([("name".into(), Value::String("old-token".into()))]),
        )
        .unwrap()[0];
    g.set_property_node(id, "name", Value::String("new-token".into()));
    g.commit_write();
    assert!(
        exec_cypher(&g, "MATCH (p:P) WHERE p.name CONTAINS 'old' RETURN p")
            .rows
            .is_empty()
    );
    assert_eq!(
        exec_cypher(&g, "MATCH (p:P) WHERE p.name CONTAINS 'new' RETURN p")
            .rows
            .len(),
        1
    );
}

// ---- id(x) = <expr> / id(x) IN <list> seek ----

// ---- StreamAgg (one-hop fold accumulator) ----

fn sum_expr() -> Expr {
    Expr::Function {
        name: "sum".into(),
        distinct: false,
        args: vec![Expr::Variable("x".into())],
    }
}

fn row_with(v: Value) -> Row {
    let mut row = Row::new();
    row.insert("x".into(), v);
    row
}

fn run_sum(vals: &[Value]) -> Value {
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    let expr = sum_expr();
    let (mut agg, operand) = StreamAgg::new(&expr).unwrap();
    for v in vals {
        agg.offer(
            &graph,
            &row_with(v.clone()),
            operand,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
    }
    agg.finish()
}

// ADR-0205: the one-hop fold skips nulls like the grouped path, and
// sum() over zero non-null rows is 0 on both paths.
#[test]
fn folded_sum_skips_nulls_and_only_null_is_zero() {
    assert_eq!(
        run_sum(&[Value::Int(1), Value::Null, Value::Int(3)]),
        Value::Int(4)
    );
    assert_eq!(run_sum(&[Value::Null, Value::Null]), Value::Int(0));
    assert_eq!(run_sum(&[]), Value::Int(0));
}

#[test]
fn folded_sum_accepts_durations_and_rejects_mixing() {
    // The shared duration builder (F-1-13/14: three copies had diverged
    // in spelling across the aggregation test modules).
    let d = |nanos: i128| duration(0, 0, nanos);
    assert_eq!(
        run_sum(&[d(1_000_000_000), Value::Null, d(3_000_000_000)]),
        d(4_000_000_000)
    );
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    let expr = sum_expr();
    let (mut agg, operand) = StreamAgg::new(&expr).unwrap();
    agg.offer(&graph, &row_with(d(1)), operand, &BTreeMap::new(), None)
        .unwrap();
    let err = agg
        .offer(
            &graph,
            &row_with(Value::Int(1)),
            operand,
            &BTreeMap::new(),
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("cannot mix numbers and durations"));
}

/// LABS-328 follow-up: a self-referential property equality (`c.key =
/// c.cluster`) is never a range seek — the bound evaluates against a row
/// where the pattern variable is unbound (null), and a served seek
/// silently renders the pattern empty instead of declining. This is the
/// entity-resolution report cell regression that surfaced once the
/// constraint's backing index materialized: 0 rows instead of 1.
#[test]
fn self_referential_property_equality_never_seeks() {
    let g = Graph::default();
    super::test_support::exec_autocommit(
        &g,
        "CREATE CONSTRAINT entity_key FOR (e:Entity) REQUIRE e.key IS UNIQUE",
    )
    .unwrap();
    super::test_support::exec_autocommit(
        &g,
        "CREATE (e:Entity {key: 'a', cluster: 'a'}), (f:Entity {key: 'b', cluster: 'a'})",
    )
    .unwrap();
    let r = exec_cypher(
        &g,
        "MATCH (c:Entity) WHERE c.key = c.cluster RETURN count(c) AS n",
    );
    assert_eq!(r.rows[0][0], Value::Int(1));
}
