//! The degree-stats family's incident stream (LABS-515): one rel
//! incident record and the walks that consume it — the label-mutation
//! apply path ([`Graph::apply_incident_pair_counts`]) and the
//! admission estimate's incident pricing (degree_stats_estimate.rs)
//! consult the same stream. Carved out of degree_stats.rs at the
//! family's own split precedent (the estimate file's LABS-425
//! carve-off) before the family's maps outgrew one file.

use std::collections::HashMap;

use cypherlite_core::NodeId;

use super::degree_stats::CountDelta;
use super::Graph;

/// One rel incident to a node, in the one record shape every
/// label-mutation path sees it (LABS-515): the apply walk
/// ([`Graph::apply_incident_pair_counts`]) and the admission estimate's
/// incident pricing ([`Graph::estimate_stats_entry_growth`]) consult
/// the same stream. The record owns the mutation's orientation — the
/// mutated node's labels stand on the rel's start side when the node is
/// the rel's start ([`IncidentRel::ordered`],
/// [`IncidentRel::pair_key`]) — so every consumer consults it instead
/// of re-deriving the side.
pub(super) struct IncidentRel {
    /// The mutated node's side of the rel. Private (the LABS-515
    /// boundary closure: the producers set it through the constructors,
    /// the cross-file consumers read it through [`IncidentRel::is_out`]
    /// / [`IncidentRel::pair_key`], the in-file folds through
    /// [`IncidentRel::ordered`] — one spelling per fact, compiler-held.
    side: Side,
    /// The rel's type id.
    pub(super) type_id: u32,
    /// The far endpoint — the mutated node itself for a self-loop.
    pub(super) far: NodeId,
}

/// The mutated node's side of one incident rel: the out-adjacency side
/// (the node is the rel's start, the far endpoint its end) or the
/// in-adjacency side (the node is the end, the far endpoint the
/// start).
#[derive(Clone, Copy)]
enum Side {
    Out,
    In,
}

impl IncidentRel {
    /// The record for a rel the mutated node STARTS — the
    /// out-adjacency side; `far` is the rel's end.
    pub(super) fn outgoing(type_id: u32, far: NodeId) -> Self {
        Self {
            side: Side::Out,
            type_id,
            far,
        }
    }

    /// The record for a rel that ENDS at the mutated node — the
    /// in-adjacency side; `far` is the rel's start.
    pub(super) fn incoming(type_id: u32, far: NodeId) -> Self {
        Self {
            side: Side::In,
            type_id,
            far,
        }
    }

    /// The mutated node is this rel's start — its out side, the side
    /// that prices the histogram entries (LABS-515: the
    /// one orientation fact every consumer re-derived as
    /// `if far_is_end` before, owned by the record now).
    pub(super) fn is_out(&self) -> bool {
        matches!(self.side, Side::Out)
    }

    /// Order one mutated-node value and one far-endpoint value by the
    /// rel's own direction — the one orientation primitive every
    /// consumer consults: the out side keeps the mutated node's value
    /// on the start end, the in side puts the far value there.
    /// [`IncidentRel::pair_key`] and the apply folds are defined over
    /// it.
    fn ordered<A>(&self, mine: A, far: A) -> (A, A) {
        match self.side {
            Side::Out => (mine, far),
            Side::In => (far, mine),
        }
    }

    /// The (start, type, end) pair key for one mutated-node label
    /// `mine` against one far-endpoint label `far` — the pair keys
    /// follow the rel's own direction (the estimate's key-at-a-time
    /// shape).
    pub(super) fn pair_key(&self, mine: u32, far: u32) -> (u32, u32, u32) {
        let (s, e) = self.ordered(mine, far);
        (s, self.type_id, e)
    }

    /// The self-loop charge sets of one record (apply's pass 2): the
    /// side's own set is selected here and the pair ordered through
    /// [`IncidentRel::ordered`], so the call site consults one accessor
    /// instead of re-deriving the side beside it. The charging policy —
    /// which set rides which side, the diagonal charged once — is
    /// [`Graph::self_loop_charge_sets`]'s, stated there once.
    fn self_loop_sets<'a>(
        &self,
        mine: &'a [u32],
        own_out: &'a [u32],
        own_in: &'a [u32],
    ) -> (&'a [u32], &'a [u32]) {
        let own = match self.side {
            Side::Out => own_out,
            Side::In => own_in,
        };
        self.ordered(mine, own)
    }
}

impl Graph {
    /// The committed rels incident to `node` on both adjacency sides, as
    /// one stream (LABS-515): the enumeration the apply walk
    /// ([`Graph::apply_incident_pair_counts`]) and the admission
    /// estimate's incident pricing
    /// ([`Graph::estimate_stats_entry_growth`]) hand-rolled separately
    /// before — every record carries the mutated node's `side`, the
    /// rel's type, and the far endpoint.
    ///
    /// The rel's LIVE version decides (inside the write
    /// window a rel created earlier in the same batch is invisible at
    /// `commit_ts` (its xmin > commit_ts) and one deleted in the batch
    /// keeps a live-looking pre-delete version — probing the live
    /// version is the same fix `live_label_ids` applies), so the stream
    /// carries exactly the incidents the apply path walks. A two-sided
    /// incidence — a self-loop (far == node) or a reciprocal same-type
    /// pair — enumerates once per side: every side prices its own keys.
    ///
    /// A hop is live history unless the LIVE version's identity matches
    /// the hop on every axis — walked-side end AND rel type: a rel id
    /// deleted and re-created with different endpoints or a different
    /// rel type keeps its old hops (older snapshots still traverse
    /// them), so a walk can meet a hop whose live rel now touches other
    /// nodes or rides another type — that hop is not a live incidence
    /// of this node and is skipped, exactly like a hop whose rel has no
    /// live version left.
    pub(super) fn incident_rels(&self, node: NodeId) -> Vec<IncidentRel> {
        let mut incidents = Vec::new();
        for (adj, side) in [(&self.out_adj, Side::Out), (&self.in_adj, Side::In)] {
            let Some(adj) = adj.get(&node) else {
                continue;
            };
            for (type_id, hops) in &adj.chunks {
                for hop in hops {
                    let Some((start, end, live_type)) = self.rel_live_identity(hop.id) else {
                        continue;
                    };
                    // One dispatch on the walked side: the near end
                    // feeds the live-identity guard, the record carries
                    // the side — the out adjacency means the node
                    // starts the rel (far its end), the in adjacency
                    // means the node ends it (far its start).
                    let (near, record) = match side {
                        Side::Out => (start, IncidentRel::outgoing(*type_id, end)),
                        Side::In => (end, IncidentRel::incoming(*type_id, start)),
                    };
                    // The live-identity match the doc above states.
                    if near != node || live_type != *type_id {
                        continue;
                    }
                    incidents.push(record);
                }
            }
        }
        incidents
    }

    /// LABS-370: the canonical endpoint cross-product walk — every rel
    /// incident to `node` (both adjacency sides), cross-producted with
    /// `mutated_labels` on its own side, applied to the pair counts
    /// with `delta`. This is the one walk the label-mutation paths
    /// share: `rebucket` passes the added ids and +1, `unbucket` the
    /// actually-removed ids and -1 — no per-caller mirrors, and the
    /// charge folds uniformly through [`Graph::count_entry`] and applied
    /// via [`Graph::charge`] so a `-1` that drains an entry always
    /// refunds its creation bytes.
    ///
    /// The far endpoint's labels need a rel-store probe per rel (the
    /// adjacency hops carry no far endpoint, AdjHop is (ts, id)), so a
    /// label mutation on a supernode pays its fanout in probes: rare by
    /// nature, bounded by the fanout, correct by construction. The
    /// per-node label cache keeps one read per distinct far node. The
    /// incident rels and their far labels are collected BEFORE the
    /// pair-count mutex is taken (bump's shape): the lock never spans a
    /// rel-store probe or a label read.
    ///
    /// The self-loop rels ride their own list (LABS-425):
    /// a self-loop's far endpoint is the mutated node
    /// itself, so its far label set is not a far node's but the node's
    /// own, mutation-aware on both sides — charging it is a separate
    /// pass over the same locked section, not a 2x2 branch woven into
    /// the far-node loop.
    pub(super) fn apply_incident_pair_counts(
        &self,
        node: NodeId,
        mutated_labels: &[u32],
        delta: CountDelta,
    ) {
        if mutated_labels.is_empty() {
            return;
        }
        // The incident stream, partitioned by its own record (LABS-515:
        // the named record survives to the charge sites —
        // no positional re-split): the far-node rels and the self-loop
        // rels, whose far endpoint is the mutated node itself (so it
        // rides its own list — its far label set is the node's own,
        // mutation-aware on both sides, not a far node's). The mutated
        // node's own label set is supplied by the caller and stands on
        // its side of the cross product. The far labels stay in the
        // per-node cache — borrowed at the fold, not cloned per rel
        // (one read per distinct far node, one Vec alive per node,
        // not per hop).
        let (far_incidents, self_loops): (Vec<IncidentRel>, Vec<IncidentRel>) = self
            .incident_rels(node)
            .into_iter()
            .partition(|inc| inc.far != node);
        let mut far_labels: HashMap<NodeId, Vec<u32>> = HashMap::new();
        for inc in &far_incidents {
            far_labels
                .entry(inc.far)
                .or_insert_with(|| self.live_label_ids(inc.far));
        }
        // The self-loop charge sets, two plain once-computed sets
        // (LABS-425): the mutated node's label sets are
        // the same for every self-loop rel of this node, so they are
        // computed once, here, outside the lock — pass 2's loop charges
        // nothing when the node has no self-loop, so no vacant-sentinel
        // branch guards the computation.
        let (own_out, own_in) =
            Self::self_loop_charge_sets(&self.live_label_ids(node), mutated_labels);
        let mut charge = 0i64;
        {
            let mut pairs = self.pair_counts.lock().unwrap();
            // Pass 1 — the far-node rels: the pair the stats hold is
            // (start, type, end) in the rel's own direction — the
            // orientation [`IncidentRel::ordered`] owns. Every record's
            // far node has its cache entry (the invariant the expect
            // states).
            for inc in &far_incidents {
                let far = far_labels
                    .get(&inc.far)
                    .expect("the cache pass filled every far record's labels");
                let (start_set, end_set) = inc.ordered(mutated_labels, far.as_slice());
                charge +=
                    Self::charge_pair_cross(&mut pairs, start_set, inc.type_id, end_set, delta);
            }
            // Pass 2 — the self-loop rels, in the same locked section:
            // the mutated node's set stands on the record's near side
            // (as in pass 1) and the far side is the self-loop charge
            // set the side selects — the charging policy is
            // [`Graph::self_loop_charge_sets`]'s, stated there once.
            for inc in &self_loops {
                let (start_set, end_set) = inc.self_loop_sets(mutated_labels, &own_out, &own_in);
                charge +=
                    Self::charge_pair_cross(&mut pairs, start_set, inc.type_id, end_set, delta);
            }
        }
        self.charge(charge);
    }

    /// The self-loop charge sets (LABS-425): a self-loop
    /// rides both adjacency sides of its own node, so the correct far set
    /// for the mutated node itself is mutation-aware on both sides — the
    /// rel realizes (start-label, type, end-label) over the endpoint's
    /// labels on BOTH sides of the mutation:
    /// - the out side charges `mutated × (live ∪ mutated)` — the union is
    ///   the post-add set on the way up (live already carries the added
    ///   ids) and the pre-remove set on the way down (live has already
    ///   lost the removed ids);
    /// - the in side charges `(live ∪ mutated) \ mutated × mutated` —
    ///   the complement, so the (mutated, type, mutated) diagonal is
    ///   charged exactly once, by the out side
    ///   (the self-loop finding: the full cross on both sides
    ///   counted the diagonal twice on add, and the plain live set on
    ///   both sides never drained it on remove — the pair counts
    ///   diverged from the scan either way).
    fn self_loop_charge_sets(live: &[u32], mutated_labels: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let mut own_out = live.to_vec();
        for &m in mutated_labels {
            if !own_out.contains(&m) {
                own_out.push(m);
            }
        }
        let own_in = live
            .iter()
            .copied()
            .filter(|l| !mutated_labels.contains(l))
            .collect();
        (own_out, own_in)
    }
}
