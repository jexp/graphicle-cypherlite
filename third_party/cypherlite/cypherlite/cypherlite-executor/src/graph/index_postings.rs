//! The lazy equality/string postings engine (ADR-0063, ADR-0185): the pair
//! build-and-drain, the per-value postings, and the dirty-mark read seam.
//! A sibling of `index.rs` (the `degree_stats` family precedent), so the
//! parent keeps the index state, the apply seams, and the rebuild
//! orchestration.

use super::index::PropPairIndex;
use super::*;

impl Graph {
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
    /// it does not require a range index on the pair: a node constraint's
    /// backing index materializes seek postings (LABS-328), but relationship
    /// constraints have no backing-index postings at all, so this lazy pair
    /// is the one probe both entity kinds share. Empty when the label or key
    /// was never interned or `value` contains a NaN. Callers re-verify every
    /// id.
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
