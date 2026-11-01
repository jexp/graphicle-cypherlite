use super::adjacency::adj_insert;
use super::*;

/// Lazily materialized equality postings for one `(class, key)` pair
/// (ADR-0063): the postings are a **superset** of true matches — stale ids
/// from overwritten/deleted versions are filtered by the seek's residual
/// snapshot re-verification (ADR-0018). A pair exists only after an
/// equality seek materialized it; writes to unmaterialized pairs cost
/// nothing, writes after materialization dirty-mark the entity. A node
/// pair's class is a label id; a relationship pair's class is a type id
/// (ADR-0185); the ids share `NodeId`'s/`RelId`'s `i64` shape.
#[derive(Debug, Default)]
pub(super) struct PropPairIndex {
    /// True once the pair has been built from the label index at least once.
    pub(super) built: bool,
    /// Property value → ids that have carried it for this pair.
    pub(super) postings: HashMap<GroupKey, HashSet<NodeId>>,
    /// Ids written since `postings` was last brought current for this pair.
    pub(super) dirty: HashSet<NodeId>,
}

/// All lazily maintained index state of a [`Graph`] (ADR-0063 lazy
/// postings, ADR-0185 relationship pairs): the label index, the equality
/// and declared pair maps, the interned `*`/empty-label sentinel ids, and
/// the committed index memory charge. Bundled so `Graph`'s fields name
/// what the graph is, and the index lifecycle lives beside the index code.
#[derive(Debug)]
pub(super) struct Indexes {
    pub(super) label_index: CoreMap<u32, HashSet<NodeId>>,
    /// `(label_id, key_id)` → lazily materialized equality postings
    /// (ADR-0063 contract applied to equality seeks). A pair exists only
    /// after an equality seek built it; writes after the build dirty-mark.
    pub(super) prop_pairs: CoreMap<(u32, u32), PropPairIndex>,
    /// `(rel_type_id, key_id)` → lazily materialized equality postings, the
    /// relationship twin of `prop_pairs` (ADR-0185).
    pub(super) rel_prop_pairs: CoreMap<(u32, u32), PropPairIndex>,
    /// `(label_id, key_id)` → trigram postings for a `CREATE TEXT INDEX`.
    pub(super) gram_pairs: CoreMap<(u32, u32), GramIndex>,
    /// `(label_id, key_id)` → ordered postings for a `CREATE RANGE INDEX`.
    pub(super) range_pairs: CoreMap<(u32, u32), RangeIndex>,
    /// `(label_id, key_id)` → spatial postings for a `CREATE POINT INDEX`.
    pub(super) point_pairs: CoreMap<(u32, u32), PointIndex>,
    /// Interned id of the `*` any-label bucket used when a seek has no label.
    pub(super) star_label_id: u32,
    /// Interned id of the empty-string bucket for label-less nodes.
    pub(super) empty_label_id: u32,
    /// Committed [`crate::memory::IndexCharge`]. Rebuilt when `index_details`
    /// changes so each node write clones an `Arc` instead of rescanning the
    /// catalog.
    pub(super) charge_cache: Mutex<Arc<crate::memory::IndexCharge>>,
}

impl Indexes {
    pub(super) fn new(interner: &Interner) -> Self {
        Self {
            label_index: CoreMap::default(),
            prop_pairs: CoreMap::default(),
            rel_prop_pairs: CoreMap::default(),
            gram_pairs: CoreMap::default(),
            range_pairs: CoreMap::default(),
            point_pairs: CoreMap::default(),
            star_label_id: interner.intern("*"),
            empty_label_id: interner.intern(""),
            charge_cache: Mutex::new(Arc::new(crate::memory::IndexCharge::default())),
        }
    }

    /// Drop every lazily built structure (ADR-0063): the next seek or probe
    /// rebuilds from the live graph on demand. `rebuild_indexes` re-fills
    /// the label index and the declared pairs afterwards.
    fn clear(&self) {
        self.label_index.clear();
        self.prop_pairs.clear();
        self.rel_prop_pairs.clear();
        self.gram_pairs.clear();
        self.range_pairs.clear();
        self.point_pairs.clear();
    }
}

impl Graph {
    /// Insert `id` into the label index: the empty-label bucket when it
    /// carries no labels, then one entry per label. The one spelling the
    /// write path ([`index_add`](Self::index_add)) and the wholesale rebuild
    /// ([`rebuild_indexes`](Self::rebuild_indexes)) share.
    fn label_index_insert(&self, id: NodeId, label_ids: &[u32]) {
        // One insert policy: a label-less node lands in the empty-label
        // bucket, a labelled one in each of its label buckets.
        let lids: &[u32] = if label_ids.is_empty() {
            &[self.indexes.empty_label_id]
        } else {
            label_ids
        };
        for &lid in lids {
            self.indexes.label_index.entry(lid).or_default().insert(id);
        }
    }

    /// Add `id`'s label/equality postings, read straight from the live
    /// record — no `Node` materialization on the bulk path (ADR-0063).
    /// Same visibility rule as `node()`: the version visible at `read_ts`.
    pub(super) fn index_add(&self, id: NodeId) {
        let ts = self.read_ts();
        let Some(vers) = self.nodes.get(&id) else {
            return;
        };
        let Some(v) = vers.iter().rev().find(|v| visible(v.xmin, v.xmax, ts)) else {
            return;
        };
        let live = &v.live;
        self.label_index_insert(id, &live.labels);
        // Equality postings are lazy (ADR-0063 contract): dirty-mark only the
        // `(label, key)` pairs already materialized by a seek. A pair nobody
        // seeks costs nothing on the write path.
        for (lid, kid) in self.declared_dirty_pairs(live) {
            if let Some(mut st) = self.indexes.prop_pairs.get_mut(&(lid, kid)) {
                if st.built {
                    st.dirty.insert(id);
                }
            }
            self.declared_dirty(lid, kid, id);
        }
    }

    /// The `(label id, key id)` pairs a live node's declared postings cover:
    /// its labels plus the any-label bucket, crossed with every property key.
    /// The one walk `index_add` and `string_dirty_mark_node` share.
    fn declared_dirty_pairs<'a>(
        &self,
        live: &'a LiveNode,
    ) -> impl Iterator<Item = (u32, u32)> + 'a {
        let star = self.indexes.star_label_id;
        live.labels
            .iter()
            .copied()
            .chain(std::iter::once(star))
            .flat_map(move |lid| live.props.iter().map(move |(kid, _)| (lid, kid)))
    }

    /// Dirty-mark the built string pairs of `id`'s installed live version —
    /// the id-taking shape the other lazy-posting helpers use; the caller has
    /// just installed the version (apply-before-dirty ordering).
    pub(super) fn string_dirty_mark_node(&self, id: NodeId) {
        let vers = self
            .nodes
            .get(&id)
            .expect("string dirty-mark follows an installed node version");
        let live = &vers
            .last()
            .expect("a node version chain is never empty")
            .live;
        for (lid, kid) in self.declared_dirty_pairs(live) {
            self.declared_dirty(lid, kid, id);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Whole-graph adjacency builds (`rebuild_adj`) on this thread.
    static ADJ_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_adj_builds() -> u64 {
    ADJ_BUILDS.with(|n| n.replace(0))
}

impl Graph {
    /// Derive adjacency from each relationship's live version, inserting in
    /// ascending id order: a node's type chunks then follow their first id,
    /// as an id-ordered load leaves them, not the map's hash order. The
    /// degree-statistic half of the rebuild (rel-type/pair counts,
    /// histograms) lives behind the family seam:
    /// [`Graph::rebuild_degree_stats`].
    pub(super) fn rebuild_adj(&self) {
        #[cfg(test)]
        ADJ_BUILDS.with(|n| n.set(n.get() + 1));
        let mut rows: Vec<_> = self
            .rels
            .iter()
            .filter_map(|e| {
                let live = &live_version(e.as_slice())?.live;
                let ts = live.payload.ts(&self.interner);
                Some((*e.key(), live.start, live.type_id, live.end, ts))
            })
            .collect();
        rows.sort_unstable_by_key(|&(id, ..)| id);
        self.out_adj.clear();
        self.in_adj.clear();
        for &(id, start, type_id, end, ts) in &rows {
            adj_insert(&self.out_adj, start, type_id, id, ts);
            adj_insert(&self.in_adj, end, type_id, id, ts);
        }
        self.rebuild_degree_stats(
            rows.iter()
                .map(|&(_id, start, type_id, end, _ts)| (start, type_id, end)),
        );
        // The wholesale rebuild replaces the histogram map: recompute the
        // incremental `used_bytes` so it tracks the new entry count (the
        // recount oracle includes the histogram term).
        self.recompute_used();
    }

    /// Rebuild the indexes from the live versions. Adjacency is left as it
    /// is: writes and replayed redo keep it current, and
    /// `Graph::finish_restore` builds it after a checkpoint load.
    pub fn rebuild_indexes(&self) {
        // Equality and string postings are both lazy (ADR-0063), and
        // relationship equality pairs are lazy the same way (ADR-0185):
        // drop them; the next seek rebuilds from the live graph on demand.
        self.indexes.clear();
        let ts = self.commit_ts();
        for id in self.all_node_ids_at(ts) {
            if let Some(n) = self.node_at(id, ts) {
                let label_ids: Vec<u32> =
                    n.labels.iter().map(|l| self.interner.intern(l)).collect();
                self.label_index_insert(id, &label_ids);
            }
        }
        // LABS-370 (v1 follow-up): the exact label counts rebuild wholesale
        // with the other node-derived indexes (the family owns the policy).
        self.rebuild_label_counts();
        // ADR-0439: the committed constraint-key map rebuilds wholesale from
        // the live graph on open, checkpoint restore, and abort — like
        // `label_counts`, and with replay-installed duplicates carried
        // forward. Its charge is re-tallied by `recompute_used` below.
        self.rebuild_constraint_keys();
        // Equality, prefix, trigram, and point postings follow catalog
        // indexes. `rebuild_declared_indexes` refills the pairs named by
        // TEXT / RANGE / POINT; anything else stays a scan.
        self.rebuild_declared_indexes();
        // Checkpoint restore installs `index_details` without going through
        // `apply_redo`, so the charge cache is still empty until here.
        self.refresh_index_charge();
        self.recompute_used();
    }

    pub fn gc_versions(&self) {
        let horizon = self.gc_horizon();
        let nids: Vec<NodeId> = self.nodes.iter().map(|e| *e.key()).collect();
        for id in nids {
            self.gc_node_chain(id, horizon);
        }
        let rids: Vec<RelId> = self.rels.iter().map(|e| *e.key()).collect();
        for id in rids {
            self.gc_rel_chain(id, horizon);
        }
        self.recompute_used();
    }

    pub(super) fn gc_node_chain(&self, id: NodeId, horizon: u64) {
        let Some(mut vers) = self.nodes.get_mut(&id) else {
            return;
        };
        let dead: Vec<Node> = vers
            .iter()
            .filter(|v| v.dead_at(horizon))
            .map(|v| v.live.to_node(id, &self.interner))
            .collect();
        if dead.is_empty() {
            return;
        }
        vers.retain(|v| !v.dead_at(horizon));
        if !vers.is_empty() {
            crate::memory::fit_reserved(&mut vers);
        }
        let remaining: Vec<Node> = vers
            .iter()
            .map(|v| v.live.to_node(id, &self.interner))
            .collect();
        let empty = vers.is_empty();
        drop(vers);
        if empty {
            self.nodes.remove(&id);
        }
        self.index_drop_unused(id, &dead, &remaining);
    }

    pub(super) fn gc_rel_chain(&self, id: RelId, horizon: u64) {
        // Pairs are lazy (ADR-0185): when none was ever built there is
        // nothing to prune, so the dead/remaining collection — two
        // `LiveRel` clones per collected version — is skipped. Checked
        // before the `rels` guard is taken: the pair guard is never taken
        // while a `rels` guard is held. A pair built racing this check
        // keeps the collected ids as stale postings: a superset leak,
        // never a correctness one.
        let prune = !self.indexes.rel_prop_pairs.is_empty();
        let Some(mut vers) = self.rels.get_mut(&id) else {
            return;
        };
        if !vers.iter().any(|v| v.dead_at(horizon)) {
            return;
        }
        let dead: Vec<LiveRel> = if prune {
            vers.iter()
                .filter(|v| v.dead_at(horizon))
                .map(|v| v.live.clone())
                .collect()
        } else {
            Vec::new()
        };
        vers.retain(|v| !v.dead_at(horizon));
        if !vers.is_empty() {
            crate::memory::fit_reserved(&mut vers);
        }
        let remaining: Vec<LiveRel> = if prune {
            vers.iter().map(|v| v.live.clone()).collect()
        } else {
            Vec::new()
        };
        let empty = vers.is_empty();
        drop(vers);
        if empty {
            self.rels.remove(&id);
        }
        // Prune after the `rels` guard is released — the pair guard is never
        // taken while a `rels` guard is held. When `prune` is false both
        // slices are empty and this is a no-op.
        self.rel_index_drop_unused(id, &dead, &remaining);
    }

    pub(super) fn index_drop_unused(&self, id: NodeId, dead: &[Node], remaining: &[Node]) {
        let mut keep_labels = HashSet::new();
        let mut keep_props = HashSet::new();
        for n in remaining {
            collect_index_keys(n, &mut keep_labels, &mut keep_props);
        }
        let mut drop_labels = HashSet::new();
        let mut drop_props = HashSet::new();
        for n in dead {
            collect_index_keys(n, &mut drop_labels, &mut drop_props);
        }
        for l in drop_labels {
            if keep_labels.contains(&l) {
                continue;
            }
            let lid = if l.is_empty() {
                self.indexes.empty_label_id
            } else {
                match self.interner.get_id(&l) {
                    Some(lid) => lid,
                    None => continue,
                }
            };
            if let Some(mut set) = self.indexes.label_index.get_mut(&lid) {
                set.remove(&id);
            }
        }
        // The pair-side prune shares `drop_unused_pairs` with the
        // relationship twin; the string (label, key) triples resolve to
        // interned ids here through `string_pair_id`, the one pair-id
        // resolver (`*` bucket included).
        let drop_props: Vec<(String, String, GroupKey)> = drop_props.into_iter().collect();
        let keep_props: Vec<(String, String, GroupKey)> = keep_props.into_iter().collect();
        self.drop_unused_pairs(
            &self.indexes.prop_pairs,
            id,
            &drop_props,
            &keep_props,
            |(label, key, gk), f| {
                let Some((lid, kid)) = self.string_pair_id(label, key) else {
                    return;
                };
                f(lid, kid, gk.clone());
            },
        );
    }
}

fn collect_index_keys(
    n: &Node,
    labels: &mut HashSet<String>,
    props: &mut HashSet<(String, String, GroupKey)>,
) {
    if n.labels.is_empty() {
        labels.insert(String::new());
    }
    labels.extend(n.labels.iter().cloned());
    for (k, v) in &*n.properties {
        if let Some(gk) = GroupKey::for_property_index(v) {
            for l in &*n.labels {
                props.insert((l.clone(), k.clone(), gk.clone()));
            }
            props.insert(("*".into(), k.clone(), gk));
        }
    }
}

impl Graph {
    /// Nodes stored under `label`. `0` when the label is absent.
    /// The label index can hold stale ids, so this is an upper bound used
    /// only to compare pattern costs — the same label-index estimate as
    /// [`Self::label_count`], which owns the one implementation, so this
    /// pattern-stats spelling delegates to it.
    pub fn pattern_label_rows(&self, label: &str) -> u64 {
        self.label_count(label)
    }

    /// Equality hits for a RANGE index on `(label, property)`. `None` when
    /// that index does not exist.
    pub fn pattern_equality_rows(&self, label: &str, property: &str, value: &Value) -> Option<u64> {
        let lid = self.interner.get_id(label)?;
        let kid = self.interner.get_id(property)?;
        let ids = self.range_equality_ids(lid, kid, value)?;
        Some(ids.len() as u64)
    }

    /// `CONTAINS` candidates for a TEXT index on `(label, property)`.
    /// `None` when that index does not exist.
    pub fn pattern_contains_rows(&self, label: &str, property: &str, needle: &str) -> Option<u64> {
        let ids =
            self.lookup_text_contains(&[label.to_string()], property, needle, self.commit_ts())?;
        Some(ids.len() as u64)
    }

    /// Record a DDL-named graph catalog name in the namespace its entity
    /// kind selects (ADR-0363 decision 5): `names` are node labels when
    /// `relationship` is false, relationship types when true. This is the
    /// committed twin of the write overlay's scratch facts: an index or
    /// constraint DDL records its subject through the same catalog sets, so
    /// a name a transaction mentions stays known after COMMIT publishes it.
    /// Monotone like the data-write feeds: a later drop never removes a name.
    pub(crate) fn record_schema_names(
        &self,
        relationship: bool,
        names: impl IntoIterator<Item = String>,
    ) {
        let mut catalog = self.catalog.lock().unwrap();
        for name in names {
            catalog.record_schema_name(relationship, name);
        }
    }

    /// Whether the store catalog knows `label` — the same label set
    /// `Graph::create_node` / `Graph::add_labels` record on first sight and
    /// the restore paths (checkpoint rows, WAL redo) record on reload. The
    /// catalog never removes a label, so a miss proves the label has never
    /// existed in this store (no false positive for the plan-time warning);
    /// a hit can outlive the last node carrying it.
    pub(crate) fn has_label(&self, label: &str) -> bool {
        self.catalog.lock().unwrap().labels.contains(label)
    }

    /// Whether the store catalog knows relationship type `rel_type` — the
    /// one rel-type set `Graph::create_rel` / the restore paths record.
    /// The same monotone contract as [`Graph::has_label`]: a miss proves
    /// the type has never existed, a hit may outlive the last relationship.
    pub(crate) fn has_rel_type(&self, rel_type: &str) -> bool {
        self.catalog.lock().unwrap().rel_types.contains(rel_type)
    }

    /// Whether property key `key` has ever been interned by this store —
    /// a **superset** of the keys `db.propertyKeys` reports.
    ///
    /// Every stored property key is interned: `CompactProps::from_parts`
    /// interns each key a CREATE/SET/MERGE writes, and the checkpoint
    /// restore and WAL redo paths build their properties through the same
    /// interner. A key the store has never held is therefore absent, and a
    /// miss is a true miss. The converse is not true — labels, relationship
    /// types and the internal relationship `ts` key share the table, and a
    /// key removed from every entity stays interned — so a hit can only
    /// suppress a warning, never invent one. That one-sided guarantee is
    /// what plan-time unknown-key diagnostics need: never warn about a key
    /// the store holds, at the cost of a false negative for a stale name.
    pub(crate) fn has_property_key(&self, key: &str) -> bool {
        self.interner.get_id(key).is_some()
    }
}

#[cfg(test)]
mod tests;
