//! Ordered redo applicability preflight: a read-only state machine that checks
//! each record's operations against the live graph and earlier records in the
//! same group, without copying property payloads.

use std::collections::{HashMap, HashSet};

use cypherlite_catalog::{Catalog, ConstraintDef, IndexDef};
use cypherlite_core::{Error, NodeId, RelId, Result};

use crate::redo::{self, RedoOp};

use super::{matching_enforced_label, requires_enforced_label, Graph};

/// Only IDs and catalog names touched by the ordered redo are stored here.
/// The live graph and catalog remain the read-only baseline for untouched keys.
struct RedoPreflight<'a> {
    graph: &'a Graph,
    catalog: &'a Catalog,
    base_ts: u64,
    nodes: HashMap<NodeId, bool>,
    rels: HashMap<RelId, Option<(NodeId, NodeId)>>,
    indexes: HashMap<String, Option<IndexDef>>,
    constraints: HashMap<String, Option<ConstraintDef>>,
    derived: HashMap<String, Option<cypherlite_model::Constraint>>,
    derived_baseline: HashMap<String, cypherlite_model::Constraint>,
    reset_derived: bool,
}

impl RedoPreflight<'_> {
    fn check_op(&mut self, op: &RedoOp) -> Result<()> {
        op.validate_ids()?;
        match op {
            RedoOp::CreateNode { id, .. } => {
                self.nodes.insert(*id, true);
            }
            RedoOp::CreateRel { id, start, end, .. } => {
                if !self.node_exists(*start) || !self.node_exists(*end) {
                    return Err(cypherlite_core::Error::semantic(
                        "cannot create relationship on missing node",
                    ));
                }
                self.rels.insert(*id, Some((*start, *end)));
            }
            RedoOp::DeleteNode { id } => self.detach_node(*id),
            RedoOp::DeleteRel { id } => {
                self.rels.insert(*id, None);
            }
            RedoOp::CreateIndex {
                name,
                label,
                property,
            } => {
                self.insert_index(IndexDef {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                })?;
            }
            RedoOp::DropIndex { name } => {
                self.indexes.insert(name.clone(), None);
            }
            RedoOp::CreateConstraint {
                name,
                relationship,
                label,
                properties,
                kind,
                property_type,
            } => {
                self.insert_constraint(ConstraintDef {
                    name: name.clone(),
                    relationship: *relationship,
                    label: label.clone(),
                    properties: properties.clone(),
                    kind: kind.clone(),
                    property_type: property_type.clone(),
                })?;
            }
            RedoOp::DropConstraint { name } => {
                self.constraints.insert(name.clone(), None);
                self.derived.insert(name.clone(), None);
            }
            RedoOp::CatalogDerivedConstraint { constraint } => {
                self.derived
                    .insert(constraint.name.clone(), Some(constraint.clone()));
            }
            RedoOp::SetCurrentGraphType { body } => {
                self.reset_derived = true;
                self.derived = crate::graph_type::derived_constraints_map(&body.to_body())
                    .map_err(crate::graph_type::persisted_derive_error)?
                    .into_iter()
                    .map(|(name, constraint)| (name, Some(constraint)))
                    .collect();
            }
            RedoOp::CatalogIndexDetail { options, .. } => {
                redo::props_from_json(options)?;
            }
            RedoOp::SetNodeProp { .. }
            | RedoOp::RemoveNodeProp { .. }
            | RedoOp::SetRelProp { .. }
            | RedoOp::RemoveRelProp { .. }
            | RedoOp::AddLabels { .. }
            | RedoOp::RemoveLabels { .. }
            | RedoOp::PutDatabase { .. }
            | RedoOp::DropDatabase { .. }
            | RedoOp::PutGraphType { .. }
            | RedoOp::PullIdentityPut { .. }
            | RedoOp::OntologySet { .. }
            | RedoOp::OntologyDrop { .. } => {}
        }
        Ok(())
    }

    fn constraint(&self, name: &str) -> Option<&ConstraintDef> {
        self.constraints
            .get(name)
            .map(Option::as_ref)
            .unwrap_or_else(|| self.catalog.constraints.get(name))
    }

    fn derived(&self, name: &str) -> Option<&cypherlite_model::Constraint> {
        self.derived
            .get(name)
            .map(Option::as_ref)
            .unwrap_or_else(|| {
                if self.reset_derived {
                    None
                } else {
                    self.derived_baseline.get(name)
                }
            })
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.nodes
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.graph.node_visible_at(id, self.base_ts))
    }

    fn rel_ends(&self, id: RelId) -> Option<(NodeId, NodeId)> {
        self.rels
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.graph.rel_ends_at(id, self.base_ts))
    }

    fn detach_node(&mut self, id: NodeId) {
        self.nodes.insert(id, false);
        // Include relationships created earlier in this redo, as well as
        // untouched live adjacency. No property maps or whole-graph copy.
        for (ends, state) in self
            .rels
            .values_mut()
            .filter_map(|state| state.as_ref().copied().map(|ends| (ends, state)))
        {
            if ends.0 == id || ends.1 == id {
                *state = None;
            }
        }
        for adj in [&self.graph.out_adj, &self.graph.in_adj] {
            if let Some(by_type) = adj.get(&id) {
                for (_, chunks) in &by_type.chunks {
                    for hop in chunks {
                        if self.rel_ends(hop.id).is_some() {
                            self.rels.insert(hop.id, None);
                        }
                    }
                }
            }
        }
    }

    // Seed only potentially conflicting entries, then let Catalog's own
    // insertion methods decide idempotency, name and equivalent-schema errors.
    fn insert_index(&mut self, def: IndexDef) -> Result<()> {
        let mut candidate = Catalog::default();
        for existing in self.catalog.indexes.values().filter(|existing| {
            !self.indexes.contains_key(&existing.name)
                && (existing.name == def.name
                    || (existing.label == def.label && existing.property == def.property))
        }) {
            candidate
                .indexes
                .insert(existing.name.clone(), existing.clone());
        }
        for existing in self.indexes.values().flatten().filter(|existing| {
            existing.name == def.name
                || (existing.label == def.label && existing.property == def.property)
        }) {
            candidate
                .indexes
                .insert(existing.name.clone(), existing.clone());
        }
        candidate.insert_index(def.clone())?;
        self.indexes.insert(def.name.clone(), Some(def));
        Ok(())
    }

    fn insert_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        let relevant = |existing: &ConstraintDef| {
            existing.name == def.name
                || (existing.relationship == def.relationship
                    && existing.label == def.label
                    && existing.properties == def.properties
                    && existing.kind == def.kind)
        };
        let mut candidate = Catalog::default();
        for existing in
            self.catalog.constraints.values().filter(|existing| {
                !self.constraints.contains_key(&existing.name) && relevant(existing)
            })
        {
            candidate
                .constraints
                .insert(existing.name.clone(), existing.clone());
        }
        for existing in self.constraints.values().flatten().filter(|v| relevant(v)) {
            candidate
                .constraints
                .insert(existing.name.clone(), existing.clone());
        }
        candidate.insert_constraint(def.clone())?;
        self.constraints.insert(def.name.clone(), Some(def));
        Ok(())
    }
}

impl Graph {
    /// Read-only ordered applicability check for a record against a stable
    /// graph. The caller serializes it with writers.
    pub fn validate_redo_applicability(&self, ops: &[RedoOp], commit_ts: u64) -> Result<()> {
        self.validate_redo_group_applicability(std::iter::once((ops, commit_ts)))
    }

    /// Validate independent records in commit order against one touched-state
    /// view, without copying any of the group's property payloads.
    pub fn validate_redo_group_applicability<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a [RedoOp], u64)>,
    ) -> Result<()> {
        self.validate_redo_group(records, true)
    }

    pub(super) fn validate_redo_group<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a [RedoOp], u64)>,
        enforce_new: bool,
    ) -> Result<()> {
        let records: Vec<_> = records.into_iter().collect();
        let derived_baseline = {
            let theater = self.theater.lock().unwrap();
            records
                .iter()
                .flat_map(|(ops, _)| ops.iter())
                .filter_map(|op| match op {
                    RedoOp::CreateConstraint { name, .. } => theater
                        .derived_constraints
                        .get(name)
                        .map(|c| (name.clone(), c.clone())),
                    _ => None,
                })
                .collect()
        };
        let catalog = self.catalog.lock().unwrap();
        let mut state = RedoPreflight {
            graph: self,
            catalog: &catalog,
            base_ts: self.commit_ts(),
            nodes: HashMap::new(),
            rels: HashMap::new(),
            indexes: HashMap::new(),
            constraints: HashMap::new(),
            derived: HashMap::new(),
            derived_baseline,
            reset_derived: false,
        };
        let mut last_ts = state.base_ts;
        for (ops, commit_ts) in records {
            if commit_ts == 0 || commit_ts == u64::MAX || commit_ts <= last_ts {
                return Err(cypherlite_core::Error::storage(format!(
                    "WAL commit timestamp {commit_ts} must advance past {last_ts} and be below u64::MAX"
                )));
            }
            // Installation checks endpoints at the proposed write timestamp.
            // Untouched baseline versions must be read at the same timestamp;
            // touched entries still take precedence in the ordered scratch view.
            state.base_ts = commit_ts;
            let mut created = HashSet::new();
            for op in ops {
                state.check_op(op)?;
                if let RedoOp::CreateConstraint { name, .. } = op {
                    created.insert(name.as_str());
                }
            }
            if enforce_new {
                for name in created {
                    if let Some(def) = state
                        .constraint(name)
                        .filter(|d| requires_enforced_label(d))
                    {
                        if !state
                            .derived(name)
                            .is_some_and(|c| matching_enforced_label(def, c))
                        {
                            return Err(Error::storage(format!(
                                "named label constraint {name} is missing matching enforced label"
                            )));
                        }
                    }
                }
            }
            last_ts = commit_ts;
        }
        Ok(())
    }
}
