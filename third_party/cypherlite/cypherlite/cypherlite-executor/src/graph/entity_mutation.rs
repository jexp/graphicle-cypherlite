//! The entity-mutation family every cross-cutting feature hooks: the create /
//! install / copy-on-write / property-edit / delete seams for nodes and
//! relationships, in their own impl-block file (the `label_mutation.rs` and
//! `degree_stats.rs` precedent, a sibling of the graph family). `Graph`
//! (graph/mod.rs) keeps the struct and the family-independent plumbing
//! (ids, pins, charging, catalog reads); the seams that install a version,
//! maintain the indexes/adjacency/counts, and push redo live here.

use super::*;

impl Graph {
    pub fn create_node(
        &self,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> Result<NodeId> {
        let id = self.alloc_node_id()?;
        self.install_node(id, labels, properties)?;
        Ok(id)
    }

    pub(in crate::graph) fn install_node(
        &self,
        id: NodeId,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> Result<()> {
        let next = checked_persisted_id(id, "node")?;
        self.next_node_id.fetch_max(next, Ordering::Relaxed);
        self.catalog
            .lock()
            .unwrap()
            .record_labels(labels.iter().cloned());
        self.bump_stats(|s| {
            s.nodes_created += 1;
            s.labels_added += labels.len() as i64;
            s.properties_set += properties.len() as i64;
        });
        if self.recording() {
            self.push_redo(RedoOp::create_node(
                id,
                std::sync::Arc::new(labels.clone()),
                properties.clone(),
            ));
        }
        let xmin = self.xmin_for_install();
        let live = LiveNode::from_parts(&self.interner, labels, properties);
        let charge =
            crate::memory::live_node_account_live(&live, &self.interner, &self.index_charge());
        // LABS-370 (v1 follow-up): the exact label counts follow every
        // create, before the version owns the live set.
        self.bump_label_counts(&live.labels, CountDelta::Add);
        // ADR-0439: a created node introduces its constrained tuple(s); a
        // replay that re-creates an id frees the previous live tuple first.
        // The map's scope decides whether the node can change any tuple.
        let edit = self.tracked_node_edit(id, |scope| {
            scope.touches_node(&live.labels, live.props.iter().map(|(kid, _)| kid))
        });
        self.finish_node_edit(edit, id, Some(&live));
        {
            let mut vers = self.nodes.entry(id).or_default();
            vers.push(NodeVer {
                xmin,
                xmax: 0,
                live,
            });
            crate::memory::fit_reserved(&mut vers);
        }
        self.index_add(id);
        self.mark_node_dirty(id);
        // ADR-0063: lazy string postings — just dirty-mark built pairs, and
        // only after the version is installed (apply-before-dirty ordering).
        self.string_dirty_mark_node(id);
        self.charge(charge as i64 + crate::memory::node_chain_bytes(1) as i64);
        Ok(())
    }

    pub fn create_rel(
        &self,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        self.validate_rel_endpoints(start, end)?;
        let id = self.alloc_rel_id()?;
        self.install_rel(id, start, end, rel_type, properties)?;
        Ok(id)
    }

    pub(in crate::graph) fn validate_rel_endpoints(
        &self,
        start: NodeId,
        end: NodeId,
    ) -> Result<()> {
        checked_persisted_id(start, "relationship start")?;
        checked_persisted_id(end, "relationship end")?;
        // Endpoint existence without materializing the endpoint nodes.
        let ts_now = self.read_ts();
        if !self.node_visible_at(start, ts_now) || !self.node_visible_at(end, ts_now) {
            return Err(super::missing_rel_endpoint_error());
        }
        Ok(())
    }

    pub(in crate::graph) fn install_rel(
        &self,
        id: RelId,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: BTreeMap<String, Value>,
    ) -> Result<()> {
        let next = checked_persisted_id(id, "relationship")?;
        self.validate_rel_endpoints(start, end)?;
        self.next_rel_id.fetch_max(next, Ordering::Relaxed);
        self.catalog
            .lock()
            .unwrap()
            .record_rel_type(rel_type.clone());
        self.bump_stats(|s| {
            s.relationships_created += 1;
            s.properties_set += properties.len() as i64;
        });
        if self.recording() {
            let op = RedoOp::create_rel(id, start, end, rel_type.clone(), properties.clone());
            self.push_redo(op);
        }
        let xmin = self.xmin_for_install();
        let live = LiveRel::from_parts(&self.interner, start, end, &rel_type, properties);
        let charge = crate::memory::live_rel_account_live(&live);
        let ts = live.payload.ts(&self.interner);
        let out_before = adj_floor(&self.out_adj, start);
        let in_before = adj_floor(&self.in_adj, end);
        let degree_before = self.out_chunk_len(start, live.type_id);
        adj_insert(&self.out_adj, start, live.type_id, id, ts);
        adj_insert(&self.in_adj, end, live.type_id, id, ts);
        self.bump_rel_type(live.type_id, CountDelta::Add);
        self.bump_degree_histograms(start, live.type_id, degree_before);
        self.bump_pair_counts(start, live.type_id, end, CountDelta::Add);
        let adj = adj_floor(&self.out_adj, start) as i64 - out_before as i64
            + adj_floor(&self.in_adj, end) as i64
            - in_before as i64;
        // ADR-0439: a created relationship introduces its constrained
        // tuple(s); relationship constraints have no backing-index postings.
        // The map's scope decides whether the relationship can change one.
        // Its key ids resolve before the scope probe takes the map lock
        // (interner shard read, then map lock — never the reverse).
        let key_ids = live.payload.key_ids(&self.interner);
        let edit = self.tracked_rel_edit(id, |scope| scope.touches_rel(live.type_id, &key_ids));
        self.finish_rel_edit(edit, id, Some(&live));
        {
            let mut vers = self.rels.entry(id).or_default();
            vers.push(RelVer {
                xmin,
                xmax: 0,
                live,
            });
            crate::memory::fit_reserved(&mut vers);
        }
        self.mark_rel_dirty(id);
        // ADR-0185: lazy relationship postings — dirty-mark built pairs, and
        // only after the version is installed (apply-before-dirty ordering).
        self.rel_index_add(id);
        self.charge(charge as i64 + crate::memory::rel_chain_bytes(1) as i64 + adj);
        Ok(())
    }

    pub(in crate::graph) fn cow_node(&self, id: NodeId) -> Option<LiveNode> {
        cow(&self.write_ts, &self.nodes, id)
    }

    pub(in crate::graph) fn put_node(&self, id: NodeId, live: LiveNode) {
        // The write-back follows a successful `cow_node`, so the version
        // chain exists by construction; a missing one is a broken invariant,
        // never a silent discard of the edit.
        let mut vers = self
            .nodes
            .get_mut(&id)
            .expect("put_node follows a successful cow_node");
        let last = vers
            .last_mut()
            .expect("a node version chain is never empty");
        last.live = live;
    }

    pub(in crate::graph) fn cow_rel(&self, id: RelId) -> Option<LiveRel> {
        cow(&self.write_ts, &self.rels, id)
    }

    pub(in crate::graph) fn put_rel(&self, id: RelId, live: LiveRel) {
        // The write-back follows a successful `cow_rel`, so the version chain
        // exists by construction; a missing one is a broken invariant, never
        // a silent discard of the edit.
        let mut vers = self
            .rels
            .get_mut(&id)
            .expect("put_rel follows a successful cow_rel");
        let last = vers
            .last_mut()
            .expect("a relationship version chain is never empty");
        last.live = live;
    }

    pub fn set_property_node(&self, id: NodeId, key: &str, value: Value) {
        let before = self.node(id);
        if before.is_none() {
            // No live entity: there is nothing to edit or to hand off.
            return;
        }
        let before_len = self.node_chain_len(id);
        // ADR-0439: capture the before-image only when a uniqueness
        // constraint reads this key; a SET of an unconstrained property
        // cannot change any tuple, so it skips the map entirely. The map's
        // scope is the only source of that answer.
        let keys = self.tracked_node_key_edit(id, key);
        let mut n = self
            .cow_node(id)
            .expect("a live node has a copy-on-write version");
        let kid = self.interner.intern(key);
        let (op, props_set) = if value.is_null() {
            n.props.remove(kid);
            (RedoOp::remove_node_prop(id, key.to_string()), 0i64)
        } else {
            n.props.set(kid, value.clone());
            (RedoOp::set_node_prop(id, key.to_string(), value.clone()), 1)
        };
        // The edited live view is in hand: hand it to the map before it
        // moves into the version chain, never re-read it from the map.
        self.finish_node_edit(keys, id, Some(&n));
        self.put_node(id, n);
        self.bump_stats(|s| s.properties_set += props_set);
        self.push_redo(op);
        self.mark_node_dirty(id);
        // Dirty-mark only after the new version is installed — the ADR-0185
        // invariant: `index_add` marks every (label, key) pair the installed
        // version carries; a removal leaves stale postings, which the
        // residual re-verification filters.
        self.index_add(id);
        self.charge_node_after_edit(id, before.as_ref(), before_len);
    }

    pub fn set_property_rel(&self, id: RelId, key: &str, value: Value) {
        if !self.rel_exists(id) {
            // No live entity: there is nothing to edit or to hand off.
            return;
        }
        let before_bytes = self.visible_rel_account(id);
        let before_len = self.rel_chain_len(id);
        let keys = self.tracked_rel_key_edit(id, key);
        let mut r = self
            .cow_rel(id)
            .expect("a live relationship has a copy-on-write version");
        let old_ts = r.payload.ts(&self.interner);
        let start = r.start;
        let end = r.end;
        let type_id = r.type_id;
        let (op, props_set) = if value.is_null() {
            r.payload.remove(&self.interner, key);
            (RedoOp::remove_rel_prop(id, key.to_string()), 0i64)
        } else {
            r.payload.set(&self.interner, key, value.clone());
            (RedoOp::set_rel_prop(id, key.to_string(), value.clone()), 1)
        };
        let new_ts = r.payload.ts(&self.interner);
        // The edited live view is in hand: hand it to the map before it
        // moves into the version chain, never re-read it from the map.
        self.finish_rel_edit(keys, id, Some(&r));
        self.put_rel(id, r);
        if old_ts != new_ts {
            adj_reposition(&self.out_adj, start, type_id, id, new_ts);
            adj_reposition(&self.in_adj, end, type_id, id, new_ts);
        }
        // Dirty-mark only after the new version is installed — the
        // ADR-0185 invariant: `rel_index_add` reads the installed version's
        // own type and keys; a removal's key is gone from the payload, and
        // its stale postings are filtered by the residual re-verification.
        self.rel_index_add(id);
        self.bump_stats(|s| s.properties_set += props_set);
        self.push_redo(op);
        self.mark_rel_dirty(id);
        self.charge_rel_after_edit(id, before_bytes, before_len);
    }

    pub fn merge_properties_node(&self, id: NodeId, map: &BTreeMap<String, Value>) {
        // One cycle for the whole map, mirroring how `install_node` takes a
        // map in one pass: one scope probe, one cow, one tracked-edit
        // completion, one index/charge update.
        let before = self.node(id);
        if before.is_none() || map.is_empty() {
            return;
        }
        let before_len = self.node_chain_len(id);
        let keys = self.tracked_node_edit(id, |scope| {
            map.keys().any(|key| {
                self.interner
                    .get_id(key)
                    .is_some_and(|kid| scope.touches_node_key(kid))
            })
        });
        let mut n = self
            .cow_node(id)
            .expect("a live node has a copy-on-write version");
        let mut props_set = 0i64;
        let mut ops = Vec::with_capacity(map.len());
        for (key, value) in map {
            let kid = self.interner.intern(key);
            if value.is_null() {
                n.props.remove(kid);
                ops.push(RedoOp::remove_node_prop(id, key.clone()));
            } else {
                n.props.set(kid, value.clone());
                props_set += 1;
                ops.push(RedoOp::set_node_prop(id, key.clone(), value.clone()));
            }
        }
        // The edited live view is in hand: hand it to the map before it
        // moves into the version chain.
        self.finish_node_edit(keys, id, Some(&n));
        self.put_node(id, n);
        self.bump_stats(|s| s.properties_set += props_set);
        for op in ops {
            self.push_redo(op);
        }
        self.mark_node_dirty(id);
        self.index_add(id);
        self.charge_node_after_edit(id, before.as_ref(), before_len);
    }

    /// Tombstone timestamp for a delete: the write ts while in a write,
    /// else the commit ts clamped to ≥ 1 so the tombstone is never the
    /// "no tombstone" sentinel 0. Both MVCC delete paths share this rule.
    pub(in crate::graph) fn tombstone_ts(&self) -> u64 {
        if self.in_write.load(Ordering::Acquire) {
            self.write_ts.load(Ordering::Acquire)
        } else {
            self.commit_ts().max(1)
        }
    }

    pub fn delete_rel(&self, id: RelId) {
        let ts = self.tombstone_ts();
        // ADR-0439: free the constrained tuple from the live version before
        // the tombstone (entity guard first, map lock last).
        self.constraint_keys_delete_rel(id);
        let mut found = false;
        if let Some(mut vers) = self.rels.get_mut(&id) {
            if let Some(v) = vers.iter_mut().rev().find(|v| v.xmax == 0) {
                let type_id = v.live.type_id;
                let (start, end) = (v.live.start, v.live.end);
                v.xmax = ts;
                self.bump_rel_type(type_id, CountDelta::Remove);
                // LABS-370: the pair counts are exact (the counting
                // operator answers from them), so a delete leaves its
                // (start-label, type, end-label) triples — the mirror of
                // the create-path bump.
                self.bump_pair_counts(start, type_id, end, CountDelta::Remove);
                found = true;
            }
        }
        if found {
            self.bump_stats(|s| s.relationships_deleted += 1);
            self.push_redo(RedoOp::delete_rel(id));
            self.mark_rel_dirty(id);
        }
    }

    pub fn delete_node(&self, id: NodeId, detach: bool) -> Result<()> {
        // The canonical incident scan: visibility-filtered at the read
        // timestamp and self-loop-deduplicated (graph/adjacency.rs).
        let incident = self.neighbors(id, cypherlite_ir::Direction::Both);
        if !detach && !incident.is_empty() {
            return Err(delete_connected_node_error());
        }
        for rid in incident {
            self.delete_rel(rid);
        }
        // ADR-0439: free the node's constrained tuple(s) from the live
        // version before the tombstone (entity guard first, map lock last).
        self.constraint_keys_delete_node(id);
        let ts = self.tombstone_ts();
        let mut found = false;
        if let Some(mut vers) = self.nodes.get_mut(&id) {
            if let Some(v) = vers.iter_mut().rev().find(|v| v.xmax == 0) {
                v.xmax = ts;
                // LABS-370 (v1 follow-up): the exact label counts follow
                // every delete — the label index keeps the tombstoned id
                // until GC, this map does not.
                self.bump_label_counts(&v.live.labels, CountDelta::Remove);
                found = true;
            }
        }
        if found {
            self.bump_stats(|s| s.nodes_deleted += 1);
            self.push_redo(RedoOp::delete_node(id));
            self.mark_node_dirty(id);
        }
        Ok(())
    }
}
