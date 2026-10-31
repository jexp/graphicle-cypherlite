use std::collections::BTreeMap;

use cypherlite_core::Value;

use super::*;
use crate::redo::CatalogName;

fn node(id: i64) -> RedoOp {
    RedoOp::create_node(id, Vec::new(), BTreeMap::new())
}

fn drop_index(name: &str) -> RedoOp {
    RedoOp::DropIndex { name: name.into() }
}

#[test]
fn catalog_ops_are_the_catalog_entries_in_log_order() {
    let mut log = RedoLog::default();
    log.push(node(1));
    log.push(drop_index("a"));
    log.push(node(2));
    log.push(drop_index("b"));
    let catalog: Vec<&RedoOp> = log.catalog_ops().collect();
    assert_eq!(catalog, [&drop_index("a"), &drop_index("b")]);
}

#[test]
fn the_cursor_moves_only_when_marked() {
    let mut log = RedoLog::default();
    log.push(node(1));
    log.push(node(2));
    let (ops, end) = log.unvalidated();
    assert_eq!((ops.len(), end), (2, 2));
    // Not marked (the validation failed): the same slice again.
    assert_eq!(log.unvalidated(), (vec![node(1), node(2)], 2));
    log.mark_validated(end);
    assert_eq!(log.unvalidated(), (vec![], 2));
    log.push(node(3));
    assert_eq!(log.unvalidated(), (vec![node(3)], 3));
}

#[test]
fn take_resets_the_catalog_positions_and_the_cursor() {
    let mut log = RedoLog::default();
    log.push(drop_index("a"));
    log.push(node(1));
    let (_, end) = log.unvalidated();
    log.mark_validated(end);
    assert_eq!(log.take(), [drop_index("a"), node(1)]);
    assert_eq!(log.catalog_ops().count(), 0);
    log.push(node(2));
    assert_eq!(log.unvalidated(), (vec![node(2)], 1));
}

#[test]
fn mentions_reads_create_nodes_in_place_and_borrows_other_ops() {
    let mut props = BTreeMap::new();
    props.insert("k".to_string(), Value::Int(1));
    let mut log = RedoLog::default();
    log.push(RedoOp::create_node(1, vec!["L".to_string()], props));
    log.push(RedoOp::create_rel(
        7,
        1,
        2,
        "T".to_string(),
        BTreeMap::new(),
    ));
    log.push(drop_index("a"));
    assert!(log.mentions(CatalogName::Label("L")));
    assert!(log.mentions(CatalogName::PropertyKey("k")));
    assert!(log.mentions(CatalogName::RelType("T")));
    assert!(!log.mentions(CatalogName::Label("missing")));
    assert!(!log.mentions(CatalogName::RelType("L")));
    assert!(!log.mentions(CatalogName::PropertyKey("a")));
}

#[test]
fn a_create_node_slot_stays_within_two_pointers_of_the_inline_record() {
    // id + two Arcs is 24 bytes. The discriminant may round the slot up,
    // and it must stay well under the 160-byte catalog-sized `RedoOp`.
    assert!(std::mem::size_of::<super::LiveRedo>() <= 32);
}

#[test]
#[should_panic(expected = "mark_validated")]
#[cfg(debug_assertions)]
fn marking_past_the_log_is_an_invariant_violation() {
    let mut log = RedoLog::default();
    log.push(node(1));
    log.mark_validated(2);
}

#[test]
#[should_panic(expected = "rollback cursor")]
fn a_rollback_cursor_past_the_log_fails_loudly() {
    // A `truncate` past the end silently drops nothing, which would leave the
    // failed batch's ops in the log; the rollback contract break panics in
    // every profile instead (an `assert`, not a `debug_assert`).
    let mut log = RedoLog::default();
    log.push(node(1));
    log.restore_cursor(RedoCursor {
        len: 2,
        catalog: Vec::new(),
        validated: 0,
        validated_catalog: None,
    });
}
