//! ADR-0439 admission property suite: generated flush groups against an
//! independent reference model.
//!
//! `flush_group` serializes a group under one write guard, so every
//! behaviorally distinct race outcome is a slot ordering. This suite
//! therefore generates a committed prefix plus a random sequence of slots,
//! drives the real [`ConstraintAdmission`](super::admission::ConstraintAdmission)
//! over that sequence, and checks:
//!
//! 1. every slot's `Ok`/`Err(Error::Conflict)` decision matches the model's;
//! 2. after the accepted slots are applied, the maintained committed map's
//!    holder sets match the model's; and
//! 3. no tuple gains a second holder through admission.
//!
//! The model ([`model`]) is an independent re-derivation of ADR-0439's
//! stated semantics (earlier-slot-wins, earlier-removal-frees, re-assert is
//! a no-op, null/NaN/missing -> no tuple, in-group install over an earlier
//! introduction -> conservative conflict, no second holder installed). It
//! never calls into `SlotSimulation` or the group overlay; it reads the same
//! op shapes at the name level and tracks the holder sets itself.
//!
//! Generation ([`generate`]) is constrained to groups the engine's other
//! admission gate (`redo_conflicts_with_later`) accepts: slots own disjoint
//! entities and relationship endpoints are never deleted. ADR-0439 only
//! defines admission semantics for that shape ("two accepted slots never
//! touch the same entity"); generating outside it would test undefined
//! configurations. An in-group DDL gets a slot of its own, so the
//! statement-time validator's same-slot coverage stays out of the admission
//! contract.

use cypherlite_core::{Error, PropertyMap};
use proptest::prelude::*;

use crate::graph::Graph;
use crate::redo::RedoOp;

mod generate;
mod model;

use self::generate::{build_case, TestCase, TestOp, VALUE_ALPHABET};
use self::model::{MValue, Model};

// Default config (256 cases); scale with PROPTEST_CASES per ADR-0268 §3 —
// no explicit `cases` here, which would override the env knob.

fn prop_map(pairs: &[(&'static str, MValue)]) -> PropertyMap {
    super::tests::props(pairs.iter().map(|(key, value)| (*key, value.to_value())))
}

fn to_redo(op: &TestOp) -> RedoOp {
    match op {
        TestOp::CreateNode { id, labels, props } => RedoOp::create_node(
            *id,
            labels
                .iter()
                .map(|label| (*label).to_string())
                .collect::<Vec<_>>(),
            prop_map(props),
        ),
        TestOp::CreateRel {
            id,
            start,
            end,
            rel_type,
            props,
        } => RedoOp::create_rel(*id, *start, *end, (*rel_type).to_string(), prop_map(props)),
        TestOp::DeleteNode { id } => RedoOp::delete_node(*id),
        TestOp::DeleteRel { id } => RedoOp::delete_rel(*id),
        TestOp::SetNodeProp { id, key, value } => {
            RedoOp::set_node_prop(*id, (*key).to_string(), value.to_value())
        }
        TestOp::RemoveNodeProp { id, key } => RedoOp::remove_node_prop(*id, (*key).to_string()),
        TestOp::SetRelProp { id, key, value } => {
            RedoOp::set_rel_prop(*id, (*key).to_string(), value.to_value())
        }
        TestOp::RemoveRelProp { id, key } => RedoOp::remove_rel_prop(*id, (*key).to_string()),
        TestOp::AddLabels { id, labels } => RedoOp::add_labels(
            *id,
            labels.iter().map(|label| (*label).to_string()).collect(),
        ),
        TestOp::RemoveLabels { id, labels } => RedoOp::remove_labels(
            *id,
            labels.iter().map(|label| (*label).to_string()).collect(),
        ),
        TestOp::CreateConstraint {
            name,
            relationship,
            label,
            properties,
        } => super::tests::constraint(name, *relationship, label, properties),
        TestOp::DropConstraint { name } => RedoOp::DropConstraint {
            name: (*name).to_string(),
        },
    }
}

struct EngineRun {
    graph: Graph,
    /// One entry per slot: `true` when admission accepted it.
    results: Vec<bool>,
}

fn drive_engine(case: &TestCase) -> Result<EngineRun, String> {
    let graph = Graph::default();
    let prefix: Vec<RedoOp> = case.prefix.iter().map(to_redo).collect();
    graph
        .apply_redo(prefix, 1)
        .map_err(|error| format!("prefix apply failed: {error:?}"))?;

    let mut admission = graph.begin_constraint_admission();
    let mut accepted: Vec<Vec<RedoOp>> = Vec::new();
    let mut results = Vec::new();
    for (index, slot) in case.slots.iter().enumerate() {
        let ops: Vec<RedoOp> = slot.iter().map(to_redo).collect();
        let position = crate::SlotPosition::of(index, case.slots.len());
        let mut earlier = accepted.iter().map(Vec::as_slice);
        match admission.check_with_earlier(&ops, &mut earlier, position) {
            Ok((pending, _growth)) => {
                admission.accept(pending);
                accepted.push(ops);
                results.push(true);
            }
            Err(Error::Conflict) => results.push(false),
            Err(other) => return Err(format!("slot {index}: unexpected error {other:?}")),
        }
    }

    for (offset, ops) in accepted.into_iter().enumerate() {
        let commit_ts = 2 + offset as u64;
        graph
            .apply_redo(ops, commit_ts)
            .map_err(|error| format!("accepted slot apply failed: {error:?}"))?;
    }

    Ok(EngineRun { graph, results })
}

fn check_case(case: &TestCase) -> Result<(), String> {
    let run = drive_engine(case)?;

    let mut model = Model::from_prefix(&case.prefix);
    let mut accepted: Vec<&[TestOp]> = Vec::new();
    let mut results = Vec::new();
    for (index, slot) in case.slots.iter().enumerate() {
        let final_slot = index + 1 == case.slots.len();
        match model.check_slot(slot, &accepted, final_slot) {
            Ok(pending) => {
                model.accept(pending);
                model.apply_slot_to_final(slot);
                accepted.push(slot.as_slice());
                results.push(true);
            }
            Err(()) => results.push(false),
        }
    }

    if run.results != results {
        return Err(format!(
            "slot decisions differ: engine={:?} model={:?}\ncase={case:#?}",
            run.results, results
        ));
    }
    model.compare_map(&run.graph)
}

fn entropy_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 16..=96)
}

proptest! {
    #![proptest_config(ProptestConfig::default())]

    /// ADR-0439: every generated multi-slot flush group's admission
    /// decisions and resulting committed holder sets match the independent
    /// reference model, and no tuple gains a second holder.
    #[test]
    fn generated_flush_groups_match_the_reference_admission_model(
        entropy in entropy_strategy(),
    ) {
        let case = build_case(&entropy);
        if let Err(message) = check_case(&case) {
            prop_assert!(false, "{}", message);
        }
    }
}

/// The fixed case for ADR-0439's create-registration rule (the Decision's
/// One scope: a create always registers its scope-filtered view): a fresh
/// `:Temp {email: 1}` is pulled under `:Person` by a later `AddLabels` in
/// the same slot, while a committed `:Person {email: 1}` holds the tuple.
/// The create registration rejects it; a label-gated `tracks_tuple` would
/// accept it. Pinned as a fixed case so the mutation check is deterministic.
#[test]
fn create_then_label_repro_is_rejected_by_engine_and_model() {
    let case = TestCase {
        prefix: vec![
            TestOp::CreateNode {
                id: 0,
                labels: vec!["P"],
                props: vec![("k", MValue::Int(0))],
            },
            TestOp::CreateConstraint {
                name: "c",
                relationship: false,
                label: "P",
                properties: vec!["k"],
            },
        ],
        slots: vec![vec![
            TestOp::CreateNode {
                id: 1,
                labels: vec!["T"],
                props: vec![("k", MValue::Int(0))],
            },
            TestOp::AddLabels {
                id: 1,
                labels: vec!["P"],
            },
        ]],
    };

    let run = drive_engine(&case).expect("engine harness");
    assert_eq!(
        run.results,
        vec![false],
        "the committed holder must reject the create-then-label slot"
    );
    check_case(&case).expect("the reference model agrees with the engine");
}

/// The checker enumerates the WHOLE shared alphabet: a future partial slice
/// (e.g. `&VALUE_ALPHABET[..4]` after a widening) would otherwise false-green
/// compare_map — the exact hole the shared constant exists to close, turned
/// loud here instead of relying on the doc's claim.
#[test]
fn tuples_over_enumerates_the_whole_value_alphabet() {
    let singles: Vec<Vec<i64>> = VALUE_ALPHABET.iter().map(|v| vec![*v]).collect();
    assert_eq!(model::tuples_over(1), singles);
}
