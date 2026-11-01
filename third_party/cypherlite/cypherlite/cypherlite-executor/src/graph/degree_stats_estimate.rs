//! The admission-path estimate of the degree-statistics maps' entry
//! growth (LABS-370, ADR-0387 Decision 1): the batch simulator behind
//! [`Graph::estimate_stats_entry_growth`], in its own impl-block file —
//! the repo's methods-in-their-own-file precedent, a sibling of
//! `degree_stats.rs` (which owns the maps' mutation seams and readers).
//! This file owns the read-only simulation that prices a
//! not-yet-applied batch against the count family (the exact label
//! counts, the pair counts, the degree histograms); no mutation path
//! lives here. The simulation's accumulators are one named state —
//! [`BatchSim`], its steps the `RedoOp` arms (LABS-515: the accumulator
//! tangle the estimate threaded as function locals before) — and the
//! incident stream its AddLabels step prices is the one the apply path
//! walks ([`Graph::incident_rels`], LABS-515): apply and estimate
//! consult one enumeration form.

use std::collections::{HashMap, HashSet};

use cypherlite_core::NodeId;

use super::degree_stats_incident::IncidentRel;
use super::{FreshIds, Graph};
use crate::redo::RedoOp;

impl Graph {
    /// LABS-370 (security follow-up): the stats-map entry growth a
    /// committed batch would create — the admission estimate's term for
    /// the count family (the exact label counts, the pair counts, the
    /// degree histograms). Charged BEFORE the commit applies: an
    /// oversized-label write (one rel between two multi-label nodes
    /// materializes labels(start) × labels(end) pair entries) is
    /// rejected at admission, not after the entries land past the cap.
    ///
    /// Exact for the same batches the redo-delta oracle is exact for:
    /// the label sets are simulated through the batch (a create sets
    /// them, add/remove edits them in op order, committed nodes start
    /// from their live sets), and a fresh name interns to an id no
    /// committed map can hold, so a fresh label's entry is always a
    /// creation. The AddLabels growth simulates the batch's in-batch
    /// rels too: the apply path's incident walk sees
    /// them (the live-version probe), so the estimate must count their
    /// pair entries — an estimate that walks committed adjacency only
    /// drifts from the applied `used_bytes` on mixed
    /// [CreateRel, AddLabels] batches.
    ///
    /// A two-sided incidence — a self-loop, or a reciprocal same-type
    /// pair — realizes its pair keys on BOTH sides (the apply path's
    /// own discipline, extension of it to the estimate): the incident walk prices every
    /// side of every incident rel, and repeated keys charge nothing
    /// twice through the seen-key sets (the `seen_far` early-out
    /// the first version carried skipped the second side's keys and
    /// under-priced admission).
    pub(super) fn estimate_stats_entry_growth(&self, ops: &[RedoOp]) -> i64 {
        // The simulation's phases as steps over one state (LABS-515):
        // the op loop drives the state in op order; the state's growth
        // is the estimate.
        let mut sim = BatchSim::new(self);
        for op in ops {
            sim.step(self, op);
        }
        // The one conversion to the caller's i64 delta world (the
        // ledger counts in the entry-byte constants' own u64 domain).
        sim.charges.growth as i64
    }
}

/// The batch simulator's state (LABS-515): the accumulators the estimate
/// threaded as function locals before, as named fields. The op loop in
/// [`Graph::estimate_stats_entry_growth`] drives one state through the
/// batch, each `RedoOp` arm one step over it. Three field groups: the
/// label-set simulation (a create sets a node's labels, an add edits
/// them in op order, a committed node starts from its live set), the
/// entry-growth ledger ([`ChargeLedger`] — the keys already accounted
/// for and the total they charged, one accumulator), and the batch's
/// rels.
struct BatchSim {
    /// The batch's simulated label sets, per node — the cache-fill half
    /// of the cache-fill-and-borrow pattern: the
    /// batch's simulated set, or the live set, cached once so no
    /// per-lookup clone of the live set rides the admission path.
    node_labels: HashMap<NodeId, Vec<u32>>,
    /// Fresh-name interning: a fresh name interns to an id no committed
    /// map can hold, so a fresh label's entry is always a creation.
    fresh_ids: FreshIds,
    /// The entry-growth ledger: the keys already accounted for, one
    /// seeded set per count map, and the total they charged.
    charges: ChargeLedger,
    /// The batch's simulated rels ([`BatchRel`]) — the AddLabels
    /// growth must count them: apply walks the
    /// adjacency after the batch's CreateRel ops land (rebucket and the
    /// incident walk see them), so an estimate that walks committed
    /// adjacency only drifts from the applied `used_bytes` on mixed
    /// [CreateRel, AddLabels] batches. Deletes stay out of the
    /// simulation like every other `_` arm.
    batch_rels: Vec<BatchRel>,
}

/// One of the batch's simulated rels, its direction as field names
/// (the LABS-515 naming: the rel's direction is the
/// fact `batch_incidents` translates into incident records and the
/// pair-key orientation rests on — a field, not a slot position).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BatchRel {
    start: NodeId,
    end: NodeId,
    type_id: u32,
}

/// The entry-growth ledger (the LABS-515 grouping): the
/// keys already accounted for and the total they charged — one
/// accumulator, not four sibling fields. The sets are seeded with the
/// committed maps' own keys at construction, then every key this batch
/// charges; one `insert` decides the LABS-425 two-tier policy.
struct ChargeLedger {
    /// The keys already accounted for, one set per count map.
    labels: HashSet<u32>,
    pairs: HashSet<(u32, u32, u32)>,
    hists: HashSet<(u32, u32)>,
    /// The total the accounted-for keys charged — the estimate's
    /// answer, counted in the entry-byte constants' own domain (the
    /// ledger only ever adds, so it is structurally non-negative; the
    /// one conversion to the caller's i64 delta world happens at the
    /// public boundary).
    growth: u64,
}

impl ChargeLedger {
    /// The ledger seeded with the committed maps' keys (the
    /// entry-growth policy's committed tier, folded into the one set)
    /// through the family's own key readers — the locks live in
    /// degree_stats.rs, one reader per map, each held only for its own
    /// read.
    fn new(g: &Graph) -> Self {
        Self {
            labels: g.label_count_keys(),
            pairs: g.pair_count_keys(),
            hists: g.degree_histogram_keys(),
            growth: 0,
        }
    }

    /// One label-count entry's charge: each charge binds its seen set
    /// to its own entry price — the association the open generic left
    /// as two independent inputs.
    fn charge_label(&mut self, key: u32) {
        Self::charge_new(
            &mut self.labels,
            key,
            Graph::LABEL_COUNT_ENTRY_BYTES,
            &mut self.growth,
        );
    }

    /// One histogram entry's charge.
    fn charge_hist(&mut self, key: (u32, u32)) {
        Self::charge_new(
            &mut self.hists,
            key,
            Graph::HISTOGRAM_ENTRY_BYTES,
            &mut self.growth,
        );
    }

    /// One pair-count entry's charge.
    fn charge_pair(&mut self, key: (u32, u32, u32)) {
        Self::charge_new(
            &mut self.pairs,
            key,
            Graph::PAIR_COUNT_ENTRY_BYTES,
            &mut self.growth,
        );
    }

    /// The entry-growth policy, named once (LABS-425;
    /// the two tiers folded into one seeded set in the LABS-515 stellar
    /// round): a key is a new entry iff neither the committed map nor
    /// an earlier op in this batch holds it — the estimate-side twin of
    /// `count_entry`'s apply-side policy.
    fn charge_new<K>(seen: &mut HashSet<K>, key: K, entry_bytes: u64, growth: &mut u64)
    where
        K: std::hash::Hash + Eq,
    {
        if seen.insert(key) {
            *growth += entry_bytes;
        }
    }
}

impl BatchSim {
    /// The simulator's initial state: the ledger seeded with the
    /// committed maps' keys, everything else empty.
    fn new(g: &Graph) -> Self {
        Self {
            node_labels: HashMap::new(),
            fresh_ids: FreshIds::new(),
            charges: ChargeLedger::new(g),
            batch_rels: Vec::new(),
        }
    }

    /// One op's step over the state, in op order: the three arms the
    /// count family's growth rides; every other op leaves the state
    /// untouched (deletes stay out of the simulation).
    fn step(&mut self, g: &Graph, op: &RedoOp) {
        match op {
            RedoOp::CreateNode { id, labels, .. } => self.create_node(g, *id, labels),
            RedoOp::CreateRel {
                start,
                end,
                rel_type,
                ..
            } => self.create_rel(g, *start, *end, rel_type),
            RedoOp::AddLabels { id, labels } => self.add_labels(g, *id, labels),
            _ => {}
        }
    }

    /// CreateNode: the node's labels become its simulated set, and each
    /// one prices its exact-count entry — a fresh name interns to an id
    /// no committed map can hold, so every one of them is a creation.
    fn create_node(&mut self, g: &Graph, id: NodeId, labels: &[String]) {
        let ids: Vec<u32> = labels
            .iter()
            .map(|l| self.fresh_ids.resolve(g, l))
            .collect();
        for &lid in &ids {
            self.charges.charge_label(lid);
        }
        self.node_labels.insert(id, ids);
    }

    /// CreateRel: both endpoint label sets are ensured into the
    /// simulation, the rel's out-side histogram entry and its endpoint
    /// pair cross each price, and the rel joins the simulation for the
    /// AddLabels steps that follow.
    fn create_rel(&mut self, g: &Graph, start: NodeId, end: NodeId, rel_type: &str) {
        let t = self.fresh_ids.resolve(g, rel_type);
        Self::ensure_labels(&mut self.node_labels, g, start);
        Self::ensure_labels(&mut self.node_labels, g, end);
        // Both endpoint sets are ensured above — the index read is the
        // borrow, not a clone.
        let start_ids = &self.node_labels[&start];
        let end_ids = &self.node_labels[&end];
        for &s in start_ids {
            self.charges.charge_hist((s, t));
            for &e in end_ids {
                self.charges.charge_pair((s, t, e));
            }
        }
        self.batch_rels.push(BatchRel {
            start,
            end,
            type_id: t,
        });
    }

    /// AddLabels: every requested label charges its exact-count entry
    /// through the [`ChargeLedger`] — a carried label's key already sits
    /// in the ledger's seed set, so it prices nothing — while the
    /// set-integrity guard keeps the simulation a set. The early-out keys
    /// on the ids that actually joined, the fact
    /// [`Graph::rebucket_added_labels`] and `apply_incident_pair_counts`
    /// guard on; the pricers then price the node's full simulated set (a
    /// carried label's keys are seeded or already charged, a fresh one's
    /// charge exactly what apply materializes). The committed
    /// chunk-granular histogram term prices against the out-adjacency
    /// chunks; the incident half prices the stream (the entries
    /// [`Graph::apply_incident_pair_counts`] and
    /// [`Graph::rebucket_added_labels`] materialize at apply time,
    /// estimated against the adjacency the apply path walks).
    fn add_labels(&mut self, g: &Graph, id: NodeId, labels: &[String]) {
        let ids = Self::ensure_labels(&mut self.node_labels, g, id);
        let before = ids.len();
        for l in labels {
            let lid = self.fresh_ids.resolve(g, l);
            self.charges.charge_label(lid);
            if !ids.contains(&lid) {
                ids.push(lid);
            }
        }
        // The apply-side guard's mirror (both apply phases skip an
        // empty mutated set, and no incident prices an empty one here):
        // a no-op AddLabels — no label actually joined — ends the step
        // before the incident walk. The joined ids are the set's own
        // growth, so no second collection tracks them.
        if ids.len() == before {
            return;
        }
        // The simulated set, snapshotted once and released from the
        // borrow before the pricing calls.
        let mine = ids.clone();
        // The batch's incident enumeration, built once for both phases
        // (LABS-515): the committed live hops on
        // both adjacency sides ([`Graph::incident_rels`], the apply
        // path's own walk) plus the batch's created rels
        // ([`BatchSim::batch_incidents`]) — the histogram phase reads
        // its out-side types from the stream, the pair phase prices its
        // keys.
        let mut incidents = g.incident_rels(id);
        incidents.extend(self.batch_incidents(id));
        self.price_histogram_entries(g, id, &mine, &incidents);
        self.price_incidents(g, &mine, &incidents);
    }

    /// The AddLabels step's histogram-entry pricing, one site (the
    /// LABS-515 consolidation: the committed term and
    /// the in-batch term were split across two methods before): a
    /// label mutation materializes its out-side histogram entries at
    /// apply time — [`Graph::rebucket_added_labels`] walks every
    /// out-adjacency CHUNK (CHUNK-granular: a type whose hops are all
    /// tombstoned still prices, because apply's rebucket charges per
    /// chunk, not per live hop), and the batch's created rels
    /// contribute their out-side types the committed chunks cannot hold
    ///. One type set covers both terms.
    fn price_histogram_entries(
        &mut self,
        g: &Graph,
        id: NodeId,
        mine: &[u32],
        incidents: &[IncidentRel],
    ) {
        // The type set is a set by construction: the term is
        // CHUNK-granular (per (label, type)), so the committed out-side
        // records' duplicates absorb at collection time and the ledger
        // probes only distinct keys. The types come through the incident
        // vocabulary, not endpoint equality over the batch's rels (the
        // LABS-515 one enumeration, one side spelling).
        let mut types: HashSet<u32> = g
            .out_chunks(id)
            .into_iter()
            .map(|(type_id, _)| type_id)
            .collect();
        for inc in incidents {
            if inc.is_out() {
                types.insert(inc.type_id);
            }
        }
        for &lid in mine {
            for &t in &types {
                self.charges.charge_hist((lid, t));
            }
        }
    }

    /// The incident half of the AddLabels step (LABS-515:
    /// one step method per phase, `create_node`/`create_rel`'s
    /// granularity): the batch's incident enumeration priced in one
    /// pass — every side of every incident rel prices its own pair keys
    ///: a two-sided incidence, a self-loop (far == id) or a
    /// reciprocal same-type pair, realizes (start-label, type,
    /// end-label) on both sides — and repeated keys charge nothing
    /// twice through the ledger's sets (the previous `seen_far`
    /// early-out keyed (type, far) and skipped the second side's keys
    /// entirely, under-pricing admission).
    fn price_incidents(&mut self, g: &Graph, mine: &[u32], incidents: &[IncidentRel]) {
        // The pricing pass over the stream: the pair keys follow the
        // rel's own direction ([`IncidentRel::pair_key`]). The
        // histogram entries are the step's other phase
        // (`price_histogram_entries`), already priced over the one
        // type set.
        for inc in incidents {
            let far_ids = &*Self::ensure_labels(&mut self.node_labels, g, inc.far);
            for &lid in mine {
                for &f in far_ids {
                    self.charges.charge_pair(inc.pair_key(lid, f));
                }
            }
        }
    }

    /// The batch's created rels incident to `id`, in the incident
    /// stream's own record shape and ordering — out side first, then in
    /// side, per rel, matching [`Graph::incident_rels`]: a
    /// batch self-loop (start == end == id) is the mutated
    /// node on BOTH ends, so it enumerates twice — once per side,
    /// exactly as a committed self-loop appears on both adjacency sides
    ///.
    fn batch_incidents(&self, id: NodeId) -> Vec<IncidentRel> {
        let mut incidents = Vec::new();
        for rel in &self.batch_rels {
            if id == rel.start {
                incidents.push(IncidentRel::outgoing(rel.type_id, rel.end));
            }
            if id == rel.end {
                incidents.push(IncidentRel::incoming(rel.type_id, rel.start));
            }
        }
        incidents
    }

    /// The batch's label-set simulation's fill: cache the node's labels
    /// once (the batch's simulated set, or the live set), then borrow —
    /// no per-lookup clone of the live set on the admission path (the
    /// cache-fill-and-borrow pattern). The fill returns
    /// the entry's set so every site — the AddLabels step included —
    /// borrows through the one fill shape.
    fn ensure_labels<'m>(
        node_labels: &'m mut HashMap<NodeId, Vec<u32>>,
        g: &Graph,
        id: NodeId,
    ) -> &'m mut Vec<u32> {
        node_labels
            .entry(id)
            .or_insert_with(|| g.live_label_ids(id))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// LABS-515: the simulator's state transitions, pinned per step —
    /// each op's step moves exactly the fields it owns, and the `_` arm
    /// moves nothing. The growth figures mirror
    /// `stats_entry_growth_estimates_mixed_batches` end to end.
    #[test]
    fn batch_sim_state_follows_each_step() {
        let g = Graph::default();
        g.begin_write().unwrap();
        let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
        let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
        g.commit_write();
        let a_id = g.interner.get_id("A").unwrap();
        let b_id = g.interner.get_id("B").unwrap();

        let mut sim = BatchSim::new(&g);
        // The initial state: the ledger's sets are seeded with the
        // committed maps' keys (A, B from the committed nodes); the simulated sets
        // and the rels start empty; growth is zero.
        assert!(sim.charges.labels.contains(&a_id) && sim.charges.labels.contains(&b_id));
        assert_eq!(sim.charges.labels.len(), 2);
        assert!(sim.charges.pairs.is_empty());
        assert!(sim.charges.hists.is_empty());
        assert!(sim.node_labels.is_empty());
        assert!(sim.batch_rels.is_empty());
        assert_eq!(sim.charges.growth, 0);

        // CreateRel: the rel joins the simulation, both endpoints'
        // label sets are ensured into it, and the (A, R) histogram
        // entry plus the (A, R, B) pair entry charge.
        sim.step(
            &g,
            &RedoOp::create_rel(1, a, b, "R".to_string(), BTreeMap::new()),
        );
        // "R" is a fresh name in this batch — the simulator's memoized
        // mint is the id the state's rels carry.
        let r_id = sim.fresh_ids.resolve(&g, "R");
        assert_eq!(
            sim.batch_rels,
            vec![BatchRel {
                start: a,
                end: b,
                type_id: r_id
            }]
        );
        assert_eq!(sim.node_labels[&a], vec![a_id]);
        assert_eq!(sim.node_labels[&b], vec![b_id]);
        assert_eq!(
            sim.charges.growth,
            Graph::HISTOGRAM_ENTRY_BYTES + Graph::PAIR_COUNT_ENTRY_BYTES
        );

        // AddLabels: the actually-added id joins the node's simulated
        // set and the seen set, and the incident stream prices the
        // in-batch rel's keys — its out side carries the (A2, R)
        // histogram term the chunk walk cannot see.
        sim.step(&g, &RedoOp::add_labels(a, vec!["A2".into()]));
        // "A2" is fresh too — resolve returns the same memoized mint the
        // step used.
        let a2_id = sim.fresh_ids.resolve(&g, "A2");
        assert_eq!(sim.node_labels[&a], vec![a_id, a2_id]);
        assert!(sim.charges.labels.contains(&a2_id));
        assert_eq!(
            sim.charges.growth,
            2 * Graph::HISTOGRAM_ENTRY_BYTES
                + 2 * Graph::PAIR_COUNT_ENTRY_BYTES
                + Graph::LABEL_COUNT_ENTRY_BYTES
        );

        // A non-growth op is a no-op on the state: label removal stays
        // out of the simulation like every other `_` arm.
        let before_labels = sim.node_labels.clone();
        let before_seen = sim.charges.labels.clone();
        let before_pairs = sim.charges.pairs.clone();
        let before_hists = sim.charges.hists.clone();
        let before_rels = sim.batch_rels.clone();
        let before_growth = sim.charges.growth;
        sim.step(
            &g,
            &RedoOp::RemoveLabels {
                id: a,
                labels: vec!["A2".into()],
            },
        );
        assert_eq!(sim.node_labels, before_labels);
        assert_eq!(sim.charges.labels, before_seen);
        assert_eq!(sim.charges.pairs, before_pairs);
        assert_eq!(sim.charges.hists, before_hists);
        assert_eq!(sim.batch_rels, before_rels);
        assert_eq!(sim.charges.growth, before_growth);
    }

    /// LABS-515: the AddLabels step's carried-label discipline — a
    /// requested label the node already carries moves neither the
    /// simulated set nor the pricing (its key already sits in the
    /// ledger's committed seed, so it charges nothing; a fully-carried
    /// request trips the growth guard and ends the step before any
    /// incident pricing).
    #[test]
    fn batch_sim_add_labels_skips_already_carried() {
        let g = Graph::default();
        g.begin_write().unwrap();
        let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
        let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
        // A committed rel incident to `a`, so the incident walk has a
        // far endpoint it would touch — the growth guard's skip stays
        // observable. Committed, not an in-batch CreateRel step: the
        // step pre-ensures both endpoints into the cache, which would
        // mask the footprint.
        g.create_rel(a, b, "R".to_string(), BTreeMap::new())
            .unwrap();
        g.commit_write();
        let a_id = g.interner.get_id("A").unwrap();

        let mut sim = BatchSim::new(&g);
        // The ledger's committed seed already holds every live label
        // key (both nodes'), so the carried label's charge inserts
        // nothing new.
        let seeded_labels = sim.charges.labels.clone();
        sim.step(&g, &RedoOp::add_labels(a, vec!["A".into()]));
        // The set is unchanged, the seed is unchanged (the insert
        // fails, no charge), and the growth guard ends the step before
        // any incident pricing — the far endpoint never enters the
        // simulation's label cache.
        assert_eq!(sim.node_labels[&a], vec![a_id]);
        assert_eq!(sim.charges.labels, seeded_labels);
        assert_eq!(sim.charges.growth, 0);
        assert_eq!(
            sim.node_labels.len(),
            1,
            "the incident walk never ran: the far endpoint never enters the cache"
        );
    }
}
