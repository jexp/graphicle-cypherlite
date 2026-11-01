//! ADR-0439: the maintained committed per-constraint tuple map. The
//! `Graph` struct (graph/mod.rs) keeps the map field; this file owns its
//! vocabulary, charge family, and backfill/rebuild lifecycle. The family is
//! a directory: the op classification, the constrained `KeyView`, and the
//! `SlotSimulation` live in `simulation`; the commit-loop admission probe
//! lives in [`admission`]; the entry-growth estimate lives in [`estimate`];
//! the batch DDL accumulator lives in [`ddl`]; and the `apply_redo`-seam
//! maintenance lives in [`seams`] (the
//! `degree_stats_estimate.rs` split precedent: the family outgrew one file).
//! Dependencies run one way — the admission and estimate sides import this
//! module's vocabulary; this module never names their types.
//!
//! The map is eagerly current state, unlike the lazily built postings in
//! [`Indexes`](super::index::Indexes): one entry per live constrained entity
//! per uniqueness (`unique`/`key`) constraint, keyed by the constraint name and
//! the full tuple in the constraint's property order, each component built
//! with [`GroupKey::for_property_index`] (ADR-0172 §3, ADR-0184,
//! ADR-0185). The value records the live holders; a second holder is a
//! pre-existing violation recorded by replay, never installed by admission.

pub(crate) mod admission;
mod ddl;
mod estimate;
mod seams;
mod simulation;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use cypherlite_catalog::ConstraintDef;
use cypherlite_core::{NodeId, RelId, Value};
use cypherlite_model::{Constraint, ConstraintType};

use crate::group_key::GroupKey;
use crate::store::{Interner, LiveNode, LiveRel, PayloadKeyIds};

use super::Graph;

/// The live entity holding a constrained tuple. A constraint's entity kind
/// fixes which variant its entries carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EntityId {
    Node(NodeId),
    Rel(RelId),
}

/// One entry's tuple: the full constrained property list in the
/// constraint's declared order.
pub(crate) type KeyTuple = Box<[GroupKey]>;

/// The constrained-value scope of the currently maintained constraints: the
/// union of every effective constraint's label (or relationship type) and
/// property ids. [`ConstraintKeyIndex`] owns and recomputes it from its own
/// spec set on every install, drop, and rebuild; every consumer asks this
/// one value — the admission probe classifies ops against it, the
/// `apply_redo` primitives gate their before-image on it, and the create
/// seams skip the map entirely when a new entity is outside it. A second
/// derivation per seam is parallel state over one fact: a maintenance gate
/// narrower than the check gate would admit a value the map never records —
/// a false negative, i.e. the duplicate commit this map exists to close
/// (ADR-0439's One scope rule).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConstraintKeyScope {
    pub(super) node_labels: HashSet<u32>,
    pub(super) rel_types: HashSet<u32>,
    pub(super) node_keys: HashSet<u32>,
    pub(super) rel_keys: HashSet<u32>,
}

impl ConstraintKeyScope {
    /// The scope of `specs`, the one derivation every caller uses (the map
    /// on refresh, the admission probe for a candidate or group overlay).
    pub(super) fn from_specs<'a>(specs: impl IntoIterator<Item = &'a KeySpec>) -> Self {
        let mut scope = Self::default();
        for spec in specs {
            scope.add_spec(spec);
        }
        scope
    }

    /// Add one constraint's label/type and property ids to the scope.
    pub(super) fn add_spec(&mut self, spec: &KeySpec) {
        match spec.side {
            SpecSide::Rel { type_id } => {
                self.rel_types.insert(type_id);
                self.rel_keys.extend(spec.property_ids.iter().copied());
            }
            SpecSide::Node { label_id } => {
                self.node_labels.insert(label_id);
                self.node_keys.extend(spec.property_ids.iter().copied());
            }
        }
    }

    /// Whether a node carrying `labels` and `keys` could change a
    /// constrained tuple. Conservative: a constrained label alone qualifies
    /// even when the node lacks a key property — it is in a constraint's
    /// domain, so the apply seam must still read its tuple.
    pub(super) fn touches_node(&self, labels: &[u32], keys: impl IntoIterator<Item = u32>) -> bool {
        self.touches_node_labels(labels) || keys.into_iter().any(|id| self.node_keys.contains(&id))
    }

    /// Whether any of `labels` is constrained — the set predicate every
    /// label seam asks (the single-id form was only ever looped).
    pub(super) fn touches_node_labels(&self, labels: &[u32]) -> bool {
        labels.iter().any(|id| self.node_labels.contains(id))
    }

    /// Whether a relationship of `type_id` carrying `key_ids` could change a
    /// constrained tuple: its type or one of its key ids is constrained.
    pub(super) fn touches_rel(&self, type_id: u32, key_ids: &PayloadKeyIds<'_>) -> bool {
        self.rel_types.contains(&type_id) || key_ids.any(|kid| self.rel_keys.contains(&kid))
    }

    pub(super) fn touches_node_key(&self, key_id: u32) -> bool {
        self.node_keys.contains(&key_id)
    }

    pub(super) fn touches_rel_key(&self, key_id: u32) -> bool {
        self.rel_keys.contains(&key_id)
    }
}

/// Which entity side a constraint's label identifies: the node label or the
/// relationship type. Structural, so a spec cannot carry the wrong kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpecSide {
    Node { label_id: u32 },
    Rel { type_id: u32 },
}

/// One uniqueness constraint's identity and the interned ids its
/// maintenance reads. The names are interned in one place
/// ([`KeySpec::new`]) and only the ids are retained: every seam, the
/// admission probe, and the entry-growth estimate read the ids (the label
/// count readers and the tuple views), so a stored string copy would be a
/// second spelling of the same fact. The entity side is a [`SpecSide`], so
/// membership is structural rather than a boolean beside an id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeySpec {
    pub(crate) name: String,
    pub(crate) side: SpecSide,
    pub(crate) property_ids: Box<[u32]>,
}

impl KeySpec {
    pub(super) fn new(
        interner: &Interner,
        name: String,
        relationship: bool,
        label: String,
        properties: Vec<String>,
    ) -> Self {
        let label_id = interner.intern(&label);
        let side = if relationship {
            SpecSide::Rel { type_id: label_id }
        } else {
            SpecSide::Node { label_id }
        };
        let property_ids: Box<[u32]> = properties
            .iter()
            .map(|property| interner.intern(property))
            .collect();
        Self {
            name,
            side,
            property_ids,
        }
    }

    pub(crate) fn is_relationship(&self) -> bool {
        matches!(self.side, SpecSide::Rel { .. })
    }

    /// The interned label (node) or relationship-type id.
    pub(crate) fn label_id(&self) -> u32 {
        match self.side {
            SpecSide::Node { label_id } => label_id,
            SpecSide::Rel { type_id } => type_id,
        }
    }

    /// The tuple a live node carries for this constraint, or `None` when it
    /// does not carry the label or any component is missing/null/NaN. A
    /// null never equals under `unique` (several nulls coexist), a NaN
    /// never equals itself, and a missing `key` property is the
    /// required-property check, not uniqueness.
    fn live_node_tuple(&self, node: &LiveNode) -> Option<KeyTuple> {
        let SpecSide::Node { label_id } = self.side else {
            return None;
        };
        if !node.labels.contains(&label_id) {
            return None;
        }
        Self::tuple_of(self.property_ids.iter().map(|&kid| node.props.get(kid)))
    }

    /// The tuple a live relationship carries for this constraint — the
    /// relationship twin of [`KeySpec::live_node_tuple`]. Relationship
    /// constraints have no backing-index postings at all, so this map is
    /// their only exact committed-state probe.
    fn live_rel_tuple(&self, interner: &Interner, rel: &LiveRel) -> Option<KeyTuple> {
        let SpecSide::Rel { type_id } = self.side else {
            return None;
        };
        if rel.type_id != type_id {
            return None;
        }
        let mut values: Vec<Option<Value>> = vec![None; self.property_ids.len()];
        rel.payload.for_each_key_value(interner, |kid, value| {
            if let Some(slot) = self.property_ids.iter().position(|&wanted| wanted == kid) {
                values[slot] = Some(value.clone());
            }
        });
        Self::tuple_of(values.iter().map(Option::as_ref))
    }

    /// The one equivalence-key derivation every tuple source routes through:
    /// the caller orders the components (the constraint's declared property
    /// order), and a missing (`None`), null, or NaN component yields no tuple
    /// at all — several nulls coexist under `unique`, a NaN never equals
    /// itself, and a missing `key` component is the required-property check,
    /// not uniqueness. The label/type membership check stays at each call
    /// site; only the values→tuple loop lives here.
    fn tuple_of<'a>(values: impl ExactSizeIterator<Item = Option<&'a Value>>) -> Option<KeyTuple> {
        let mut keys = Vec::with_capacity(values.len());
        for value in values {
            let value = value?;
            if value.is_null() {
                return None;
            }
            keys.push(GroupKey::for_property_index(value)?);
        }
        Some(keys.into_boxed_slice())
    }
}

/// One constraint's entries. `charged` records the bytes each entry was
/// charged so removal refunds exactly what creation charged (the tuple is
/// variable length).
#[derive(Debug)]
struct KeyedConstraint {
    spec: KeySpec,
    entries: HashMap<KeyTuple, TupleEntry>,
}

#[derive(Debug)]
struct TupleEntry {
    holders: Vec<EntityId>,
    charged: u64,
}

impl KeyedConstraint {
    /// Backfill one constraint's entries from the live graph. The construction
    /// every install and the wholesale rebuild share: node backfill reads the
    /// constraint label's `label_index` bucket (an upper bound; each
    /// candidate is liveness-checked), O(L) over the live entities of that
    /// label; a relationship backfill walks `rels` (no relationship-type
    /// index), O(R).
    fn backfilled(graph: &Graph, spec: KeySpec) -> Self {
        let entries = graph.backfill_constraint(&spec);
        Self { spec, entries }
    }

    /// The bytes every entry of this constraint carries: the one spelling of
    /// the per-entry [`TupleEntry::charged`] sum (the install charge, the
    /// removal refund, the rebuild delta and the recount oracle all read it).
    fn charge_total(&self) -> u64 {
        self.entries.values().map(|entry| entry.charged).sum()
    }
}

impl TupleEntry {
    /// The refund releasing `entity`'s hold yields: the whole entry charge
    /// when it is the last holder, otherwise one holder id. `None` when the
    /// entity is not a holder — the caller decides what that means (the map
    /// no-ops, the estimate charges nothing). The last-holder test is
    /// "every holder is this entity", duplicate-robust by construction
    /// (`introduce_entry`, rebuild and install never push the same id twice;
    /// this rule does not rely on that).
    fn refund_for(&self, entity: EntityId) -> Option<u64> {
        if !self.holders.contains(&entity) {
            return None;
        }
        if self.holders.iter().all(|holder| *holder == entity) {
            Some(self.charged)
        } else {
            Some(HOLDER_BYTES)
        }
    }
}

/// The maintained committed map. The entries are current committed state:
/// the admission probe is the only rejecting path, and apply is a state
/// recorder (a duplicate found there increments the holder list instead of
/// failing a durable group).
#[derive(Debug, Default)]
pub(crate) struct ConstraintKeyIndex {
    constraints: BTreeMap<String, KeyedConstraint>,
    /// The one constrained scope of `constraints`, derived from their specs
    /// by [`ConstraintKeyIndex::refresh_scope`] on every mutation. Never
    /// maintained by a consumer.
    scope: ConstraintKeyScope,
}

impl ConstraintKeyIndex {
    fn is_empty(&self) -> bool {
        self.constraints.is_empty()
    }

    /// The constrained scope every seam and the admission probe ask.
    pub(super) fn scope(&self) -> &ConstraintKeyScope {
        &self.scope
    }

    /// Recompute the scope from the specs currently in the map. Private: the
    /// owning mutations ([`install`](Self::install),
    /// [`remove_constraint`](Self::remove_constraint),
    /// [`replace_all`](Self::replace_all)) are the only callers, so no
    /// consumer can leave the scope narrower than the entries it describes.
    fn refresh_scope(&mut self) {
        self.scope = ConstraintKeyScope::from_specs(self.constraints.values().map(|kc| &kc.spec));
    }

    /// The map spec for a name (test and property-model support; production
    /// asks [`specs`](Self::specs) or the entries directly).
    #[cfg(test)]
    pub(super) fn spec(&self, name: &str) -> Option<&KeySpec> {
        self.constraints.get(name).map(|kc| &kc.spec)
    }

    pub(super) fn specs(&self) -> impl Iterator<Item = &KeySpec> {
        self.constraints.values().map(|kc| &kc.spec)
    }

    /// The bytes every entry in the map carries: the one spelling of the
    /// per-constraint [`KeyedConstraint::charge_total`] sum (the rebuild
    /// delta and the recount oracle read it).
    fn charge_total(&self) -> u64 {
        self.constraints
            .values()
            .map(KeyedConstraint::charge_total)
            .sum()
    }

    /// The bytes one constraint's entries are charged. The entry-growth
    /// estimate's snapshot builder names every maintained constraint from
    /// [`specs`](Self::specs), so a miss is map drift, not a query result —
    /// fail loudly (the [`holders`](Self::holders) precedent).
    pub(super) fn charge_of(&self, name: &str) -> u64 {
        self.constraints
            .get(name)
            .expect("a maintained constraint's map entry exists")
            .charge_total()
    }

    /// The refund releasing `entity`'s hold on `tuple` yields; 0 when there
    /// is none. [`TupleEntry::refund_for`] owns the rule — this is the
    /// estimate's read of the same arithmetic `free_entry` applies.
    ///
    /// Two misses are legitimate and distinct from map drift, so neither
    /// panics: a name the current batch installed has no committed row yet
    /// (its entries do not exist, so nothing can be refunded — the estimate's
    /// effective set names it), and a committed constraint whose tuple is
    /// unoccupied has no entry (the probe only asks for a tuple the entity's
    /// before-image carried).
    pub(super) fn refund_for(&self, name: &str, tuple: &KeyTuple, entity: EntityId) -> u64 {
        let Some(kc) = self.constraints.get(name) else {
            return 0;
        };
        kc.entries
            .get(tuple)
            .and_then(|entry| entry.refund_for(entity))
            .unwrap_or(0)
    }

    /// Install (or re-install) one backfilled constraint: refund the previous
    /// entries under the same name, insert the new ones, and refresh the
    /// scope. Returns the refunded bytes. The one install spelling — the
    /// caller (the `Graph` install seam) never touches `constraints` or the
    /// scope itself.
    fn install(&mut self, kc: KeyedConstraint) -> u64 {
        let name = kc.spec.name.clone();
        let refunded = self.take(&name);
        self.constraints.insert(name, kc);
        self.refresh_scope();
        refunded
    }

    /// Replace the whole map with a freshly backfilled set, refreshing the
    /// scope once over the new set. A rebuild path only.
    fn replace_all(&mut self, kcs: impl IntoIterator<Item = KeyedConstraint>) {
        self.constraints = kcs
            .into_iter()
            .map(|kc| (kc.spec.name.clone(), kc))
            .collect();
        self.refresh_scope();
    }

    /// Remove one constraint's entries and return their charge, without
    /// refreshing the scope — [`install`](Self::install) re-inserts under the
    /// same name and refreshes once.
    fn take(&mut self, name: &str) -> u64 {
        self.constraints
            .remove(name)
            .map(|kc| kc.charge_total())
            .unwrap_or(0)
    }

    /// The holders of one tuple. The constraint name is invariant-bound: it
    /// comes from this map's own spec list, so a missing entry is map drift,
    /// not a query result — fail loudly. A missing *tuple* is not drift (the
    /// tuple is simply unoccupied) and answers empty.
    pub(super) fn holders(&self, name: &str, tuple: &KeyTuple) -> Vec<EntityId> {
        self.constraints
            .get(name)
            .expect("a maintained constraint's map entry exists")
            .entries
            .get(tuple)
            .map(|entry| entry.holders.clone())
            .unwrap_or_default()
    }

    /// Introduce `entity` as a holder of `tuple` in `kc`; returns the charge
    /// delta. A re-assert by an existing holder is a no-op, never a
    /// self-collision.
    fn introduce_entry(kc: &mut KeyedConstraint, tuple: KeyTuple, entity: EntityId) -> i64 {
        match kc.entries.get_mut(&tuple) {
            Some(entry) => {
                if entry.holders.contains(&entity) {
                    return 0;
                }
                entry.holders.push(entity);
                entry.charged = entry.charged.saturating_add(HOLDER_BYTES);
                HOLDER_BYTES as i64
            }
            None => {
                let charged = entry_charge(&tuple, 1);
                kc.entries.insert(
                    tuple,
                    TupleEntry {
                        holders: vec![entity],
                        charged,
                    },
                );
                charged as i64
            }
        }
    }

    /// Record one entity's state transition on one constraint: free the
    /// `from` tuple, introduce the `to` tuple (`None` = the entity holds no
    /// tuple on that side), refunding the free and charging the introduce.
    /// An unchanged pair is a no-op. The caller walks `values_mut()` and
    /// hands in that constraint, so there is no name lookup and no
    /// missing-constraint case to fall back on.
    fn transition(
        kc: &mut KeyedConstraint,
        entity: EntityId,
        from: Option<KeyTuple>,
        to: Option<KeyTuple>,
    ) -> i64 {
        if from == to {
            return 0;
        }
        let freed = from
            .as_ref()
            .map_or(0, |tuple| Self::free_entry(kc, tuple, entity));
        let introduced = to.map_or(0, |tuple| Self::introduce_entry(kc, tuple, entity));
        freed + introduced
    }

    /// Free one entity's hold on one entry, refunding exactly what the
    /// removal releases ([`TupleEntry::refund_for`] owns that rule). The
    /// transition helper's one removal spelling.
    fn free_entry(kc: &mut KeyedConstraint, tuple: &KeyTuple, entity: EntityId) -> i64 {
        let Some(entry) = kc.entries.get_mut(tuple) else {
            return 0;
        };
        let Some(refund) = entry.refund_for(entity) else {
            return 0;
        };
        entry.holders.retain(|holder| *holder != entity);
        if entry.holders.is_empty() {
            kc.entries.remove(tuple);
        } else {
            entry.charged = entry.charged.saturating_sub(HOLDER_BYTES);
        }
        -(refund as i64)
    }

    fn remove_constraint(&mut self, name: &str) -> u64 {
        let refunded = self.take(name);
        self.refresh_scope();
        refunded
    }
}

/// One entry's fixed overhead (the map slot and allocator floor, the
/// LABS-351 family style). The variable part is the tuple's
/// [`GroupKey::logical_bytes`] plus one id per holder.
pub(crate) const CONSTRAINT_KEY_ENTRY_BYTES: u64 = 48 + 64;
const HOLDER_BYTES: u64 = std::mem::size_of::<EntityId>() as u64;
/// Fixed per-component tuple estimate for the admission path, which prices
/// a backfill before its values are known (the family's floor-estimate
/// style, matching the degree-stat entry constants).
const COMPONENT_ESTIMATE_BYTES: u64 = 32;

/// The bytes one entry with `holders` holders is charged: the map slot and
/// allocator floor plus the tuple's logical bytes plus one entity id per
/// holder. The one spelling the install charge, the backfill charge and the
/// estimate's introduction charge all read; it reads no constraint state, so
/// it sits beside the constants it sums rather than on [`KeySpec`].
fn entry_charge(tuple: &KeyTuple, holders: usize) -> u64 {
    CONSTRAINT_KEY_ENTRY_BYTES
        .saturating_add(tuple.iter().map(GroupKey::logical_bytes).sum())
        .saturating_add((holders as u64).saturating_mul(HOLDER_BYTES))
}

pub(super) fn spec_from_def(interner: &Interner, def: &ConstraintDef) -> Option<KeySpec> {
    // The catalog's kind vocabulary is closed (`Catalog::insert_constraint`
    // rejects anything `ConstraintType` cannot parse), so a catalog-accepted
    // definition always parses; a build that does not is a programming
    // error, not a silent "not uniqueness".
    let kind = ConstraintType::from_str(&def.kind)
        .expect("Catalog::insert_constraint rejects unknown constraint kinds");
    if !kind.enforces_uniqueness() {
        return None;
    }
    Some(KeySpec::new(
        interner,
        def.name.clone(),
        def.relationship,
        def.label.clone(),
        def.properties.clone(),
    ))
}

pub(super) fn spec_from_model(interner: &Interner, constraint: &Constraint) -> Option<KeySpec> {
    if !constraint.constraint_type.enforces_uniqueness() {
        return None;
    }
    // `None` covers both a non-uniqueness kind (above) and an `entity_type`
    // naming neither kind (below). This function is a pre-preflight
    // classifier: the admission loop and the entry-growth estimate call it
    // on the replicated commit path before the WAL preflight runs, so it is
    // total and never panics there. An unclassifiable value yields no key to
    // maintain, and the preflight
    // (`validate_redo_group_applicability` → `RedoPreflight::check_op`)
    // rejects the record with the typed storage error before apply, so no
    // corrupt constraint is ever silently installed.
    // Total classification before the preflight: an unknown `entity_type`
    // means "no key here" (the preflight rejects the record with its typed
    // storage error before apply).
    let relationship = constraint.entity_kind().ok()?.is_relationship();
    Some(KeySpec::new(
        interner,
        constraint.name.clone(),
        relationship,
        constraint.label.clone(),
        constraint.properties.clone(),
    ))
}

impl Graph {
    /// Install (or re-install) a uniqueness constraint's entries: backfill
    /// from the live graph ([`KeyedConstraint::backfilled`]), hand it to the
    /// index, and charge the new entries net of the refund the index returns
    /// for the previous entries of the same name. The index owns the removal,
    /// the insert and the scope refresh.
    pub(super) fn constraint_keys_install(&self, spec: KeySpec) {
        let kc = KeyedConstraint::backfilled(self, spec);
        let charged = kc.charge_total();
        let refunded = self.constraint_keys.lock().unwrap().install(kc);
        self.charge(charged as i64 - refunded as i64);
    }

    /// Drop a constraint's entries and refund their charge.
    pub(super) fn constraint_keys_drop(&self, name: &str) {
        let refunded = self.constraint_keys.lock().unwrap().remove_constraint(name);
        self.charge(-(refunded as i64));
    }

    fn backfill_constraint(&self, spec: &KeySpec) -> HashMap<KeyTuple, TupleEntry> {
        let mut holders: HashMap<KeyTuple, Vec<EntityId>> = HashMap::new();
        if spec.is_relationship() {
            for entry in self.rels.iter() {
                let Some(live) = super::live_version(entry.value()) else {
                    continue;
                };
                if let Some(tuple) = spec.live_rel_tuple(&self.interner, &live.live) {
                    holders
                        .entry(tuple)
                        .or_default()
                        .push(EntityId::Rel(*entry.key()));
                }
            }
        } else {
            let ids: Vec<NodeId> = self
                .indexes
                .label_index
                .get(&spec.label_id())
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default();
            for id in ids {
                let Some(vers) = self.nodes.get(&id) else {
                    continue;
                };
                let Some(live) = super::live_version(&vers) else {
                    continue;
                };
                if let Some(tuple) = spec.live_node_tuple(&live.live) {
                    holders.entry(tuple).or_default().push(EntityId::Node(id));
                }
            }
        }
        holders
            .into_iter()
            .map(|(tuple, holders)| {
                let charged = entry_charge(&tuple, holders.len());
                (tuple, TupleEntry { holders, charged })
            })
            .collect()
    }

    /// Every uniqueness constraint the committed catalog and the theater's
    /// derived set name, catalog rows first and one entry per name.
    fn uniqueness_specs(&self) -> Vec<KeySpec> {
        // Each source's specs are collected inside its own lock scope (the
        // catalog row first, then the theater's derived set); the one dedup
        // step below is shared — a name is installed once.
        let catalog_specs: Vec<KeySpec> = {
            let catalog = self.catalog.lock().unwrap();
            catalog
                .constraints
                .values()
                .filter_map(|def| spec_from_def(&self.interner, def))
                .collect()
        };
        let derived_specs: Vec<KeySpec> = {
            // Live half: a `SetCurrentGraphType` body derives its declared
            // `IS UNIQUE`/`IS KEY` constraints into the theater set, and a
            // replay may carry that op without the per-constraint rows
            // (`sync_derived_constraints` skips one equivalent to a
            // surviving catalog name). No production emitter writes a
            // uniqueness kind into a per-op `CatalogDerivedConstraint`
            // record (see the install note in `apply_redo`), so this loop's
            // reachable shape is the graph type's declared constraints.
            let theater = self.theater.lock().unwrap();
            theater
                .derived_constraints
                .values()
                .filter_map(|constraint| spec_from_model(&self.interner, constraint))
                .collect()
        };
        let mut out = Vec::with_capacity(catalog_specs.len() + derived_specs.len());
        let mut seen = std::collections::HashSet::new();
        for spec in catalog_specs.into_iter().chain(derived_specs) {
            if seen.insert(spec.name.clone()) {
                out.push(spec);
            }
        }
        out
    }

    /// Rebuild the map wholesale from the live graph (open, checkpoint
    /// restore, abort). The map is not checkpointed: it is reproducible, and
    /// its charge is re-tallied by `recompute_used`. Replay-installed
    /// duplicates keep their holder list instead of collapsing to one entry.
    pub(super) fn rebuild_constraint_keys(&self) {
        let mut index = ConstraintKeyIndex::default();
        index.replace_all(
            self.uniqueness_specs()
                .into_iter()
                .map(|spec| KeyedConstraint::backfilled(self, spec)),
        );
        let rebuilt = index.charge_total();
        let mut current = self.constraint_keys.lock().unwrap();
        let previous = current.charge_total();
        *current = index;
        drop(current);
        // Keep the incremental charge in step with the rebuilt entries;
        // `rebuild_indexes` re-tallies the whole account afterwards anyway.
        self.charge(rebuilt as i64 - previous as i64);
    }

    /// The map's share of the retained-structure recount (the account
    /// oracle): each entry carries the bytes it was charged, so the recount
    /// and the incremental charge cannot drift (the #398 lesson).
    pub(super) fn constraint_keys_structure_bytes(&self) -> u64 {
        self.constraint_keys.lock().unwrap().charge_total()
    }
}

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod tests;
