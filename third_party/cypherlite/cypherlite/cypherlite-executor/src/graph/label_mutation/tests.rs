//! The label-mutation twins' contract tests: the no-op model's one early
//! exit and the effect-recording WAL row (LABS-425).

use super::*;

/// LABS-425 (PR #486's round): remove_labels guards on the effect, not
/// the request's resolution — every no-op shape (an empty request, a
/// name interned elsewhere but not carried by the node, a name never
/// interned, a missing node) takes the one early exit and leaves no
/// trace: no version push (the node's chain does not grow), no WAL row.
/// A real removal still pushes both — the guard must not eat the effect.
#[test]
fn remove_labels_no_op_shapes_push_no_version_and_no_wal_row() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    // "Foo" is interned — on another node — but not carried by `a`.
    g.create_node(vec!["Foo".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    let chain = g.node_chain_len(a);

    g.begin_write().unwrap();
    // Interned elsewhere, not carried: the request resolves, the effect
    // does not — no version push, no WAL row.
    g.remove_labels(a, &["Foo".into()]);
    assert_eq!(g.node_chain_len(a), chain);
    // The empty request: the same single early exit.
    g.remove_labels(a, &[]);
    assert_eq!(g.node_chain_len(a), chain);
    // A name never interned at all: ditto.
    g.remove_labels(a, &["Ghost".into()]);
    assert_eq!(g.node_chain_len(a), chain);
    assert!(g.pending_redo.lock().unwrap().is_empty());

    // The effect still lands: a carried label pushes its version and its
    // effect-recording WAL row.
    g.remove_labels(a, &["A".into()]);
    assert_eq!(g.node_chain_len(a), chain + 1);
    assert_eq!(
        g.pending_redo.lock().unwrap().clone(),
        vec![RedoOp::remove_labels(a, vec!["A".to_string()])]
    );
    g.commit_write();
    assert_eq!(g.pattern_label_count_exact("A"), 0);

    // A missing node: the same no trace, no panic.
    g.begin_write().unwrap();
    g.remove_labels(9999, &["Foo".into()]);
    assert!(g.pending_redo.lock().unwrap().is_empty());
    g.commit_write();
}
