//! The commit-loop admission probe: the per-slot overlay, its rejections,
//! the final-slot short-circuit, and the pre-preflight classification
//! boundary.

use cypherlite_core::{Error, Value};

use crate::graph::Graph;
use crate::redo::RedoOp;
use crate::SlotPosition;

use super::{create_node, create_rel, seeded, seeded_constrained, tuple, unique, unique_rel};

/// The create-then-X fixture shared by the in-slot hole tests: one committed
/// `:Person {email: 1}`, then the slot builder run twice — value 1 conflicts
/// with the committed holder, value 2 (the same shape, a distinct value)
/// admits.
fn assert_conflict_then_distinct_admits(build: impl Fn(i64, Value) -> Vec<RedoOp>) {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(1, "Person", &[("email", Value::Int(1))]),
                unique("c", "Person", &["email"]),
            ],
            1,
        )
        .unwrap();
    let mut admission = graph.begin_constraint_admission();
    let err = admission.admit(&build(2, Value::Int(1))).unwrap_err();
    assert!(matches!(err, Error::Conflict));
    // A distinct value in the same shape admits through the same admission
    // (a leaked hold on the rejected tuple would be structural, not caught
    // here: `admit` is check-then-accept and `accept` only runs on `Ok`).
    admission.admit(&build(3, Value::Int(2))).unwrap();
}

#[test]
fn admission_rejects_a_duplicate_and_accepts_distinct_values() {
    let graph = Graph::default();
    graph
        .apply_redo(vec![unique("c", "P", &["email"])], 1)
        .unwrap();
    let mut admission = graph.begin_constraint_admission();
    admission
        .admit(&[create_node(0, "P", &[("email", Value::Int(1))])])
        .unwrap();
    let err = admission
        .admit(&[create_node(1, "P", &[("email", Value::Int(1))])])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
    admission
        .admit(&[create_node(1, "P", &[("email", Value::Int(2))])])
        .unwrap();
}

/// ADR-0439's create-registration rule (the Decision's One scope: a create
/// always registers its scope-filtered view): a create outside the
/// constrained label is pulled into scope by a later `AddLabels` in the same
/// slot. The create registers its entity unconditionally, so the label add
/// finds the state the create established instead of seeding the committed
/// (absent) view and no-opping; the introduction then conflicts with the
/// committed holder.
#[test]
fn admission_create_then_label_in_slot_conflicts() {
    assert_conflict_then_distinct_admits(|id, value| {
        vec![
            create_node(id, "Temp", &[("email", value)]),
            RedoOp::AddLabels {
                id,
                labels: vec!["Person".into()],
            },
        ]
    });
}

/// The create-then-label hole with the key set after the label: the create
/// registers an empty constrained view, `AddLabels` pulls it under the
/// constraint, and the `SetNodeProp` completes the conflicting tuple.
#[test]
fn admission_create_then_label_then_set_in_slot_conflicts() {
    assert_conflict_then_distinct_admits(|id, value| {
        vec![
            create_node(id, "Temp", &[]),
            RedoOp::AddLabels {
                id,
                labels: vec!["Person".into()],
            },
            RedoOp::SetNodeProp {
                id,
                key: "email".into(),
                value,
            },
        ]
    });
}

/// The mirror ordering — key before label. `SetNodeProp` mutates the view
/// the create registered even though the entity carries no constrained
/// label yet; the later `AddLabels` completes the tuple.
#[test]
fn admission_create_then_set_then_label_in_slot_conflicts() {
    assert_conflict_then_distinct_admits(|id, value| {
        vec![
            create_node(id, "Temp", &[]),
            RedoOp::SetNodeProp {
                id,
                key: "email".into(),
                value,
            },
            RedoOp::AddLabels {
                id,
                labels: vec!["Person".into()],
            },
        ]
    });
}

/// The relationship form cannot reproduce the node hole: a relationship's
/// type is fixed at creation and cannot be added later, so an out-of-scope
/// create can never be pulled under a constrained type in-slot. What this
/// pins is the `rel_view` Option path unconditional create registration
/// made reachable: a create whose type is not interned yet yields no view,
/// and a later `SetRelProp` on a constrained key name is a no-op — never a
/// panic (the pre-fix `expect`) and never a false conflict with a holder of
/// the constrained type. The constrained type's own create-then-set shape
/// still conflicts.
#[test]
fn admission_uninterned_rel_create_never_panics_or_false_conflicts() {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(0, "N", &[]),
                create_node(1, "N", &[]),
                create_rel(7, 0, 1, "R", &[("k", Value::Int(1))]),
                unique_rel("r", "R", &["k"]),
            ],
            1,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("r", &tuple([Value::Int(1)])),
        1
    );
    let mut admission = graph.begin_constraint_admission();
    // `Unknown` was never interned: no constraint can cover it, so setting
    // its constrained key name does not duplicate `r`'s tuple.
    admission
        .admit(&[
            create_rel(8, 0, 1, "Unknown", &[]),
            RedoOp::SetRelProp {
                id: 8,
                key: "k".into(),
                value: Value::Int(1),
            },
        ])
        .unwrap();
    // The interned, constrained type still conflicts on the same shape.
    let err = admission
        .admit(&[
            create_rel(9, 0, 1, "R", &[]),
            RedoOp::SetRelProp {
                id: 9,
                key: "k".into(),
                value: Value::Int(1),
            },
        ])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

#[test]
fn admission_allows_an_earlier_free_in_the_same_group() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    // Slot A frees the committed tuple, slot B may reuse it.
    admission.admit(&[RedoOp::DeleteNode { id: 0 }]).unwrap();
    admission
        .admit(&[create_node(3, "P", &[("email", Value::Int(1))])])
        .unwrap();
}

/// The final-slot short-circuit: a last slot whose ops can only free has no
/// later reader, so admission records no overlay hold for it. The committed
/// holder therefore stays visible — an introduction checked next (the
/// observation seam; a real group has no later slot) still conflicts.
/// `check_with_earlier` is the seam the commit loop calls with
/// [`SlotPosition::of`], so the position is pinned here.
#[test]
fn final_slot_delete_only_records_no_overlay_free() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    let pending = admission
        .check_with_earlier(
            &[RedoOp::DeleteNode { id: 0 }],
            &mut std::iter::empty(),
            SlotPosition::Final,
        )
        .unwrap()
        .0;
    admission.accept(pending);
    // No free was recorded (the graph is not mutated here), so the committed
    // holder of `email = 1` still blocks the value.
    let err = admission
        .check_with_earlier(
            &[create_node(3, "P", &[("email", Value::Int(1))])],
            &mut std::iter::empty(),
            SlotPosition::Followed,
        )
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

/// The other direction of the final-slot short-circuit: the same delete-only
/// slot before the last one records its free, so a later slot may reuse the
/// value.
#[test]
fn non_final_slot_delete_only_records_its_free() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    let pending = admission
        .check_with_earlier(
            &[RedoOp::DeleteNode { id: 0 }],
            &mut std::iter::empty(),
            SlotPosition::Followed,
        )
        .unwrap()
        .0;
    admission.accept(pending);
    admission
        .check_with_earlier(
            &[create_node(3, "P", &[("email", Value::Int(1))])],
            &mut std::iter::empty(),
            SlotPosition::Followed,
        )
        .unwrap();
}

#[test]
fn admission_allows_a_free_earlier_in_the_same_slot() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    admission
        .admit(&[
            RedoOp::DeleteNode { id: 0 },
            create_node(3, "P", &[("email", Value::Int(1))]),
        ])
        .unwrap();
}

#[test]
fn admission_does_not_flag_a_reassert_of_the_same_value() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    admission
        .admit(&[RedoOp::SetNodeProp {
            id: 0,
            key: "email".into(),
            value: Value::Int(1),
        }])
        .unwrap();
}

#[test]
fn admission_checks_the_full_composite_tuple() {
    let graph = Graph::default();
    graph
        .apply_redo(vec![unique("c", "P", &["email", "tenant"])], 1)
        .unwrap();
    let mut admission = graph.begin_constraint_admission();
    admission
        .admit(&[create_node(
            0,
            "P",
            &[("email", Value::Int(1)), ("tenant", Value::Int(7))],
        )])
        .unwrap();
    // Same first component, different full tuple.
    admission
        .admit(&[create_node(
            1,
            "P",
            &[("email", Value::Int(1)), ("tenant", Value::Int(8))],
        )])
        .unwrap();
    let err = admission
        .admit(&[create_node(
            2,
            "P",
            &[("email", Value::Int(1)), ("tenant", Value::Int(7))],
        )])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

#[test]
fn admission_rejects_a_constraint_install_over_an_accepted_introduction() {
    let graph = Graph::default();
    let mut admission = graph.begin_constraint_admission();
    let first_slot = [create_node(0, "P", &[("email", Value::Int(1))])];
    admission.admit(&first_slot).unwrap();
    let mut earlier = std::iter::once(first_slot.as_slice());
    let err = admission
        .check_with_earlier(
            &[unique("c", "P", &["email"])],
            &mut earlier,
            SlotPosition::Followed,
        )
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

#[test]
fn admission_rejects_an_introduction_into_a_constraint_installed_in_the_group() {
    let graph = Graph::default();
    let mut admission = graph.begin_constraint_admission();
    admission.admit(&[unique("c", "P", &["email"])]).unwrap();
    let err = admission
        .admit(&[create_node(0, "P", &[("email", Value::Int(1))])])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

#[test]
fn a_rejected_slot_leaves_no_tuple_hold() {
    let graph = Graph::default();
    graph
        .apply_redo(vec![unique("c", "P", &["email"])], 1)
        .unwrap();
    let mut admission = graph.begin_constraint_admission();
    // The slot is checked but never accepted (the capacity/timestamp arm
    // rejects it after the check): its introduction must leave no hold.
    let pending = admission
        .check(&[create_node(0, "P", &[("email", Value::Int(1))])])
        .unwrap();
    drop(pending);
    admission
        .admit(&[create_node(1, "P", &[("email", Value::Int(1))])])
        .unwrap();
}

#[test]
fn a_rejected_drop_constraint_does_not_free() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    // An accepted slot introduces a value.
    admission
        .admit(&[create_node(3, "P", &[("email", Value::Int(9))])])
        .unwrap();
    // A slot that drops the constraint is checked but rejected afterwards:
    // the drop must not take effect for later slots.
    let pending = admission
        .check(&[RedoOp::DropConstraint { name: "c".into() }])
        .unwrap();
    drop(pending);
    let err = admission
        .admit(&[create_node(4, "P", &[("email", Value::Int(9))])])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict));
}

#[test]
fn admission_allows_a_value_after_an_earlier_constraint_drop() {
    let graph = seeded_constrained();
    let mut admission = graph.begin_constraint_admission();
    admission
        .admit(&[RedoOp::DropConstraint { name: "c".into() }])
        .unwrap();
    admission
        .admit(&[create_node(3, "P", &[("email", Value::Int(1))])])
        .unwrap();
}

#[test]
fn two_constraints_in_one_group_keep_distinct_overlay_ids() {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![unique("c1", "P", &["email"]), unique("c2", "P", &["ssn"])],
            1,
        )
        .unwrap();
    let mut admission = graph.begin_constraint_admission();
    // Two constraints over the same label: the group overlay's TupleId must
    // not collide across them, so an email hold must not block an ssn write.
    admission
        .admit(&[create_node(0, "P", &[("email", Value::Int(1))])])
        .unwrap();
    admission
        .admit(&[create_node(1, "P", &[("ssn", Value::Int(1))])])
        .unwrap();
    // A node carrying both keys under both constraints, with distinct
    // values, admits cleanly.
    admission
        .admit(&[create_node(
            2,
            "P",
            &[("ssn", Value::Int(2)), ("email", Value::Int(2))],
        )])
        .unwrap();
    // A genuine duplicate under c1 still conflicts.
    assert!(admission
        .admit(&[create_node(3, "P", &[("email", Value::Int(1))])])
        .is_err());
}

/// The two pre-preflight seams — the entry-growth estimate (replicated
/// path) and the admission probe (`flush_group`) — classify a `Derived`
/// record before the WAL preflight runs. An `entity_type` naming neither
/// kind is corruption the preflight rejects, but neither seam may panic on
/// the way there: the estimate is unpriceable (no charge) and the probe
/// installs no key, while the preflight returns the typed storage error.
#[test]
fn corrupt_derived_entity_type_is_unpriceable_before_preflight_and_rejected_by_it() {
    let corrupt = RedoOp::CatalogDerivedConstraint {
        constraint: cypherlite_model::Constraint {
            name: "c".into(),
            label: "P".into(),
            properties: vec!["email".into()],
            constraint_type: cypherlite_model::ConstraintType::Unique,
            property_type: None,
            entity_type: "FOO".into(),
            enforced_label: None,
        },
    };

    // Seam 1: the capacity estimate (`commit_replicated` →
    // `estimate_redo_delta`, before the preflight). The seeded graph has
    // live `P` entities, so a classifiable uniqueness record would charge a
    // backfill here; an unclassifiable one is skipped instead — the
    // contrast pins that the arm skips only the unclassifiable record.
    let graph = seeded();
    let classifiable = RedoOp::CatalogDerivedConstraint {
        constraint: cypherlite_model::Constraint {
            name: "c".into(),
            label: "P".into(),
            properties: vec!["email".into()],
            constraint_type: cypherlite_model::ConstraintType::Unique,
            property_type: None,
            entity_type: "NODE".into(),
            enforced_label: None,
        },
    };
    assert!(graph.estimate_redo_delta(&[classifiable]) > 0);
    assert_eq!(graph.estimate_redo_delta(&[corrupt.clone()]), 0);
    assert!(graph.constraint_key_spec("c").is_none());

    // Seam 2: the admission probe. It checks the slot, finds no key to
    // install, and does not panic.
    let mut admission = graph.begin_constraint_admission();
    admission.admit(&[corrupt.clone()]).unwrap();

    // The preflight rejects the same record with its typed storage error
    // before apply can reach the classification.
    let record = [corrupt];
    let err = graph
        .validate_redo_group_applicability([(&record[..], 1)])
        .unwrap_err();
    assert!(matches!(err, Error::Storage(_)));
}
