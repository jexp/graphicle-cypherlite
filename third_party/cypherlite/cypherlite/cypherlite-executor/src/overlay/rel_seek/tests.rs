use std::collections::BTreeMap;

use cypherlite_core::Value;

use crate::exec::test_support::write_scratch;
use crate::graph::test_support::endpoints;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

#[test]
fn candidates_merge_committed_and_scratch_ascending_and_deduplicated() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let committed = g
        .create_rel(
            a,
            b,
            "R".into(),
            BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    g.commit_write();

    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    let scratch_rel = view
        .create_rel(
            a,
            b,
            "R".into(),
            BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    // A second scratch relationship of another type must not leak in.
    view.create_rel(
        a,
        b,
        "S".into(),
        BTreeMap::from([("x".into(), Value::Int(1))]),
    )
    .unwrap();

    assert_eq!(
        view.rel_equality_candidates("R", "x", &Value::Int(1)),
        vec![committed, scratch_rel]
    );
    assert_eq!(view.rel_equality_bucket_len("R", "x", &Value::Int(1)), 2);
    // The committed bucket alone answers an unrelated value.
    assert!(view
        .rel_equality_candidates("R", "x", &Value::Int(9))
        .is_empty());
    assert_eq!(view.rel_equality_bucket_len("R", "x", &Value::Int(9)), 0);
}

#[test]
fn a_scratch_set_posts_the_new_value_while_the_committed_side_keeps_the_old() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let id = g
        .create_rel(
            a,
            b,
            "R".into(),
            BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    g.commit_write();
    // Build the committed pair first (the scratch pair builds on its seek).
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(1)), vec![id]);

    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    view.set_property_rel(id, "x", Value::Int(2));

    // The scratch copy serves the new value...
    assert_eq!(
        view.rel_equality_candidates("R", "x", &Value::Int(2)),
        vec![id]
    );
    assert_eq!(view.rel_equality_bucket_len("R", "x", &Value::Int(2)), 1);
    // ... and the committed postings still serve the old one (superset).
    assert_eq!(
        view.rel_equality_candidates("R", "x", &Value::Int(1)),
        vec![id]
    );
    assert_eq!(view.rel_equality_bucket_len("R", "x", &Value::Int(1)), 1);
}
