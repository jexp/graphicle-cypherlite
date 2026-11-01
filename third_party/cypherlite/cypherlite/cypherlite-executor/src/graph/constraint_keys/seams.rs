//! ADR-0439: the `apply_redo`-seam maintenance of the maintained
//! constraint-key map — the tracked-edit hand-offs and the entity transition
//! seams, in their own impl-block file (the `label_mutation.rs` /
//! `degree_stats.rs` precedent, a sibling of the map). The map's vocabulary,
//! charge family, and backfill/rebuild lifecycle stay in the parent; the
//! commit-loop probe and the entry-growth estimate are separate siblings.
//!
//! Lock order (ADR-0439): an entity guard (a live-version read) is taken
//! first, the map lock last. The scope probes here take only an interner
//! shard read and the map lock, never the reverse.

use cypherlite_core::{NodeId, RelId};

use super::super::{live_version, Graph};
use super::{ConstraintKeyIndex, ConstraintKeyScope, EntityId};
#[cfg(test)]
use super::{KeySpec, KeyTuple};
use crate::store::{LiveNode, LiveRel};

/// One node edit's constraint-key hand-off. A seam asks the map's scope one
/// question; when the answer is no, the seam pays no snapshot and no
/// transition. When it is yes, the before-image is captured at construction
/// (before the edit lands) and the seam finishes through
/// [`Graph::finish_node_edit`] with the edited live view in hand (an
/// `Untracked` edit needs no completion).
pub(in crate::graph) enum TrackedNodeEdit {
    /// The edit cannot change any constrained tuple.
    Untracked,
    /// The edit can change a constrained tuple; `before` is the committed
    /// view captured before the edit landed (`None` = the entity did not
    /// exist then).
    Tracked { before: Option<LiveNode> },
}

/// The relationship twin of [`TrackedNodeEdit`].
pub(in crate::graph) enum TrackedRelEdit {
    /// The edit cannot change any constrained tuple.
    Untracked,
    /// The edit can change a constrained tuple; `before` is the committed
    /// view captured before the edit landed (`None` = the entity did not
    /// exist then).
    Tracked { before: Option<LiveRel> },
}

impl Graph {
    /// Open a node edit's tracked hand-off: ask the map's scope whether the
    /// edit can change any constrained tuple, and capture the before-image
    /// only when it can.
    pub(in crate::graph) fn tracked_node_edit(
        &self,
        id: NodeId,
        touches: impl FnOnce(&ConstraintKeyScope) -> bool,
    ) -> TrackedNodeEdit {
        if !self.with_constraint_keys_scope(touches) {
            return TrackedNodeEdit::Untracked;
        }
        TrackedNodeEdit::Tracked {
            before: self.live_node_snapshot(id),
        }
    }

    /// The relationship twin of [`Graph::tracked_node_edit`].
    pub(in crate::graph) fn tracked_rel_edit(
        &self,
        id: RelId,
        touches: impl FnOnce(&ConstraintKeyScope) -> bool,
    ) -> TrackedRelEdit {
        if !self.with_constraint_keys_scope(touches) {
            return TrackedRelEdit::Untracked;
        }
        TrackedRelEdit::Tracked {
            before: self.live_rel_snapshot(id),
        }
    }

    /// A node property edit's tracked hand-off: the key can change a tuple
    /// iff it is interned and the map maintains it. The interner lookup runs
    /// BEFORE the scope probe takes the map lock, preserving the seam's
    /// original lock order (interner shard read, then map lock — never the
    /// reverse).
    pub(in crate::graph) fn tracked_node_key_edit(&self, id: NodeId, key: &str) -> TrackedNodeEdit {
        let kid = self.interner.get_id(key);
        self.tracked_node_edit(id, |scope| {
            kid.is_some_and(|kid| scope.touches_node_key(kid))
        })
    }

    /// The relationship twin of [`Graph::tracked_node_key_edit`].
    pub(in crate::graph) fn tracked_rel_key_edit(&self, id: RelId, key: &str) -> TrackedRelEdit {
        let kid = self.interner.get_id(key);
        self.tracked_rel_edit(id, |scope| {
            kid.is_some_and(|kid| scope.touches_rel_key(kid))
        })
    }
}

impl Graph {
    /// Finish a tracked node edit whose new live view is in hand: the token
    /// carries no graph, so the receiver binds it by construction.
    pub(in crate::graph) fn finish_node_edit(
        &self,
        edit: TrackedNodeEdit,
        id: NodeId,
        after: Option<&LiveNode>,
    ) {
        if let TrackedNodeEdit::Tracked { before } = edit {
            self.constraint_keys_node(id, before.as_ref(), after);
        }
    }

    /// The relationship twin of [`Graph::finish_node_edit`].
    pub(in crate::graph) fn finish_rel_edit(
        &self,
        edit: TrackedRelEdit,
        id: RelId,
        after: Option<&LiveRel>,
    ) {
        if let TrackedRelEdit::Tracked { before } = edit {
            self.constraint_keys_rel(id, before.as_ref(), after);
        }
    }

    /// Run `f` against the maintained map under its lock — the one accessor
    /// every read and transition seam routes through. The transition seams
    /// return their delta and charge after the closure, so the map guard is
    /// released before `charge` (ADR-0439 lock order).
    pub(super) fn with_constraint_keys<R>(
        &self,
        f: impl FnOnce(&mut ConstraintKeyIndex) -> R,
    ) -> R {
        f(&mut self.constraint_keys.lock().unwrap())
    }

    /// Run `f` against the one constrained scope the maintained map owns,
    /// instead of re-deriving it from the specs (ADR-0439's One scope rule).
    fn with_constraint_keys_scope<R>(&self, f: impl FnOnce(&ConstraintKeyScope) -> R) -> R {
        self.with_constraint_keys(|index| f(index.scope()))
    }

    /// The owned copy of the map's scope (test support: the scope-drift
    /// assertion compares whole values).
    #[cfg(test)]
    pub(super) fn constraint_keys_scope(&self) -> ConstraintKeyScope {
        self.with_constraint_keys(|index| index.scope().clone())
    }

    /// The map spec for a name, if the constraint is maintained (test
    /// support).
    #[cfg(test)]
    pub(crate) fn constraint_key_spec(&self, name: &str) -> Option<KeySpec> {
        self.with_constraint_keys(|index| index.spec(name).cloned())
    }

    /// Every maintained constraint's spec (test support: the property model
    /// and the seam tests read the whole set without locking the map).
    #[cfg(test)]
    pub(crate) fn constraint_key_specs(&self) -> Vec<KeySpec> {
        self.with_constraint_keys(|index| index.specs().cloned().collect())
    }

    /// One tuple's holders (test support: compared as entities, so callers do
    /// not lock the map).
    #[cfg(test)]
    pub(crate) fn constraint_key_holders(&self, name: &str, tuple: &KeyTuple) -> Vec<EntityId> {
        self.with_constraint_keys(|index| index.holders(name, tuple))
    }

    /// The number of holders of one tuple (test support).
    #[cfg(test)]
    pub(crate) fn constraint_key_holder_count(&self, name: &str, tuple: &KeyTuple) -> usize {
        self.with_constraint_keys(|index| index.holders(name, tuple).len())
    }

    /// Free every constrained tuple a node holds: the entity guard reads the
    /// live version in place, then the transition seam frees it with a `None`
    /// after-image (one free path; ADR-0439 lock order: guard first, map lock
    /// last, no `LiveNode` clone). The transition owns the `is_empty` guard.
    pub(in crate::graph) fn constraint_keys_delete_node(&self, id: NodeId) {
        let Some(vers) = self.nodes.get(&id) else {
            return;
        };
        let Some(live) = live_version(&vers) else {
            return;
        };
        self.constraint_keys_node(id, Some(&live.live), None);
    }

    /// The relationship twin of [`Graph::constraint_keys_delete_node`].
    pub(in crate::graph) fn constraint_keys_delete_rel(&self, id: RelId) {
        let Some(vers) = self.rels.get(&id) else {
            return;
        };
        let Some(live) = live_version(&vers) else {
            return;
        };
        self.constraint_keys_rel(id, Some(&live.live), None);
    }

    fn live_node_snapshot(&self, id: NodeId) -> Option<LiveNode> {
        self.nodes
            .get(&id)
            .and_then(|vers| live_version(&vers).map(|v| v.live.clone()))
    }

    fn live_rel_snapshot(&self, id: RelId) -> Option<LiveRel> {
        self.rels
            .get(&id)
            .and_then(|vers| live_version(&vers).map(|v| v.live.clone()))
    }

    /// Record one node state transition at an `apply_redo` seam: free the
    /// before tuple and introduce the after tuple per applicable
    /// uniqueness constraint. The map lock is taken last; the entity state
    /// is a borrowed snapshot, never an entity guard.
    fn constraint_keys_node(
        &self,
        id: NodeId,
        before: Option<&LiveNode>,
        after: Option<&LiveNode>,
    ) {
        let delta = self.with_constraint_keys(|index| {
            if index.is_empty() {
                return 0;
            }
            let entity = EntityId::Node(id);
            let mut delta = 0i64;
            for kc in index.constraints.values_mut() {
                let from = before.and_then(|node| kc.spec.live_node_tuple(node));
                let to = after.and_then(|node| kc.spec.live_node_tuple(node));
                delta += ConstraintKeyIndex::transition(kc, entity, from, to);
            }
            delta
        });
        self.charge(delta);
    }

    /// The relationship twin of [`Graph::constraint_keys_node`].
    fn constraint_keys_rel(&self, id: RelId, before: Option<&LiveRel>, after: Option<&LiveRel>) {
        let delta = self.with_constraint_keys(|index| {
            if index.is_empty() {
                return 0;
            }
            let entity = EntityId::Rel(id);
            let mut delta = 0i64;
            for kc in index.constraints.values_mut() {
                let from = before.and_then(|rel| kc.spec.live_rel_tuple(&self.interner, rel));
                let to = after.and_then(|rel| kc.spec.live_rel_tuple(&self.interner, rel));
                delta += ConstraintKeyIndex::transition(kc, entity, from, to);
            }
            delta
        });
        self.charge(delta);
    }
}
