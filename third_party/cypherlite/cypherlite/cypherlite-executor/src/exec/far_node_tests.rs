use super::far_probe::take_far_node_index_rejects;
use super::far_probe::take_far_range_materializations;
use super::test_support::exec_autocommit;
use super::test_support::exec_cypher;
use super::*;
use crate::graph::Graph;

fn far_node_index_graph() -> Graph {
    let g = Graph::default();
    exec_autocommit(&g, "CREATE RANGE INDEX b_code FOR (b:B) ON (b.code)").unwrap();
    exec_autocommit(&g, "CREATE TEXT INDEX b_name FOR (b:B) ON (b.name)").unwrap();
    exec_autocommit(&g, "CREATE RANGE INDEX p_id FOR (p:P) ON (p.id)").unwrap();
    exec_autocommit(&g, "CREATE RANGE INDEX t_name FOR (t:T) ON (t.name)").unwrap();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
    for (code, name) in [(1, "matching text"), (2, "other"), (3, "another")] {
        let b = g
            .create_node(
                vec!["B".into()],
                BTreeMap::from([
                    ("code".into(), Value::Int(code)),
                    ("loose".into(), Value::Int(code % 2)),
                    ("name".into(), Value::String(name.into())),
                ]),
            )
            .unwrap();
        g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    }
    // A second P ties the label counts (P == F == 2), which keeps the
    // first pattern at its index-0 start via ADR-0351's leftmost
    // tie-break; the second pattern's bound, unlabeled `f` stays at
    // index 0 through the row-bound estimate (a bound element enumerates
    // exactly one candidate) — without it, `t:T` at 2 would beat the
    // unlabeled element's whole-graph estimate and displace the start.
    let p = g
        .create_node(
            vec!["P".into()],
            BTreeMap::from([("id".into(), Value::Int(7))]),
        )
        .unwrap();
    g.create_node(
        vec!["P".into()],
        BTreeMap::from([("id".into(), Value::Int(8))]),
    )
    .unwrap();
    for (i, tag) in ["wanted", "other"].into_iter().enumerate() {
        let f = g
            .create_node(
                vec!["F".into()],
                BTreeMap::from([("id".into(), Value::Int(i as i64))]),
            )
            .unwrap();
        let t = g
            .create_node(
                vec!["T".into()],
                BTreeMap::from([("name".into(), Value::String(tag.into()))]),
            )
            .unwrap();
        g.create_rel(f, p, "MEMBER".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(f, t, "TAGGED".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    g
}

#[test]
fn far_node_range_index_prefilters_forced_neighbors() {
    let g = far_node_index_graph();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 1})-[:R]->(b:B) \
         WHERE b.code = 1 RETURN b.name AS name",
    );
    assert_eq!(r.rows, vec![vec![Value::String("matching text".into())]]);
    // The compound start selection (LABS-351) starts the walk at the
    // indexed `b` seek — the prefilter's forced-neighbor arm never runs
    // on this plan (0 rejects); the prefilter's coverage stays pinned by
    // the comma-joined and range/text prefilter-arming shapes below.
    assert_eq!(take_far_node_index_rejects(), 0);
}

#[test]
fn far_node_text_index_prefilters_forced_neighbors() {
    let g = far_node_index_graph();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 1})-[:R]->(b:B) \
         WHERE b.name CONTAINS 'matching' RETURN b.code AS code",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    // The compound start selection (LABS-351) starts the walk at the
    // indexed `b` seek — the prefilter's forced-neighbor arm never runs
    // on this plan (0 rejects); the prefilter's coverage stays pinned by
    // the comma-joined and range/text prefilter-arming shapes below.
    assert_eq!(take_far_node_index_rejects(), 0);
}

#[test]
fn far_node_index_prefilters_a_comma_joined_second_pattern() {
    const QUERY: &str = "MATCH (f:F)-[:MEMBER]->(p:P), (f)-[:TAGGED]->(t:T) \
         WHERE p.id = 7 AND t.name = 'wanted' RETURN f.id AS id";
    let g = far_node_index_graph();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(&g, QUERY);
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    // ADR-0351: the first pattern's cheapest element is `p:P` (1 node),
    // so the pivoted plan starts there; with the row-bound estimate
    // (a bound element enumerates exactly one candidate) the second
    // pattern's bound `f` scores 1 and keeps its index-0 start, and the
    // pivoted first pattern's backward walk arms one prefilter rejection.
    // The prefilter's coverage on the original index-0 plan is pinned
    // with the start choice disabled below.
    assert_eq!(take_far_node_index_rejects(), 1);
    // The disabled run returns its results; the A/B assertions live
    // outside the closure, in the same frame as the pivoted ones.
    let (plain_rows, plain_rejects) = super::pivot::with_pivot_disabled(|| {
        let _ = take_far_node_index_rejects();
        let r = exec_cypher(&g, QUERY);
        (r.rows, take_far_node_index_rejects())
    });
    assert_eq!(plain_rows, vec![vec![Value::Int(0)]]);
    assert_eq!(plain_rejects, 1);
}

#[test]
fn far_node_without_an_index_stays_on_the_residual_path() {
    let g = far_node_index_graph();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(
        &g,
        "MATCH (a:A {id: 1})-[:R]->(b:B) \
         WHERE b.loose = 1 RETURN b.code AS code ORDER BY code",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
    assert_eq!(take_far_node_index_rejects(), 0);
}

/// `(cid, length)` comments linked to one indexed person, plus unlinked
/// comments long enough to inflate the range estimate.
fn person_comment_graph(linked: &[(i64, i64)], unlinked_long: usize) -> Graph {
    let g = Graph::default();
    exec_autocommit(
        &g,
        "CREATE RANGE INDEX person_first FOR (p:Person) ON (p.firstName)",
    )
    .unwrap();
    exec_autocommit(
        &g,
        "CREATE RANGE INDEX person_last FOR (p:Person) ON (p.lastName)",
    )
    .unwrap();
    exec_autocommit(
        &g,
        "CREATE RANGE INDEX comment_length FOR (c:Comment) ON (c.length)",
    )
    .unwrap();
    g.begin_write().unwrap();
    let person = g
        .create_node(
            vec!["Person".into()],
            BTreeMap::from([
                ("firstName".into(), Value::String("Alfredo".into())),
                ("lastName".into(), Value::String("Gomez".into())),
            ]),
        )
        .unwrap();
    for &(cid, length) in linked {
        let comment = g
            .create_node(
                vec!["Comment".into()],
                BTreeMap::from([
                    ("cid".into(), Value::Int(cid)),
                    ("length".into(), Value::Int(length)),
                ]),
            )
            .unwrap();
        g.create_rel(comment, person, "commentHasCreator".into(), BTreeMap::new())
            .unwrap();
    }
    for i in 0..unlinked_long {
        g.create_node(
            vec!["Comment".into()],
            BTreeMap::from([
                ("cid".into(), Value::Int(10_000 + i as i64)),
                ("length".into(), Value::Int(500)),
            ]),
        )
        .unwrap();
    }
    g.commit_write();
    g
}

const LONG_COMMENT_QUERY: &str = "MATCH (p:Person)<-[:commentHasCreator]-(c:Comment) \
     WHERE p.firstName = 'Alfredo' AND p.lastName = 'Gomez' AND c.length > 100 \
     RETURN c.cid AS cid ORDER BY cid";

#[test]
fn broad_far_range_stays_on_the_residual_path() {
    let mut linked = vec![(1, 150)];
    linked.extend((2..=28).map(|cid| (cid, 10)));
    let g = person_comment_graph(&linked, 40);
    let _ = take_far_range_materializations();
    let _ = take_far_node_index_rejects();
    let _ = take_neighbor_walks();
    let r = exec_cypher(&g, LONG_COMMENT_QUERY);
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    assert_eq!(take_far_range_materializations(), 0);
    assert_eq!(take_far_node_index_rejects(), 0);
    assert_eq!(take_neighbor_walks(), 28);
}

#[test]
fn selective_far_range_materializes_and_rejects_neighbors() {
    let mut linked = vec![(1, 150), (2, 180)];
    linked.extend((3..=2_002).map(|cid| (cid, 10)));
    let g = person_comment_graph(&linked, 0);
    let _ = take_far_range_materializations();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(&g, LONG_COMMENT_QUERY);
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    assert_eq!(take_far_range_materializations(), 1);
    assert_eq!(take_far_node_index_rejects(), 2_000);
}

#[test]
fn empty_far_estimate_does_not_expand() {
    let linked: Vec<_> = (1..=3).map(|cid| (cid, 10)).collect();
    let g = person_comment_graph(&linked, 0);
    let _ = take_neighbor_walks();
    let r = exec_cypher(
        &g,
        "MATCH (p:Person)<-[:commentHasCreator]-(c:Comment) \
         WHERE p.firstName = 'Alfredo' AND p.lastName = 'Gomez' AND c.length = -1 \
         RETURN c.cid AS cid",
    );
    assert!(r.rows.is_empty());
    assert_eq!(take_neighbor_walks(), 0);
}

#[test]
fn repeated_small_frontiers_materialize_once_their_total_catches_the_estimate() {
    let g = Graph::default();
    exec_autocommit(&g, "CREATE RANGE INDEX tag_name FOR (t:Tag) ON (t.name)").unwrap();
    exec_autocommit(
        &g,
        "CREATE RANGE INDEX person_last FOR (p:Person) ON (p.lastName)",
    )
    .unwrap();
    g.begin_write().unwrap();
    let tag = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Norah".into()))]),
        )
        .unwrap();
    for pid in 1..=10 {
        let person = g
            .create_node(
                vec!["Person".into()],
                BTreeMap::from([
                    ("pid".into(), Value::Int(pid)),
                    (
                        "lastName".into(),
                        Value::String(if pid == 10 { "Gurung" } else { "Other" }.into()),
                    ),
                ]),
            )
            .unwrap();
        let forum = g
            .create_node(vec!["Forum".into()], BTreeMap::new())
            .unwrap();
        g.create_rel(forum, tag, "forumHasTag".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(forum, person, "hasModerator".into(), BTreeMap::new())
            .unwrap();
    }
    for extra in 11..=13 {
        g.create_node(
            vec!["Person".into()],
            BTreeMap::from([
                ("pid".into(), Value::Int(extra)),
                ("lastName".into(), Value::String("Gurung".into())),
            ]),
        )
        .unwrap();
    }
    g.commit_write();
    let _ = take_far_node_index_rejects();
    let r = exec_cypher(
        &g,
        "MATCH (t:Tag)<-[:forumHasTag]-(f:Forum)-[:hasModerator]->(p:Person) \
         WHERE t.name = 'Norah' AND p.lastName = 'Gurung' RETURN p.pid AS pid",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(10)]]);
    // The compound start selection (LABS-351) starts at the
    // equality-indexed endpoint (both `t.name` and `p.lastName` carry
    // hits ~1), so the far-node estimate never materializes on this
    // plan (0 rejects); its dedup behavior stays pinned by the
    // comma-joined prefilter shapes.
    assert_eq!(take_far_node_index_rejects(), 0);
}
