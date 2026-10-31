//! Point reads of one node or relationship through the write overlay: a
//! scratch deletion hides the element, a scratch copy answers for it, and
//! otherwise the snapshot at `snapshot_ts` does.

use cypherlite_core::{Node, NodeId, RelId, Relationship, Value};
use cypherlite_ir::Direction;

use super::ExecGraph;

/// What the overlay scratch says about one element (ADR-0016). Every point
/// read below starts from this prologue: a scratch deletion hides the
/// element, a scratch copy answers for it, and scratch silence falls
/// through to the committed snapshot at `snapshot_ts`.
enum ScratchAnswer<T> {
    /// The scratch deleted the element: the read answers "gone".
    Deleted,
    /// The scratch holds the element's copy, which answered the read.
    Answered(T),
    /// The scratch says nothing: read the snapshot.
    Snapshot,
}

impl<T> ScratchAnswer<T> {
    /// Resolve the prologue into the read's answer: `deleted` when the
    /// scratch hid the element, the scratch copy's answer when it spoke,
    /// and `snapshot()` otherwise.
    fn resolve(self, deleted: T, snapshot: impl FnOnce() -> T) -> T {
        match self {
            Self::Deleted => deleted,
            Self::Answered(answer) => answer,
            Self::Snapshot => snapshot(),
        }
    }
}

impl ExecGraph<'_> {
    /// The node side of the visibility prologue: `read` answers from the
    /// scratch's copy of `id` when it holds one.
    fn node_prologue<T>(&self, id: NodeId, read: impl FnOnce(&Node) -> T) -> ScratchAnswer<T> {
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            if s.batch.deleted_nodes.contains(&id) {
                return ScratchAnswer::Deleted;
            }
            if let Some(n) = s.batch.nodes.get(&id) {
                return ScratchAnswer::Answered(read(n));
            }
        }
        ScratchAnswer::Snapshot
    }

    /// The relationship side of the visibility prologue: `read` answers
    /// from the scratch's copy of `id` when it holds one.
    fn rel_prologue<T>(
        &self,
        id: RelId,
        read: impl FnOnce(&Relationship) -> T,
    ) -> ScratchAnswer<T> {
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            if s.batch.deleted_rels.contains(&id) {
                return ScratchAnswer::Deleted;
            }
            if let Some(r) = s.batch.rels.get(&id) {
                return ScratchAnswer::Answered(read(r));
            }
        }
        ScratchAnswer::Snapshot
    }

    pub fn node(&self, id: NodeId) -> Option<Node> {
        self.node_prologue(id, |n| Some(n.clone()))
            .resolve(None, || self.base.node_at(id, self.snapshot_ts()))
    }

    /// Existence check without materializing the node. The profiled hot path
    /// (`node` clones the full `Node` — labels + property map) makes this the
    /// common-case read in a write statement; callers that only need to know
    /// a node is present must not pay for its properties.
    pub fn node_exists(&self, id: NodeId) -> bool {
        self.node_prologue(id, |_| true)
            .resolve(false, || self.base.node_visible_at(id, self.snapshot_ts()))
    }

    /// Label ids without materializing properties. Returns owned label
    /// strings; callers that only compare labels pay no property map clone.
    pub fn node_labels(&self, id: NodeId) -> Option<Vec<String>> {
        self.node_prologue(id, |n| Some((*n.labels).clone()))
            .resolve(None, || self.base.node_labels_at(id, self.snapshot_ts()))
    }

    pub fn rel(&self, id: RelId) -> Option<Relationship> {
        self.rel_prologue(id, |r| Some(r.clone()))
            .resolve(None, || self.base.rel_at(id, self.snapshot_ts()))
    }

    pub fn rel_ends(&self, id: RelId) -> Option<(NodeId, NodeId)> {
        self.rel_prologue(id, |r| Some((r.start, r.end)))
            .resolve(None, || self.base.rel_ends_at(id, self.snapshot_ts()))
    }

    pub fn hop(&self, current: NodeId, rid: RelId, dir: Direction) -> Option<NodeId> {
        let (start, end) = self.rel_ends(rid)?;
        crate::store::hop_other(current, start, end, dir)
    }

    /// Plan-time label cardinality estimate (ADR-0218), forwarded for the
    /// exec layer's pattern-start estimates (ADR-0351).
    pub fn label_count(&self, label: &str) -> u64 {
        self.base.label_count(label)
    }

    /// Existence check without materializing the relationship.
    pub fn rel_exists(&self, id: RelId) -> bool {
        self.rel_prologue(id, |_| true)
            .resolve(false, || self.base.rel_visible_at(id, self.snapshot_ts()))
    }

    /// Type name without materializing the relationship's property map.
    pub fn rel_type(&self, id: RelId) -> Option<String> {
        self.rel_prologue(id, |r| Some(r.rel_type.clone()))
            .resolve(None, || {
                self.base
                    .rel_stub_at(id, self.snapshot_ts())
                    .map(|r| r.rel_type)
            })
    }

    pub fn node_stub(&self, id: NodeId) -> Option<Node> {
        self.node_prologue(id, |n| {
            Some(Node {
                id: n.id,
                labels: n.labels.clone(),
                properties: crate::store::empty_properties(),
            })
        })
        .resolve(None, || self.base.node_stub_at(id, self.snapshot_ts()))
    }

    pub fn rel_stub(&self, id: RelId) -> Option<Relationship> {
        self.rel_prologue(id, |r| {
            Some(Relationship {
                id: r.id,
                start: r.start,
                end: r.end,
                rel_type: r.rel_type.clone(),
                properties: crate::store::empty_properties(),
            })
        })
        .resolve(None, || self.base.rel_stub_at(id, self.snapshot_ts()))
    }

    pub fn node_prop(&self, id: NodeId, key: &str) -> Option<Value> {
        self.node_prologue(id, |n| n.properties.get(key).cloned())
            .resolve(None, || self.base.node_prop_at(id, self.snapshot_ts(), key))
    }

    pub fn rel_prop(&self, id: RelId, key: &str) -> Option<Value> {
        self.rel_prologue(id, |r| r.properties.get(key).cloned())
            .resolve(None, || self.base.rel_prop_at(id, self.snapshot_ts(), key))
    }

    pub fn node_prop_keys(&self, id: NodeId) -> Vec<String> {
        self.node_prologue(id, |n| n.properties.keys().cloned().collect())
            .resolve(Vec::new(), || {
                self.base.node_prop_keys_at(id, self.snapshot_ts())
            })
    }

    pub fn rel_prop_keys(&self, id: RelId) -> Vec<String> {
        self.rel_prologue(id, |r| r.properties.keys().cloned().collect())
            .resolve(Vec::new(), || {
                self.base.rel_prop_keys_at(id, self.snapshot_ts())
            })
    }
}
