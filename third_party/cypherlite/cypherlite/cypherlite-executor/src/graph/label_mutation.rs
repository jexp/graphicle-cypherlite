//! LABS-425 (stellar round 1, the mod.rs 1k-creep finding): the
//! label-mutation twins — `add_labels` / `remove_labels`, one input
//! domain, one direction each — in their own impl-block file (the
//! graph module's methods-in-their-own-file precedent, the sibling of
//! the degree-stats family). The twins' shape is deliberately paired:
//! an entry guard per twin (the remove side's drop-set guard strictly
//! generalizes the add side's input-shaped one — a request whose names
//! are all uninterned removes nothing), the same
//! actually-mutated-ids guard on the redo/dirty pair, the same
//! effect-recording WAL payload, and the same unguarded hand-off to
//! the degree-stats family's own empty-set early returns (LABS-425
//! round-1 F-1-17, rounds 3-4).
//! `Graph` (graph/mod.rs) keeps the struct; every label-set mutation
//! seam lives here.

use super::*;

impl Graph {
    pub fn add_labels(&self, id: NodeId, labels: &[String]) {
        if labels.is_empty() {
            return;
        }
        let before = self.node(id);
        let before_len = self.node_chain_len(id);
        let mut added = Vec::new();
        let mut added_ids: Vec<u32> = Vec::new();
        let mut string_kids: Vec<u32> = Vec::new();
        if let Some(mut n) = self.cow_node(id) {
            for l in labels {
                let lid = self.interner.intern(l);
                if !n.has_label(lid) {
                    n.labels.push(lid);
                    added.push(l.clone());
                    added_ids.push(lid);
                }
            }
            crate::memory::fit_reserved(&mut n.labels);
            if !added_ids.is_empty() {
                // New label pairs must pick up existing properties.
                string_kids = n.props.iter().map(|(kid, _)| kid).collect();
            }
            self.put_node(id, n);
        }
        self.bump_stats(|s| s.labels_added += added.len() as i64);
        self.catalog
            .lock()
            .unwrap()
            .record_labels(added.iter().cloned());
        if !added.is_empty() {
            self.push_redo(RedoOp::add_labels(id, added));
            self.mark_node_dirty(id);
        }
        self.index_add(id);
        for &lid in &added_ids {
            for &kid in &string_kids {
                self.declared_dirty(lid, kid, id);
            }
        }
        self.charge_node_after_edit(id, before.as_ref(), before_len);
        // LABS-351: a new label inherits the node's current out-degree
        // buckets, so `SET n:Hub` on an already-connected supernode lands
        // in the (Hub, type) histogram immediately — the never-miss
        // invariant holds for labels added at any time, not only at node
        // create (the blocker the local passes caught).
        self.rebucket_added_labels(id, &added_ids);
        // LABS-370: the added labels inherit the node's rel
        // contributions on both endpoint sides — the canonical incident
        // walk (graph/degree_stats.rs).
        self.apply_incident_pair_counts(id, &added_ids, CountDelta::Add);
        // LABS-370 (v1 follow-up): the exact label counts follow the
        // actually-added ids.
        self.bump_label_counts(&added_ids, CountDelta::Add);
    }

    pub fn remove_labels(&self, id: NodeId, labels: &[String]) {
        // A name never interned cannot be on the node (its label set is
        // interned ids): the drop set resolves through lookups only, so
        // a stray request name neither mints an id nor charges
        // interned-string bytes.
        let drop: HashSet<u32> = labels
            .iter()
            .filter_map(|l| self.interner.get_id(l))
            .collect();
        // The effect guard, one early exit for every no-op shape: an
        // empty request, one whose names are all uninterned, one whose
        // names are interned elsewhere but not carried by this node, or
        // a missing node — none of it intersects the live label set the
        // COW pass would walk (`live_label_ids`, the xmax == 0 version —
        // the in-write set included), so nothing is removed. The
        // request's resolution alone is not the effect: without this
        // guard, `REMOVE n:Foo` with Foo interned elsewhere passed it
        // and paid a full version push (an identical clone per `cow`)
        // for a no-op — a content-free version, invisible to reads at
        // any ts and absent from the checkpoint payload (which
        // serializes the visible version), but charged to `used_bytes`
        // all the same.
        let live = self.live_label_ids(id);
        if !drop.iter().any(|l| live.contains(l)) {
            return;
        }
        let before = self.node(id);
        let before_len = self.node_chain_len(id);
        let mut actually_removed: Vec<u32> = Vec::new();
        if let Some(mut n) = self.cow_node(id) {
            // One pass, the overlay twin's canonical move: extract_if
            // both collects the actually-removed ids and retains the
            // rest.
            actually_removed = n.labels.extract_if(.., |l| drop.contains(l)).collect();
            crate::memory::fit_reserved(&mut n.labels);
            self.put_node(id, n);
        }
        // The count reads at the bump site; a COW miss is the empty set.
        self.bump_stats(|s| s.labels_removed += actually_removed.len() as i64);
        // The redo/dirty pair is guarded on the actually-mutated ids; the
        // degree-stats family calls below ride the family's own
        // empty-set early returns.
        if !actually_removed.is_empty() {
            // The effect, not the request: the WAL row records the names
            // the node actually lost (the add and overlay twins' policy,
            // ADR-0309), derived from the effect ids through the
            // canonical id→name reverse lookup — the ids are node
            // labels, so the published-before-use invariant holds.
            let lost: Vec<String> = actually_removed
                .iter()
                .map(|&lid| String::from(&*self.interner.get(lid)))
                .collect();
            self.push_redo(RedoOp::remove_labels(id, lost));
            self.mark_node_dirty(id);
        }
        self.index_add(id);
        self.charge_node_after_edit(id, before.as_ref(), before_len);
        // LABS-351: a removed label no longer owns the node's degree
        // buckets (saturating: the high-water degree may exceed the
        // current live count) — the ids are the labels ACTUALLY removed;
        // the requested list would decrement buckets a label never
        // contributed.
        self.unbucket_removed_labels(id, &actually_removed);
        // LABS-370: the actually-removed labels give up the node's rel
        // contributions on both endpoint sides — the canonical incident
        // walk, direction -1 (graph/degree_stats.rs).
        self.apply_incident_pair_counts(id, &actually_removed, CountDelta::Remove);
        // LABS-370 (v1 follow-up): the exact label counts follow the
        // actually-removed ids.
        self.bump_label_counts(&actually_removed, CountDelta::Remove);
    }
}

#[cfg(test)]
mod tests;
