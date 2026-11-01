//! The reference model: ADR-0439's admission semantics re-derived from the
//! ADR text, independent of [`SlotSimulation`](super::super::simulation) and
//! the group overlay.
//!
//! `committed_*` is the frozen group-start graph the admission probe reads;
//! `final_*` is the same graph with the accepted slots applied in order
//! (what `apply_redo` will produce). The model does not mirror
//! `ConstraintKeyScope`: scope filtering only skips entity views outside
//! every effective constraint, and a constraint outside a spec's label/key
//! cannot change that spec's tuple.

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::Value;

use super::super::{EntityId, KeyTuple};
use super::generate::{ConstraintDesc, TestOp, VALUE_ALPHABET};
use crate::graph::Graph;

/// One model-level value. `Null` and `Nan` are representable (a tuple
/// component that makes the whole tuple unkeyable) but never appear inside a
/// model tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MValue {
    Int(i64),
    Null,
    Nan,
}

impl MValue {
    pub(super) fn to_value(self) -> Value {
        match self {
            Self::Int(value) => Value::Int(value),
            Self::Null => Value::Null,
            Self::Nan => Value::Float(f64::NAN),
        }
    }

    /// The tuple component this value contributes, or `None` when it makes
    /// the tuple unkeyable (null never equals, NaN never equals itself).
    fn tuple_component(self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(value),
            Self::Null | Self::Nan => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum MEntity {
    Node(i64),
    Rel(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MNode {
    labels: BTreeSet<&'static str>,
    props: BTreeMap<&'static str, MValue>,
}

impl MNode {
    fn new(labels: &[&'static str], props: &[(&'static str, MValue)]) -> Self {
        Self {
            labels: labels.iter().copied().collect(),
            props: props.iter().copied().collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MRel {
    rel_type: &'static str,
    props: BTreeMap<&'static str, MValue>,
}

impl MRel {
    fn new(rel_type: &'static str, props: &[(&'static str, MValue)]) -> Self {
        Self {
            rel_type,
            props: props.iter().copied().collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MEntityState {
    Node(MNode),
    Rel(MRel),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MAdmissionSpec {
    spec: ConstraintDesc,
    new_in_group: bool,
}

type EntityStates = BTreeMap<MEntity, (Option<MEntityState>, Option<MEntityState>)>;

/// The slot's folded DDL state: the effective specs, the names installed this
/// slot, and the names it dropped.
type FoldedSpecs = (
    BTreeMap<&'static str, MAdmissionSpec>,
    BTreeSet<&'static str>,
    BTreeSet<&'static str>,
);

pub(super) struct Model {
    committed_nodes: BTreeMap<i64, MNode>,
    committed_rels: BTreeMap<i64, MRel>,
    specs: BTreeMap<&'static str, MAdmissionSpec>,
    /// Committed graph plus every accepted slot applied in order. Its holder
    /// sets are the engine's committed map composed with the group overlay,
    /// so the model needs no separate overlay channel.
    final_nodes: BTreeMap<i64, MNode>,
    final_rels: BTreeMap<i64, MRel>,
}

#[derive(Default)]
pub(super) struct MPending {
    drops: BTreeSet<&'static str>,
    installs: BTreeMap<&'static str, MAdmissionSpec>,
}

struct Touched {
    entity: MEntity,
    before: Option<MEntityState>,
    after: Option<MEntityState>,
}

impl Model {
    pub(super) fn from_prefix(prefix: &[TestOp]) -> Self {
        let mut nodes = BTreeMap::new();
        let mut rels = BTreeMap::new();
        let mut specs = BTreeMap::new();
        for op in prefix {
            if let Some(spec) = op.constraint_desc() {
                specs.insert(
                    spec.name,
                    MAdmissionSpec {
                        spec,
                        new_in_group: false,
                    },
                );
                continue;
            }
            if let TestOp::DropConstraint { name } = op {
                specs.remove(name);
                continue;
            }
            // Every remaining op is an entity op (the DDL variants were
            // handled above); `apply_to_state`'s `entity_of` guard no-ops
            // anything else.
            apply_to_state(&mut nodes, &mut rels, op);
        }
        Self {
            committed_nodes: nodes.clone(),
            committed_rels: rels.clone(),
            specs,
            final_nodes: nodes,
            final_rels: rels,
        }
    }

    fn committed_state(&self, entity: MEntity) -> Option<MEntityState> {
        live_state(&self.committed_nodes, &self.committed_rels, entity)
    }

    fn final_holders(&self, spec: &ConstraintDesc, tuple: &[i64]) -> BTreeSet<MEntity> {
        holders_in(&self.final_nodes, &self.final_rels, spec, tuple)
    }

    /// Fold `ops` into first-touch-ordered `(before, after)` state, seeded
    /// from the committed graph. The one simulation fold the slot view and
    /// the install replay share.
    fn fold_states<'a>(
        &self,
        ops: impl IntoIterator<Item = &'a TestOp>,
    ) -> (EntityStates, Vec<MEntity>) {
        let mut states: EntityStates = EntityStates::new();
        let mut order: Vec<MEntity> = Vec::new();
        for op in ops {
            let Some(entity) = entity_of(op) else {
                continue;
            };
            match states.entry(entity) {
                std::collections::btree_map::Entry::Occupied(entry) => {
                    apply_entity_op(&mut entry.into_mut().1, op);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let before = self.committed_state(entity);
                    order.push(entity);
                    apply_entity_op(&mut entry.insert((before.clone(), before)).1, op);
                }
            }
        }
        (states, order)
    }

    /// The slot's net per-entity state, computed over the committed graph.
    /// `from == to` on the full state is equivalent to the engine's
    /// per-constraint `from == to`: a change no effective constraint can see
    /// yields equal tuples under every spec.
    fn simulate_slot(&self, ops: &[TestOp]) -> Vec<Touched> {
        let (mut states, order) = self.fold_states(ops.iter());
        order
            .into_iter()
            .filter_map(|entity| {
                let (before, after) = states.remove(&entity).expect("touched entity has state");
                (before != after).then_some(Touched {
                    entity,
                    before,
                    after,
                })
            })
            .collect()
    }

    /// The conservative install check: replay every earlier accepted slot
    /// against the committed graph, and reject when an entity would leave
    /// the slot carrying a tuple the installing constraint can see.
    fn check_install(&self, spec: &ConstraintDesc, earlier: &[&[TestOp]]) -> Result<(), ()> {
        let (states, _) = self.fold_states(earlier.iter().flat_map(|slot| slot.iter()));
        for (before, after) in states.values() {
            let from = state_tuple(before, spec);
            let to = state_tuple(after, spec);
            if to.is_some() && from != to {
                return Err(());
            }
        }
        Ok(())
    }

    /// Whether introducing `tuple` for `entity` would give it a second
    /// holder. `final_*` is committed plus every accepted slot, i.e. the
    /// engine's map composed with the group overlay; this slot's frees
    /// release their holders, and an earlier same-slot introducer is one.
    fn introduction_conflicts(
        &self,
        spec: &ConstraintDesc,
        tuple: &[i64],
        entity: MEntity,
        freed: &BTreeSet<MEntity>,
        introduced: &BTreeSet<MEntity>,
    ) -> bool {
        self.final_holders(spec, tuple)
            .into_iter()
            .filter(|holder| !freed.contains(holder))
            .chain(introduced.iter().copied())
            .any(|holder| holder != entity)
    }

    pub(super) fn check_slot(
        &self,
        ops: &[TestOp],
        earlier: &[&[TestOp]],
        final_slot: bool,
    ) -> Result<MPending, ()> {
        if self.specs.is_empty() && !ops.iter().any(TestOp::is_ddl) {
            return Ok(MPending::default());
        }
        if final_slot && !ops.iter().any(TestOp::needs_admission) {
            return Ok(MPending::default());
        }

        let (effective, installed_this_slot, drops) = self.fold_effective_specs(ops, earlier)?;
        self.slot_pending(effective, installed_this_slot, drops, ops)
    }

    /// The slot's DDL phase: the effective set (committed specs, mutated in
    /// op order), the names the slot installed, and the names it dropped.
    fn fold_effective_specs(
        &self,
        ops: &[TestOp],
        earlier: &[&[TestOp]],
    ) -> Result<FoldedSpecs, ()> {
        let mut effective = self.specs.clone();
        let mut installed_this_slot: BTreeSet<&'static str> = BTreeSet::new();
        let mut drops: BTreeSet<&'static str> = BTreeSet::new();
        for op in ops {
            if let Some(spec) = op.constraint_desc() {
                if installed_this_slot.contains(spec.name) {
                    continue;
                }
                if effective
                    .get(spec.name)
                    .is_some_and(|existing| existing.spec == spec)
                {
                    continue;
                }
                let existed_before = self
                    .specs
                    .get(spec.name)
                    .is_some_and(|existing| existing.spec == spec);
                self.check_install(&spec, earlier)?;
                drops.remove(spec.name);
                installed_this_slot.insert(spec.name);
                effective.insert(
                    spec.name,
                    MAdmissionSpec {
                        spec,
                        new_in_group: !existed_before,
                    },
                );
                continue;
            }
            if let TestOp::DropConstraint { name } = op {
                effective.remove(*name);
                installed_this_slot.remove(*name);
                drops.insert(*name);
            }
        }
        Ok((effective, installed_this_slot, drops))
    }

    /// The slot's transition phase: net frees and introductions over the
    /// effective set, the introduction-conflict policy, and the pending
    /// delta.
    fn slot_pending(
        &self,
        effective: BTreeMap<&'static str, MAdmissionSpec>,
        installed_this_slot: BTreeSet<&'static str>,
        drops: BTreeSet<&'static str>,
        ops: &[TestOp],
    ) -> Result<MPending, ()> {
        let touched = self.simulate_slot(ops);
        let mut frees: Vec<(&'static str, Vec<i64>, MEntity)> = Vec::new();
        let mut introductions: Vec<(&'static str, Vec<i64>, MEntity, &ConstraintDesc)> = Vec::new();
        for Touched {
            entity,
            before,
            after,
        } in &touched
        {
            for (&name, admission) in &effective {
                if installed_this_slot.contains(name) {
                    continue;
                }
                let spec = &admission.spec;
                let from = state_tuple(before, spec);
                let to = state_tuple(after, spec);
                if from == to {
                    continue;
                }
                if let Some(tuple) = from {
                    frees.push((name, tuple, *entity));
                }
                if let Some(tuple) = to {
                    if admission.new_in_group {
                        return Err(());
                    }
                    introductions.push((name, tuple, *entity, spec));
                }
            }
        }

        let mut freed: BTreeMap<(&'static str, Vec<i64>), BTreeSet<MEntity>> = BTreeMap::new();
        for (name, tuple, entity) in frees {
            freed.entry((name, tuple)).or_default().insert(entity);
        }
        let mut introduced: BTreeMap<(&'static str, Vec<i64>), BTreeSet<MEntity>> = BTreeMap::new();
        let empty = BTreeSet::new();
        for (name, tuple, entity, spec) in introductions {
            let key = (name, tuple.clone());
            let freed_here = freed.get(&key).unwrap_or(&empty);
            let introduced_here = introduced.get(&key).unwrap_or(&empty);
            if self.introduction_conflicts(spec, &tuple, entity, freed_here, introduced_here) {
                return Err(());
            }
            introduced.entry(key).or_default().insert(entity);
        }
        let installs = effective
            .into_iter()
            .filter(|(name, _)| installed_this_slot.contains(*name))
            .collect();
        Ok(MPending { drops, installs })
    }

    pub(super) fn accept(&mut self, pending: MPending) {
        let MPending { drops, installs } = pending;
        for name in drops {
            self.specs.remove(name);
        }
        for (name, admission) in installs {
            self.specs.insert(name, admission);
        }
    }

    pub(super) fn apply_slot_to_final(&mut self, ops: &[TestOp]) {
        for op in ops {
            apply_to_state(&mut self.final_nodes, &mut self.final_rels, op);
        }
    }

    /// Compare the engine's committed map against the model's final holder
    /// sets. Every generated constraint is single-property, and the keyable
    /// values are exactly [`VALUE_ALPHABET`] (plus the unkeyable null/NaN,
    /// which contributes no tuple), so enumerating the alphabet per
    /// component covers every tuple the group can occupy.
    pub(super) fn compare_map(&self, graph: &Graph) -> Result<(), String> {
        let engine_specs = graph.constraint_key_specs();
        // The union of both sides' names, derived — never a hardcoded copy of
        // the generator's vocabulary, which would silently skip a renamed or
        // new constraint (parallel state over the name set).
        let mut names: Vec<String> = self.specs.keys().map(|key| key.to_string()).collect();
        names.extend(engine_specs.iter().map(|spec| spec.name.clone()));
        names.sort();
        names.dedup();
        for name in &names {
            let expected_spec = self
                .specs
                .get(name.as_str())
                .map(|admission| &admission.spec);
            let engine_spec = engine_specs.iter().find(|spec| spec.name == *name);
            match (expected_spec, engine_spec) {
                (None, None) => {}
                (Some(expected), Some(engine)) => {
                    // The engine retains only interned ids; resolve the
                    // model's names through the same interner.
                    let same_schema = expected.relationship == engine.is_relationship()
                        && graph.interner.get_id(expected.label) == Some(engine.label_id())
                        && expected.properties.len() == engine.property_ids.len()
                        && expected
                            .properties
                            .iter()
                            .zip(engine.property_ids.iter())
                            .all(|(model, engine)| graph.interner.get_id(model) == Some(*engine));
                    if !same_schema {
                        return Err(format!(
                            "constraint {name}: schema drift; model {expected:?}, engine \
                             relationship={} label_id={} property_ids={:?}",
                            engine.is_relationship(),
                            engine.label_id(),
                            engine.property_ids
                        ));
                    }
                }
                (Some(expected), None) => {
                    return Err(format!(
                        "constraint {name}: model maintains {expected:?}, engine maintains none"
                    ));
                }
                (None, Some(_)) => {
                    return Err(format!(
                        "constraint {name}: engine maintains a constraint the model dropped"
                    ));
                }
            }
            let Some(spec) = expected_spec else {
                continue;
            };
            for tuple in tuples_over(spec.properties.len()) {
                let expected = self.final_holders(spec, &tuple);
                let engine: BTreeSet<MEntity> = graph
                    .constraint_key_holders(name.as_str(), &engine_tuple(&tuple))
                    .into_iter()
                    .map(m_entity)
                    .collect();
                if expected != engine {
                    return Err(format!(
                        "constraint {name} tuple {tuple:?}: model holders {expected:?} != \
                         engine holders {engine:?}"
                    ));
                }
                if engine.len() > 1 {
                    return Err(format!(
                        "constraint {name} tuple {tuple:?}: {} holders; admission must not \
                         install a second",
                        engine.len()
                    ));
                }
            }
        }
        Ok(())
    }
}

fn node_tuple(node: &MNode, spec: &ConstraintDesc) -> Option<Vec<i64>> {
    if !node.labels.contains(spec.label) {
        return None;
    }
    spec.properties
        .iter()
        .map(|key| {
            node.props
                .get(key)
                .and_then(|value| value.tuple_component())
        })
        .collect()
}

fn rel_tuple(rel: &MRel, spec: &ConstraintDesc) -> Option<Vec<i64>> {
    if rel.rel_type != spec.label {
        return None;
    }
    spec.properties
        .iter()
        .map(|key| rel.props.get(key).and_then(|value| value.tuple_component()))
        .collect()
}

/// Every entity in a model graph pair that holds `tuple` under `spec` — the
/// one holder scan the committed and final views share.
fn holders_in(
    nodes: &BTreeMap<i64, MNode>,
    rels: &BTreeMap<i64, MRel>,
    spec: &ConstraintDesc,
    tuple: &[i64],
) -> BTreeSet<MEntity> {
    let mut holders = BTreeSet::new();
    if spec.relationship {
        for (id, rel) in rels {
            if rel_tuple(rel, spec).as_deref() == Some(tuple) {
                holders.insert(MEntity::Rel(*id));
            }
        }
    } else {
        for (id, node) in nodes {
            if node_tuple(node, spec).as_deref() == Some(tuple) {
                holders.insert(MEntity::Node(*id));
            }
        }
    }
    holders
}

fn state_tuple(state: &Option<MEntityState>, spec: &ConstraintDesc) -> Option<Vec<i64>> {
    match state {
        Some(MEntityState::Node(node)) if !spec.relationship => node_tuple(node, spec),
        Some(MEntityState::Rel(rel)) if spec.relationship => rel_tuple(rel, spec),
        _ => None,
    }
}

/// The live state of `entity` in a model graph pair, cloned — the projection
/// the map view ([`apply_to_state`]) and the slot view ([`apply_entity_op`])
/// share.
fn live_state(
    nodes: &BTreeMap<i64, MNode>,
    rels: &BTreeMap<i64, MRel>,
    entity: MEntity,
) -> Option<MEntityState> {
    match entity {
        MEntity::Node(id) => nodes.get(&id).map(|node| MEntityState::Node(node.clone())),
        MEntity::Rel(id) => rels.get(&id).map(|rel| MEntityState::Rel(rel.clone())),
    }
}

/// Write `state` back into a model graph pair: a live state replaces the
/// entity, `None` removes it — one encoding of non-existence for both
/// views.
fn store_state(
    nodes: &mut BTreeMap<i64, MNode>,
    rels: &mut BTreeMap<i64, MRel>,
    entity: MEntity,
    state: Option<MEntityState>,
) {
    match (entity, state) {
        (MEntity::Node(id), Some(MEntityState::Node(node))) => {
            nodes.insert(id, node);
        }
        (MEntity::Node(id), None) => {
            nodes.remove(&id);
        }
        (MEntity::Rel(id), Some(MEntityState::Rel(rel))) => {
            rels.insert(id, rel);
        }
        (MEntity::Rel(id), None) => {
            rels.remove(&id);
        }
        _ => unreachable!("an entity's state carries its own kind"),
    }
}

/// Apply one entity op to a model graph pair — the map-vs-slot adapter:
/// project the live state, apply the one op policy, write it back. Ops on a
/// missing or deleted entity are no-ops, matching the engine's `None`
/// seeding.
fn apply_to_state(nodes: &mut BTreeMap<i64, MNode>, rels: &mut BTreeMap<i64, MRel>, op: &TestOp) {
    let Some(entity) = entity_of(op) else {
        return;
    };
    let mut state = live_state(nodes, rels, entity);
    apply_entity_op(&mut state, op);
    store_state(nodes, rels, entity, state);
}

/// Apply one entity op to a simulation's `after` state. Ops on a missing or
/// deleted entity are no-ops, matching the engine's `None` seeding.
fn apply_entity_op(after: &mut Option<MEntityState>, op: &TestOp) {
    match op {
        TestOp::CreateNode { labels, props, .. } => {
            *after = Some(MEntityState::Node(MNode::new(labels, props)));
        }
        TestOp::CreateRel {
            rel_type, props, ..
        } => {
            *after = Some(MEntityState::Rel(MRel::new(rel_type, props)));
        }
        TestOp::DeleteNode { .. } | TestOp::DeleteRel { .. } => {
            *after = None;
        }
        TestOp::AddLabels { labels, .. } => {
            if let Some(MEntityState::Node(node)) = after {
                node.labels.extend(labels.iter().copied());
            }
        }
        TestOp::RemoveLabels { labels, .. } => {
            if let Some(MEntityState::Node(node)) = after {
                for label in labels {
                    node.labels.remove(label);
                }
            }
        }
        TestOp::SetNodeProp { key, value, .. } => {
            if let Some(MEntityState::Node(node)) = after {
                node.props.insert(*key, *value);
            }
        }
        TestOp::RemoveNodeProp { key, .. } => {
            if let Some(MEntityState::Node(node)) = after {
                node.props.remove(*key);
            }
        }
        TestOp::SetRelProp { key, value, .. } => {
            if let Some(MEntityState::Rel(rel)) = after {
                rel.props.insert(*key, *value);
            }
        }
        TestOp::RemoveRelProp { key, .. } => {
            if let Some(MEntityState::Rel(rel)) = after {
                rel.props.remove(*key);
            }
        }
        TestOp::CreateConstraint { .. } | TestOp::DropConstraint { .. } => {}
    }
}

fn entity_of(op: &TestOp) -> Option<MEntity> {
    match op {
        TestOp::CreateNode { id, .. }
        | TestOp::AddLabels { id, .. }
        | TestOp::RemoveLabels { id, .. }
        | TestOp::SetNodeProp { id, .. }
        | TestOp::RemoveNodeProp { id, .. }
        | TestOp::DeleteNode { id } => Some(MEntity::Node(*id)),
        TestOp::CreateRel { id, .. }
        | TestOp::SetRelProp { id, .. }
        | TestOp::RemoveRelProp { id, .. }
        | TestOp::DeleteRel { id } => Some(MEntity::Rel(*id)),
        TestOp::CreateConstraint { .. } | TestOp::DropConstraint { .. } => None,
    }
}

pub(super) fn tuples_over(arity: usize) -> Vec<Vec<i64>> {
    let mut tuples = vec![Vec::new()];
    for _ in 0..arity {
        let mut next = Vec::with_capacity(tuples.len() * VALUE_ALPHABET.len());
        for prefix in &tuples {
            for value in VALUE_ALPHABET {
                let mut tuple = prefix.clone();
                tuple.push(value);
                next.push(tuple);
            }
        }
        tuples = next;
    }
    tuples
}

fn engine_tuple(values: &[i64]) -> KeyTuple {
    super::super::tests::tuple(values.iter().map(|value| Value::Int(*value)))
}

fn m_entity(entity: EntityId) -> MEntity {
    match entity {
        EntityId::Node(id) => MEntity::Node(id),
        EntityId::Rel(id) => MEntity::Rel(id),
    }
}
