//! Adjacency access (ADR-0031/ADR-0033 typed chunks): degree, neighbor
//! iteration, and snapshot-visible traversal — split out of `mod.rs`.
use super::hop_counts;
use super::*;

fn flatten_adj(by_type: Option<&TypedAdj>) -> Vec<RelId> {
    match by_type {
        Some(m) => m.flatten(),
        None => Vec::new(),
    }
}

pub(super) fn adj_insert(
    map: &CoreMap<NodeId, TypedAdj>,
    node: NodeId,
    type_id: u32,
    id: RelId,
    ts: Option<i64>,
) {
    map.entry(node).or_default().insert(type_id, id, ts);
}

pub(super) fn adj_reposition(
    map: &CoreMap<NodeId, TypedAdj>,
    node: NodeId,
    type_id: u32,
    id: RelId,
    ts: Option<i64>,
) {
    map.entry(node).or_default().reposition(type_id, id, ts);
}

impl Graph {
    pub fn degree_out(&self, id: NodeId) -> Vec<RelId> {
        flatten_adj(self.out_adj.get(&id).as_deref())
    }

    /// Typed adjacency length without snapshot filtering. An approximate
    /// frontier for deciding whether a far-node index probe is cheaper than
    /// checking the reached neighbors. An empty `types` list counts every type.
    pub(crate) fn typed_adj_len(
        &self,
        id: NodeId,
        dir: cypherlite_ir::Direction,
        types: &[String],
    ) -> usize {
        if types.is_empty() {
            return self.adj_len(id, dir);
        }
        let type_ids = self.interned_type_ids(types);
        let len = |m: &CoreMap<NodeId, TypedAdj>| {
            let Some(adj) = m.get(&id) else {
                return 0;
            };
            type_ids
                .iter()
                .map(|type_id| adj.chunk(*type_id).len())
                .sum()
        };
        match dir {
            cypherlite_ir::Direction::Outgoing => len(&self.out_adj),
            cypherlite_ir::Direction::Incoming => len(&self.in_adj),
            cypherlite_ir::Direction::Both => len(&self.out_adj) + len(&self.in_adj),
        }
    }

    /// One node's out-degree on one rel type (the typed adjacency chunk
    /// length) — the id-based core `typed_adj_len`'s per-type arm and the
    /// LABS-351 histogram maintenance both consume. No string re-interning:
    /// the caller holds the interned type id.
    pub(crate) fn out_chunk_len(&self, node: NodeId, type_id: u32) -> u64 {
        self.out_adj
            .get(&node)
            .map(|adj| adj.chunk(type_id).len() as u64)
            .unwrap_or(0)
    }

    /// A node's out-adjacency chunks, one `(rel type, out-degree)` pair
    /// per chunk: the module's one `(type, count)` chunk enumeration
    /// ([`hop_counts`], which the adjacency-delta estimate already
    /// consults), widened to the u64 the degree-statistic entries count
    /// in. The LABS-351 histogram maintenance
    /// ([`Graph::rebucket_added_labels`] /
    /// [`Graph::unbucket_removed_labels`]) and the admission estimate's
    /// histogram term all consult it. `Graph::incident_rels` hand-walks
    /// the same chunk map at hop granularity — the other sanctioned walk,
    /// which this accessor deliberately does not replace.
    pub(super) fn out_chunks(&self, node: NodeId) -> Vec<(u32, u64)> {
        hop_counts(&self.out_adj, node)
            .into_iter()
            .map(|(type_id, len)| (type_id, len as u64))
            .collect()
    }

    /// Distinct interned type ids, skipping names this graph has never seen.
    fn interned_type_ids(&self, types: &[String]) -> Vec<u32> {
        let mut seen = HashSet::new();
        types
            .iter()
            .filter_map(|ty| {
                if !seen.insert(ty.as_str()) {
                    return None;
                }
                self.interner.get_id(ty)
            })
            .collect()
    }

    /// Adjacency length without allocation or type/overlay filtering —
    /// an approximate degree for choosing which side of an existence
    /// probe to walk (LABS-193).
    pub fn adj_len(&self, id: NodeId, dir: cypherlite_ir::Direction) -> usize {
        let len = |m: &CoreMap<NodeId, TypedAdj>| {
            m.get(&id)
                .map(|a| a.chunks.iter().map(|(_, v)| v.len()).sum())
                .unwrap_or(0)
        };
        match dir {
            cypherlite_ir::Direction::Outgoing => len(&self.out_adj),
            cypherlite_ir::Direction::Incoming => len(&self.in_adj),
            cypherlite_ir::Direction::Both => len(&self.out_adj) + len(&self.in_adj),
        }
    }

    pub fn degree_in(&self, id: NodeId) -> Vec<RelId> {
        flatten_adj(self.in_adj.get(&id).as_deref())
    }

    pub fn neighbors(&self, id: NodeId, dir: cypherlite_ir::Direction) -> Vec<RelId> {
        self.neighbors_at(id, dir, self.read_ts())
    }

    /// Snapshot-visible neighbors without cloning adjacency. `types` empty = all types.
    pub fn for_each_neighbor_at(
        &self,
        id: NodeId,
        dir: cypherlite_ir::Direction,
        ts: u64,
        types: &[String],
        mut f: impl FnMut(RelId),
    ) {
        let _ = self.try_for_each_neighbor_at::<()>(id, dir, ts, types, |rid| {
            f(rid);
            Ok(())
        });
    }

    /// Fallible [`Self::for_each_neighbor_at`]: the error channel is typed
    /// and the first error stops the iteration.
    pub fn try_for_each_neighbor_at<E>(
        &self,
        id: NodeId,
        dir: cypherlite_ir::Direction,
        ts: u64,
        types: &[String],
        mut f: impl FnMut(RelId) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        use cypherlite_ir::Direction;
        match dir {
            Direction::Outgoing => {
                self.for_each_typed(&self.out_adj, id, ts, types, &mut f)?;
            }
            Direction::Incoming => {
                self.for_each_typed(&self.in_adj, id, ts, types, &mut f)?;
            }
            Direction::Both => {
                let mut seen = HashSet::new();
                let mut dedup = |rid: RelId| -> std::result::Result<(), E> {
                    if seen.insert(rid) {
                        f(rid)?;
                    }
                    Ok(())
                };
                self.for_each_typed(&self.out_adj, id, ts, types, &mut dedup)?;
                self.for_each_typed(&self.in_adj, id, ts, types, &mut dedup)?;
            }
        }
        Ok(())
    }

    fn for_each_typed<E>(
        &self,
        map: &CoreMap<NodeId, TypedAdj>,
        id: NodeId,
        ts: u64,
        types: &[String],
        f: &mut impl FnMut(RelId) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let Some(by_type) = map.get(&id) else {
            return Ok(());
        };
        if types.is_empty() {
            for (_, vec) in &by_type.chunks {
                for hop in vec {
                    if self.rel_visible_at(hop.id, ts) {
                        f(hop.id)?;
                    }
                }
            }
        } else {
            for tid in self.interned_type_ids(types) {
                for hop in by_type.chunk(tid) {
                    if self.rel_visible_at(hop.id, ts) {
                        f(hop.id)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn neighbors_at(&self, id: NodeId, dir: cypherlite_ir::Direction, ts: u64) -> Vec<RelId> {
        let mut out = Vec::new();
        self.for_each_neighbor_at(id, dir, ts, &[], |rid| out.push(rid));
        out
    }
}
