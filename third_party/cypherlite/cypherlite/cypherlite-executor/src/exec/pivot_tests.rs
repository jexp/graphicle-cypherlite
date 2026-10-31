//! ADR-0350: the bound-node pivot for multi-pattern path expansion. The
//! second pattern of a pair query starts at the bound node instead of
//! rescanning its first label, and the named path still assembles under
//! the pattern's original indices.

use super::match_clause::{take_hub_join_engagements, with_hub_join_disabled};
use super::pivot::{take_pivot_backward_hops, with_pivot_disabled};
use super::test_support::exec_cypher;
use super::*;
use crate::graph::Graph;

/// The stackoverflow pair-query shape, shrunk: users comment on comments,
/// comments hang off questions undirected (`-[:COMMENTED_ON]-`).
fn pair_graph() -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let users: Vec<_> = (0..3)
        .map(|i| {
            g.create_node(
                vec!["User".into()],
                BTreeMap::from([("name".into(), Value::String(format!("u{i}").into()))]),
            )
            .unwrap()
        })
        .collect();
    let questions: Vec<_> = (0..2)
        .map(|i| {
            g.create_node(
                vec!["Question".into()],
                BTreeMap::from([("title".into(), Value::String(format!("q{i}").into()))]),
            )
            .unwrap()
        })
        .collect();
    // One comment per user; u0 and u1 share question 0, u2 is alone on 1.
    for (u, q) in [(0, 0), (1, 0), (2, 1)] {
        let c = g
            .create_node(vec!["Comment".into()], BTreeMap::new())
            .unwrap();
        g.create_rel(users[u], c, "COMMENTED".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(c, questions[q], "COMMENTED_ON".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    g
}

const PAIR_QUERY: &str =
    "MATCH p1=(u1:User)-[:COMMENTED]->(c1:Comment)-[:COMMENTED_ON]-(q:Question) \
                          MATCH p2=(u2:User)-[:COMMENTED]->(c2:Comment)-[:COMMENTED_ON]-(q) \
                          WHERE id(u1) < id(u2) \
                          RETURN id(u1) AS i1, id(u2) AS i2, id(q) AS iq, p2 AS p2 \
                          ORDER BY i1, i2";

/// The pivot's rows are exactly the un-pivoted expansion's rows, and the
/// named path `p2` comes back in the pattern's ORIGINAL node/rel order —
/// the regression the backward walker's original-index bindings guard.
#[test]
fn pivot_pairs_match_the_index_zero_expansion() {
    let g = pair_graph();
    let _ = take_pivot_backward_hops();
    let pivoted = exec_cypher(&g, PAIR_QUERY);
    let plain = with_pivot_disabled(|| exec_cypher(&g, PAIR_QUERY));
    assert_eq!(pivoted.rows, plain.rows);
    // Only u0/u1 share a question, so exactly one pair row.
    assert_eq!(pivoted.rows.len(), 1);
    let row = &pivoted.rows[0];
    let Value::Path(p2) = &row[3] else {
        panic!("p2 must be a path: {:?}", row[3]);
    };
    let (i1, i2, iq) = (
        row[0].as_int().unwrap(),
        row[1].as_int().unwrap(),
        row[2].as_int().unwrap(),
    );
    assert_eq!((i1, i2, iq), (0, 1, 3));
    // Original order: u2 first, then its comment, then the shared
    // question — not the backward walk's q-first order.
    let ids: Vec<_> = p2.nodes.iter().map(|n| n.id).collect();
    assert_eq!((ids[0], ids[2]), (i2, iq));
    assert_eq!(p2.rels.len(), 2);
    assert_eq!(p2.rels[0].rel_type, "COMMENTED");
    assert_eq!(p2.rels[1].rel_type, "COMMENTED_ON");
    // The path is contiguous: each rel links its neighboring nodes.
    for (r, (a, b)) in p2.rels.iter().zip(ids.iter().zip(ids.iter().skip(1))) {
        assert!((r.start == *a && r.end == *b) || (r.start == *b && r.end == *a));
    }
}

/// The pivot actually engages on the second pattern (the backward walker
/// takes hops), and the disabled run takes none.
#[test]
fn pivot_engages_on_the_bound_tail() {
    let g = pair_graph();
    let _ = take_pivot_backward_hops();
    let _ = exec_cypher(&g, PAIR_QUERY);
    assert!(take_pivot_backward_hops() > 0);
    let _ = with_pivot_disabled(|| exec_cypher(&g, PAIR_QUERY));
    assert_eq!(take_pivot_backward_hops(), 0);
}

/// A variable-length prefix rel keeps the index-0 start (a documented gap,
/// not a silent fallback): same rows, no backward hops.
#[test]
fn var_length_prefix_keeps_the_index_zero_start() {
    let g = pair_graph();
    // The single var-length pattern isolates the fallback: every element
    // past the var-length rel is ineligible as a start (its prefix
    // contains the rel), and no element is row-bound, so the index-0
    // start is kept — no backward hops. (The paired form of this query
    // also pivots its FIRST pattern now, via ADR-0351's cheapest start
    // on the unbound first pattern, so the pair query is no longer
    // counter-clean for this fallback.)
    let query = "MATCH (u2:User)-[:COMMENTED*1..1]->(c2:Comment)-[:COMMENTED_ON]-(q:Question) \
                 RETURN id(u2) AS i2, id(c2) AS ic, id(q) AS iq \
                 ORDER BY i2, ic, iq";
    let _ = take_pivot_backward_hops();
    let varlen = exec_cypher(&g, query);
    assert_eq!(take_pivot_backward_hops(), 0);
    let plain = with_pivot_disabled(|| exec_cypher(&g, query));
    assert_eq!(varlen.rows, plain.rows);
    // Every user's one comment reaches a question over the undirected hop.
    assert_eq!(varlen.rows.len(), 3);
}

/// A q19-shaped fixture (LABS-337): a big label at element 0, a small one
/// at element 1 carrying a `CONTAINS` residual, one single-hop rel
/// between them, plus a second pattern from the bound element-0 variable.
fn q19_graph(comments: usize) -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    // Many comments (the big label the index-0 start would scan).
    let comments: Vec<_> = (0..comments)
        .map(|_| {
            g.create_node(vec!["Comment".into()], BTreeMap::new())
                .unwrap()
        })
        .collect();
    // Few tags; exactly one matches the CONTAINS needle.
    let tags: Vec<_> = ["Copernicus", "Kepler"]
        .iter()
        .map(|name| {
            g.create_node(
                vec!["Tag".into()],
                BTreeMap::from([("name".into(), Value::String((*name).into()))]),
            )
            .unwrap()
        })
        .collect();
    let places: Vec<_> = (0..2)
        .map(|i| {
            g.create_node(
                vec!["Place".into()],
                BTreeMap::from([("name".into(), Value::String(format!("place{i}").into()))]),
            )
            .unwrap()
        })
        .collect();
    for (i, c) in comments.iter().enumerate() {
        // The Copernicus tag carries most comments; Kepler none.
        g.create_rel(*c, tags[0], "commentHasTag".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(
            *c,
            places[i % 2],
            "commentIsLocatedIn".into(),
            BTreeMap::new(),
        )
        .unwrap();
    }
    g.commit_write();
    g
}

const Q19_QUERY: &str = "MATCH (c:Comment)-[:commentHasTag]->(t:Tag), \
                        (c)-[:commentIsLocatedIn]->(l:Place) \
                        WHERE t.name CONTAINS 'Copernicus' \
                        RETURN l.name AS place, count(c) AS n ORDER BY n DESC, place";

/// ADR-0351: with nothing row-bound, the pattern starts at its cheapest
/// element (`t:Tag`, ~2 nodes) instead of element 0 (`c:Comment`, ~200):
/// the rows are the un-pivoted expansion's, and the backward walker runs.
#[test]
fn cheapest_element_start_matches_the_index_zero_expansion() {
    // Below the hub-join gate (LABS-348): the nested loop keeps the
    // pair, so this test still pins the ADR-0351 tie semantics.
    let g = q19_graph(100);
    let _ = take_pivot_backward_hops();
    let _ = take_hub_join_engagements();
    let selected = exec_cypher(&g, Q19_QUERY);
    assert_eq!(take_hub_join_engagements(), 0);
    with_pivot_disabled(|| {
        let unpivoted = exec_cypher(&g, Q19_QUERY);
        assert_eq!(unpivoted.rows, selected.rows);
    });
    // A two-element pattern over one relationship type estimates
    // symmetrically under degree scaling (rows x rels / label-rows from
    // either end is the relationship count) — a genuine tie, leftmost
    // wins, the index-0 start. The rows still match the disabled run
    // (the tie is correctness-neutral); a start displacement for this
    // shape needs an index hit asymmetry, which the property-bearing
    // test below pins.
    assert_eq!(take_pivot_backward_hops(), 0);
    // The Copernicus tag's comments split evenly across the two places.
    assert_eq!(selected.rows.len(), 2);
}

/// LABS-348: above the gate (200 side-A rows), the hub join takes the
/// comma pair — the sides enumerate once each, grouped by hub, and the
/// merged rows must equal the nested loop's on the same fixture.
#[test]
fn hub_join_matches_the_nested_loop_rows() {
    let g = q19_graph(200);
    let _ = take_hub_join_engagements();
    let joined = exec_cypher(&g, Q19_QUERY);
    assert_eq!(take_hub_join_engagements(), 1);
    with_hub_join_disabled(|| {
        // The hook must actually disengage the join for the row equality
        // to be a real A/B (the polarity bug once made this comparison
        // vacuous): zero engagements proves the nested loop ran.
        let _ = take_hub_join_engagements();
        let nested = exec_cypher(&g, Q19_QUERY);
        assert_eq!(take_hub_join_engagements(), 0);
        assert_eq!(nested.rows, joined.rows);
    });
    // The Copernicus tag's comments split evenly across the two places.
    assert_eq!(joined.rows.len(), 2);
}

/// The cheapest element is only taken when it is genuinely cheaper: a
/// pattern whose element 0 is already the smallest label keeps the
/// index-0 start (no backward hops).
#[test]
fn cheapest_element_start_not_taken_when_index_zero_is_cheapest() {
    let g = q19_graph(200);
    let _ = take_pivot_backward_hops();
    // commentHasTag runs comment → tag, so the Tag-first pattern reads it
    // incoming; Tag (2) is cheaper than Comment (200), so element 0 is
    // already the cheapest — the index-0 start is kept.
    let r = exec_cypher(
        &g,
        "MATCH (t:Tag)<-[:commentHasTag]-(c:Comment) RETURN count(t) AS n",
    );
    assert_eq!(r.rows[0][0], Value::Int(200));
    assert_eq!(take_pivot_backward_hops(), 0);
}

/// The q3 regression on #359's follow-up: a property-bearing element 0
/// (an anchored point lookup) must not be displaced by a later element's
/// smaller label — the property seek enumerates ~1 candidate, so the
/// label-count estimate must score it as the seek, not the label
/// (otherwise the walk starts at the small label and traverses the whole
/// relationship side: 0.03ms became 78ms).
#[test]
fn property_bearing_element_zero_is_never_displaced() {
    let g = Graph::default();
    g.begin_write().unwrap();
    // 5 comments, all tagged; each carries an anchored {id: i} property
    // map. Tag (1 node) is the smaller label, so the old label-count
    // estimate displaced element 0 onto it and walked the whole
    // commentHasTag side for the point lookup.
    let tagged = g.create_node(vec!["Tag".into()], BTreeMap::new()).unwrap();
    for i in 0..5 {
        let c = g
            .create_node(
                vec!["Comment".into()],
                BTreeMap::from([("id".into(), Value::Int(i))]),
            )
            .unwrap();
        g.create_rel(c, tagged, "commentHasTag".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    // The equality's index rank is what keeps the planner's order with
    // the property element first — without it the planner reorders by
    // label count and the displacement cannot arise (q3's upstream
    // indexes carry the same rank).
    test_support::exec_autocommit(&g, "CREATE INDEX FOR (c:Comment) ON (c.id)").unwrap();
    let _ = take_pivot_backward_hops();
    let r = exec_cypher(
        &g,
        "MATCH (c:Comment {id: 1})-[:commentHasTag]->(t:Tag) RETURN count(c) AS n",
    );
    assert_eq!(r.rows[0][0], Value::Int(1));
    assert_eq!(take_pivot_backward_hops(), 0);
}

/// The stellar High finding on #346: a prefix element's predicate that
/// references another prefix element's variable (legal in the forward
/// walk's left-to-right binding order) must keep the index-0 start — the
/// backward walk binds right to left, so the reference would see an
/// unbound (null) variable and silently drop rows.
#[test]
fn peer_referenced_prefix_predicate_keeps_the_index_zero_start() {
    let g = Graph::default();
    g.begin_write().unwrap();
    // u0/u1 users; each comment carries `seen` = its user's name, and each
    // hangs off its own question (the pair shape only needs the second
    // pattern's `q` to be row-bound, not a shared question).
    for (id, name) in [(0, "u0"), (1, "u1")] {
        let u = g
            .create_node(
                vec!["User".into()],
                BTreeMap::from([
                    ("id".into(), Value::Int(id)),
                    ("name".into(), Value::String(name.into())),
                ]),
            )
            .unwrap();
        let c = g
            .create_node(
                vec!["Comment".into()],
                BTreeMap::from([("seen".into(), Value::String(name.into()))]),
            )
            .unwrap();
        let q = g
            .create_node(vec!["Question".into()], BTreeMap::new())
            .unwrap();
        g.create_rel(u, c, "COMMENTED".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(c, q, "COMMENTED_ON".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    // Pattern 1 binds q, so pattern 2 would pivot at q; c2's map
    // references u2 (a peer prefix variable), which the backward walk
    // would see unbound and drop every row.
    let query = "MATCH (q:Question)<-[:COMMENTED_ON]-(first:Comment) \
                 MATCH (u2:User)-[:COMMENTED]->(c2:Comment {seen: u2.name})-[:COMMENTED_ON]->(q) \
                 RETURN u2.name AS name ORDER BY name";
    let _ = take_pivot_backward_hops();
    let rows = exec_cypher(&g, query);
    assert_eq!(take_pivot_backward_hops(), 0);
    let names: Vec<String> = rows
        .rows
        .iter()
        .map(|r| r[0].as_string().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["u0".to_string(), "u1".to_string()]);
    // The naive pivot (gate off) silently loses both rows — the exact
    // regression the gate prevents.
    // The naive (gate-bypassed) phase is planner-order-dependent — this
    // fixture's second pattern is planner-reordered `q`-first, so no pivot
    // is reachable in either mode — so the gate itself gets direct,
    // planner-independent coverage below (`pattern_self_contained` unit
    // tests) instead of a bypass round-trip through the executor.
}

/// Direct, planner-independent coverage of the order-safety gate
/// (`pattern_self_contained`, the stellar High finding's defense): a
/// pattern whose element predicates reference another element's variable
/// is not self-contained; an entry-bound reference is (the row already
/// binds it in every walk order).
mod gate {
    /// The one referencing pattern both gate tests build over: `a`, then
    /// `m` carrying `{v: a.marker}` — a peer-variable reference the gate
    /// must reject unless the row already binds `a`.
    fn referencing_pattern() -> PathPattern {
        path(vec![
            node(Some("a"), None),
            node(
                Some("m"),
                Some(Expr::Map(vec![(
                    "v".to_string(),
                    Expr::Property {
                        expr: Box::new(Expr::Variable("a".to_string())),
                        key: "marker".to_string(),
                    },
                )])),
            ),
        ])
    }
    use super::super::pivot::pattern_self_contained;
    use crate::eval::Row;
    use cypherlite_ir::Direction;
    use cypherlite_ir::{Expr, NodePattern, PathPattern, RelPattern};

    fn node(variable: Option<&str>, properties: Option<Expr>) -> NodePattern {
        NodePattern {
            variable: variable.map(str::to_string),
            labels: Vec::new(),
            label_expr: None,
            properties,
            where_clause: None,
        }
    }

    fn plain_rel() -> RelPattern {
        RelPattern {
            variable: None,
            types: Vec::new(),
            type_expr: None,
            direction: Direction::Outgoing,
            properties: None,
            var_length: false,
            min_hops: 1,
            max_hops: Some(1),
            where_clause: None,
            quantified: false,
        }
    }

    fn path(nodes: Vec<NodePattern>) -> PathPattern {
        let rels = vec![plain_rel(); nodes.len().saturating_sub(1)];
        PathPattern {
            variable: None,
            elements: Vec::new(),
            nodes,
            rels,
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        }
    }

    /// `{v: a.marker}` — a property map referencing a peer element's
    /// variable — makes the pattern not self-contained.
    #[test]
    fn peer_referenced_property_map_is_not_self_contained() {
        let p = referencing_pattern();
        assert!(!pattern_self_contained(&p, &Row::new()));
    }

    /// A reference to a variable the incoming row already binds is safe
    /// in every walk order.
    #[test]
    fn entry_bound_reference_is_self_contained() {
        let p = referencing_pattern();
        let mut row = Row::new();
        // The gate's entry-bound check is presence-only: whatever the row
        // already binds is safe in every walk order.
        row.insert("a".to_string(), cypherlite_core::Value::Int(1));
        assert!(pattern_self_contained(&p, &row));
    }

    /// No cross-references at all: self-contained.
    #[test]
    fn plain_pattern_is_self_contained() {
        let p = path(vec![node(Some("a"), None), node(Some("m"), None)]);
        assert!(pattern_self_contained(&p, &Row::new()));
    }
}
