//! ADR-0439: the commit-time uniqueness admission engine — the group-local
//! tuple overlay and the slot DDL accumulator both the loop and the
//! entry-growth estimate drive. The op classification, the constrained view,
//! and the entity simulation it drives live in [`super::simulation`]; the
//! entry-growth estimate lives in [`super::estimate`]; the batch DDL
//! accumulator both drive lives in [`super::ddl`]; the map itself and its
//! `apply_redo`-seam maintenance live in the parent [`super`] (the family is
//! a directory: the `degree_stats_estimate.rs` split precedent).
//!
//! Dependencies run one way: this module imports the map's vocabulary and
//! the simulation; the map never names either.

use std::collections::{BTreeMap, HashMap, HashSet};

use cypherlite_core::{Error, Result};

use super::ddl::{net_transitions, AdmissionSpec, BatchDdlState, Origin};
use super::entry_charge;
use super::estimate::{price_drop, price_install};
use super::simulation::{
    constraint_effect, is_constraint_ddl, ConstraintOp, DdlOp, SlotSimulation,
};
use super::{ConstraintKeyIndex, ConstraintKeyScope, EntityId, KeySpec, KeyTuple};
use crate::graph::Graph;
use crate::redo::RedoOp;

/// The identity of one constrained tuple in the group overlay: the interned
/// constraint name (one entry in [`NameTable`]) and the full tuple. Holds are
/// keyed by the id, so the per-op loops never clone a constraint name.
type TupleId = (u32, KeyTuple);

/// One entity's net hold on a constrained tuple in the group overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// The entity introduced the tuple: it is a holder.
    Introduced,
    /// The entity freed the tuple: a later slot may take the value.
    Freed,
}

/// The group overlay's per-constraint holds: for each tuple, every touched
/// entity's net [`Hold`].
type TupleHolds = HashMap<TupleId, HashMap<EntityId, Hold>>;

/// The group's interned constraint names. A hold outlives the slot that
/// created it, so the table is group-scoped; `names` is the reverse lookup
/// the committed-holder probe needs. Interning once per name replaces a
/// `String` clone per touched entity per op.
#[derive(Debug, Default)]
struct NameTable {
    names: Vec<String>,
    ids: HashMap<String, u32>,
}

impl NameTable {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    /// The id interned for `name`, if any — the name↔id bijection's reader.
    fn lookup(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    fn name(&self, id: u32) -> &str {
        self.names
            .get(id as usize)
            .map(String::as_str)
            .expect("a tuple hold names a constraint interned in this group")
    }
}

/// A checked-but-not-yet-accepted slot's overlay delta. The commit loop
/// merges it via [`ConstraintAdmission::accept`] only after the capacity
/// and timestamp decisions pass, so a slot rejected by either leaves no
/// tuple holds or constraint-set changes behind.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct PendingAdmission {
    drops: HashSet<String>,
    installs: BTreeMap<String, AdmissionSpec>,
    holds: TupleHolds,
}

/// A slot's position in its group. Only the last slot has no later reader,
/// so only it may skip recording overlay frees (the bulk `DETACH DELETE`
/// shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum SlotPosition {
    /// A later slot follows; its frees must stay visible to that slot.
    Followed,
    /// The group's last slot; nothing reads its overlay afterwards.
    Final,
}

impl SlotPosition {
    /// The position of `index` in a group of `len` slots.
    #[doc(hidden)]
    pub fn of(index: usize, len: usize) -> Self {
        if index + 1 == len {
            Self::Final
        } else {
            Self::Followed
        }
    }
}

/// The group-local admission state: the effective constraint set plus the
/// tuple holds earlier accepted slots established. Driven slot by slot by
/// [`Graph::begin_constraint_admission`]. Internal commit pipeline seam
/// (ADR-0439), not advertised client API.
#[doc(hidden)]
pub struct ConstraintAdmission<'g> {
    graph: &'g Graph,
    specs: BTreeMap<String, AdmissionSpec>,
    /// The committed constrained scope, snapshotted from the map at group
    /// start and widened by any constraint installed earlier in the group.
    /// The same one value the apply seams ask — never re-derived here.
    scope: ConstraintKeyScope,
    /// The committed per-constraint entry charges, snapshotted with the
    /// specs at group start: the admission-side entry-growth pricing reads
    /// them so the slot is classified once (the capacity check reuses the
    /// delta instead of re-walking the ops).
    charges: BTreeMap<String, u64>,
    /// The group's interned constraint names, so a tuple hold carries an id
    /// instead of a cloned name.
    names: NameTable,
    /// Per (name id, tuple), each entity's net hold after the earlier
    /// accepted slots.
    holds: TupleHolds,
}

impl<'g> ConstraintAdmission<'g> {
    /// Check and accept one slot: the convenience form for callers whose
    /// slot is committed unconditionally (tests). The commit loop uses
    /// [`ConstraintAdmission::check_with_earlier`] plus
    /// [`ConstraintAdmission::accept`], so a slot rejected by a later
    /// admission stage (capacity, timestamp overflow) leaves no delta.
    #[cfg(test)]
    pub fn admit(&mut self, ops: &[RedoOp]) -> Result<()> {
        let pending = self.check(ops)?;
        self.accept(pending);
        Ok(())
    }

    /// [`ConstraintAdmission::check_with_earlier`] with no earlier slots.
    #[cfg(test)]
    pub fn check(&mut self, ops: &[RedoOp]) -> Result<PendingAdmission> {
        self.check_with_earlier(ops, &mut std::iter::empty(), SlotPosition::Followed)
            .map(|(pending, _growth)| pending)
    }

    /// Check one slot against the committed map plus the group overlay and
    /// return its delta without merging it. `Err(Error::Conflict)` rejects
    /// the slot. The caller merges via [`ConstraintAdmission::accept`] only
    /// once the slot is truly accepted (after the capacity and timestamp
    /// decisions) — a slot rejected there must not leave tuple holds or
    /// constraint-set changes behind. The check runs under the publication
    /// and WAL guards in commit order.
    #[doc(hidden)]
    /// Returns the slot's pending delta plus its constraint-key entry-growth
    /// estimate: the slot's ops are classified once here, and the capacity
    /// check consumes the growth instead of re-walking them.
    pub fn check_with_earlier<'e>(
        &mut self,
        ops: &[RedoOp],
        mut earlier: impl Iterator<Item = &'e [RedoOp]>,
        position: SlotPosition,
    ) -> Result<(PendingAdmission, i64)> {
        // The no-constraint common case: nothing can conflict and no DDL
        // can install one, so skip the per-op simulation entirely.
        if self.specs.is_empty() && !ops.iter().any(is_constraint_ddl) {
            return Ok((PendingAdmission::default(), 0));
        }
        // The last slot of a group has no later reader: when none of its ops
        // can introduce a constrained tuple and it carries no constraint
        // DDL, its frees need no overlay entry and nothing is left to check
        // (the statement-time validator already validated its own writes).
        // This is the bulk `DETACH DELETE` shape.
        if position == SlotPosition::Final
            && !ops.iter().any(|op| constraint_effect(op).needs_admission())
        {
            return Ok((PendingAdmission::default(), 0));
        }
        let mut simulation = SlotSimulation::new(self.graph, &self.scope);
        let mut slot_holds: TupleHolds = HashMap::new();
        // The slot's DDL state: the effective set (cloned from the group's
        // committed set on the first DDL), the earlier-slot cache (collected
        // only when an install needs it), and the names this slot installed
        // and dropped. One accumulator, one `apply` call per DDL op — no arm
        // repeats the clone/cache/install sequence.
        let mut installs = SlotInstalls::new(self.graph, &self.specs, &self.charges);

        for op in ops {
            match constraint_effect(op) {
                ConstraintOp::Ddl(ddl) => installs.apply(ddl, &mut earlier)?,
                ConstraintOp::Introduce(tuple_op) | ConstraintOp::Free(tuple_op) => {
                    simulation.apply_tuple(tuple_op);
                }
                ConstraintOp::Irrelevant => {}
            }
        }
        let (state, mut growth) = installs.finish();
        let installed_this_slot = state.installed();
        let drops = state.drops().clone();

        // The slot's net effect, not its transient in-statement states: the
        // statement-time validator already validated those (a swap of two
        // constrained values is one legitimate statement). All of the
        // slot's frees are visible before its introductions, matching the
        // atomic statement boundary; introductions still check each other,
        // so a genuinely duplicated value inside one slot is refused.
        let mut frees: Vec<(TupleId, EntityId)> = Vec::new();
        let mut introductions: Vec<(TupleId, EntityId)> = Vec::new();
        let touched_entities = simulation.into_touched();
        // The entry-growth term: every effective constraint's net transitions,
        // priced exactly as the batch estimate prices them. This is the one
        // place that classification is consumed; the capacity check below
        // reuses the total.
        {
            let index = self.graph.constraint_keys.lock().unwrap();
            for transition in net_transitions(state.effective(), &touched_entities) {
                if let Some(tuple) = &transition.from {
                    growth -=
                        index.refund_for(&transition.spec.name, tuple, transition.entity) as i64;
                }
                if let Some(tuple) = &transition.to {
                    growth += entry_charge(tuple, 1) as i64;
                }
            }
        }
        for transition in net_transitions(state.effective(), &touched_entities) {
            if installed_this_slot.contains(&transition.spec.name) {
                // The slot's own writes are covered by the statement-time
                // validator, which rechecks a constraint created in the
                // slice.
                continue;
            }
            let entry = transition.entry;
            let entity = transition.entity;
            let name_id = self.names.id(&transition.spec.name);
            if let Some(tuple) = transition.from {
                frees.push(((name_id, tuple), entity));
            }
            if let Some(tuple) = transition.to {
                match entry.origin {
                    Origin::InstalledInGroup => return Err(Error::conflict()),
                    Origin::Committed | Origin::InstalledThisSlot => {}
                }
                introductions.push(((name_id, tuple), entity));
            }
        }
        for (id, entity) in frees {
            slot_holds
                .entry(id)
                .or_default()
                .insert(entity, Hold::Freed);
        }
        // One map lock for the whole introduction loop: the introductions
        // are materialised above and nothing mutates the map between
        // iterations, so the committed-holder probe reads the same map once
        // per slot instead of once per introduction. A frees-only slot (the
        // `DELETE` shape) skips the lock entirely.
        if !introductions.is_empty() {
            let index = self.graph.constraint_keys.lock().unwrap();
            for (id, entity) in introductions {
                if self.introduction_conflicts(&index, &id, entity, &slot_holds) {
                    return Err(Error::conflict());
                }
                slot_holds
                    .entry(id)
                    .or_default()
                    .insert(entity, Hold::Introduced);
            }
        }

        // The pending delta: entries under a constraint dropped later in
        // this slot die with it. Nothing is merged until `accept`.
        slot_holds.retain(|(id, _), _| !drops.contains(self.names.name(*id)));
        let pending_installs: BTreeMap<String, AdmissionSpec> = state
            .effective()
            .iter()
            .filter(|(name, _)| state.installed().contains(*name))
            .map(|(name, admission)| (name.clone(), admission.clone()))
            .collect();
        Ok((
            PendingAdmission {
                drops,
                installs: pending_installs,
                holds: slot_holds,
            },
            growth,
        ))
    }

    /// Merge a checked slot's delta into the group overlay. Called only
    /// once the slot is truly accepted, so a slot rejected by a later
    /// admission stage leaves no holds or constraint-set changes behind.
    #[doc(hidden)]
    pub fn accept(&mut self, pending: PendingAdmission) {
        for name in pending.drops {
            self.specs.remove(&name);
            self.forget_holds(&name);
        }
        for (name, mut admission) in pending.installs {
            self.forget_holds(&name);
            // An accepted slot installed the constraint: later slots must
            // see its domain in the same scope the apply seams ask, and his
            // origin now marks the group entry (no committed map yet).
            self.scope.add_spec(&admission.spec);
            admission.origin = Origin::InstalledInGroup;
            self.specs.insert(name, admission);
        }
        for (id, states) in pending.holds {
            self.holds.entry(id).or_default().extend(states);
        }
    }

    /// Drop every group-overlay hold recorded under `name` — the one policy
    /// both a constraint's drop and its (re-)install share: the overlay only
    /// tracks holds for constraints in the current effective set.
    fn forget_holds(&mut self, name: &str) {
        if let Some(id) = self.names.lookup(name) {
            self.holds.retain(|(constraint, _), _| *constraint != id);
        }
    }

    /// The effective holders of `id` for a candidate introduction by
    /// `entity`: committed holders, then the earlier accepted slots' holds,
    /// then the current slot's holds. A later removal frees the value. The
    /// caller holds the map lock for the whole introduction loop.
    fn introduction_conflicts(
        &self,
        index: &ConstraintKeyIndex,
        id: &TupleId,
        entity: EntityId,
        slot_holds: &TupleHolds,
    ) -> bool {
        let mut holders = index.holders(self.names.name(id.0), &id.1);
        for overlay in [&self.holds, slot_holds] {
            let Some(states) = overlay.get(id) else {
                continue;
            };
            for (holder, held) in states {
                match held {
                    Hold::Introduced => {
                        if !holders.contains(holder) {
                            holders.push(*holder);
                        }
                    }
                    Hold::Freed => holders.retain(|have| have != holder),
                }
            }
        }
        holders.iter().any(|holder| *holder != entity)
    }
}

/// One slot's DDL accumulation: the shared [`BatchDdlState`] plus the
/// earlier-slot replay cache an install's conservative check needs.
struct SlotInstalls<'a, 'e> {
    graph: &'a Graph,
    state: BatchDdlState<'a, AdmissionSpec>,
    /// The group's committed charges, for the entry-growth pricing.
    charges: &'a BTreeMap<String, u64>,
    /// The slot's DDL entry-growth delta so far.
    growth: i64,
    earlier_cache: Option<Vec<&'e [RedoOp]>>,
}

impl<'a, 'e> SlotInstalls<'a, 'e> {
    fn new(
        graph: &'a Graph,
        committed: &'a BTreeMap<String, AdmissionSpec>,
        charges: &'a BTreeMap<String, u64>,
    ) -> Self {
        Self {
            graph,
            state: BatchDdlState::new(committed),
            charges,
            growth: 0,
            earlier_cache: None,
        }
    }

    /// Apply one DDL op. A drop removes the name from the effective set;
    /// every other op installs the specs its [`DdlOp::installs`] names — the
    /// same source the entry-growth estimate iterates.
    fn apply(
        &mut self,
        ddl: DdlOp<'_>,
        earlier: &mut impl Iterator<Item = &'e [RedoOp]>,
    ) -> Result<()> {
        match ddl {
            DdlOp::DropConstraint { name } => {
                let step = self.state.drop_constraint(name);
                self.growth += price_drop(self.charges, name, step);
            }
            install => {
                for spec in install.installs(&self.graph.interner)? {
                    self.add(spec, earlier)?;
                }
            }
        }
        Ok(())
    }

    /// Install one spec into the effective set: an unchanged schema is a
    /// no-op, and a new or re-installed one passes the conservative
    /// earlier-slot check first; the entry origin records where it came from.
    ///
    /// The state records the install before the check runs; an `Err` here is
    /// propagated out of [`ConstraintAdmission::check_with_earlier`], which
    /// discards this whole slot accumulator, so a rejected slot leaves the
    /// group untouched either way.
    fn add(
        &mut self,
        spec: KeySpec,
        earlier: &mut impl Iterator<Item = &'e [RedoOp]>,
    ) -> Result<()> {
        // A name this slot already installed: the first install owns it.
        if self.state.installed().contains(&spec.name) {
            return Ok(());
        }
        // The conservative earlier-slot reply is the entry origin's job once
        // the slot is accepted.
        let entry = AdmissionSpec {
            spec: spec.clone(),
            origin: Origin::InstalledThisSlot,
        };
        let charge = self.graph.estimate_backfill_charge(&spec);
        let Some(step) = self.state.install(entry) else {
            return Ok(());
        };
        self.growth += price_install(self.charges, &spec.name, charge, step);
        let earlier = self.earlier_cache.get_or_insert_with(|| earlier.collect());
        check_install(self.graph, &spec, earlier)
    }

    fn finish(self) -> (BatchDdlState<'a, AdmissionSpec>, i64) {
        (self.state, self.growth)
    }
}

/// A constraint installed by this slot must not cover a value an earlier
/// accepted slot introduced: the installing statement's snapshot could not
/// see that commit, and the admission loop must not build a second O(L)
/// backfill. The conservative reply is exact by retry.
fn check_install(graph: &Graph, spec: &KeySpec, earlier: &[&[RedoOp]]) -> Result<()> {
    // Replay the earlier accepted slots' ops against the committed graph
    // (accepted slots never share an entity, so each entity's net
    // before/after is the slot's own).
    let scope = ConstraintKeyScope::from_specs(std::iter::once(spec));
    let mut simulation = SlotSimulation::new(graph, &scope);
    for ops in earlier {
        for op in *ops {
            simulation.simulate_redo(op);
        }
    }
    // The candidate is the batch's only effective constraint: a net
    // transition that leaves an entity carrying a tuple is exactly the
    // "would leave the slot holding a value the installer cannot see" case.
    let effective: BTreeMap<String, KeySpec> =
        std::iter::once((spec.name.clone(), spec.clone())).collect();
    for transition in net_transitions(&effective, &simulation.into_touched()) {
        if transition.to.is_some() {
            return Err(Error::conflict());
        }
    }
    Ok(())
}

impl Graph {
    /// Start a group's constraint admission state. Specs are snapshotted
    /// once per group; the per-slot deltas accumulate in the returned
    /// value.
    #[doc(hidden)]
    pub fn begin_constraint_admission(&self) -> ConstraintAdmission<'_> {
        let (specs, scope, charges) = {
            let index = self.constraint_keys.lock().unwrap();
            let specs: BTreeMap<String, AdmissionSpec> = index
                .specs()
                .map(|spec| {
                    (
                        spec.name.clone(),
                        AdmissionSpec {
                            spec: spec.clone(),
                            origin: Origin::Committed,
                        },
                    )
                })
                .collect();
            let charges = specs
                .keys()
                .map(|name| (name.clone(), index.charge_of(name)))
                .collect();
            (specs, index.scope().clone(), charges)
        };
        ConstraintAdmission {
            graph: self,
            specs,
            scope,
            charges,
            names: NameTable::default(),
            holds: HashMap::new(),
        }
    }
}
