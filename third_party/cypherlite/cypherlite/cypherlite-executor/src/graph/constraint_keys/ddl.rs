//! ADR-0439: the batch DDL accumulator both the commit-loop admission probe
//! and the entry-growth estimate drive — the effective constraint set in op
//! order, the names the batch installed and dropped, and the one net
//! transition projection both consumers price. A sibling of the map, the
//! simulation, the probe, and the estimate: the map's vocabulary is imported
//! here, and neither consumer owns the accumulator privately.
//!
//! The projection is generic over the entry payload ([`DdlSpec`]): the probe
//! carries the group marker ([`AdmissionSpec`]), the estimate the bare
//! [`KeySpec`]. One `from`/`to` derivation and one `from == to` skip serve
//! both, so the two sides cannot drift apart.

use std::collections::{BTreeMap, HashSet};

use super::simulation::TouchedEntity;
use super::{EntityId, KeySpec, KeyTuple};

/// Where an effective constraint's entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The map's committed constraint set at group start.
    Committed,
    /// Installed by an earlier accepted slot of this group. It has no
    /// committed map yet, so any later introduction into its domain is
    /// conservative `Error::Conflict` (the retry sees the committed
    /// constraint).
    InstalledInGroup,
    /// Installed by the current slot (filtered out of the admission check —
    /// the statement-time validator rechecks a constraint created in the
    /// slice).
    InstalledThisSlot,
}

/// A constraint's entry in an admission batch: the schema plus where the
/// effective entry came from.
#[derive(Clone, Debug)]
pub(super) struct AdmissionSpec {
    pub(super) spec: KeySpec,
    pub(super) origin: Origin,
}

/// One spec in a [`BatchDdlState`]. The admission loop's entries carry the
/// group marker ([`AdmissionSpec`]); the estimate's are the bare
/// [`KeySpec`].
pub(super) trait DdlSpec: Clone {
    fn name(&self) -> &str;

    /// The schema this entry constrains.
    fn key_spec(&self) -> &KeySpec;

    /// Whether two entries describe the same schema — the admission loop
    /// compares specs, never the group marker.
    fn same_schema(&self, other: &Self) -> bool;
}

impl DdlSpec for KeySpec {
    fn name(&self) -> &str {
        &self.name
    }

    fn key_spec(&self) -> &KeySpec {
        self
    }

    fn same_schema(&self, other: &Self) -> bool {
        self == other
    }
}

impl DdlSpec for AdmissionSpec {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn key_spec(&self) -> &KeySpec {
        &self.spec
    }

    fn same_schema(&self, other: &Self) -> bool {
        self.spec == other.spec
    }
}

/// One batch's DDL accumulation, shared by the admission loop and the
/// entry-growth estimate: the effective constraint set (the committed set
/// until the first DDL, then the same set mutated in op order), the names
/// the batch installed, and the names it dropped. Each DDL op is one
/// [`install`](Self::install)/[`drop_constraint`](Self::drop_constraint)
/// call, so both consumers drive one accumulator instead of each
/// re-deriving the batch's DDL state.
pub(super) struct BatchDdlState<'a, S> {
    /// The committed set at batch start; never mutated.
    committed: &'a BTreeMap<String, S>,
    /// The effective set, materialized on the first mutation.
    effective: Option<BTreeMap<String, S>>,
    /// The names the batch installed: their effective entry is the batch's.
    installed: HashSet<String>,
    /// The names the batch dropped.
    drops: HashSet<String>,
}

/// What one real install did: it replaced a committed (not in-batch) entry,
/// so the install refunds that entry's map charge.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct InstallStep {
    pub(super) replaced_committed: bool,
}

/// What one drop did: it removed a committed (not in-batch) entry, so the
/// drop refunds that entry's map charge.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DropStep {
    pub(super) refunds_committed: bool,
}

impl<'a, S: DdlSpec> BatchDdlState<'a, S> {
    pub(super) fn new(committed: &'a BTreeMap<String, S>) -> Self {
        Self {
            committed,
            effective: None,
            installed: HashSet::new(),
            drops: HashSet::new(),
        }
    }

    /// The effective set: the committed one until the first mutation.
    pub(super) fn effective(&self) -> &BTreeMap<String, S> {
        self.effective.as_ref().unwrap_or(self.committed)
    }

    fn effective_mut(&mut self) -> &mut BTreeMap<String, S> {
        self.effective.get_or_insert_with(|| self.committed.clone())
    }

    pub(super) fn installed(&self) -> &HashSet<String> {
        &self.installed
    }

    pub(super) fn drops(&self) -> &HashSet<String> {
        &self.drops
    }

    /// Install `entry`. `None` when the effective set already held the same
    /// schema, so no install happened (a derived constraint's companion
    /// `CreateConstraint`, or a re-derivation).
    pub(super) fn install(&mut self, entry: S) -> Option<InstallStep> {
        let name = entry.name().to_string();
        let existing = self.effective().get(&name);
        if existing.is_some_and(|existing| existing.same_schema(&entry)) {
            return None;
        }
        let replaced_committed = existing.is_some() && !self.installed.contains(&name);
        self.effective_mut().insert(name.clone(), entry);
        self.installed.insert(name.clone());
        self.drops.remove(&name);
        Some(InstallStep { replaced_committed })
    }

    /// Drop `name` from the effective set.
    pub(super) fn drop_constraint(&mut self, name: &str) -> DropStep {
        let existed = self.effective_mut().remove(name).is_some();
        let was_installed = self.installed.remove(name);
        self.drops.insert(name.to_string());
        DropStep {
            refunds_committed: existed && !was_installed,
        }
    }
}

/// One entity's net change under one effective constraint: the tuples it
/// stopped and started carrying, with `from == to` already skipped.
pub(super) struct Transition<'a, S> {
    pub(super) entry: &'a S,
    pub(super) spec: &'a KeySpec,
    pub(super) entity: EntityId,
    pub(super) from: Option<KeyTuple>,
    pub(super) to: Option<KeyTuple>,
}

/// The batch's net per-entity transitions: every touched entity against
/// every effective constraint, one `from`/`to` derivation and one
/// `from == to` skip. The one projection the admission probe (which adds
/// its installed-this-slot filter and conflict reply) and the estimate
/// (which prices the transitions) both consume.
pub(super) fn net_transitions<'a, S: DdlSpec>(
    effective: &'a BTreeMap<String, S>,
    touched: &'a [TouchedEntity],
) -> impl Iterator<Item = Transition<'a, S>> + 'a {
    touched.iter().flat_map(move |touched| {
        effective.values().filter_map(move |entry| {
            let spec = entry.key_spec();
            let from = touched
                .before
                .as_ref()
                .and_then(|view| spec.view_tuple(view));
            let to = touched
                .after
                .as_ref()
                .and_then(|view| spec.view_tuple(view));
            (from != to).then_some(Transition {
                entry,
                spec,
                entity: touched.entity,
                from,
                to,
            })
        })
    })
}
