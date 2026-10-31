//! Checkpoint-facing graph API: restoring checkpoint rows into a graph
//! being loaded, and the payload sizes and dirty ids a checkpoint reads.

use super::*;

/// Every checkpoint row is restored as a version born at ts 1.
const RESTORED_XMIN: u64 = 1;

/// Install a checkpoint row as `id`'s only version. Before any write a
/// chain holds at most an earlier row for the same id — overlapping legacy
/// parts (ADR-0183) can hold an id twice — and the later row replaces it
/// rather than stacking a second live version a delete would leave visible.
fn restore<I, Live>(map: &CoreMap<I, Vec<Ver<Live>>>, id: I, live: Live) -> Result<()>
where
    I: Eq + std::hash::Hash,
{
    let ver = Ver {
        xmin: RESTORED_XMIN,
        xmax: 0,
        live,
    };
    let mut vers = map.entry(id).or_default();
    match vers.as_mut_slice() {
        [] => vers.push(ver),
        [prev] => *prev = ver,
        _ => {
            return Err(cypherlite_core::Error::storage(
                "checkpoint restore into a graph with committed writes",
            ))
        }
    }
    Ok(())
}

/// Checkpoint payload of `id`'s version visible at `ts`, measured by
/// `bytes`; `None` when no version is visible.
fn visible_checkpoint_bytes<I, Live>(
    map: &CoreMap<I, Vec<Ver<Live>>>,
    id: I,
    ts: u64,
    bytes: impl FnOnce(&Live) -> Result<u64>,
) -> Result<Option<u64>>
where
    I: Eq + std::hash::Hash,
{
    let Some(vers) = map.get(&id) else {
        return Ok(None);
    };
    vers.iter()
        .rev()
        .find(|v| visible(v.xmin, v.xmax, ts))
        .map(|v| bytes(&v.live))
        .transpose()
}

impl Graph {
    /// Node ids changed since the last `clear_checkpoint_dirty` (ADR-0022).
    pub fn dirty_node_ids(&self) -> HashSet<NodeId> {
        self.dirty_nodes.lock().unwrap().clone()
    }

    /// Relationship ids changed since the last `clear_checkpoint_dirty`.
    pub fn dirty_rel_ids(&self) -> HashSet<RelId> {
        self.dirty_rels.lock().unwrap().clone()
    }

    pub fn clear_checkpoint_dirty(&self) {
        self.dirty_nodes.lock().unwrap().clear();
        self.dirty_rels.lock().unwrap().clear();
    }

    /// Variable checkpoint payload of the node visible at `ts`: label bytes
    /// plus tagged property JSON. The Parquet codec adds its Int64 column
    /// width. Borrows compact properties; does not build the property map.
    pub fn node_checkpoint_bytes(&self, id: NodeId, ts: u64) -> Result<Option<u64>> {
        visible_checkpoint_bytes(&self.nodes, id, ts, |live| {
            live.checkpoint_bytes(&self.interner)
        })
    }

    /// Variable checkpoint payload of the relationship visible at `ts`:
    /// type-name bytes plus tagged property JSON. The Parquet codec adds
    /// its Int64 column width.
    pub fn rel_checkpoint_bytes(&self, id: RelId, ts: u64) -> Result<Option<u64>> {
        visible_checkpoint_bytes(&self.rels, id, ts, |live| {
            live.checkpoint_bytes(&self.interner)
        })
    }

    /// Restore a node from a checkpoint (visible at commit_ts). Loading an
    /// id again replaces its row; the later row wins. Only valid on a graph
    /// being restored, before any write: an open write is `Error::Storage`.
    /// The label index is built by `Graph::finish_restore` after the load.
    pub fn load_checkpoint_node(&self, node: Node) -> Result<()> {
        self.check_restoring()?;
        let next = checked_persisted_id(node.id, "node")?;
        self.next_node_id.fetch_max(next, Ordering::Relaxed);
        self.catalog
            .lock()
            .unwrap()
            .record_labels(node.labels.iter().cloned());
        let live = LiveNode::from_parts(
            &self.interner,
            std::sync::Arc::unwrap_or_clone(node.labels),
            std::sync::Arc::unwrap_or_clone(node.properties).into(),
        );
        restore(&self.nodes, node.id, live)
    }

    /// Restore a relationship from a checkpoint. Loading an id again
    /// replaces its row, endpoints and type included; the later row wins.
    /// Only valid on a graph being restored, before any write: an open
    /// write is `Error::Storage`. Adjacency is built from the restored rows
    /// by `Graph::finish_restore` after the load.
    pub fn load_checkpoint_rel(&self, rel: Relationship) -> Result<()> {
        self.check_restoring()?;
        let next = checked_persisted_id(rel.id, "relationship")?;
        checked_persisted_id(rel.start, "relationship start")?;
        checked_persisted_id(rel.end, "relationship end")?;
        self.next_rel_id.fetch_max(next, Ordering::Relaxed);
        let live = LiveRel::from_parts(
            &self.interner,
            rel.start,
            rel.end,
            &rel.rel_type,
            std::sync::Arc::unwrap_or_clone(rel.properties).into(),
        );
        self.catalog.lock().unwrap().record_rel_type(rel.rel_type);
        restore(&self.rels, rel.id, live)
    }

    /// End a checkpoint load, after its last row: build adjacency and the
    /// indexes from the restored versions and clear the dirty ids, which
    /// the checkpoint already holds. Called once per load; WAL replay after
    /// it keeps adjacency current and ends in [`Graph::rebuild_indexes`].
    #[doc(hidden)]
    pub fn finish_restore(&self) {
        self.rebuild_adj();
        self.rebuild_indexes();
        self.clear_checkpoint_dirty();
    }

    fn check_restoring(&self) -> Result<()> {
        if self.in_write.load(Ordering::Acquire) {
            return Err(cypherlite_core::Error::storage(
                "checkpoint restore while a write is open",
            ));
        }
        Ok(())
    }
}
