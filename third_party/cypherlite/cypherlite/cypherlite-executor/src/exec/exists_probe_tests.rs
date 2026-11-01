use std::collections::{BTreeMap, HashSet};

use cypherlite_core::NodeId;

use crate::graph::Graph;

use super::test_support::exec_cypher;
use super::*;

fn person(g: &Graph, id: Option<i64>) -> NodeId {
    let mut props = BTreeMap::new();
    if let Some(id) = id {
        props.insert("id".into(), Value::Int(id));
    }
    g.create_node(vec!["Person".into()], props).unwrap()
}

fn org(g: &Graph, name: &str, kind: &str) -> NodeId {
    g.create_node(
        vec!["Org".into()],
        BTreeMap::from([
            ("name".into(), Value::String(name.into())),
            ("kind".into(), Value::String(kind.into())),
        ]),
    )
    .unwrap()
}

/// Residual `WHERE` drops hops, nulls do not enter `COUNT(DISTINCT ...)`,
/// and `ORDER BY ... LIMIT 1` keeps the winning group. The grouped result
/// matches a full scan of the same pattern.
#[test]
fn filtered_count_distinct_matches_scan_and_limit() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let alpha = org(&g, "Alpha", "company");
    let beta = org(&g, "Beta", "lab");
    let gamma = org(&g, "Gamma", "company");
    let quiet = org(&g, "Quiet", "company");
    for (person_id, org_id) in [
        (Some(1), alpha),
        (Some(2), alpha),
        (Some(1), alpha),
        (None, alpha),
        (Some(9), beta),
        (Some(3), gamma),
        (None, quiet),
    ] {
        let p = person(&g, person_id);
        g.create_rel(p, org_id, "AT".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();

    let scanned = exec_cypher(
        &g,
        "MATCH (p:Person)-[:AT]->(o:Org) WHERE o.kind <> 'lab' \
         RETURN p.id AS id, o.name AS name",
    );
    let mut expected: BTreeMap<String, HashSet<i64>> = BTreeMap::new();
    for row in &scanned.rows {
        let name = row[1].as_string().unwrap().to_string();
        let entry = expected.entry(name).or_default();
        if let Value::Int(id) = row[0] {
            entry.insert(id);
        }
    }
    assert_eq!(
        expected.keys().cloned().collect::<Vec<_>>(),
        vec![
            "Alpha".to_string(),
            "Gamma".to_string(),
            "Quiet".to_string()
        ]
    );
    assert_eq!(expected["Alpha"].len(), 2);
    assert_eq!(expected["Gamma"].len(), 1);
    assert!(expected["Quiet"].is_empty());

    let folded = exec_cypher(
        &g,
        "MATCH (p:Person)-[:AT]->(o:Org) WHERE o.kind <> 'lab' \
         RETURN count(DISTINCT p.id) AS num, o.name AS name",
    );
    let mut got: Vec<(String, i64)> = folded
        .rows
        .iter()
        .map(|row| {
            (
                row[1].as_string().unwrap().to_string(),
                row[0].as_int().unwrap(),
            )
        })
        .collect();
    got.sort();
    let mut want: Vec<(String, i64)> = expected
        .iter()
        .map(|(name, ids)| (name.clone(), ids.len() as i64))
        .collect();
    want.sort();
    assert_eq!(got, want);

    let top = exec_cypher(
        &g,
        "MATCH (p:Person)-[:AT]->(o:Org) WHERE o.kind <> 'lab' \
         RETURN count(DISTINCT p.id) AS num, o.name AS name \
         ORDER BY num DESC LIMIT 1",
    );
    assert_eq!(top.rows.len(), 1);
    assert_eq!(top.rows[0][0], Value::Int(2));
    assert_eq!(top.rows[0][1], Value::String("Alpha".into()));
}

fn comment_reply_graph(connect: bool, comment_id: Option<i64>, decoys: i64) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let author = person(&g, Some(7));
    let other = person(&g, Some(8));
    let post = g.create_node(vec!["Post".into()], BTreeMap::new()).unwrap();
    g.create_rel(
        post,
        if connect { author } else { other },
        "BY".into(),
        BTreeMap::new(),
    )
    .unwrap();
    let mut props = BTreeMap::new();
    if let Some(id) = comment_id {
        props.insert("id".into(), Value::Int(id));
    }
    let comment = g.create_node(vec!["Comment".into()], props).unwrap();
    g.create_rel(comment, author, "CREATED".into(), BTreeMap::new())
        .unwrap();
    for _ in 0..decoys {
        let extra = g.create_node(vec!["Post".into()], BTreeMap::new()).unwrap();
        g.create_rel(extra, other, "BY".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(comment, extra, "REPLY".into(), BTreeMap::new())
            .unwrap();
    }
    // Typed adjacency walks newest edges first, so the qualifying reply
    // is the first one the probe sees.
    g.create_rel(comment, post, "REPLY".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();
    g
}

#[test]
fn boolean_count_stops_at_first_qualifying_binding() {
    let g = comment_reply_graph(true, Some(1), 40);
    let scanned = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:CREATED]->(p:Person) \
         MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         RETURN c.id AS id",
    );
    let expected = scanned.rows.iter().any(|row| !row[0].is_null());
    assert!(expected);

    let _ = take_neighbor_walks();
    let folded = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:CREATED]->(p:Person) \
         MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         RETURN count(DISTINCT c.id) > 0 AS hit",
    );
    let walks = take_neighbor_walks();
    assert_eq!(folded.rows, vec![vec![Value::Bool(true)]]);
    assert!(
        walks < 10,
        "existence probe kept walking after the first hit ({walks} walks)"
    );

    let at_least = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:CREATED]->(p:Person) \
         MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         RETURN count(DISTINCT c.id) >= 1 AS hit",
    );
    assert_eq!(at_least.rows, vec![vec![Value::Bool(true)]]);

    let exists = exec_cypher(
        &g,
        "MATCH (c:Comment) \
         RETURN EXISTS { \
           MATCH (c)-[:CREATED]->(p:Person), (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         } AS hit",
    );
    assert_eq!(exists.rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn boolean_count_is_false_when_nothing_qualifies() {
    let g = comment_reply_graph(false, Some(1), 5);
    let folded = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:CREATED]->(p:Person) \
         MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         RETURN count(DISTINCT c.id) > 0 AS hit",
    );
    assert_eq!(folded.rows, vec![vec![Value::Bool(false)]]);
    let exists = exec_cypher(
        &g,
        "MATCH (c:Comment) \
         RETURN EXISTS { \
           MATCH (c)-[:CREATED]->(p:Person) \
           MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         } AS hit",
    );
    assert_eq!(exists.rows, vec![vec![Value::Bool(false)]]);
}

#[test]
fn null_distinct_value_does_not_satisfy_boolean_count() {
    let g = comment_reply_graph(true, None, 0);
    let folded = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:CREATED]->(p:Person) \
         MATCH (c)-[:REPLY]->(:Post)-[:BY]->(p) \
         RETURN count(DISTINCT c.id) > 0 AS hit",
    );
    assert_eq!(folded.rows, vec![vec![Value::Bool(false)]]);
}
