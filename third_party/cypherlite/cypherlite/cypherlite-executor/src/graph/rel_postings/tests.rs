use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cypherlite_core::Value;

use super::*;
use crate::graph::test_support::endpoints;

fn props(value: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([("x".into(), value)])
}

#[test]
fn postings_see_relationships_created_before_and_after_the_build() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let before = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    g.commit_write();

    // The first probe builds the pair with one pass over `rels`.
    assert_eq!(
        g.rel_equality_postings("R", "x", &Value::Int(1)),
        vec![before]
    );
    assert_eq!(g.rel_equality_postings_len("R", "x", &Value::Int(1)), 1);

    g.begin_write().unwrap();
    let after = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    g.commit_write();

    let mut ids = g.rel_equality_postings("R", "x", &Value::Int(1));
    ids.sort_unstable();
    assert_eq!(ids, vec![before, after]);
    assert_eq!(g.rel_equality_postings_len("R", "x", &Value::Int(1)), 2);
    // The new relationship is in no unrelated bucket.
    assert!(g.rel_equality_postings("R", "x", &Value::Int(2)).is_empty());
}

#[test]
fn a_set_after_the_build_dirty_marks_the_committed_pair() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let id = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    g.commit_write();
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(1)), vec![id]);

    g.begin_write().unwrap();
    g.set_property_rel(id, "x", Value::Int(2));
    g.commit_write();

    // The new value's bucket picks the id up on the next probe...
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(2)), vec![id]);
    // ... and the old value stays a superset (ADR-0063).
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(1)), vec![id]);
    assert_eq!(g.rel_equality_postings_len("R", "x", &Value::Int(2)), 1);
}

#[test]
fn a_collected_version_leaves_the_built_pair() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let id = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    g.commit_write();
    // Build the pair first: GC prunes only built pairs.
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(1)), vec![id]);

    g.begin_write().unwrap();
    g.delete_rel(id);
    g.commit_write();
    // No pin is held, so the tombstoned version is collected.
    g.gc_versions();

    assert!(g.rel(id).is_none());
    assert!(g.rel_equality_postings("R", "x", &Value::Int(1)).is_empty());
    assert_eq!(g.rel_equality_postings_len("R", "x", &Value::Int(1)), 0);
}

#[test]
fn rebuild_rebuilds_the_same_postings() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let one = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    let two = g
        .create_rel(a, b, "R".into(), props(Value::Int(2)))
        .unwrap();
    g.commit_write();

    let before = g.rel_equality_postings("R", "x", &Value::Int(1));
    assert_eq!(before, vec![one]);

    g.rebuild_indexes();

    let after = g.rel_equality_postings("R", "x", &Value::Int(1));
    assert_eq!(after, before);
    assert_eq!(g.rel_equality_postings("R", "x", &Value::Int(2)), vec![two]);
    assert_eq!(g.rel_equality_postings_len("R", "x", &Value::Int(1)), 1);
}

#[test]
fn nan_and_never_interned_names_return_nothing() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    g.create_rel(a, b, "R".into(), props(Value::Float(f64::NAN)))
        .unwrap();
    let found = g
        .create_rel(a, b, "R".into(), props(Value::Int(1)))
        .unwrap();
    g.commit_write();

    assert!(g
        .rel_equality_postings("R", "x", &Value::Float(f64::NAN))
        .is_empty());
    assert_eq!(
        g.rel_equality_postings_len("R", "x", &Value::Float(f64::NAN)),
        0
    );
    assert!(g
        .rel_equality_postings("Never", "x", &Value::Int(1))
        .is_empty());
    assert!(g
        .rel_equality_postings("R", "never", &Value::Int(1))
        .is_empty());
    assert_eq!(g.rel_equality_postings_len("Never", "x", &Value::Int(1)), 0);
    assert_eq!(g.rel_equality_postings_len("R", "never", &Value::Int(1)), 0);
    // The NaN write did not disturb the indexable bucket.
    assert_eq!(
        g.rel_equality_postings("R", "x", &Value::Int(1)),
        vec![found]
    );
}

/// One thread probes `rel_equality_postings` while another commits 500
/// relationships through `apply_redo`: the pair is built before the commits,
/// so every committed id must arrive through the dirty catch-up.
#[test]
fn probes_stay_current_beside_concurrent_commits() {
    let g = Arc::new(Graph::default());
    let (a, b) = endpoints(&g);
    // Build the pair before the commits.
    assert!(g
        .rel_equality_postings("R", "x", &Value::Int(-1))
        .is_empty());

    let deadline = Instant::now() + Duration::from_secs(10);
    let reader = {
        let g = Arc::clone(&g);
        std::thread::spawn(move || loop {
            if g.rel_equality_postings("R", "x", &Value::Int(-1)).len() >= 500 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reader did not observe the committed relationships within 10 s"
            );
            std::thread::yield_now();
        })
    };

    let start = Instant::now();
    for id in 0..500 {
        let ts = g.commit_ts() + 1;
        g.apply_redo(
            vec![RedoOp::create_rel(
                id,
                a,
                b,
                "R".into(),
                props(Value::Int(-1)),
            )],
            ts,
        )
        .unwrap();
    }
    reader.join().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "commits and probes took {:?}",
        start.elapsed()
    );

    let mut ids = g.rel_equality_postings("R", "x", &Value::Int(-1));
    ids.sort_unstable();
    assert_eq!(ids, (0..500).collect::<Vec<_>>());
}
