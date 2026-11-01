//! ADR-0439: the admission-path estimate of the maintained constraint-key
//! map's entry growth — the constraint-key term of
//! [`Graph::estimate_redo_delta`], in its own impl-block file (the
//! `degree_stats_estimate.rs` split precedent, a sibling of the map and of
//! the admission engine). This file only prices a not-yet-applied batch; no
//! mutation path lives here.
//!
//! Both halves of the estimate — the batch's DDL and its net per-entity
//! transitions — price the same effective constraint set, driven by the
//! shared [`BatchDdlState`] the admission loop uses (one accumulator, one
//! install/drop policy). The lock discipline is structural: the map lock is
//! held only to snapshot the committed specs, their charges, and the scope,
//! and once more for the per-entity refund queries. It is released across
//! the simulation, which interns DDL names (an interner write) and takes
//! entity guards — the same order the apply seams take (entity guard, then
//! map lock), so the estimate can never invert it.

use std::collections::BTreeMap;

use super::ddl::{net_transitions, BatchDdlState};
use super::simulation::{
    constraint_effect, is_constraint_ddl, ConstraintOp, DdlOp, SlotSimulation,
};
use super::{
    entry_charge, KeySpec, COMPONENT_ESTIMATE_BYTES, CONSTRAINT_KEY_ENTRY_BYTES, HOLDER_BYTES,
};
use crate::graph::Graph;
use crate::redo::RedoOp;

impl Graph {
    /// The admission estimate's entry-growth term: price the entries a
    /// not-yet-applied batch would create or remove, so an over-cap write is
    /// refused at admission instead of after its entries land. A constraint
    /// install derives its entry count from the exact live label / type
    /// count in O(1); the backfill itself still walks the ids.
    ///
    /// Every caller reaches this under the store's publication write guard
    /// (`shared::admit_redo_capacity`), which excludes the apply seams, so
    /// the snapshot below cannot go stale mid-estimate.
    pub(in crate::graph) fn estimate_constraint_key_entry_growth(&self, ops: &[RedoOp]) -> i64 {
        // Snapshot what the estimate prices, then release the map lock: the
        // DDL derivation interns (an interner write) and the simulation takes
        // entity guards, both of which the apply seams take BEFORE the map
        // lock (ADR-0439 lock order). Holding it across them would invert
        // that order.
        let (committed, scope, charges) = {
            let index = self.constraint_keys.lock().unwrap();
            // The no-constraint common case: no maintained spec and no
            // constraint DDL means there is no entry to price — the
            // simulation is skipped and nothing is cloned.
            if index.specs().next().is_none() && !ops.iter().any(is_constraint_ddl) {
                return 0;
            }
            let committed: BTreeMap<String, KeySpec> = index
                .specs()
                .cloned()
                .map(|spec| (spec.name.clone(), spec))
                .collect();
            let charges: BTreeMap<String, u64> = committed
                .keys()
                .map(|name| (name.clone(), index.charge_of(name)))
                .collect();
            (committed, index.scope().clone(), charges)
        };
        // The simulation's scope is the committed one; an in-batch install
        // widens it only for later slots. Whether tuple ops are simulated at
        // all is therefore decided by the committed set.
        let simulate_tuples = !committed.is_empty();
        let mut ddl = BatchDdlState::new(&committed);
        let mut simulation = SlotSimulation::new(self, &scope);
        let mut delta = 0i64;
        for op in ops {
            match constraint_effect(op) {
                ConstraintOp::Ddl(DdlOp::DropConstraint { name }) => {
                    let step = ddl.drop_constraint(name);
                    delta += price_drop(&charges, name, step);
                }
                ConstraintOp::Ddl(ddl_op) => {
                    // A DDL op whose derivation fails installs nothing: the
                    // preflight rejects it before apply. Every other op is
                    // priced from the same `installs` source the admission
                    // loop uses.
                    let Ok(installs) = ddl_op.installs(&self.interner) else {
                        continue;
                    };
                    for spec in installs {
                        let charge = self.estimate_backfill_charge(&spec);
                        let name = spec.name.clone();
                        let Some(step) = ddl.install(spec) else {
                            continue;
                        };
                        delta += price_install(&charges, &name, charge, step);
                    }
                }
                ConstraintOp::Introduce(tuple_op) | ConstraintOp::Free(tuple_op) => {
                    if simulate_tuples {
                        simulation.apply_tuple(tuple_op);
                    }
                }
                ConstraintOp::Irrelevant => {}
            }
        }
        // Price the batch's net per-entity changes (the simulation records
        // one before/after per touched entity; a create-then-delete inside
        // the batch nets to nothing) against the same effective set. The map
        // lock is taken here only — after the simulation released every
        // interner write and entity guard — for the refund queries.
        let touched = simulation.into_touched();
        if !touched.is_empty() {
            let index = self.constraint_keys.lock().unwrap();
            for transition in net_transitions(ddl.effective(), &touched) {
                let name = &transition.spec.name;
                let entity = transition.entity;
                if let Some(tuple) = &transition.from {
                    delta -= index.refund_for(name, tuple, entity) as i64;
                }
                if let Some(tuple) = transition.to {
                    delta += entry_charge(&tuple, 1) as i64;
                }
            }
        }
        delta
    }
}

impl Graph {
    /// The backfill charge one install's entry count bounds: the exact live
    /// count of the constraint's label (or relationship type) times the
    /// per-entry floor estimate. The counts come from the degree-stats
    /// family's canonical exact readers, which answer 0 for a name no entity
    /// carries — no separate interner probe is needed.
    pub(super) fn estimate_backfill_charge(&self, spec: &KeySpec) -> i64 {
        let label = self.interner.get(spec.label_id());
        let count = match spec.side {
            super::SpecSide::Rel { .. } => self.pattern_rel_type_rows(&label),
            super::SpecSide::Node { .. } => self.pattern_label_count_exact(&label),
        };
        let per_entry = CONSTRAINT_KEY_ENTRY_BYTES
            .saturating_add(HOLDER_BYTES)
            .saturating_add(
                (spec.property_ids.len() as u64).saturating_mul(COMPONENT_ESTIMATE_BYTES),
            );
        count.saturating_mul(per_entry) as i64
    }
}

/// The entry-growth delta one real install contributes: its backfill charge
/// minus the committed charge it replaces. The one spelling both the batch
/// estimate and the admission loop's per-slot growth use.
pub(super) fn price_install(
    charges: &BTreeMap<String, u64>,
    name: &str,
    charge: i64,
    step: super::ddl::InstallStep,
) -> i64 {
    let refund = if step.replaced_committed {
        committed_charge(charges, name)
    } else {
        0
    };
    charge - refund
}

/// The entry-growth delta one drop contributes: the committed refund, or 0
/// for a name the batch installed.
pub(super) fn price_drop(
    charges: &BTreeMap<String, u64>,
    name: &str,
    step: super::ddl::DropStep,
) -> i64 {
    if step.refunds_committed {
        -committed_charge(charges, name)
    } else {
        0
    }
}

/// The committed map charge of one constraint. Both callers price a name the
/// effective set says came from the committed snapshot, so a miss is map
/// drift, not a query result — fail loudly (the
/// [`ConstraintKeyIndex::holders`](super::ConstraintKeyIndex::holders)
/// precedent).
fn committed_charge(charges: &BTreeMap<String, u64>, name: &str) -> i64 {
    *charges
        .get(name)
        .expect("a batch DDL refund names a committed constraint") as i64
}
