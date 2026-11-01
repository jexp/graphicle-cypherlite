use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use cypherlite_core::{Node, NodeId, RelId, Relationship, Value};

use super::ScratchPropIndex;
use crate::overlay::WriteScratch;
use crate::redo::RedoOp;

fn node(id: NodeId, labels: &[&str], props: &[(&str, Value)]) -> (NodeId, Node) {
    (
        id,
        Node {
            id,
            labels: Arc::new(labels.iter().map(|l| l.to_string()).collect()),
            properties: cypherlite_core::PropertyMap::share(
                props
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect::<BTreeMap<_, _>>(),
            ),
        },
    )
}

fn rel(id: RelId, rel_type: &str, props: &[(&str, Value)]) -> (RelId, Relationship) {
    (
        id,
        Relationship {
            id,
            start: 0,
            end: 1,
            rel_type: rel_type.to_string(),
            properties: cypherlite_core::PropertyMap::share(
                props
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect::<BTreeMap<_, _>>(),
            ),
        },
    )
}

fn ids<T: Copy + Eq + std::hash::Hash>(xs: &[T]) -> HashSet<T> {
    xs.iter().copied().collect()
}

const NONE: [&str; 0] = [];

#[test]
fn seek_builds_a_pair_from_the_scratch_nodes() {
    let nodes = HashMap::from([
        node(1, &["A"], &[("x", Value::Int(1))]),
        node(2, &["B"], &[("x", Value::Float(1.0))]),
        node(3, &["A"], &[("x", Value::Int(2))]),
    ]);
    let mut ix = ScratchPropIndex::<Node>::default();
    assert_eq!(ix.seek(&nodes, &["A"], "x", &Value::Int(1)), ids(&[1]));
    assert_eq!(ix.seek(&nodes, &NONE, "x", &Value::Int(1)), ids(&[1, 2]));
    assert_eq!(
        ix.seek(&nodes, &["A", "B"], "x", &Value::Int(1)),
        ids(&[1, 2])
    );
    assert!(ix
        .seek(&nodes, &["A"], "x", &Value::Float(f64::NAN))
        .is_empty());
}

/// ADR-0184: `seek_len` counts one label's bucket in place, equal to
/// `seek(..).len()`, and a post after the pair was built joins both.
#[test]
fn seek_len_counts_one_label_and_sees_late_posts() {
    let nodes = HashMap::from([
        node(1, &["A"], &[("x", Value::Int(1))]),
        node(2, &["A"], &[("x", Value::Int(1))]),
        node(3, &["A"], &[("x", Value::Int(2))]),
        node(4, &["B"], &[("x", Value::Int(1))]),
    ]);
    let mut ix = ScratchPropIndex::<Node>::default();
    assert_eq!(ix.seek_len(&nodes, "A", "x", &Value::Int(1)), 2);
    assert_eq!(
        ix.seek_len(&nodes, "A", "x", &Value::Int(1)),
        ix.seek(&nodes, &["A"], "x", &Value::Int(1)).len()
    );
    assert_eq!(ix.seek_len(&nodes, "B", "x", &Value::Int(1)), 1);
    assert_eq!(ix.seek_len(&nodes, "A", "x", &Value::Float(f64::NAN)), 0);
    // A node posted after the pair was built joins the counted bucket.
    ix.post_value(5, &["A".to_string()], "x", &Value::Int(1));
    assert_eq!(ix.seek_len(&nodes, "A", "x", &Value::Int(1)), 3);
    assert_eq!(
        ix.seek_len(&nodes, "A", "x", &Value::Int(1)),
        ix.seek(&nodes, &["A"], "x", &Value::Int(1)).len()
    );
}

#[test]
fn posts_reach_only_built_pairs() {
    let empty = HashMap::new();
    let mut ix = ScratchPropIndex::<Node>::default();
    // Nothing built yet: the post is dropped, the later build scans `nodes`.
    ix.post_value(1, &["A".to_string()], "x", &Value::Int(1));
    assert!(ix.seek(&empty, &["A"], "x", &Value::Int(1)).is_empty());
    ix.post_value(1, &["A".to_string()], "x", &Value::Int(1));
    assert_eq!(ix.seek(&empty, &["A"], "x", &Value::Int(1)), ids(&[1]));
}

#[test]
fn a_value_posts_under_any_label_and_added_labels_do_not() {
    let empty = HashMap::new();
    let mut ix = ScratchPropIndex::<Node>::default();
    ix.seek(&empty, &NONE, "x", &Value::Int(1));
    ix.seek(&empty, &["A"], "x", &Value::Int(1));
    ix.seek(&empty, &["B"], "x", &Value::Int(1));
    ix.post_value(1, &["A".to_string()], "x", &Value::Int(1));
    let added =
        cypherlite_core::PropertyMap::from(BTreeMap::from([("x".to_string(), Value::Int(1))]));
    ix.post_labels(2, &["B".to_string()], &added);
    assert_eq!(ix.seek(&empty, &NONE, "x", &Value::Int(1)), ids(&[1]));
    assert_eq!(ix.seek(&empty, &["A"], "x", &Value::Int(1)), ids(&[1]));
    assert_eq!(ix.seek(&empty, &["B"], "x", &Value::Int(1)), ids(&[2]));
    // A NaN-bearing value is unindexable.
    ix.post_value(3, &["A".to_string()], "x", &Value::Float(f64::NAN));
    assert_eq!(ix.seek(&empty, &["A"], "x", &Value::Int(1)), ids(&[1]));
}

/// `push_redo` posts a `CreateNode` from the scratch copy: `create_node`
/// installs the node into `WriteScratch::nodes` before it pushes.
#[test]
fn push_redo_posts_a_create_node_from_the_scratch_copy() {
    let mut s = WriteScratch::new(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let seek = |s: &mut WriteScratch| {
        s.batch
            .prop_ix
            .seek(&s.batch.nodes, &["A"], "x", &Value::Int(1))
    };
    assert!(seek(&mut s).is_empty());
    s.batch.nodes.insert(
        7,
        Node {
            id: 7,
            labels: Arc::new(vec!["A".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "x".into(),
                Value::Int(1),
            )])),
        },
    );
    s.push_redo(RedoOp::create_node(
        7,
        vec!["A".into()],
        BTreeMap::from([("x".into(), Value::Int(1))]),
    ));
    assert_eq!(seek(&mut s), ids(&[7]));
}

/// The relationship side of the one index: the class is the rel's single
/// type, so a build selects by type and a NaN-bearing value is unindexable.
#[test]
fn rel_seek_builds_a_pair_from_the_scratch_rels() {
    let rels = HashMap::from([
        rel(1, "R", &[("x", Value::Int(1))]),
        rel(2, "S", &[("x", Value::Int(1))]),
        rel(3, "R", &[("x", Value::Float(1.0))]),
        rel(4, "R", &[("x", Value::Int(2))]),
    ]);
    let mut ix = ScratchPropIndex::<Relationship>::default();
    assert_eq!(ix.seek(&rels, "R", "x", &Value::Int(1)), ids(&[1, 3]));
    assert_eq!(ix.seek(&rels, "S", "x", &Value::Int(1)), ids(&[2]));
    assert_eq!(ix.seek(&rels, "R", "x", &Value::Int(2)), ids(&[4]));
    // A NaN-bearing value is unindexable, and an absent type is empty.
    assert!(ix.seek(&rels, "R", "x", &Value::Float(f64::NAN)).is_empty());
    assert!(ix.seek(&rels, "T", "x", &Value::Int(1)).is_empty());
}

/// ADR-0184: the rel `seek_len` counts one type's bucket in place, equal
/// to `seek(..).len()`, and a post after the pair was built joins both.
#[test]
fn rel_seek_len_counts_one_type_and_sees_late_posts() {
    let rels = HashMap::from([
        rel(1, "R", &[("x", Value::Int(1))]),
        rel(2, "R", &[("x", Value::Int(1))]),
        rel(3, "R", &[("x", Value::Int(2))]),
        rel(4, "S", &[("x", Value::Int(1))]),
    ]);
    let mut ix = ScratchPropIndex::<Relationship>::default();
    assert_eq!(ix.seek_len(&rels, "R", "x", &Value::Int(1)), 2);
    assert_eq!(
        ix.seek_len(&rels, "R", "x", &Value::Int(1)),
        ix.seek(&rels, "R", "x", &Value::Int(1)).len()
    );
    assert_eq!(ix.seek_len(&rels, "S", "x", &Value::Int(1)), 1);
    assert_eq!(ix.seek_len(&rels, "R", "x", &Value::Float(f64::NAN)), 0);
    // A rel posted after the pair was built joins the counted bucket.
    ix.post_value(5, "R", "x", &Value::Int(1));
    assert_eq!(ix.seek_len(&rels, "R", "x", &Value::Int(1)), 3);
    assert_eq!(
        ix.seek_len(&rels, "R", "x", &Value::Int(1)),
        ix.seek(&rels, "R", "x", &Value::Int(1)).len()
    );
}

#[test]
fn rel_posts_reach_only_built_pairs() {
    let empty = HashMap::new();
    let mut ix = ScratchPropIndex::<Relationship>::default();
    // Nothing built yet: the post is dropped, the later build scans `rels`.
    ix.post_value(1, "R", "x", &Value::Int(1));
    assert!(ix.seek(&empty, "R", "x", &Value::Int(1)).is_empty());
    ix.post_value(1, "R", "x", &Value::Int(1));
    assert_eq!(ix.seek(&empty, "R", "x", &Value::Int(1)), ids(&[1]));
    // A NaN-bearing value is never posted.
    ix.post_value(2, "R", "x", &Value::Float(f64::NAN));
    assert_eq!(ix.seek(&empty, "R", "x", &Value::Int(1)), ids(&[1]));
}

/// `push_redo` posts a `CreateRel` from the scratch copy: `create_rel`
/// installs the relationship into `WriteScratch::rels` before it pushes.
#[test]
fn push_redo_posts_a_create_rel_from_the_scratch_copy() {
    let mut s = WriteScratch::new(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let seek = |s: &mut WriteScratch| {
        s.batch
            .rel_prop_ix
            .seek(&s.batch.rels, "R", "x", &Value::Int(1))
    };
    assert!(seek(&mut s).is_empty());
    s.batch
        .rels
        .insert(7, rel(7, "R", &[("x", Value::Int(1))]).1);
    s.push_redo(RedoOp::create_rel(
        7,
        0,
        1,
        "R".into(),
        BTreeMap::from([("x".into(), Value::Int(1))]),
    ));
    assert_eq!(seek(&mut s), ids(&[7]));
}

/// `push_redo` posts a non-null `SetRelProp` from the scratch copy and
/// skips a null one (the index is additions-only).
#[test]
fn push_redo_posts_a_non_null_set_rel_prop_and_skips_null() {
    let mut s = WriteScratch::new(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    s.batch.rels.insert(
        7,
        Relationship {
            id: 7,
            start: 0,
            end: 1,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
    );
    // Build both pairs before the writes so the posts have somewhere to go.
    assert!(s
        .batch
        .rel_prop_ix
        .seek(&s.batch.rels, "R", "x", &Value::Int(2))
        .is_empty());
    assert!(s
        .batch
        .rel_prop_ix
        .seek(&s.batch.rels, "R", "y", &Value::Null)
        .is_empty());

    s.push_redo(RedoOp::set_rel_prop(7, "x".into(), Value::Int(2)));
    assert_eq!(
        s.batch
            .rel_prop_ix
            .seek(&s.batch.rels, "R", "x", &Value::Int(2)),
        ids(&[7])
    );

    s.push_redo(RedoOp::set_rel_prop(7, "y".into(), Value::Null));
    assert!(s
        .batch
        .rel_prop_ix
        .seek(&s.batch.rels, "R", "y", &Value::Null)
        .is_empty());
}
