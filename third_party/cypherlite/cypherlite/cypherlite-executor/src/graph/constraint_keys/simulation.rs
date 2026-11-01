//! ADR-0439: the one redo-op classification and the entity simulation the
//! commit loop drives — the `TupleOp`/`DdlOp`/`ConstraintOp` vocabulary, the
//! constrained [`KeyView`] (the store's compact props shape, never a whole
//! entity), and [`SlotSimulation`], which applies a slot's redo over only the
//! constrained parts of the committed entities. The map and its maintenance
//! live in the parent [`super`]; the admission engine and the entry-growth
//! estimate live in [`super::admission`]. Dependencies run one way from the
//! admission side to the map — this module imports the map's vocabulary and
//! the map never names this module's types.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cypherlite_core::{NodeId, PropertyMap, RelId, Result, Value};
use cypherlite_model::{Constraint, ConstraintType};

use super::{spec_from_model, ConstraintKeyScope, EntityId, KeySpec, KeyTuple, SpecSide};
use crate::graph::{live_version, Graph};
use crate::graph_type::GraphTypeSpec;
use crate::redo::RedoOp;
use crate::store::{CompactProps, Interner, RelPayload};

/// The tuple-changing payload of one redo op, borrowed from it. The slot
/// simulation reads these typed fields instead of re-destructuring
/// [`RedoOp`]; the two enums together are the one op vocabulary
/// (ADR-0439's Op vocabulary).
#[derive(Clone, Copy, Debug)]
pub(super) enum TupleOp<'a> {
    CreateNode {
        id: NodeId,
        labels: &'a [String],
        properties: &'a PropertyMap,
    },
    CreateRel {
        id: RelId,
        rel_type: &'a str,
        properties: &'a PropertyMap,
    },
    DeleteNode {
        id: NodeId,
    },
    DeleteRel {
        id: RelId,
    },
    SetNodeProp {
        id: NodeId,
        key: &'a str,
        value: &'a Value,
    },
    RemoveNodeProp {
        id: NodeId,
        key: &'a str,
    },
    SetRelProp {
        id: RelId,
        key: &'a str,
        value: &'a Value,
    },
    RemoveRelProp {
        id: RelId,
        key: &'a str,
    },
    AddLabels {
        id: NodeId,
        labels: &'a [String],
    },
    RemoveLabels {
        id: NodeId,
        labels: &'a [String],
    },
}

/// The constraint-set payload of one DDL op, borrowed from it.
#[derive(Clone, Copy, Debug)]
pub(super) enum DdlOp<'a> {
    CreateConstraint {
        name: &'a str,
        relationship: bool,
        label: &'a str,
        properties: &'a [String],
        kind: &'a str,
    },
    DropConstraint {
        name: &'a str,
    },
    Derived {
        constraint: &'a Constraint,
    },
    CurrentGraphType {
        body: &'a GraphTypeSpec,
    },
}

impl DdlOp<'_> {
    /// The uniqueness constraints this DDL op installs — the one source of
    /// truth the group admission loop and the entry-growth estimate both
    /// iterate, so they cannot disagree about which ops install a key. Each
    /// op yields the specs its own vocabulary names: a `CreateConstraint`
    /// parses its kind tag, a `Derived` record classifies its model
    /// constraint, and a graph type derives its declared constraints.
    ///
    /// Total by policy: a non-uniqueness kind, an unparsable kind tag (a
    /// corrupt replicated record on its way to the preflight) and an
    /// `entity_type` naming neither kind all yield no spec rather than
    /// panic — this runs before the WAL preflight on the replicated commit
    /// path (the [`kind_enforces_uniqueness`] precedent; the panicking
    /// `spec_from_def` is the post-preflight apply-path counterpart).
    /// `CurrentGraphType` is the only variant that can fail: its derivation
    /// validates the body, and the caller propagates that error (the
    /// preflight rejects the op).
    pub(super) fn installs(&self, interner: &Interner) -> Result<Vec<KeySpec>> {
        match self {
            Self::CreateConstraint {
                name,
                relationship,
                label,
                properties,
                kind,
            } => {
                if !kind_enforces_uniqueness(kind) {
                    return Ok(Vec::new());
                }
                Ok(vec![KeySpec::new(
                    interner,
                    (*name).to_string(),
                    *relationship,
                    (*label).to_string(),
                    properties.to_vec(),
                )])
            }
            Self::DropConstraint { .. } => Ok(Vec::new()),
            Self::Derived { constraint } => {
                Ok(spec_from_model(interner, constraint).into_iter().collect())
            }
            Self::CurrentGraphType { body } => {
                Ok(crate::graph_type::derive_constraints(&body.to_body())?
                    .iter()
                    .filter_map(|constraint| spec_from_model(interner, constraint))
                    .collect())
            }
        }
    }
}

/// What one redo op does to the constraint-key map. This is the one
/// classification of that fact: every consumer ([`SlotSimulation::simulate`],
/// the admission loop, [`is_constraint_ddl`], the entry-growth estimate)
/// projects [`constraint_effect`] instead of matching `RedoOp` itself.
#[derive(Clone, Copy, Debug)]
pub(super) enum ConstraintOp<'a> {
    /// Can add a constrained tuple: a create, a property set, a label add.
    Introduce(TupleOp<'a>),
    /// Can only remove a constrained tuple: a delete or a property/label
    /// removal.
    Free(TupleOp<'a>),
    /// Changes the maintained constraint set itself, not a tuple.
    Ddl(DdlOp<'a>),
    /// Touches no constrained tuple and no constraint.
    Irrelevant,
}

impl ConstraintOp<'_> {
    /// Whether the op can introduce a constrained tuple or change the
    /// constraint set — the two reasons a slot needs the admission path (a
    /// `Free` op only removes tuples, so it never does).
    pub(super) fn needs_admission(&self) -> bool {
        matches!(self, Self::Introduce(_) | Self::Ddl(_))
    }
}

/// The single exhaustive classification of a redo op's effect on the
/// constraint-key map. Every variant is named; no `_` arm, so a new
/// [`RedoOp`] variant cannot compile without a decision here.
pub(super) fn constraint_effect(op: &RedoOp) -> ConstraintOp<'_> {
    match op {
        RedoOp::CreateNode {
            id,
            labels,
            properties,
        } => ConstraintOp::Introduce(TupleOp::CreateNode {
            id: *id,
            labels: labels.as_slice(),
            properties,
        }),
        RedoOp::CreateRel {
            id,
            rel_type,
            properties,
            ..
        } => ConstraintOp::Introduce(TupleOp::CreateRel {
            id: *id,
            rel_type,
            properties,
        }),
        RedoOp::SetNodeProp { id, key, value } => ConstraintOp::Introduce(TupleOp::SetNodeProp {
            id: *id,
            key,
            value,
        }),
        RedoOp::SetRelProp { id, key, value } => ConstraintOp::Introduce(TupleOp::SetRelProp {
            id: *id,
            key,
            value,
        }),
        RedoOp::AddLabels { id, labels } => {
            ConstraintOp::Introduce(TupleOp::AddLabels { id: *id, labels })
        }
        RedoOp::DeleteNode { id } => ConstraintOp::Free(TupleOp::DeleteNode { id: *id }),
        RedoOp::DeleteRel { id } => ConstraintOp::Free(TupleOp::DeleteRel { id: *id }),
        RedoOp::RemoveNodeProp { id, key } => {
            ConstraintOp::Free(TupleOp::RemoveNodeProp { id: *id, key })
        }
        RedoOp::RemoveRelProp { id, key } => {
            ConstraintOp::Free(TupleOp::RemoveRelProp { id: *id, key })
        }
        RedoOp::RemoveLabels { id, labels } => {
            ConstraintOp::Free(TupleOp::RemoveLabels { id: *id, labels })
        }
        RedoOp::CreateConstraint {
            name,
            relationship,
            label,
            properties,
            kind,
            ..
        } => ConstraintOp::Ddl(DdlOp::CreateConstraint {
            name,
            relationship: *relationship,
            label,
            properties,
            kind,
        }),
        RedoOp::DropConstraint { name } => ConstraintOp::Ddl(DdlOp::DropConstraint { name }),
        RedoOp::CatalogDerivedConstraint { constraint } => {
            ConstraintOp::Ddl(DdlOp::Derived { constraint })
        }
        RedoOp::SetCurrentGraphType { body } => ConstraintOp::Ddl(DdlOp::CurrentGraphType { body }),
        RedoOp::CreateIndex { .. }
        | RedoOp::DropIndex { .. }
        | RedoOp::CatalogIndexDetail { .. }
        | RedoOp::PutDatabase { .. }
        | RedoOp::DropDatabase { .. }
        | RedoOp::PutGraphType { .. }
        | RedoOp::PullIdentityPut { .. }
        | RedoOp::OntologySet { .. }
        | RedoOp::OntologyDrop { .. } => ConstraintOp::Irrelevant,
    }
}

/// Whether an op can change the maintained constraint set (and therefore
/// must run the admission/estimate paths even when the map is empty): a
/// one-line projection of the one op classification.
pub(super) fn is_constraint_ddl(op: &RedoOp) -> bool {
    matches!(constraint_effect(op), ConstraintOp::Ddl(_))
}

/// Whether a constraint kind string names a uniqueness-enforcing
/// constraint.
///
/// This classification is total and non-panicking on purpose: the
/// entry-growth estimate calls it on the replicated commit path
/// (`commit_replicated` → `admit_redo_capacity` → `estimate_redo_delta`,
/// which runs before the WAL preflight when a logical graph size limit is
/// configured) and the group-commit probe calls it on every constrained
/// group (`check_with_earlier`), so an unparsable kind must classify as
/// "not uniqueness" rather than panic there — the WAL preflight
/// (`validate_redo_group_applicability` → `RedoPreflight::check_op`)
/// rejects it before apply. The panicking `expect` in `spec_from_def` is
/// the post-preflight invariant on the apply path. One boundary, two
/// policies, ordered by that preflight.
///
/// The catalog's kind vocabulary is closed (ADR-0439's Closed
/// constraint-kind vocabulary), so the only way an unknown kind reaches the
/// estimate is a corrupt replicated record on its way to rejection.
pub(super) fn kind_enforces_uniqueness(kind: &str) -> bool {
    ConstraintType::from_str(kind).is_ok_and(|kind| kind.enforces_uniqueness())
}

/// `KeySpec`'s projection of an admission view: the tuple the view carries
/// for this constraint. The view holds only interned constrained ids, so
/// this is O(P) map lookups, never a string or full-property walk.
impl KeySpec {
    pub(super) fn view_tuple(&self, view: &KeyView) -> Option<KeyTuple> {
        let props = match view {
            KeyView::Node { labels, props } => {
                let SpecSide::Node { label_id } = self.side else {
                    return None;
                };
                if !labels.contains(&label_id) {
                    return None;
                }
                props
            }
            KeyView::Rel { type_id, props } => {
                let SpecSide::Rel { type_id: expected } = self.side else {
                    return None;
                };
                if *type_id != expected {
                    return None;
                }
                props
            }
        };
        Self::tuple_of(self.property_ids.iter().map(|&kid| props.get(kid)))
    }
}

/// The admission engine's entity view: only the constrained properties
/// (interned key ids) plus the interned labels/relationship type, held in
/// the store's one compact props shape. Never the whole entity — the full
/// `to_node`/`to_rel` materialization is the cost this shape exists to
/// avoid.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum KeyView {
    Node {
        labels: Vec<u32>,
        props: CompactProps,
    },
    Rel {
        type_id: u32,
        props: CompactProps,
    },
}

/// Project the constrained subset of a property map: one lookup per
/// constrained key id, in the caller's value shape (the committed compact
/// map directly, or a named map through the interner).
fn project<'v>(
    keys: &HashSet<u32>,
    mut lookup: impl FnMut(u32) -> Option<&'v Value>,
) -> CompactProps {
    let mut out = CompactProps::default();
    for &kid in keys {
        if let Some(value) = lookup(kid) {
            out.set(kid, value.clone());
        }
    }
    out
}

impl ConstraintKeyScope {
    /// The committed constrained view of `entity`, read under the entity
    /// guard but cloning only the labels/type and the constrained values.
    fn committed_view(&self, graph: &Graph, entity: EntityId) -> Option<KeyView> {
        match entity {
            EntityId::Node(id) => {
                let vers = graph.nodes.get(&id)?;
                let live = live_version(&vers)?;
                let labels = live
                    .live
                    .labels
                    .iter()
                    .copied()
                    .filter(|label| self.node_labels.contains(label))
                    .collect();
                Some(KeyView::Node {
                    labels,
                    props: project(&self.node_keys, |kid| live.live.props.get(kid)),
                })
            }
            EntityId::Rel(id) => {
                let vers = graph.rels.get(&id)?;
                let live = live_version(&vers)?;
                Some(KeyView::Rel {
                    type_id: live.live.type_id,
                    props: self.rel_props(&graph.interner, &live.live.payload),
                })
            }
        }
    }

    /// The constrained subset of a relationship payload — the relationship
    /// twin of [`project`] over the payload's own walk (the triple form
    /// resolves its fixed keys there).
    fn rel_props(&self, interner: &Interner, payload: &RelPayload) -> CompactProps {
        let mut out = CompactProps::default();
        payload.for_each_key_value(interner, |kid, value| {
            if self.rel_keys.contains(&kid) {
                out.set(kid, value.clone());
            }
        });
        out
    }

    /// The interned ids of `labels` this scope constrains — the one
    /// label-name→id resolution the committed walk, the simulated node view,
    /// and the two label arms share (an uninterned or unconstrained label
    /// cannot be under a node constraint: the constraint interned its label
    /// in `KeySpec::new`).
    fn in_scope_label_ids<'a>(
        &'a self,
        interner: &'a Interner,
        labels: &'a [String],
    ) -> impl Iterator<Item = u32> + 'a {
        labels
            .iter()
            .filter_map(move |label| interner.get_id(label))
            .filter(move |id| self.node_labels.contains(id))
    }

    /// The constrained view of a redo op's node payload. A label that
    /// resolves to no interned id cannot be under any constraint — a
    /// constraint interned its label when it was created
    /// (`KeySpec::new`) — the same interning invariant [`Self::rel_view`]
    /// states for relationship types, applied by [`Self::in_scope_label_ids`].
    fn node_view(
        &self,
        interner: &Interner,
        labels: &[String],
        properties: &PropertyMap,
    ) -> KeyView {
        let labels = self.in_scope_label_ids(interner, labels).collect();
        let props = project(&self.node_keys, |kid| properties.get(&interner.get(kid)));
        KeyView::Node { labels, props }
    }

    /// The constrained view of a redo op's relationship payload. A
    /// `CreateRel` registers unconditionally, so its relationship type may
    /// not be interned yet (internment happens at apply): such a type cannot
    /// be under any constraint — a constraint interned its type when it was
    /// created — so it contributes no view.
    fn rel_view(
        &self,
        interner: &Interner,
        rel_type: &str,
        properties: &PropertyMap,
    ) -> Option<KeyView> {
        let type_id = interner.get_id(rel_type)?;
        let props = project(&self.rel_keys, |kid| properties.get(&interner.get(kid)));
        Some(KeyView::Rel { type_id, props })
    }
}

/// A slot's redo applied in order over only the constrained parts of the
/// committed entities. One view per touched entity, mutated in place; the
/// net before/after is what the uniqueness probe and a later slot's
/// constraint install read. Net state is the right grain: the statement-time
/// validator already validated the slot's own writes, so only what the slot
/// leaves behind can conflict with committed state (a within-slot swap is
/// not a duplicate).
pub(super) struct SlotSimulation<'g> {
    graph: &'g Graph,
    /// The constrained scope the map owns (or a candidate's): read-only
    /// here, so the slot borrows it instead of cloning four sets.
    scope: &'g ConstraintKeyScope,
    /// Per touched entity, the `(before, after)` committed views. `None` on
    /// either side means the entity did not exist on that side.
    slots: HashMap<EntityId, (Option<KeyView>, Option<KeyView>)>,
    /// First-touch order, so net evaluation is deterministic (HashMap
    /// iteration is not).
    order: Vec<EntityId>,
}

impl<'g> SlotSimulation<'g> {
    /// `scope` is the constrained scope the map owns (or, for a candidate
    /// constraint, that candidate's scope); the simulation never derives a
    /// scope of its own.
    pub(super) fn new(graph: &'g Graph, scope: &'g ConstraintKeyScope) -> Self {
        Self {
            graph,
            scope,
            slots: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Apply one already-classified tuple op. The scope guard lives in each
    /// arm (a create/delete registers unconditionally; a property or label
    /// edit returns before touching state when out of scope), so the op is
    /// classified once and applied once.
    pub(super) fn apply_tuple(&mut self, op: TupleOp<'_>) {
        self.apply_tuple_inner(op);
    }

    /// Classify and simulate one raw op — the replay loop's form, where the
    /// op is not classified yet.
    pub(super) fn simulate_redo(&mut self, op: &RedoOp) {
        match constraint_effect(op) {
            ConstraintOp::Introduce(tuple_op) | ConstraintOp::Free(tuple_op) => {
                self.apply_tuple_inner(tuple_op)
            }
            ConstraintOp::Ddl(_) | ConstraintOp::Irrelevant => {}
        }
    }

    /// Lazy-init the entity's `(before, after)` pair from the committed
    /// view, in first-touch order, and hand back the pair.
    fn track(&mut self, entity: EntityId) -> &mut (Option<KeyView>, Option<KeyView>) {
        match self.slots.entry(entity) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let view = self.scope.committed_view(self.graph, entity);
                self.order.push(entity);
                entry.insert((view.clone(), view))
            }
        }
    }

    /// The entity's mutable after-view, lazily seeded from its committed
    /// state.
    fn state(&mut self, entity: EntityId) -> &mut Option<KeyView> {
        &mut self.track(entity).1
    }

    /// Whether `key` resolves to an interned id this map's side constrains.
    /// The one scope predicate the four property arms consult.
    fn key_in_scope(&self, key: &str, relationship: bool) -> bool {
        let Some(kid) = self.graph.interner.get_id(key) else {
            return false;
        };
        if relationship {
            self.scope.touches_rel_key(kid)
        } else {
            self.scope.touches_node_key(kid)
        }
    }

    /// Apply one property edit to the entity's tracked after-view: resolve
    /// the key once (an uninterned key is outside every constraint), then
    /// match the view variant the entity kind fixes.
    fn edit_tracked_props(
        &mut self,
        entity: EntityId,
        key: &str,
        edit: impl FnOnce(&mut CompactProps, u32),
    ) {
        let Some(kid) = self.graph.interner.get_id(key) else {
            return;
        };
        match (entity, self.state(entity).as_mut()) {
            (EntityId::Node(_), Some(KeyView::Node { props, .. }))
            | (EntityId::Rel(_), Some(KeyView::Rel { props, .. })) => edit(props, kid),
            _ => {}
        }
    }

    /// The per-variant view transition of one tuple-changing op — the only
    /// place that needs the op's payload, and exhaustive over [`TupleOp`].
    fn apply_tuple_inner(&mut self, op: TupleOp<'_>) {
        match op {
            TupleOp::CreateNode {
                id,
                labels,
                properties,
            } => {
                let entity = EntityId::Node(id);
                let after = Some(
                    self.scope
                        .node_view(&self.graph.interner, labels, properties),
                );
                self.replace_new(entity, after);
            }
            TupleOp::CreateRel {
                id,
                rel_type,
                properties,
            } => {
                let entity = EntityId::Rel(id);
                let after = self
                    .scope
                    .rel_view(&self.graph.interner, rel_type, properties);
                self.replace_new(entity, after);
            }
            TupleOp::DeleteNode { id } => self.delete(EntityId::Node(id)),
            TupleOp::DeleteRel { id } => self.delete(EntityId::Rel(id)),
            TupleOp::SetNodeProp { id, key, value } => {
                if !self.key_in_scope(key, false) {
                    return;
                }
                self.edit_tracked_props(EntityId::Node(id), key, |props, kid| {
                    props.set(kid, value.clone());
                });
            }
            TupleOp::RemoveNodeProp { id, key } => {
                if !self.key_in_scope(key, false) {
                    return;
                }
                self.edit_tracked_props(EntityId::Node(id), key, |props, kid| props.remove(kid));
            }
            TupleOp::SetRelProp { id, key, value } => {
                if !self.key_in_scope(key, true) {
                    return;
                }
                self.edit_tracked_props(EntityId::Rel(id), key, |props, kid| {
                    props.set(kid, value.clone());
                });
            }
            TupleOp::RemoveRelProp { id, key } => {
                if !self.key_in_scope(key, true) {
                    return;
                }
                self.edit_tracked_props(EntityId::Rel(id), key, |props, kid| props.remove(kid));
            }
            TupleOp::AddLabels { id, labels } => {
                let entity = EntityId::Node(id);
                let added: Vec<u32> = self
                    .scope
                    .in_scope_label_ids(&self.graph.interner, labels)
                    .collect();
                if added.is_empty() {
                    return;
                }
                if let Some(KeyView::Node { labels, .. }) = self.state(entity).as_mut() {
                    for lid in added {
                        if !labels.contains(&lid) {
                            labels.push(lid);
                        }
                    }
                }
            }
            TupleOp::RemoveLabels { id, labels } => {
                let entity = EntityId::Node(id);
                let lost: HashSet<u32> = self
                    .scope
                    .in_scope_label_ids(&self.graph.interner, labels)
                    .collect();
                if lost.is_empty() {
                    return;
                }
                if let Some(KeyView::Node { labels, .. }) = self.state(entity).as_mut() {
                    labels.retain(|lid| !lost.contains(lid));
                }
            }
        }
    }

    /// Record a deletion: keep the committed before-view and clear the
    /// after-view, through the canonical first-touch registration.
    fn delete(&mut self, entity: EntityId) {
        self.track(entity).1 = None;
    }

    /// Record a created entity's view. Both callers are creates of a fresh
    /// id: it did not exist committed, so the before slot stays empty.
    fn replace_new(&mut self, entity: EntityId, after: Option<KeyView>) {
        *self.track(entity) = (None, after);
    }

    /// The slot's net state changes, one record per touched entity in
    /// first-touch order.
    pub(super) fn into_touched(mut self) -> Vec<TouchedEntity> {
        let mut out = Vec::new();
        for entity in std::mem::take(&mut self.order) {
            let (before, after) = self
                .slots
                .remove(&entity)
                .expect("every ordered entity has a tracked slot pair");
            if before != after {
                out.push(TouchedEntity {
                    entity,
                    before,
                    after,
                });
            }
        }
        out
    }
}

pub(super) struct TouchedEntity {
    pub(super) entity: EntityId,
    pub(super) before: Option<KeyView>,
    pub(super) after: Option<KeyView>,
}
