//! LABS-425 (stellar round 1, the mod.rs 1k-creep finding): the
//! label-mutation twins — `add_labels` / `remove_labels`, one input
//! domain, one direction each — in their own impl-block file (the
//! graph module's methods-in-their-own-file precedent, the sibling of
//! the degree-stats family). The twins' shape is deliberately paired:
//! one effect-shaped guard per twin (the request's ids resolved against
//! the live label set, so an empty request, an all-already-carried add,
//! an all-uninterned remove, and a missing node all no-op), one effect id
//! vector, the same actually-mutated-ids guard on the redo/dirty pair,
//! the same effect-derived WAL payload, and the same unguarded hand-off to
//! the degree-stats family's own empty-set early returns (LABS-425
//! round-1 F-1-17, rounds 3-4).
//! `Graph` (graph/mod.rs) keeps the struct; every label-set mutation
//! seam lives here.

use super::*;

impl Graph {
    pub fn add_labels(&self, id: NodeId, labels: &[String]) {
        let before = self.node(id);
        if before.is_none() {
            return;
        }
        let before_len = self.node_chain_len(id);
        // The effect, not the request: the ids the node does not already
        // carry. ADD mints a name it has never seen (unlike REMOVE, whose
        // drop set is lookups only), so each request label is interned here
        // — before the scope probe takes the map lock (interner shard read,
        // then map lock, the seam lock order). A request of only carried
        // labels has no effect and no-ops below.
        let live = self.live_label_ids(id);
        let mut added_ids: Vec<u32> = Vec::new();
        for label in labels {
            let lid = self.interner.intern(label);
            if !live.contains(&lid) && !added_ids.contains(&lid) {
                added_ids.push(lid);
            }
        }
        if added_ids.is_empty() {
            return;
        }
        // ADR-0439: adding a constraint label introduces the node's
        // constrained tuple; capture the before-image now. A label no
        // constraint carries cannot change any tuple — the map's scope is
        // the only source of that answer.
        let keys = self.tracked_node_edit(id, |scope| scope.touches_node_labels(&added_ids));
        let mut n = self
            .cow_node(id)
            .expect("a live node has a copy-on-write version");
        n.labels.extend(added_ids.iter().copied());
        crate::memory::fit_reserved(&mut n.labels);
        // New label pairs must pick up existing properties.
        let string_kids: Vec<u32> = n.props.iter().map(|(kid, _)| kid).collect();
        // The edited live view is in hand: hand it to the map before it
        // moves into the version chain (the canonical completion arm).
        self.finish_node_edit(keys, id, Some(&n));
        self.put_node(id, n);
        // The WAL/catalog names derive from the effect ids, through the
        // canonical id→name lookup (remove_labels' policy): the ids are node
        // labels, so the published-before-use invariant holds.
        let added: Vec<String> = added_ids
            .iter()
            .map(|&lid| String::from(&*self.interner.get(lid)))
            .collect();
        self.bump_stats(|s| s.labels_added += added_ids.len() as i64);
        self.catalog
            .lock()
            .unwrap()
            .record_labels(added.iter().cloned());
        self.push_redo(RedoOp::add_labels(id, added));
        self.mark_node_dirty(id);
        self.index_add(id);
        for &lid in &added_ids {
            for &kid in &string_kids {
                self.declared_dirty(lid, kid, id);
            }
        }
        self.charge_node_after_edit(id, before.as_ref(), before_len);
        // The one count-family entry: the degree rebucket (LABS-351), the
        // incident pair counts and the exact label counts (LABS-370).
        self.apply_label_delta_stats(id, &added_ids, CountDelta::Add);
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
        // ADR-0439: a removed constraint label frees the node's constrained
        // tuple; a label no constraint carries cannot change any tuple. The
        // map's scope is the only source of that answer.
        // `drop` is non-empty here (the effect guard above returned
        // otherwise), so one scope read answers for the whole request.
        let drop_ids: Vec<u32> = drop.iter().copied().collect();
        let keys = self.tracked_node_edit(id, |scope| scope.touches_node_labels(&drop_ids));
        let mut n = self
            .cow_node(id)
            .expect("a live node has a copy-on-write version");
        // One pass, the overlay twin's canonical move: extract_if both
        // collects the actually-removed ids and retains the rest.
        let actually_removed: Vec<u32> = n.labels.extract_if(.., |l| drop.contains(l)).collect();
        crate::memory::fit_reserved(&mut n.labels);
        // The edited live view is in hand: hand it to the map before it
        // moves into the version chain (the canonical completion arm).
        self.finish_node_edit(keys, id, Some(&n));
        self.put_node(id, n);
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
        // The one count-family entry, in the removal direction (the ids are
        // the labels ACTUALLY removed, never the requested list).
        self.apply_label_delta_stats(id, &actually_removed, CountDelta::Remove);
    }
}

#[cfg(test)]
mod tests;
