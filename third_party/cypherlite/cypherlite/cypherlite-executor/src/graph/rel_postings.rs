//! Lazy relationship equality postings (ADR-0185), read through the shared
//! committed-side pair engine (`Graph::with_pair` in `graph/index.rs`,
//! ADR-0063): this module holds only the relationship seam — the `(type,
//! key)` pair key, the candidate ids and the per-version value walk.
//!
//! A `(relationship type, key)` pair is built from `rels` on its first probe
//! and kept current by dirty marking. Postings are a superset (ADR-0063:
//! every version of every candidate), so callers re-verify every id; a pair
//! nobody probes costs nothing on the write path.

use super::index::PropPairIndex;
use super::*;

impl Graph {
    /// Equality postings for `(rel_type, key) = value` from the lazily built
    /// pair (ADR-0185; ADR-0063 superset: every version of every candidate),
    /// for the relationship uniqueness probe. Empty when `value` contains a
    /// NaN or the type or key was never interned. Callers re-verify every id.
    pub(crate) fn rel_equality_postings(
        &self,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> Vec<RelId> {
        let Some((type_id, kid, gk)) = self.rel_equality_pair_key(rel_type, key, value) else {
            return Vec::new();
        };
        self.rel_prop_postings(type_id, kid, key, gk)
            .into_iter()
            .collect()
    }

    /// The committed bucket's length for `(rel_type, key) = value`, without
    /// materializing the ids: the relationship twin of a node pair's postings
    /// set, for choosing the smallest committed-plus-scratch bucket
    /// (ADR-0184, ADR-0185). Zero when `value` contains a NaN or the type or
    /// key was never interned.
    pub(crate) fn rel_equality_postings_len(
        &self,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> usize {
        let Some((type_id, kid, gk)) = self.rel_equality_pair_key(rel_type, key, value) else {
            return 0;
        };
        self.with_rel_prop_pair(type_id, kid, key, |st| {
            st.postings.get(&gk).map_or(0, HashSet::len)
        })
    }

    /// The guard preamble `rel_equality_postings` and
    /// `rel_equality_postings_len` share: the interned `(type, key)` pair
    /// plus `value`'s property-index key. `None` when the type or key was
    /// never interned or `value` contains a NaN.
    fn rel_equality_pair_key(
        &self,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> Option<(u32, u32, GroupKey)> {
        let gk = GroupKey::for_property_index(value)?;
        let (type_id, kid) = self.rel_pair_id(rel_type, key)?;
        Some((type_id, kid, gk))
    }

    /// `(rel_type, key)` strings → interned pair key. `None` when either
    /// side was never interned.
    fn rel_pair_id(&self, rel_type: &str, key: &str) -> Option<(u32, u32)> {
        let type_id = self.interner.get_id(rel_type)?;
        let kid = self.interner.get_id(key)?;
        Some((type_id, kid))
    }

    /// Build `(type_id, key_id)` from `rels` on first use, drain the ids
    /// written since the pair was last current, and hand the pair to `read`
    /// while its write guard is held (ADR-0185) — the relationship adapter
    /// of [`Self::with_pair`], whose lock order applies here: the build
    /// collects the candidate ids first, so no `rels` guard is held across
    /// the per-id version walk, and the write side (`install_rel`,
    /// `set_property_rel`) dirty-marks only after the installed version's
    /// `rels` guard is released. `rel_prop_postings` and
    /// `rel_equality_postings_len` both read through this, so a bucket's
    /// contents and its size cannot drift.
    fn with_rel_prop_pair<R>(
        &self,
        type_id: u32,
        kid: u32,
        key: &str,
        read: impl FnOnce(&PropPairIndex) -> R,
    ) -> R {
        self.with_pair(
            &self.indexes.rel_prop_pairs,
            (type_id, kid),
            |g: &Graph| g.rels.iter().map(|e| *e.key()).collect(),
            |g: &Graph, id: i64, f: &mut dyn FnMut(&Value)| {
                g.for_each_rel_pair_value(id, type_id, key, f)
            },
            read,
        )
    }

    /// Postings for `(type_id, key_id)` at value `gk`, built and drained
    /// through [`Self::with_rel_prop_pair`]. A superset (ADR-0063: every
    /// version of every candidate), so callers re-verify every id; empty
    /// when no indexed version carries `gk`.
    pub(super) fn rel_prop_postings(
        &self,
        type_id: u32,
        kid: u32,
        key: &str,
        gk: GroupKey,
    ) -> HashSet<RelId> {
        self.with_rel_prop_pair(type_id, kid, key, |st| {
            st.postings.get(&gk).cloned().unwrap_or_default()
        })
    }

    /// The value of `key` on every version of `id` of `type_id`. Every pair
    /// build and drain walks versions here, so all relationship postings
    /// share one superset contract (ADR-0063): the probe re-checks the
    /// snapshot and the scratch.
    pub(super) fn for_each_rel_pair_value(
        &self,
        id: RelId,
        type_id: u32,
        key: &str,
        mut f: impl FnMut(&Value),
    ) {
        let Some(vers) = self.rels.get(&id) else {
            return;
        };
        for v in vers.iter() {
            if v.live.type_id != type_id {
                continue;
            }
            if let Some(val) = v.live.payload.get(&self.interner, key) {
                f(&val);
            }
        }
    }

    /// Dirty-mark built pairs for the freshly installed version of `id`
    /// (ADR-0185), mirroring [`Self::index_add`]: the version's own type and
    /// payload keys are read back from `rels`, so callers pass nothing but
    /// the id. Unbuilt pairs cost nothing (their build walks `rels`), and
    /// the caller invokes this only after the version is installed: the
    /// `rels` read is released before any pair guard is taken.
    pub(super) fn rel_index_add(&self, id: RelId) {
        let Some(vers) = self.rels.get(&id) else {
            return;
        };
        let Some(v) = vers.last() else {
            return;
        };
        let type_id = v.live.type_id;
        let mut kids = Vec::new();
        v.live
            .payload
            .for_each_key_value(&self.interner, |kid, _| kids.push(kid));
        drop(vers);
        for kid in kids {
            if let Some(mut st) = self.indexes.rel_prop_pairs.get_mut(&(type_id, kid)) {
                if st.built {
                    st.dirty.insert(id);
                }
            }
        }
    }

    /// Prune `id`'s collected versions from built pairs (ADR-0185) through
    /// the shared [`Self::drop_unused_pairs`]: for each value a dead version
    /// carried, remove `id` from that pair's postings set when no remaining
    /// version carries the same `(type, key, value)`. The caller has
    /// already dropped its `rels` guard.
    pub(super) fn rel_index_drop_unused(&self, id: RelId, dead: &[LiveRel], remaining: &[LiveRel]) {
        self.drop_unused_pairs(&self.indexes.rel_prop_pairs, id, dead, remaining, |r, f| {
            r.payload.for_each_key_value(&self.interner, |kid, val| {
                if let Some(gk) = GroupKey::for_property_index(val) {
                    f(r.type_id, kid, gk);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests;
