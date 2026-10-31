//! Catalog views of the overlay: the committed (or admitted) catalog with
//! the scratch's catalog ops replayed over it (ADR-0172 §8), and the named
//! index / constraint DDL that appends those ops.

use std::collections::BTreeMap;

use cypherlite_catalog::{ConstraintDef, IndexDef};
use cypherlite_core::Result;

use crate::graph::{QueryStats, ShowSchema};
use crate::redo::{CatalogName, RedoOp};

use super::{ExecGraph, WriteScratch};

/// A catalog definition keyed by its name, for the overlay fold
/// [`ExecGraph::named_defs`] shares.
trait NamedDef {
    fn def_name(&self) -> &str;
}

impl NamedDef for IndexDef {
    fn def_name(&self) -> &str {
        &self.name
    }
}

impl NamedDef for ConstraintDef {
    fn def_name(&self) -> &str {
        &self.name
    }
}

/// The catalog facts of a view (ADR-0363 decision 5): the committed catalog
/// or interner plus the names the view's write overlay named. The overlay's
/// redo log is its write record — no second name set — read through
/// [`RedoOp::catalog_names`], so a label, type or key a transaction just
/// wrote is known before the commit publishes it.
impl ExecGraph<'_> {
    /// Whether this view's catalog knows `label`: the committed catalog or a
    /// label any scratch write named. The committed contract is monotone (a
    /// label outlives its last node); the scratch side keeps it: a create or
    /// label change counts even if a later write removed it.
    pub fn has_label(&self, label: &str) -> bool {
        self.base.has_label(label) || self.scratch_wrote(|scratch| scratch.has_written_label(label))
    }

    /// Whether this view's catalog knows relationship type `rel_type`; the
    /// label contract applies unchanged.
    pub fn has_rel_type(&self, rel_type: &str) -> bool {
        self.base.has_rel_type(rel_type)
            || self.scratch_wrote(|scratch| scratch.has_written_rel_type(rel_type))
    }

    /// Whether this view's interner knows property key `key`: the committed
    /// interner or a property key any scratch write named. A hit still only
    /// suppresses a warning; a miss is a true miss.
    pub fn has_property_key(&self, key: &str) -> bool {
        self.base.has_property_key(key)
            || self.scratch_wrote(|scratch| scratch.has_written_property_key(key))
    }

    /// Run a catalog question against the write overlay when the view has
    /// one; a read view answers from the committed facts alone.
    fn scratch_wrote(&self, question: impl Fn(&WriteScratch) -> bool) -> bool {
        self.scratch
            .is_some_and(|scratch| question(&scratch.borrow()))
    }
}

/// The write overlay's catalog facts, read through
/// [`RedoOp::catalog_names`] — the one total derivation of the names an op
/// mentions — over the redo log, the one record every scratch write funnels
/// through. The redo keeps a name after a later op removes it, matching the
/// committed catalog's monotone contract.
impl WriteScratch {
    fn mentions(&self, wanted: CatalogName<'_>) -> bool {
        self.redo.mentions(wanted)
    }

    fn has_written_label(&self, label: &str) -> bool {
        self.mentions(CatalogName::Label(label))
    }

    fn has_written_rel_type(&self, rel_type: &str) -> bool {
        self.mentions(CatalogName::RelType(rel_type))
    }

    fn has_written_property_key(&self, key: &str) -> bool {
        self.mentions(CatalogName::PropertyKey(key))
    }
}

impl<'a> ExecGraph<'a> {
    pub(crate) fn show_schema(&self) -> Result<ShowSchema> {
        // A view carrying an admitted `SchemaSnapshot` reads that pinned
        // theater. Schema-less views (`read`/`read_at`/`write`) fall back to
        // the live committed theater under its lock; those constructors are
        // documented as test/diagnostic or non-SHOW callers, and the
        // autocommit write path reaches `validate_constraints` while holding
        // the publication read guard, so the fallback cannot observe a torn
        // theater. Statement execution that must be admitted uses the
        // `_with_schema` constructors.
        let mut show = self
            .schema
            .map(|schema| schema.show.clone())
            .unwrap_or_else(|| self.base.capture_show_schema());
        if let Some(scratch) = self.scratch {
            show.apply_overlay(scratch.borrow().redo.catalog_ops())?;
        }
        Ok(show)
    }

    /// The overlay fold [`Self::named_indexes`] and [`Self::named_constraints`]
    /// share (ADR-0172 §8): the committed (or admitted) definitions keyed by
    /// name, with the scratch's catalog ops replayed over them — `apply`
    /// upserts on a create and removes on a drop.
    fn named_defs<D: NamedDef>(
        &self,
        committed: Vec<D>,
        apply: impl Fn(&mut BTreeMap<String, D>, &RedoOp),
    ) -> Vec<D> {
        let mut map: BTreeMap<String, D> = committed
            .into_iter()
            .map(|d| (d.def_name().to_string(), d))
            .collect();
        if let Some(cell) = self.scratch {
            for op in cell.borrow().redo.catalog_ops() {
                apply(&mut map, op);
            }
        }
        map.into_values().collect()
    }

    pub fn named_indexes(&self) -> Vec<IndexDef> {
        let committed = self
            .schema
            .map(|s| s.indexes.clone())
            .unwrap_or_else(|| self.base.named_indexes());
        self.named_defs(committed, |map, op| match op {
            RedoOp::CreateIndex {
                name,
                label,
                property,
            } => {
                map.insert(
                    name.clone(),
                    IndexDef {
                        name: name.clone(),
                        label: label.clone(),
                        property: property.clone(),
                    },
                );
            }
            RedoOp::DropIndex { name } => {
                map.remove(name);
            }
            _ => {}
        })
    }

    pub fn named_index(&self, name: &str) -> Option<IndexDef> {
        self.named_indexes().into_iter().find(|d| d.name == name)
    }

    pub fn named_index_by_schema(&self, label: &str, property: &str) -> Option<IndexDef> {
        self.named_indexes().into_iter().find(|d| {
            d.label == label
                && crate::redo::split_legacy_property_encoding(&d.property).0 == property
        })
    }

    pub fn named_constraints(&self) -> Vec<ConstraintDef> {
        let committed = self
            .schema
            .map(|s| s.constraints.clone())
            .unwrap_or_else(|| self.base.named_constraints());
        self.named_defs(committed, |map, op| match op {
            RedoOp::CreateConstraint {
                name,
                relationship,
                label,
                properties,
                kind,
                property_type,
            } => {
                map.insert(
                    name.clone(),
                    ConstraintDef {
                        name: name.clone(),
                        relationship: *relationship,
                        label: label.clone(),
                        properties: properties.clone(),
                        kind: kind.clone(),
                        property_type: property_type.clone(),
                    },
                );
            }
            RedoOp::DropConstraint { name } => {
                map.remove(name);
            }
            _ => {}
        })
    }

    /// Index DDL: create a standalone index, counted in
    /// `QueryStats::indexes_added` (ADR-0309).
    pub fn create_named_index(&self, name: String, label: String, property: String) -> Result<()> {
        self.push_schema_redo(
            RedoOp::CreateIndex {
                name,
                label,
                property,
            },
            |stats| stats.indexes_added += 1,
        )
    }

    /// Create the range index backing an index-backed constraint. It is
    /// part of that constraint, so it is not counted as an index (ADR-0309).
    pub(crate) fn create_backing_index(
        &self,
        name: String,
        label: String,
        property: String,
    ) -> Result<()> {
        self.push_schema_redo(
            RedoOp::CreateIndex {
                name,
                label,
                property,
            },
            |_| {},
        )
    }

    pub(crate) fn push_catalog_redo(&self, op: RedoOp) {
        self.scratch_mut().push_redo(op);
    }

    /// Index DDL: drop a standalone index, counted in
    /// `QueryStats::indexes_removed` (ADR-0309).
    pub fn drop_named_index(&self, name: String) -> Result<()> {
        self.push_schema_redo(RedoOp::DropIndex { name }, |stats| {
            stats.indexes_removed += 1
        })
    }

    /// Drop the backing index of a constraint being dropped; not counted as
    /// an index (ADR-0309).
    pub(crate) fn drop_backing_index(&self, name: String) -> Result<()> {
        self.push_schema_redo(RedoOp::DropIndex { name }, |_| {})
    }

    pub fn create_named_constraint(
        &self,
        name: String,
        relationship: bool,
        label: String,
        properties: Vec<String>,
        kind: String,
        property_type: Option<String>,
    ) -> Result<()> {
        self.push_schema_redo(
            RedoOp::CreateConstraint {
                name,
                relationship,
                label,
                properties,
                kind,
                property_type,
            },
            |stats| stats.constraints_added += 1,
        )
    }

    pub fn drop_named_constraint(&self, name: String) -> Result<()> {
        self.push_schema_redo(RedoOp::DropConstraint { name }, |stats| {
            stats.constraints_removed += 1
        })
    }

    /// Append one schema op and count it where it is recorded, next to the
    /// mutation (the `writes.rs` accounting rule, ADR-0309).
    fn push_schema_redo(&self, op: RedoOp, count: impl FnOnce(&mut QueryStats)) -> Result<()> {
        let mut scratch = self.scratch_mut();
        count(&mut scratch.statement.stats);
        scratch.push_redo(op);
        Ok(())
    }
}
