//! ADR-0015 commit-conflict machinery: the redo-slice classification
//! ([`WriteSets`]), the two conflict predicates
//! ([`Graph::write_set_conflicts`], [`Graph::redo_conflicts_with_later`]),
//! and their MVCC helpers.

use std::collections::HashSet;

use cypherlite_core::{NodeId, RelId};

use super::Graph;
use crate::redo::{RedoOp, WriteTarget};

/// MVCC "changed since snapshot" predicate: a version created or deleted
/// after `snapshot_ts`. The node/rel conflict-detection twins share it.
fn changed_since(xmin: u64, xmax: u64, snapshot_ts: u64) -> bool {
    xmin > snapshot_ts || (xmax != 0 && xmax > snapshot_ts)
}

/// One classification pass over a redo slice (ADR-0015 conflict
/// detection): element ids by write kind (`mutated` includes the deletes —
/// a delete is a mutation) plus the catalog projections the
/// later-redo rules consult.
#[derive(Default)]
struct WriteSets {
    created_nodes: HashSet<NodeId>,
    created_rels: HashSet<RelId>,
    deleted_nodes: HashSet<NodeId>,
    mutated_nodes: HashSet<NodeId>,
    mutated_rels: HashSet<RelId>,
    /// Names of every CreateIndex/DropIndex.
    index_names: HashSet<String>,
    /// (label, property) of every CreateIndex.
    index_schemas: HashSet<(String, String)>,
    /// Names of every CreateConstraint/DropConstraint.
    constraint_names: HashSet<String>,
    /// Any store/catalog metadata mutation.
    catalog_meta: bool,
}

impl From<&[RedoOp]> for WriteSets {
    fn from(ops: &[RedoOp]) -> Self {
        let mut sets = Self::default();
        for op in ops {
            match op.write_target() {
                WriteTarget::CreatedNode(id) => {
                    sets.created_nodes.insert(id);
                }
                WriteTarget::CreatedRel { id, .. } => {
                    sets.created_rels.insert(id);
                }
                WriteTarget::DeletedNode(id) => {
                    sets.deleted_nodes.insert(id);
                    sets.mutated_nodes.insert(id);
                }
                WriteTarget::DeletedRel(id) => {
                    sets.mutated_rels.insert(id);
                }
                WriteTarget::MutatedNode(id) => {
                    sets.mutated_nodes.insert(id);
                }
                WriteTarget::MutatedRel(id) => {
                    sets.mutated_rels.insert(id);
                }
                WriteTarget::CreateIndex {
                    name,
                    label,
                    property,
                } => {
                    sets.index_names.insert(name.to_string());
                    sets.index_schemas
                        .insert((label.to_string(), property.to_string()));
                }
                WriteTarget::DropIndex { name } => {
                    sets.index_names.insert(name.to_string());
                }
                WriteTarget::CreateConstraint { name } | WriteTarget::DropConstraint { name } => {
                    sets.constraint_names.insert(name.to_string());
                }
                WriteTarget::CatalogMeta => sets.catalog_meta = true,
                WriteTarget::PullIdentity | WriteTarget::Ontology => {}
            }
        }
        sets
    }
}

impl Graph {
    /// True when `ops` cannot commit on top of `snapshot_ts` (ADR-0015).
    pub fn write_set_conflicts(&self, snapshot_ts: u64, ops: &[RedoOp]) -> bool {
        if self.commit_ts() == snapshot_ts {
            return false;
        }
        let writes = WriteSets::from(ops);
        for op in ops {
            match op.write_target() {
                WriteTarget::CreatedNode(id) => {
                    if self.nodes.contains_key(&id) {
                        return true;
                    }
                }
                WriteTarget::CreatedRel { id, start, end } => {
                    if self.rels.contains_key(&id) {
                        return true;
                    }
                    if !writes.created_nodes.contains(&start)
                        && self.endpoint_deleted_since(start, snapshot_ts)
                    {
                        return true;
                    }
                    if !writes.created_nodes.contains(&end)
                        && self.endpoint_deleted_since(end, snapshot_ts)
                    {
                        return true;
                    }
                }
                WriteTarget::DeletedNode(id) | WriteTarget::MutatedNode(id) => {
                    if !writes.created_nodes.contains(&id)
                        && self.node_versions_changed_since(id, snapshot_ts)
                    {
                        return true;
                    }
                }
                WriteTarget::DeletedRel(id) | WriteTarget::MutatedRel(id) => {
                    if !writes.created_rels.contains(&id)
                        && self.rel_versions_changed_since(id, snapshot_ts)
                    {
                        return true;
                    }
                }
                WriteTarget::CreateIndex {
                    name,
                    label,
                    property,
                } => {
                    // A name taken by any index conflicts, matching
                    // schema or not.
                    if self.named_index(name).is_some() {
                        return true;
                    }
                    if self.named_index_by_schema(label, property).is_some() {
                        return true;
                    }
                }
                WriteTarget::DropIndex { name } => {
                    if self.named_index(name).is_none() {
                        return true;
                    }
                }
                WriteTarget::CreateConstraint { name } => {
                    if self.has_constraint(name) {
                        return true;
                    }
                }
                WriteTarget::DropConstraint { name } => {
                    if !self.has_constraint(name) {
                        return true;
                    }
                }
                WriteTarget::CatalogMeta => return true,
                WriteTarget::PullIdentity | WriteTarget::Ontology => {}
            }
        }
        false
    }

    /// True when `ops` would conflict with a later unapplied redo list
    /// (ADR-0015 / ADR-0019 group commit).
    pub fn redo_conflicts_with_later(ops: &[RedoOp], later: &[RedoOp]) -> bool {
        let later_writes = WriteSets::from(later);
        let ops_writes = WriteSets::from(ops);
        if !ops_writes
            .created_nodes
            .is_disjoint(&later_writes.created_nodes)
        {
            return true;
        }
        if !ops_writes
            .created_rels
            .is_disjoint(&later_writes.created_rels)
        {
            return true;
        }
        for op in ops {
            match op.write_target() {
                WriteTarget::CreatedNode(_) => {}
                WriteTarget::CreatedRel { start, end, .. } => {
                    if !ops_writes.created_nodes.contains(&start)
                        && later_writes.deleted_nodes.contains(&start)
                    {
                        return true;
                    }
                    if !ops_writes.created_nodes.contains(&end)
                        && later_writes.deleted_nodes.contains(&end)
                    {
                        return true;
                    }
                }
                WriteTarget::DeletedNode(id) | WriteTarget::MutatedNode(id) => {
                    if !ops_writes.created_nodes.contains(&id)
                        && (later_writes.mutated_nodes.contains(&id)
                            || later_writes.created_nodes.contains(&id))
                    {
                        return true;
                    }
                }
                WriteTarget::DeletedRel(id) | WriteTarget::MutatedRel(id) => {
                    if !ops_writes.created_rels.contains(&id)
                        && (later_writes.mutated_rels.contains(&id)
                            || later_writes.created_rels.contains(&id))
                    {
                        return true;
                    }
                }
                // The catalog rules read the later slice's WriteSets
                // projections — no per-rule re-scan of `later`.
                WriteTarget::CreateIndex {
                    name,
                    label,
                    property,
                } => {
                    if later_writes.index_names.contains(name)
                        || later_writes
                            .index_schemas
                            .contains(&(label.to_string(), property.to_string()))
                    {
                        return true;
                    }
                }
                WriteTarget::DropIndex { name } => {
                    if later_writes.index_names.contains(name) {
                        return true;
                    }
                }
                WriteTarget::CreateConstraint { name } | WriteTarget::DropConstraint { name } => {
                    if later_writes.constraint_names.contains(name) {
                        return true;
                    }
                }
                WriteTarget::CatalogMeta => {
                    if later_writes.catalog_meta {
                        return true;
                    }
                }
                WriteTarget::PullIdentity | WriteTarget::Ontology => {}
            }
        }
        false
    }

    fn endpoint_deleted_since(&self, id: NodeId, snapshot_ts: u64) -> bool {
        self.node_visible_at(id, snapshot_ts) && !self.node_visible_at(id, self.commit_ts())
    }

    fn node_versions_changed_since(&self, id: NodeId, snapshot_ts: u64) -> bool {
        self.nodes.get(&id).is_some_and(|vers| {
            vers.iter()
                .any(|v| changed_since(v.xmin, v.xmax, snapshot_ts))
        })
    }

    fn rel_versions_changed_since(&self, id: RelId, snapshot_ts: u64) -> bool {
        self.rels.get(&id).is_some_and(|vers| {
            vers.iter()
                .any(|v| changed_since(v.xmin, v.xmax, snapshot_ts))
        })
    }
}
