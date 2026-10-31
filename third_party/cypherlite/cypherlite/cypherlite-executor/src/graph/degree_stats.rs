//! LABS-351 / LABS-370: the graph's degree statistics — the (label,
//! rel-type) out-degree histograms, the exact (start-label, type,
//! end-label) pair counts, and the per-type live rel row counts — one
//! cohesive family in its own file
//! (ADR-0387 Decision 1: the count family lives in one degree_stats
//! module). The `Graph` struct (graph/mod.rs) keeps the maps and
//! the mutation seams; the maps' own bump / rebucket / unbucket /
//! read paths live here as an `impl Graph` block (the repo's
//! methods-in-their-own-file precedent — the borrowing stays on
//! `&self`, which owns the adjacency, the rel store probe and the
//! label reads the family needs). Two siblings carry the family's
//! walk-shaped halves: the label-mutation incident stream
//! (`degree_stats_incident.rs`, LABS-515 — the incident record, its
//! enumeration, and the apply walk that bumps the pair counts over
//! it) and the admission-path batch simulator that prices a
//! not-yet-applied batch's entry growth (`degree_stats_estimate.rs`,
//! LABS-425 — the family outgrew one file at the 1000-line ceiling's
//! pace, and split again the same way).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use cypherlite_core::NodeId;

use super::Graph;

/// One count-map mutation's direction: every caller
/// mutates one entry at a time, so the delta is ±1 by construction — the
/// open-ended `i64` delta let an out-of-contract arm hide (a negative
/// delta on a vacant entry returned 0 as a no-op); the enum makes the
/// direction the only contract, and each arm is total on its own side.
#[derive(Clone, Copy)]
pub(super) enum CountDelta {
    Add,
    Remove,
}

/// The count probe the counting operator answers from : the exact per-label live count, or the exact
/// (start-label, type, end-label) pair count — read under the stats
/// locks by [`Graph::windowed_stats_count`].
pub(crate) enum CountProbe<'a> {
    Label(&'a str),
    Pair(&'a str, &'a str, &'a str),
}

impl Graph {
    /// The histogram's bucket count (LABS-311: one constant, consulted at
    /// every site — the field, the bumps, the rebuild fold, the tail
    /// scan). 21 buckets cover degrees to 2^20 (one million), the
    /// realistic supernode ceiling.
    pub(super) const DEGREE_BUCKETS: usize = 21;
    /// One histogram entry's charge: the (u32, u32) key, the bucket
    /// counts, and the map's per-entry overhead (the same floor-estimate
    /// style as the adjacency terms). Charged to `used_bytes` on every
    /// entry creation so the incremental capacity accounting tracks the
    /// recount oracle (security review: an uncharged query-controlled map
    /// bypasses the memory-limit enforcement).
    pub(super) const HISTOGRAM_ENTRY_BYTES: u64 = (std::mem::size_of::<(u32, u32)>()
        + Self::DEGREE_BUCKETS * std::mem::size_of::<u64>()
        + 48
        + 64) as u64;
    /// One pair-count entry's charge (the LABS-351 discipline, same
    /// floor-estimate style).
    pub(super) const PAIR_COUNT_ENTRY_BYTES: u64 =
        (std::mem::size_of::<(u32, u32, u32)>() + std::mem::size_of::<u64>() + 48 + 64) as u64;
    /// One exact label-count entry's charge (the LABS-351 discipline, same
    /// floor-estimate style): charged to `used_bytes` on entry creation,
    /// refunded when a delete or label removal drains the count to zero
    /// and removes the entry.
    pub(super) const LABEL_COUNT_ENTRY_BYTES: u64 =
        (std::mem::size_of::<u32>() + std::mem::size_of::<u64>() + 48 + 64) as u64;

    /// floor(log2(degree)) for degree >= 1: bucket 0 counts degree-1
    /// nodes, bucket k counts 2^k..2^(k+1)-1. Clamped into the last
    /// bucket above 2^20.
    pub(super) fn degree_bucket(degree: u64) -> usize {
        (degree.max(1).ilog2() as usize).min(Self::DEGREE_BUCKETS - 1)
    }

    /// The degree-statistics maps' share of the retained-structure
    /// recount (the `structure_bytes` oracle): the histogram entries are
    /// retained structure — an uncharged query-controlled map would
    /// bypass the memory-limit enforcement (the Capacity check compares
    /// `used_bytes`) — and the pair-count map is retained structure too:
    /// the recount oracle must include it or the incremental charge
    /// (bump's entry fee) drifts from the recount (the #398 lesson).
    pub(super) fn degree_stats_structure_bytes(&self) -> u64 {
        let histograms = self.degree_histograms.lock().unwrap();
        // One constant, consulted at every site (LABS-311): the recount
        // charges the same HISTOGRAM_ENTRY_BYTES the incremental bumps do.
        let mut n = (histograms.len() as u64).saturating_mul(Self::HISTOGRAM_ENTRY_BYTES);
        drop(histograms);
        let pairs = self.pair_counts.lock().unwrap();
        n = n.saturating_add((pairs.len() as u64).saturating_mul(Self::PAIR_COUNT_ENTRY_BYTES));
        drop(pairs);
        let labels = self.label_counts.lock().unwrap();
        n.saturating_add((labels.len() as u64).saturating_mul(Self::LABEL_COUNT_ENTRY_BYTES))
    }

    /// The committed label-count map's keys — the admission estimate's
    /// charged-tier seed (the family owns every lock-and-read site for
    /// its maps, the LABS-515 reader: the estimate file
    /// took the locks directly before). Each reader holds its stats
    /// lock only for its own read — serial holds, matching
    /// `degree_stats_structure_bytes`' shape.
    pub(super) fn label_count_keys(&self) -> HashSet<u32> {
        self.label_counts.lock().unwrap().keys().copied().collect()
    }

    /// The committed pair-count map's keys — the estimate's seed.
    pub(super) fn pair_count_keys(&self) -> HashSet<(u32, u32, u32)> {
        self.pair_counts.lock().unwrap().keys().copied().collect()
    }

    /// The committed degree-histogram map's keys — the estimate's
    /// seed.
    pub(super) fn degree_histogram_keys(&self) -> HashSet<(u32, u32)> {
        self.degree_histograms
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect()
    }

    /// Move the from-node up the degree buckets for every label it
    /// carries: called after a create's adjacency insert, with `before`
    /// the pre-insert out-degree on the rel type (LABS-351). The
    /// high-water semantics mean a move is the only outcome (never
    /// down); the create path guarantees `after = before + 1`, so the
    /// bucket moves at most one step.
    pub(super) fn bump_degree_histograms(&self, node: NodeId, type_id: u32, before: u64) {
        // One loop, both cases (stellar std-3): `old` is None for a node's
        // first edge (nothing to decrement — a degree-0 before would
        // collide with the degree-1 bucket), Some(bucket) for a move; the
        // same-bucket move changes nothing.
        let after = self.out_chunk_len(node, type_id);
        let new = Self::degree_bucket(after);
        let old = (before > 0).then(|| Self::degree_bucket(before));
        if old.is_some_and(|old| old == new) {
            return;
        }
        let labels = self.live_label_ids(node);
        let mut histograms = self.degree_histograms.lock().unwrap();
        for label in labels {
            let entry = self.histogram_entry(&mut histograms, label, type_id);
            if let Some(old) = old {
                entry[old] = entry[old].saturating_sub(1);
            }
            entry[new] = entry[new].saturating_add(1);
        }
    }

    /// The get-or-create of one histogram entry with its `used_bytes`
    /// entry charge — the one charge policy both histogram bumps share
    /// (the memory-limit charge discipline lives once, not copied
    /// verbatim per bump site). The wholesale rebuild is the one
    /// exception and stays inline in [`Graph::rebuild_degree_stats`]:
    /// its entry charges are recomputed wholesale by `recompute_used`
    /// at the caller's tail, so charging there would double-count.
    fn histogram_entry<'m>(
        &self,
        histograms: &'m mut HashMap<(u32, u32), [u64; Self::DEGREE_BUCKETS]>,
        label: u32,
        type_id: u32,
    ) -> &'m mut [u64; Self::DEGREE_BUCKETS] {
        match histograms.entry((label, type_id)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                self.used_bytes
                    .fetch_add(Self::HISTOGRAM_ENTRY_BYTES, Ordering::Relaxed);
                v.insert([0; Self::DEGREE_BUCKETS])
            }
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        }
    }

    /// The node's out-degree buckets enter the histograms of every
    /// newly added label (each per-type out-degree, bucketed, once per
    /// label). Bounded by the node's typed adjacency chunks.
    pub(super) fn rebucket_added_labels(&self, node: NodeId, added_ids: &[u32]) {
        if added_ids.is_empty() {
            return;
        }
        let mut histograms = self.degree_histograms.lock().unwrap();
        for (type_id, degree) in self.out_chunks(node) {
            let bucket = Self::degree_bucket(degree);
            for label in added_ids {
                let entry = self.histogram_entry(&mut histograms, *label, type_id);
                entry[bucket] = entry[bucket].saturating_add(1);
            }
        }
    }

    /// The node's out-degree buckets leave the histograms of every
    /// actually-removed label (saturating: a high-water degree can exceed
    /// the current live count). The paired helper of
    /// [`Graph::rebucket_added_labels`]: one input domain (&[u32] ids),
    /// one guard, one direction each.
    pub(super) fn unbucket_removed_labels(&self, node: NodeId, removed_ids: &[u32]) {
        if removed_ids.is_empty() {
            return;
        }
        let mut histograms = self.degree_histograms.lock().unwrap();
        for (type_id, degree) in self.out_chunks(node) {
            let bucket = Self::degree_bucket(degree);
            for label in removed_ids {
                if let Some(entry) = histograms.get_mut(&(*label, type_id)) {
                    entry[bucket] = entry[bucket].saturating_sub(1);
                }
            }
        }
    }

    /// LABS-351: the tail of a (label, rel-type) out-degree distribution —
    /// the conservative floor of the highest observed degree
    /// (2^k for the highest non-empty bucket k), high-water across
    /// deletes. `None` when nothing of that pair was ever written. This
    /// is the estimate the mean degree cannot express: one 50k-degree
    /// supernode in a 55-average label.
    pub fn pattern_degree_tail(&self, label: &str, rel_type: &str) -> Option<u64> {
        let label_id = self.interner.get_id(label)?;
        let type_id = self.interner.get_id(rel_type)?;
        let histograms = self.degree_histograms.lock().unwrap();
        let buckets = histograms.get(&(label_id, type_id))?;
        let highest = buckets.iter().rposition(|&c| c > 0)?;
        Some(1u64 << highest)
    }

    /// LABS-370: count one rel under every (start-label, type,
    /// end-label) pair its endpoints carry. Called from the create path
    /// beside the degree bump — the second live-label read (the end's)
    /// is the maintenance price this map pays per rel, measured against
    /// the load lanes before this lands.
    pub(super) fn bump_pair_counts(
        &self,
        start: NodeId,
        type_id: u32,
        end: NodeId,
        delta: CountDelta,
    ) {
        let start_labels = self.live_label_ids(start);
        let end_labels = self.live_label_ids(end);
        let charge = {
            let mut pairs = self.pair_counts.lock().unwrap();
            Self::charge_pair_cross(&mut pairs, &start_labels, type_id, &end_labels, delta)
        };
        self.charge(charge);
    }

    /// One rel-type row-count mutation, the rel-type map's bump seam —
    /// the create and delete paths call it with ±1 (the same
    /// create/delete seams the pair counts ride), through the family's
    /// one entry policy. This map rides no entry charge
    /// (`degree_stats_structure_bytes` does not count it — it is
    /// bounded by the type vocabulary, not the data), so the fee is 0
    /// and a drained entry's removal is unobservable: the reader
    /// treats a missing type as 0.
    pub(super) fn bump_rel_type(&self, type_id: u32, delta: CountDelta) {
        let mut counts = self.rel_type_counts.lock().unwrap();
        Self::count_entry(&mut counts, type_id, delta, 0);
    }

    /// The endpoint cross-product charge loop the pair-count family
    /// shares: one start-set × end-set walk over the
    /// pair map, `count_entry` per triple, the charge batched for the
    /// caller's [`Graph::charge`] — the create path's whole-endpoint
    /// cross product and the label mutation's per-incident-rel cross
    /// product are the same loop with different sets, hand-rolled twice
    /// before.
    pub(super) fn charge_pair_cross(
        pairs: &mut HashMap<(u32, u32, u32), u64>,
        start_set: &[u32],
        type_id: u32,
        end_set: &[u32],
        delta: CountDelta,
    ) -> i64 {
        let mut charge = 0i64;
        for s in start_set {
            for e in end_set {
                charge += Self::count_entry(
                    pairs,
                    (*s, type_id, *e),
                    delta,
                    Self::PAIR_COUNT_ENTRY_BYTES,
                );
            }
        }
        charge
    }

    /// One count-map entry mutation with its entry-creation charge — the
    /// one policy every exact count map rides (the pair counts, the label
    /// counts): the delta is +1/-1 (saturating in its direction); the
    /// return is the charge to apply (`entry_bytes` on creation, the
    /// refund when a negative delta drains an entry to zero and removes
    /// it, 0 otherwise) — the caller batches the `used_bytes` application
    /// outside the lock through [`Graph::charge`] so no
    /// caller can forget the refund (a label-churn delete that drains
    /// an entry must give its creation bytes back or `used_bytes`
    /// drifts upward while the recount stays true).
    fn count_entry<K>(
        counts: &mut HashMap<K, u64>,
        key: K,
        delta: CountDelta,
        entry_bytes: u64,
    ) -> i64
    where
        K: std::hash::Hash + Eq,
    {
        match delta {
            CountDelta::Add => match counts.entry(key) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(1);
                    entry_bytes as i64
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    *o.get_mut() = o.get().saturating_add(1);
                    0
                }
            },
            // Every Remove is paired with the Add that created its entry
            // (the create/delete and label-mutation seams pair 1:1; the
            // wholesale rebuild folds live rels only), so a vacant key on
            // the Remove side is unrepresentable in the paired contract —
            // the `None` arm exists only for map totality, never taken.
            CountDelta::Remove => match counts.get_mut(&key) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    0
                }
                Some(_) => {
                    // A drained entry leaves the map — label churn must
                    // not leak zero-count entries — and refunds its
                    // creation charge (the accounting oracle tracks both
                    // directions).
                    counts.remove(&key);
                    -(entry_bytes as i64)
                }
                None => 0,
            },
        }
    }

    /// LABS-370 (v1 follow-up): bump the exact per-label live node counts —
    /// the same seams the pair counts ride (create, delete, label add and
    /// remove, and the rebuild's wholesale fold), the same
    /// charge-on-create / refund-on-drain discipline through
    /// [`Graph::count_entry`].
    pub(super) fn bump_label_counts(&self, labels: &[u32], delta: CountDelta) {
        if labels.is_empty() {
            return;
        }
        let mut charge = 0i64;
        {
            let mut counts = self.label_counts.lock().unwrap();
            for &label in labels {
                charge +=
                    Self::count_entry(&mut counts, label, delta, Self::LABEL_COUNT_ENTRY_BYTES);
            }
        }
        self.charge(charge);
    }

    /// LABS-370: the exact count of rels between a (start-label,
    /// rel-type, end-label) triple — the counting operator's O(1)
    /// answer. `None` when nothing of that triple was ever written.
    pub fn pattern_pair_rows(
        &self,
        start_label: &str,
        rel_type: &str,
        end_label: &str,
    ) -> Option<u64> {
        let s = self.interner.get_id(start_label)?;
        let t = self.interner.get_id(rel_type)?;
        let e = self.interner.get_id(end_label)?;
        self.pair_counts.lock().unwrap().get(&(s, t, e)).copied()
    }

    /// LABS-370: the exact count of live rels between a (start-label,
    /// rel-type, end-label) triple — the counting operator's answer.
    /// Exactness makes `None ≡ 0`: a drained entry leaves the map
    /// (`count_entry` removes it on zero), so a never-written triple is a
    /// hard 0, not an unknown. The None-vs-zero distinction stays inside
    /// the degree-stats family; this reader is the boundary the
    /// recognizer answers from.
    pub fn pattern_pair_count_exact(
        &self,
        start_label: &str,
        rel_type: &str,
        end_label: &str,
    ) -> u64 {
        self.pattern_pair_rows(start_label, rel_type, end_label)
            .unwrap_or(0)
    }

    /// Live relationships of `rel_type`. `0` when the type is absent.
    pub fn pattern_rel_type_rows(&self, rel_type: &str) -> u64 {
        let Some(type_id) = self.interner.get_id(rel_type) else {
            return 0;
        };
        let counts = self.rel_type_counts.lock().unwrap();
        counts.get(&type_id).copied().unwrap_or(0)
    }

    /// LABS-370 (v1 follow-up): the exact count of live nodes carrying
    /// `label`. Unlike the label-index estimate
    /// ([`Graph::pattern_label_rows`], the planner's upper bound — the
    /// index keeps deleted / REMOVE'd ids until checkpoint GC), this map
    /// is bumped at every label-set mutation seam and drained on zero,
    /// so the count stays exact across deletes and label removals.
    pub fn pattern_label_count_exact(&self, label: &str) -> u64 {
        self.interner
            .get_id(label)
            .and_then(|lid| self.label_counts.lock().unwrap().get(&lid).copied())
            .unwrap_or(0)
    }

    /// LABS-370: the count-only reader's consistency
    /// window. A plain exactness check followed by unlocked count reads
    /// is a check-then-read a concurrent `apply_redo` can interleave
    /// between — leaking out-of-snapshot writes into the count. Here
    /// the reader holds BOTH count-map locks — the label counts and the
    /// pair counts — across the exactness check and the reads: every
    /// stats mutation path takes the map's own mutex (`count_entry`
    /// under `bump_pair_counts` / `bump_label_counts` / the incident
    /// walk), so a writer cannot mutate either map while they are held,
    /// and `apply_redo_inner` opens `in_write` BEFORE its first
    /// mutation and bumps `commit_ts` AFTER its last, so the under-lock
    /// check reads a window the mutation discipline already covers. A
    /// failed check declines (`None` — the scan answers). The degree
    /// histograms need no lock here: the count-only shapes never read
    /// them.
    pub(crate) fn windowed_stats_count(
        &self,
        exact: impl FnOnce() -> bool,
        probe: CountProbe<'_>,
    ) -> Option<u64> {
        let labels = self.label_counts.lock().unwrap();
        let pairs = self.pair_counts.lock().unwrap();
        if !exact() {
            return None;
        }
        Some(self.probe_count_under(probe, &labels, &pairs))
    }

    /// The probe read under the held locks — no re-locking: the public
    /// `pattern_*_count_exact` readers lock again and must not be called
    /// from inside the window.
    fn probe_count_under(
        &self,
        probe: CountProbe<'_>,
        labels: &HashMap<u32, u64>,
        pairs: &HashMap<(u32, u32, u32), u64>,
    ) -> u64 {
        match probe {
            CountProbe::Label(label) => self
                .interner
                .get_id(label)
                .and_then(|lid| labels.get(&lid).copied())
                .unwrap_or(0),
            // None ≡ 0 by exactness: a drained entry leaves the map, so
            // a never-written (or unknown-name) pair is a hard 0.
            CountProbe::Pair(s, t, e) => {
                match (
                    self.interner.get_id(s),
                    self.interner.get_id(t),
                    self.interner.get_id(e),
                ) {
                    (Some(s), Some(t), Some(e)) => pairs.get(&(s, t, e)).copied().unwrap_or(0),
                    _ => 0,
                }
            }
        }
    }

    /// The wholesale-rebuild fold (graph/index.rs): one rel's endpoint
    /// cross product accumulated into a fresh pair map — creation, not
    /// mutation; the rebuild replaces the map and `recompute_used`
    /// re-tallies the entry charge, so no incremental fee applies here.
    pub(super) fn fold_pair_counts(
        pairs: &mut HashMap<(u32, u32, u32), u64>,
        start_labels: &[u32],
        type_id: u32,
        end_labels: &[u32],
    ) {
        for s in start_labels {
            for e in end_labels {
                *pairs.entry((*s, type_id, *e)).or_insert(0) += 1;
            }
        }
    }

    /// The degree-stats half of the wholesale rebuild — the family seam
    /// ADR-0387 Decision 1 promises (the maps' read/bump/rebucket paths
    /// live in this file, the rebuild's folds included — the
    /// label-mutation incident walk lives in the family's incident
    /// sibling, `degree_stats_incident.rs`). Called by `rebuild_adj`
    /// (graph/index.rs) with the
    /// rebuild's rel rows projected to the (start, type, end) triples
    /// these folds read — the rel id and timestamp stay in index.rs
    /// where `adj_insert` consumes them. A pure fold over its input:
    /// the (label, rel-type) histograms come from the same rows (a
    /// node's per-type out-degree is the count of its rows of that
    /// type), not from the adjacency the caller built, so no
    /// call-ordering precondition rides the seam.
    pub(super) fn rebuild_degree_stats(&self, rows: impl Iterator<Item = (NodeId, u32, NodeId)>) {
        let mut counts: HashMap<u32, u64> = HashMap::new();
        // LABS-370: fold the (start-label, type, end-label) pair counts
        // in the same pass — the labels are read once per distinct node
        // (the local cache), not once per rel, so the rebuild pays one
        // live-label read per node rather than per edge.
        let mut pairs: HashMap<(u32, u32, u32), u64> = HashMap::new();
        let mut label_cache: HashMap<NodeId, Vec<u32>> = HashMap::new();
        // LABS-351: the per-(node, type) out-degrees counted off the
        // rows — the same number the out-adjacency chunk holds once
        // `adj_insert` ran. A fold-local map rather than an out_adj
        // read: it keeps this seam a pure function of its rows (no
        // call-ordering precondition rides it), at the cost of one
        // transient per-rebuild map. Bucket counts are
        // order-independent, so the map's iteration order does not
        // matter. Recomputed so the high-water mark recalibrates to the
        // honest live state after a restore.
        let mut out_degrees: HashMap<(NodeId, u32), u64> = HashMap::new();
        // One probe shape for this fold's own invariant: the row fold
        // below caches every row endpoint's labels before any probe.
        fn cached_labels(cache: &HashMap<NodeId, Vec<u32>>, node: NodeId) -> &[u32] {
            cache
                .get(&node)
                .expect("the row fold cached every row endpoint's labels")
                .as_slice()
        }
        for (start, type_id, end) in rows {
            *counts.entry(type_id).or_insert(0) += 1;
            // One label read per distinct node, borrowed at the fold —
            // no per-rel Vec clone of the cached set (two shared borrows
            // of the same local cache, one per endpoint).
            label_cache
                .entry(start)
                .or_insert_with(|| self.live_label_ids(start));
            label_cache
                .entry(end)
                .or_insert_with(|| self.live_label_ids(end));
            Self::fold_pair_counts(
                &mut pairs,
                cached_labels(&label_cache, start),
                type_id,
                cached_labels(&label_cache, end),
            );
            *out_degrees.entry((start, type_id)).or_insert(0) += 1;
        }
        let mut histograms: HashMap<(u32, u32), [u64; Graph::DEGREE_BUCKETS]> = HashMap::new();
        for ((start, type_id), degree) in &out_degrees {
            let bucket = Self::degree_bucket(*degree);
            let labels = cached_labels(&label_cache, *start);
            for label in labels.iter() {
                // Uncharged on purpose: the caller's tail runs
                // `recompute_used`, which retallies the entry charges
                // wholesale — charging here would double-count.
                let slot = histograms
                    .entry((*label, *type_id))
                    .or_insert([0; Graph::DEGREE_BUCKETS]);
                slot[bucket] += 1;
            }
        }
        // One publish for the whole family: the three guards are held
        // across all three stores — every other site takes exactly one
        // of these locks, so no ordering cycle exists — so a reader
        // never sees one rebuild's maps mixed with the previous
        // rebuild's.
        let mut pairs_slot = self.pair_counts.lock().unwrap();
        let mut counts_slot = self.rel_type_counts.lock().unwrap();
        let mut histograms_slot = self.degree_histograms.lock().unwrap();
        *pairs_slot = pairs;
        *counts_slot = counts;
        *histograms_slot = histograms;
    }
}
