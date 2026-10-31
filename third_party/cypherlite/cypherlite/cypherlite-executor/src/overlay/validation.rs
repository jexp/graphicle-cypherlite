//! The overlay's side of incremental constraint validation (ADR-0172): the
//! validation cursor over the scratch's redo log, the catalog fingerprint
//! the validations ran against (ADR-0174), the in-place label check, and
//! the live-entity walks of a full re-validation.
//!
//! Cursor protocol (ADR-0173): [`ExecGraph::unvalidated_redo`] hands out
//! the unvalidated slice and its end; the caller validates it and passes
//! the end to [`ExecGraph::mark_redo_validated`] only once that returned
//! `Ok`. The fingerprint follows the same validate-then-mark protocol
//! (ADR-0174): [`ExecGraph::remember_validated_catalog`] runs only after
//! a validation returned `Ok`.

use cypherlite_core::{NodeId, RelId};

use super::redo_log::ValidatedCatalog;
use crate::redo::RedoOp;

use super::ExecGraph;

impl<'a> ExecGraph<'a> {
    /// Hand `f` the redo ops not yet validated (ADR-0172 §1) and return its
    /// result with the log position just past them. The cursor does not
    /// move: pass that position to [`Self::mark_redo_validated`] once the
    /// slice validated. A view without a scratch has an empty slice.
    pub(crate) fn unvalidated_redo<R>(&self, f: impl FnOnce(&[RedoOp]) -> R) -> (R, usize) {
        let Some(cell) = self.scratch else {
            return (f(&[]), 0);
        };
        let s = cell.borrow();
        let (ops, end) = s.redo.unvalidated();
        (f(&ops), end)
    }

    /// Advance the validation cursor to `end`, a position
    /// [`Self::unvalidated_redo`] returned, after that slice validated `Ok`
    /// (ADR-0173). A failed validation leaves its slice unvalidated.
    pub(crate) fn mark_redo_validated(&self, end: usize) {
        if let Some(cell) = self.scratch {
            cell.borrow_mut().redo.mark_validated(end);
        }
    }

    /// The catalog fingerprint the scratch's last successful validation
    /// ran against (ADR-0174); `None` when it never validated, or when
    /// the view has no scratch.
    pub(crate) fn validated_catalog(&self) -> Option<ValidatedCatalog> {
        self.scratch
            .and_then(|cell| cell.borrow().redo.validated_catalog().cloned())
    }

    /// Remember `catalog` as what the scratch's validations have checked,
    /// after one returned `Ok` (ADR-0174) — the validate-then-mark
    /// protocol of [`Self::mark_redo_validated`].
    pub(crate) fn remember_validated_catalog(&self, catalog: ValidatedCatalog) {
        if let Some(cell) = self.scratch {
            cell.borrow_mut().redo.remember_validated_catalog(catalog);
        }
    }

    /// Whether live node `id` carries `label`, compared in place: no label
    /// list is cloned. `None` when the node is not live.
    pub(crate) fn node_has_label(&self, id: NodeId, label: &str) -> Option<bool> {
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            if s.batch.deleted_nodes.contains(&id) {
                return None;
            }
            if let Some(n) = s.batch.nodes.get(&id) {
                return Some(n.labels.iter().any(|l| l == label));
            }
        }
        self.base.node_has_label_at(id, self.snapshot_ts(), label)
    }

    /// Every live node carrying `label`, ascending: the snapshot's carriers
    /// the scratch neither deleted nor holds a copy of, plus the scratch's
    /// nodes carrying it. Used by the full re-validation of a node
    /// constraint (ADR-0172 §5).
    pub(crate) fn live_node_ids_with_label(&self, label: &str) -> Vec<NodeId> {
        let mut ids = self.base.node_ids_with_label_at(label, self.snapshot_ts());
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            ids.retain(|id| !s.batch.deleted_nodes.contains(id) && !s.batch.nodes.contains_key(id));
            ids.extend(
                s.batch
                    .nodes
                    .iter()
                    .filter(|(id, n)| {
                        !s.batch.deleted_nodes.contains(id) && n.labels.iter().any(|l| l == label)
                    })
                    .map(|(id, _)| *id),
            );
            ids.sort_unstable();
        }
        ids
    }

    /// Every live relationship of `rel_type`, ascending, built like
    /// [`Self::live_node_ids_with_label`]. The snapshot side is one O(R)
    /// walk that reads each relationship once and compares interned type
    /// ids: the graph keeps no relationship-by-type index. Used only by the
    /// full re-validation of a relationship constraint (ADR-0172 §5); a
    /// per-statement uniqueness probe reads the relationship equality
    /// postings instead (ADR-0185).
    pub(crate) fn live_rel_ids_of_type(&self, rel_type: &str) -> Vec<RelId> {
        let mut ids = self.base.rel_ids_of_type_at(rel_type, self.snapshot_ts());
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            ids.retain(|id| !s.batch.deleted_rels.contains(id) && !s.batch.rels.contains_key(id));
            ids.extend(
                s.batch
                    .rels
                    .iter()
                    .filter(|(id, r)| !s.batch.deleted_rels.contains(id) && r.rel_type == rel_type)
                    .map(|(id, _)| *id),
            );
            ids.sort_unstable();
        }
        ids
    }
}
