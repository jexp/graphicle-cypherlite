use super::adjacency::adj_insert;
use super::declared::intersect_acc;
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
        if live.labels.is_empty() {
            self.indexes
                .label_index
                .entry(self.indexes.empty_label_id)
                .or_default()
                .insert(id);
        }
        for &lid in &live.labels {
            self.indexes.label_index.entry(lid).or_default().insert(id);
        }
        // Equality postings are lazy (ADR-0063 contract): dirty-mark only the
        // `(label, key)` pairs already materialized by a seek. A pair nobody
        // seeks costs nothing on the write path.
        let star = self.indexes.star_label_id;
        for &lid in live.labels.iter().chain(std::iter::once(&star)) {
            for (kid, _val) in live.props.iter() {
                if let Some(mut st) = self.indexes.prop_pairs.get_mut(&(lid, kid)) {
                    if st.built {
                        st.dirty.insert(id);
                    }
                }
                self.declared_dirty(lid, kid, id);
            }
        }
    }

    /// Dirty-mark text, range, and point pairs covered by this node's labels
    /// and properties. Those pairs exist only after DDL.
    pub(super) fn string_dirty_mark_node(&self, live: &LiveNode, id: NodeId) {
        let star = self.indexes.star_label_id;
        for (kid, _) in live.props.iter() {
            for &lid in live.labels.iter().chain(std::iter::once(&star)) {
                self.declared_dirty(lid, kid, id);
            }
        }
    }

    /// `(label, key)` strings → interned pair key, honouring the `*`
    /// any-label bucket. `None` when either side was never interned.
    pub(super) fn string_pair_id(&self, label: &str, key: &str) -> Option<(u32, u32)> {
        let lid = if label == "*" {
            self.indexes.star_label_id
        } else {
            self.interner.get_id(label)?
        };
        let kid = self.interner.get_id(key)?;
        Some((lid, kid))
    }

    /// Build the pair from `candidates` on first use, drain the ids written
    /// since it was last current, and hand it to `read` while its write
    /// guard is held (ADR-0063, ADR-0185). The one committed-side pair
    /// engine for nodes and relationships: the entity seam is the two
    /// closures — `candidates` lists the pair's ids (the label index, or
    /// the whole `rels` map) and `values` reads one entity's versions (a
    /// label match plus property read, or a type match plus payload read).
    ///
    /// Lock order: the pair's write guard is held across the build and the
    /// drain, which take `label_index`, `nodes` or `rels` read guards.
    /// `index_add`/`rel_index_add` take the same guard in the reverse
    /// order: they dirty-mark the pair after the installed version's
    /// `nodes`/`rels` guard is released. Holding the pair guard is what
    /// keeps a concurrent commit's entity out of the gap between the
    /// build's candidate copy and `built = true`: the write side waits,
    /// then finds the pair built and dirty-marks it. The inversion cannot
    /// deadlock while both sides take `nodes`/`rels` as readers: dashmap
    /// 6's shard lock blocks a reader only while a writer holds the shard,
    /// not while one waits, and no `nodes`/`rels` writer waits for a pair
    /// guard while it holds its shard. The `cypherlite` tests
    /// `uniqueness_probes_in_a_transaction_complete_beside_concurrent_commits`
    /// and its relationship twin drive both sides at once.
    pub(super) fn with_pair<R>(
        &self,
        pairs: &CoreMap<(u32, u32), PropPairIndex>,
        pair: (u32, u32),
        candidates: impl Fn(&Graph) -> Vec<i64>,
        values: impl Fn(&Graph, i64, &mut dyn FnMut(&Value)),
        read: impl FnOnce(&PropPairIndex) -> R,
    ) -> R {
        let mut st = pairs.entry(pair).or_default();
        if !st.built {
            for id in candidates(self) {
                values(self, id, &mut |val| prop_pair_post(&mut st, id, val));
            }
            st.built = true;
        }
        if !st.dirty.is_empty() {
            let ids = std::mem::take(&mut st.dirty);
            for id in ids {
                values(self, id, &mut |val| prop_pair_post(&mut st, id, val));
            }
        }
        read(&st)
    }

    /// Prune `id` from the built pairs of `pairs` (ADR-0063, ADR-0185): for
    /// every (class, key, value) triple a dead version carried that no
    /// remaining version carries, remove `id` from that pair's bucket. Lazy
    /// postings: only built pairs; unbuilt pairs have no stale ids to
    /// remove. The one prune for nodes and relationships; each side's
    /// `triples` closure enumerates its versions' (class, key, value key)
    /// triples.
    pub(super) fn drop_unused_pairs<V>(
        &self,
        pairs: &CoreMap<(u32, u32), PropPairIndex>,
        id: i64,
        dead: &[V],
        remaining: &[V],
        triples: impl Fn(&V, &mut dyn FnMut(u32, u32, GroupKey)),
    ) {
        let mut keep = HashSet::new();
        for v in remaining {
            triples(v, &mut |class, kid, gk| {
                keep.insert((class, kid, gk));
            });
        }
        let mut drop_set = HashSet::new();
        for v in dead {
            triples(v, &mut |class, kid, gk| {
                drop_set.insert((class, kid, gk));
            });
        }
        for (class, kid, gk) in drop_set.difference(&keep) {
            if let Some(mut st) = pairs.get_mut(&(*class, *kid)) {
                if st.built {
                    if let Some(set) = st.postings.get_mut(gk) {
                        set.remove(&id);
                    }
                }
            }
        }
    }

    /// Build `(label_id, key_id)` from the label index on first use, drain
    /// the ids written since the pair was last current, and hand the pair to
    /// `read` while its write guard is held (ADR-0063) — the node adapter of
    /// [`Self::with_pair`], whose lock order applies here. `prop_postings`
    /// and `equality_postings_len` both read through this, so a bucket's
    /// contents and its size cannot drift.
    fn with_prop_pair<R>(&self, lid: u32, kid: u32, read: impl FnOnce(&PropPairIndex) -> R) -> R {
        self.with_pair(
            &self.indexes.prop_pairs,
            (lid, kid),
            |g: &Graph| g.pair_candidate_ids(lid),
            |g: &Graph, id: i64, f: &mut dyn FnMut(&Value)| {
                g.for_each_node_pair_value(id, lid, kid, f)
            },
            read,
        )
    }

    /// Postings for `(label_id, key_id)` at value `gk`, built and drained
    /// through [`Self::with_prop_pair`]. A superset (ADR-0063: every version
    /// of every candidate), so callers re-verify every id; empty when no
    /// indexed version carries `gk`.
    pub(super) fn prop_postings(&self, lid: u32, kid: u32, gk: GroupKey) -> HashSet<NodeId> {
        self.with_prop_pair(lid, kid, |st| {
            st.postings.get(&gk).cloned().unwrap_or_default()
        })
    }

    /// The guard preamble `equality_postings` and `equality_postings_len`
    /// share: the interned `(label, key)` pair plus `value`'s
    /// property-index key. `None` when the label or key was never interned
    /// or `value` contains a NaN.
    fn equality_pair_key(
        &self,
        label: &str,
        key: &str,
        value: &Value,
    ) -> Option<(u32, u32, GroupKey)> {
        let gk = GroupKey::for_property_index(value)?;
        let (lid, kid) = self.string_pair_id(label, key)?;
        Some((lid, kid, gk))
    }

    /// Equality postings for `(label, key) = value` from the lazily built
    /// pair (ADR-0063 superset: every version of every candidate), for the
    /// constraint uniqueness probe (ADR-0172 §3). Unlike `lookup_nodes_at`
    /// it does not require a range index on the pair: a constraint's backing
    /// index materializes no postings. Empty when the label or key was never
    /// interned or `value` contains a NaN. Callers re-verify every id.
    pub(crate) fn equality_postings(&self, label: &str, key: &str, value: &Value) -> Vec<NodeId> {
        let Some((lid, kid, gk)) = self.equality_pair_key(label, key, value) else {
            return Vec::new();
        };
        self.prop_postings(lid, kid, gk).into_iter().collect()
    }

    /// The size of the `(label, key) = value` equality bucket from the same
    /// lazily built pair `equality_postings` reads, counted in place
    /// (ADR-0184). Empty when the label or key was never interned or
    /// `value` contains a NaN.
    pub(crate) fn equality_postings_len(&self, label: &str, key: &str, value: &Value) -> usize {
        let Some((lid, kid, gk)) = self.equality_pair_key(label, key, value) else {
            return 0;
        };
        self.with_prop_pair(lid, kid, |st| st.postings.get(&gk).map_or(0, HashSet::len))
    }

    /// Candidate ids of a `(label, key)` pair: the label index, or every
    /// node for the `*` bucket.
    fn pair_candidate_ids(&self, lid: u32) -> Vec<NodeId> {
        if lid == self.indexes.star_label_id {
            return self.nodes.iter().map(|e| *e.key()).collect();
        }
        self.indexes
            .label_index
            .get(&lid)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The value of `kid` on every version of `id` that carries `lid` (any
    /// label for the `*` bucket). Every pair build and drain walks versions
    /// here, so all declared and equality postings share one superset
    /// contract (ADR-0018): the seek re-checks the snapshot.
    pub(super) fn for_each_node_pair_value(
        &self,
        id: NodeId,
        lid: u32,
        kid: u32,
        mut f: impl FnMut(&Value),
    ) {
        let Some(vers) = self.nodes.get(&id) else {
            return;
        };
        let star = lid == self.indexes.star_label_id;
        for v in vers.iter() {
            if !star && !v.live.has_label(lid) {
                continue;
            }
            if let Some(val) = v.live.props.get(kid) {
                f(val);
            }
        }
    }

    /// [`Self::for_each_node_pair_value`] over every candidate of the pair.
    pub(super) fn for_each_pair_value(
        &self,
        lid: u32,
        kid: u32,
        mut f: impl FnMut(NodeId, &Value),
    ) {
        for id in self.pair_candidate_ids(lid) {
            self.for_each_node_pair_value(id, lid, kid, |val| f(id, val));
        }
    }

    pub(super) fn node_matches(
        n: &Node,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> bool {
        // Property comparison is Cypher equality (`1 = 1.0`), matching the
        // normalized index key that produced the candidate.
        labels.iter().all(|l| n.labels.iter().any(|x| x == l))
            && props
                .iter()
                .all(|(k, v)| n.properties.get(k).is_some_and(|have| have.eq_non_null(v)))
    }
}

fn prop_pair_post(st: &mut PropPairIndex, id: NodeId, val: &Value) {
    if let Some(gk) = GroupKey::for_property_index(val) {
        st.postings.entry(gk).or_default().insert(id);
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

enum NodeCandidates {
    /// No labels and no properties: the caller scans.
    Unindexed,
    /// Index postings that still need `node_matches`.
    Indexed(HashSet<NodeId>),
    /// A finished id list. Empty means the predicate cannot match.
    Final(Vec<NodeId>),
}

impl Graph {
    /// Approximate number of nodes carrying `label`: the label-index set
    /// size (ADR-0218). Plan-time estimate only — it is O(1), ignores
    /// snapshot visibility, and counts every id the index holds; a label
    /// the store has never interned is 0.
    pub fn label_count(&self, label: &str) -> u64 {
        self.interner
            .get_id(label)
            .and_then(|lid| self.indexes.label_index.get(&lid))
            .map_or(0, |ids| ids.len() as u64)
    }

    pub fn lookup_nodes(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Option<Vec<NodeId>> {
        self.lookup_nodes_at(labels, props, self.read_ts())
    }

    pub fn lookup_nodes_at(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        match self.node_candidates(labels, props, ts) {
            NodeCandidates::Unindexed => None,
            NodeCandidates::Final(ids) => Some(ids),
            NodeCandidates::Indexed(acc) => {
                let mut ids: Vec<_> = acc
                    .into_iter()
                    .filter(|id| {
                        self.node_at(*id, ts)
                            .is_some_and(|n| Self::node_matches(&n, labels, props))
                    })
                    .collect();
                ids.sort();
                Some(ids)
            }
        }
    }

    /// The ids posted under every label in `labels`: the label-index
    /// buckets intersected through `intersect_acc`, the fold every
    /// multi-set seek uses. A label the store never interned can match
    /// nothing, so its empty bucket empties the result. Callers route a
    /// label-less pattern to the full scan — this is only reached with a
    /// non-empty `labels`.
    fn intersect_label_ids(&self, labels: &[String]) -> HashSet<NodeId> {
        let mut acc: Option<HashSet<NodeId>> = None;
        for label in labels {
            // An uninterned label can match nothing.
            let ids = self
                .interner
                .get_id(label)
                .and_then(|lid| self.indexes.label_index.get(&lid).map(|r| r.clone()))
                .unwrap_or_default();
            intersect_acc(&mut acc, ids);
        }
        acc.unwrap_or_default()
    }

    /// Index postings for a node lookup, or a finished answer when the
    /// index cannot serve the predicate.
    fn node_candidates(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> NodeCandidates {
        if labels.is_empty() && props.is_empty() {
            return NodeCandidates::Unindexed;
        }
        if !labels.is_empty() && props.is_empty() {
            return NodeCandidates::Indexed(self.intersect_label_ids(labels));
        }
        if props.is_empty() || !self.range_serves_equality(labels, props) {
            return NodeCandidates::Final(self.scan_match(labels, props, ts));
        }
        let mut acc: Option<HashSet<NodeId>> = None;
        for (key, value) in props {
            if GroupKey::for_property_index(value).is_none() {
                return NodeCandidates::Final(Vec::new());
            }
            let Some(kid) = self.interner.get_id(key) else {
                return NodeCandidates::Final(Vec::new());
            };
            let mut next: Option<HashSet<NodeId>> = None;
            for label in labels {
                let Some(lid) = self.interner.get_id(label) else {
                    continue;
                };
                if !self.indexes.range_pairs.contains_key(&(lid, kid)) {
                    continue;
                }
                let Some(ids) = self.range_equality_ids(lid, kid, value) else {
                    return NodeCandidates::Final(self.scan_match(labels, props, ts));
                };
                intersect_acc(&mut next, ids);
            }
            intersect_acc(&mut acc, next.unwrap_or_default());
        }
        NodeCandidates::Indexed(acc.unwrap_or_default())
    }

    /// Label scan (or a full scan when the pattern has no label) filtered by
    /// equality. Used when no `RANGE` index serves the properties.
    fn scan_match(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = if labels.is_empty() {
            self.all_node_ids_at(ts)
        } else {
            self.intersect_label_ids(labels).into_iter().collect()
        };
        ids.retain(|id| {
            self.node_at(*id, ts)
                .is_some_and(|n| Self::node_matches(&n, labels, props))
        });
        ids.sort();
        ids
    }

    pub fn lookup_string_contains_at(
        &self,
        labels: &[String],
        key: &str,
        needle: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_text_contains(labels, key, needle, ts)
    }

    /// `None` unless a `RANGE` index names the `(label, property)`.
    /// `STARTS WITH` reads that ordered map; without one the caller scans.
    pub fn lookup_string_prefix_at(
        &self,
        labels: &[String],
        key: &str,
        prefix: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_text_prefix(labels, key, prefix, ts)
    }

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
                let live = &e.iter().rev().find(|v| v.xmax == 0)?.live;
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
        // LABS-370 (v1 follow-up): the exact label counts rebuild wholesale
        // with the other node-derived indexes — abort and restore recompute
        // them from the live set (the abort's install-time bumps die with
        // the reverted versions), and `recompute_used` re-tallies the
        // entry charge below.
        let mut label_counts: HashMap<u32, u64> = HashMap::new();
        for id in self.all_node_ids_at(ts) {
            if let Some(n) = self.node_at(id, ts) {
                if n.labels.is_empty() {
                    self.indexes
                        .label_index
                        .entry(self.indexes.empty_label_id)
                        .or_default()
                        .insert(id);
                }
                let label_ids: Vec<u32> =
                    n.labels.iter().map(|l| self.interner.intern(l)).collect();
                for lid in label_ids {
                    self.indexes.label_index.entry(lid).or_default().insert(id);
                    *label_counts.entry(lid).or_insert(0) += 1;
                }
            }
        }
        *self.label_counts.lock().unwrap() = label_counts;
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
