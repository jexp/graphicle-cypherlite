//! The write scratch's redo log (ADR-0016) with the indexes over it
//! that ADR-0172 adds: the positions of its catalog ops (§8) and the
//! constraint-validation cursor (§1, ADR-0173) — plus the catalog
//! fingerprint the cursor's validations ran against (ADR-0174).
//!
//! Validation protocol (ADR-0173): read [`RedoLog::unvalidated`], classify
//! and check that slice, and call [`RedoLog::mark_validated`] with its end
//! only after the check returned `Ok`. A failed check leaves the slice
//! unvalidated, so checking the same log again fails again. The
//! fingerprint follows the same protocol: it is remembered only after a
//! validation returned `Ok` (ADR-0174).

use std::collections::HashSet;
use std::mem::size_of;
use std::sync::Arc;

use cypherlite_catalog::ConstraintDef;
use cypherlite_core::{NodeId, PropertyMap};

use crate::redo::RedoOp;

/// In-log record. A create node is the common case and stays inline: id
/// plus two `Arc`s. Every other op, including catalog ops whose variants
/// make `RedoOp` 160 bytes, is boxed so a create does not reserve a
/// catalog-sized slot.
#[derive(Clone, Debug)]
enum LiveRedo {
    CreateNode {
        id: NodeId,
        labels: Arc<Vec<String>>,
        properties: Arc<PropertyMap>,
    },
    Other(Box<RedoOp>),
}

impl LiveRedo {
    fn from_op(op: RedoOp) -> Self {
        match op {
            RedoOp::CreateNode {
                id,
                labels,
                properties,
            } => Self::CreateNode {
                id,
                labels,
                properties,
            },
            other => Self::Other(Box::new(other)),
        }
    }

    fn to_op(&self) -> RedoOp {
        match self {
            Self::CreateNode {
                id,
                labels,
                properties,
            } => RedoOp::CreateNode {
                id: *id,
                labels: Arc::clone(labels),
                properties: Arc::clone(properties),
            },
            Self::Other(op) => RedoOp::clone(op),
        }
    }

    fn mentions(&self, wanted: crate::redo::CatalogName<'_>) -> bool {
        match self {
            // Create nodes stay inline. Their names are the labels and
            // property keys; reading them here avoids rebuilding the op.
            Self::CreateNode {
                labels, properties, ..
            } => match wanted {
                crate::redo::CatalogName::Label(name) => labels.iter().any(|label| label == name),
                crate::redo::CatalogName::RelType(_) => false,
                crate::redo::CatalogName::PropertyKey(name) => properties.contains_key(name),
            },
            Self::Other(op) => op.catalog_names().contains(&wanted),
        }
    }

    fn into_op(self) -> RedoOp {
        match self {
            Self::CreateNode {
                id,
                labels,
                properties,
            } => RedoOp::CreateNode {
                id,
                labels,
                properties,
            },
            Self::Other(op) => *op,
        }
    }
}

/// The constraint catalog one successful validation ran against
/// (ADR-0174): every constraint's definition and derived enforced label,
/// in constraint-name order (the `named_constraints()` order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ValidatedCatalog {
    /// `(definition, enforced label)` per constraint, in name order.
    entries: Vec<(ConstraintDef, Option<String>)>,
}

impl ValidatedCatalog {
    pub(crate) fn new(entries: Vec<(ConstraintDef, Option<String>)>) -> Self {
        Self { entries }
    }

    /// The names of this catalog's constraints that are new or changed
    /// (definition or enforced label) since `remembered` (ADR-0174):
    /// they join the next validation's recheck set. A constraint only in
    /// `remembered` was dropped; it cannot be violated and needs nothing.
    pub(crate) fn new_or_changed_since(&self, remembered: &Self) -> HashSet<String> {
        let before: std::collections::HashMap<&str, &(ConstraintDef, Option<String>)> = remembered
            .entries
            .iter()
            .map(|entry| (entry.0.name.as_str(), entry))
            .collect();
        self.entries
            .iter()
            .filter(|entry| {
                before
                    .get(entry.0.name.as_str())
                    .is_none_or(|old| **old != **entry)
            })
            .map(|entry| entry.0.name.clone())
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct RedoLog {
    ops: Vec<LiveRedo>,
    /// Ascending positions in `ops` of the catalog ops
    /// ([`RedoOp::is_catalog`]), so schema overlays replay only those.
    catalog: Vec<usize>,
    /// `ops[..validated]` passed constraint validation. Never exceeds
    /// `ops.len()`: `push` only grows `ops`, `take` resets both, and
    /// `mark_validated` accepts only an end `unvalidated` handed out.
    validated: usize,
    /// The catalog the last successful validation ran against
    /// (ADR-0174); `None` until the first one. The first validation needs
    /// no diff: its slice holds every write the scratch has made, each
    /// checked against the full current catalog.
    validated_catalog: Option<ValidatedCatalog>,
}

impl RedoLog {
    pub(super) fn push(&mut self, op: RedoOp) {
        if op.is_catalog() {
            self.catalog.push(self.ops.len());
        }
        self.ops.push(LiveRedo::from_op(op));
    }

    /// Bytes of the reserved redo buffer. Create slots are the compact
    /// record; the buffer is what stays resident while the statement runs.
    pub(super) fn reserved_bytes(&self) -> u64 {
        self.ops.capacity() as u64 * size_of::<LiveRedo>() as u64
    }

    pub(super) fn len(&self) -> usize {
        self.ops.len()
    }

    /// Hand over every op and reset the catalog positions and the cursor.
    pub(super) fn take(&mut self) -> Vec<RedoOp> {
        std::mem::take(self)
            .ops
            .into_iter()
            .map(LiveRedo::into_op)
            .collect()
    }

    /// The logged ops in commit order. Materialized for validation and
    /// batch commit; the resident log stays [`LiveRedo`].
    pub(super) fn materialize(&self) -> Vec<RedoOp> {
        self.ops.iter().map(LiveRedo::to_op).collect()
    }

    /// Whether any logged op mentions `wanted`. Create nodes are read in
    /// place; every other op is borrowed, so a catalog check does not
    /// rebuild the log.
    pub(super) fn mentions(&self, wanted: crate::redo::CatalogName<'_>) -> bool {
        self.ops.iter().any(|live| live.mentions(wanted))
    }

    /// The catalog ops, in log order (ADR-0172 §8).
    pub(super) fn catalog_ops(&self) -> impl Iterator<Item = &RedoOp> {
        self.catalog.iter().filter_map(|&i| match &self.ops[i] {
            LiveRedo::Other(op) => Some(op.as_ref()),
            LiveRedo::CreateNode { .. } => None,
        })
    }

    /// The ops appended since the last [`Self::mark_validated`], and the log
    /// position just past them: the `end` to mark once they validate.
    pub(super) fn unvalidated(&self) -> (Vec<RedoOp>, usize) {
        debug_assert!(
            self.validated <= self.ops.len(),
            "validation cursor {} past the redo log ({} ops)",
            self.validated,
            self.ops.len()
        );
        (
            self.ops[self.validated..]
                .iter()
                .map(LiveRedo::to_op)
                .collect(),
            self.ops.len(),
        )
    }

    /// Advance the validation cursor to `end`, a position
    /// [`Self::unvalidated`] returned, after that slice validated `Ok`.
    pub(super) fn mark_validated(&mut self, end: usize) {
        debug_assert!(
            self.validated <= end && end <= self.ops.len(),
            "mark_validated({end}) outside {}..={}",
            self.validated,
            self.ops.len()
        );
        self.validated = end;
    }

    /// The catalog the last successful validation ran against (ADR-0174).
    pub(super) fn validated_catalog(&self) -> Option<&ValidatedCatalog> {
        self.validated_catalog.as_ref()
    }

    /// Remember `catalog` as what the scratch's validations have checked,
    /// after one returned `Ok` (ADR-0174) — the validate-then-mark
    /// protocol of [`Self::mark_validated`].
    pub(super) fn remember_validated_catalog(&mut self, catalog: ValidatedCatalog) {
        self.validated_catalog = Some(catalog);
    }

    /// Cursor a rollback restores. The op vec stays with the live log so a
    /// failed `CALL { … } IN TRANSACTIONS` batch does not keep a second
    /// copy — and so per-batch capture is O(1) in the accumulated ops, not
    /// a quadratic by-value copy on the no-sink path where each batch
    /// re-captures over an ever-growing log.
    pub(super) fn cursor(&self) -> RedoCursor {
        RedoCursor {
            len: self.ops.len(),
            catalog: self.catalog.clone(),
            validated: self.validated,
            validated_catalog: self.validated_catalog.clone(),
        }
    }

    /// Drop ops appended after `cursor` and put the catalog index and
    /// validation mark back. Id allocation is not part of the log. The
    /// truncation's size is deliberately not reported: the rollback image
    /// already holds the scratch's applied-change count
    /// ([`super::scratch::StatementState`]), so nothing derives it from the
    /// cursor again.
    pub(super) fn restore_cursor(&mut self, cursor: RedoCursor) {
        // A cursor comes from this log and the log only grows within a batch,
        // so its position is always within the live op vec. A breach is a
        // rollback contract break, not a smaller rollback: `truncate` would
        // silently drop nothing and leave the failed batch's ops in the log,
        // so it fails loudly here instead.
        assert!(
            cursor.len <= self.ops.len(),
            "rollback cursor {} past the redo log ({} ops)",
            cursor.len,
            self.ops.len()
        );
        self.ops.truncate(cursor.len);
        self.catalog = cursor.catalog;
        self.validated = cursor.validated;
        self.validated_catalog = cursor.validated_catalog;
    }
}

/// Pre-batch redo position. Owned separately from [`RedoLog::ops`] so
/// restoring it truncates the live log instead of swapping in a clone.
#[derive(Clone, Debug)]
pub(super) struct RedoCursor {
    len: usize,
    catalog: Vec<usize>,
    validated: usize,
    validated_catalog: Option<ValidatedCatalog>,
}

#[cfg(test)]
mod tests;
