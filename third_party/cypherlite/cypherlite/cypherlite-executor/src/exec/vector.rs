//! Vector index candidate search: the scoring kernels and the bounded
//! top-k window the SEARCH clause machinery (`super::search`) layers over.

use std::cmp::Ordering as CmpOrdering;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashSet};

use cypherlite_core::{Error, NodeId, PropertyMap, Result, Value};
use cypherlite_ir::{SearchFilterPred, SearchOp};
use cypherlite_model::EntityKind;

use crate::ctx::ExecCtx;
use crate::eval::{binary, eval, Row};
use crate::overlay::ExecGraph;
use crate::vector_similarity::{value_coord, value_coords, Metric};

pub(crate) fn vector_operand(value: &Value) -> Result<Option<Vec<f64>>> {
    let values = match value {
        Value::Null => return Ok(None),
        Value::Vector { values, .. } | Value::List(values) => values,
        _ => {
            return Err(Error::ty(
                "InvalidValueType: vector search requires a numeric list or VECTOR",
            ))
        }
    };
    if values.is_empty() {
        return Err(Error::ty(
            "InvalidValueType: vector search requires a non-empty vector",
        ));
    }
    let mut out = Vec::with_capacity(values.len());
    for value in values.iter() {
        out.push(value_coord(value).ok_or_else(|| {
            Error::ty("InvalidValueType: vector coordinates must be finite numbers")
        })?);
    }
    Ok(Some(out))
}

/// The per-search query facts, built once (stellar round 2, std-3): the
/// coordinates, their norm, and the all-zero guard travel together so no
/// caller can pass a bool that contradicts the slice. (The zero guard is
/// the exact `all(|v| *v == 0.0)` — denormal underflow can zero the norm of
/// a nonzero query, so the norm must not derive the guard.)
struct PreparedQuery<'a> {
    coords: &'a [f64],
    norm: f64,
    zero: bool,
}

impl PreparedQuery<'_> {
    fn prepare(query: &[f64]) -> PreparedQuery<'_> {
        PreparedQuery {
            coords: query,
            norm: crate::vector_similarity::query_norm(query),
            zero: query.iter().all(|value| *value == 0.0),
        }
    }
}

/// Score one scan candidate straight from its `Value` coordinate list —
/// no per-candidate `Vec<f64>` conversion (ADR-0256); the skip contracts
/// live in the `vector_similarity` value kernels.
fn vector_score(query: &PreparedQuery<'_>, candidate: &[Value], metric: Metric) -> Option<f64> {
    if query.coords.is_empty() || query.coords.len() != candidate.len() {
        return None;
    }
    match metric {
        // The zero-query guard is cosine's only (a zero query has no
        // direction; euclidean distance stays defined).
        Metric::Cosine if query.zero => None,
        Metric::Cosine => {
            crate::vector_similarity::cosine_similarity_values(query.coords, query.norm, candidate)
        }
        Metric::Euclidean => {
            crate::vector_similarity::euclidean_similarity_values(query.coords, candidate)
        }
    }
}

pub(crate) fn vector_filter_rhs_allowed(value: &Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Temporal(_)
    )
}

pub(crate) fn vector_filter_passes(
    properties: &PropertyMap,
    filters: &[(&str, SearchOp, Value)],
) -> Result<bool> {
    for (property, op, rhs) in filters {
        // A duration has no total order, so it is comparable only by
        // equality: a range predicate over one filters the candidate out
        // (CIP-240's value-type rules).
        if matches!(
            rhs,
            Value::Temporal(cypherlite_core::Temporal::Duration { .. })
        ) && !matches!(op, SearchOp::Eq)
        {
            return Ok(false);
        }
        // The canonical binary comparison (stellar round 1, std-4): no
        // ad-hoc Expr tree for two already-evaluated values.
        let left = properties.get(property).cloned().unwrap_or(Value::Null);
        if binary((*op).into(), left, rhs.clone())?.truthy() != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The catalog-descriptor phase: fetch the index detail and options,
/// validate the kind and `indexConfig` leaves, and parse the metric and
/// the one indexed property — everything the scan needs before a
/// candidate is ever scored.
fn resolve_vector_index(
    graph: &ExecGraph<'_>,
    index_name: &str,
    query: &[f64],
) -> Result<(crate::redo::IndexDetailSpec, String, Metric)> {
    let theater = graph.show_schema()?;
    // The two catalog maps are installed by the same redo op under the
    // same name (apply_redo.rs, theater.rs; drops remove both), so either
    // miss is a missing index — one typed policy for both reads, never
    // a panic beside the handled form.
    let (detail, options) = match (
        theater.data.index_details.get(index_name),
        theater.data.index_options.get(index_name),
    ) {
        (Some(detail), Some(options)) => (detail.clone(), options.clone()),
        _ => return Err(Error::execution(format!("IndexDoesNotExist: {index_name}"))),
    };
    drop(theater);
    if detail.kind != crate::redo::IndexKindSpec::Vector {
        return Err(Error::execution(format!("WrongIndexType: {index_name}")));
    }
    let config = crate::vector_similarity::stored_config(&options, index_name)?;
    if config
        .dimension
        .is_some_and(|dimension| dimension != query.len())
    {
        return Err(Error::ty("VectorIndexDimensionalityMismatch"));
    }
    let metric = config.metric;
    // A vector index always indexes exactly one property (catalog DDL
    // invariant) — no fabricated empty property name, and a drifted
    // multi-property detail fails loudly instead of first() quietly
    // picking a winner (stellar round 1, std-5).
    if detail.properties.len() != 1 {
        return Err(Error::execution(format!(
            "InvalidIndexDefinition: vector index {index_name} must index exactly one property"
        )));
    }
    let property = detail.properties[0].clone();
    Ok((detail, property, metric))
}

/// The CIP-240 filter preparation: Rule 2 (index property membership --
/// the one catalog-dependent rule), the RHS value-type gate, and the
/// per-search RHS evaluation. Rules 1/6 fired at lowering, Rules 3/7 at
/// the grammar walk.
fn prepare_vector_filters<'a>(
    filters: &'a [SearchFilterPred],
    detail: &crate::redo::IndexDetailSpec,
    graph: &ExecGraph<'_>,
    input: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<(&'a str, SearchOp, Value)>> {
    let additional: HashSet<_> = detail.with_properties.iter().map(String::as_str).collect();
    let mut evaluated = Vec::with_capacity(filters.len());
    for filter in filters {
        if !additional.contains(filter.property.as_str()) {
            return Err(Error::execution("WrongPropertyForVectorSearchFilter"));
        }
        let rhs = eval(graph, input, &filter.rhs, params, cypher_version)?;
        if !vector_filter_rhs_allowed(&rhs) {
            return Err(Error::ty(
                "InvalidValueType: invalid vector search filter RHS type",
            ));
        }
        evaluated.push((filter.property.as_str(), filter.op, rhs));
    }
    Ok(evaluated)
}

/// The one canonical CIP-225 candidate ordering: `a` is `Greater` (better)
/// than `b` when its score is higher, or the scores tie and its element id
/// is lower. NaN scores tie with everything — the `unwrap_or(Equal)` of the
/// pre-ADR-0256 comparator, kept so a pathological NaN ranks the same way.
pub(crate) fn ranked_better_cmp(score_a: f64, id_a: i64, score_b: f64, id_b: i64) -> CmpOrdering {
    score_a
        .partial_cmp(&score_b)
        .unwrap_or(CmpOrdering::Equal)
        .then_with(|| id_b.cmp(&id_a))
}

/// A scan candidate ordered by [`ranked_better_cmp`]: `Greater` is better.
/// `PartialEq` agrees with `Ord` by construction (stellar round 1, std-5):
/// the derived-eq disagreement on NaN ties is gone.
#[derive(Clone, Copy)]
struct Ranked {
    score: f64,
    id: i64,
}

// Total under `ranked_better_cmp` (NaN ties with everything), so `Ord`
// applies even though the bare `f64` score is not `Eq`.
impl Eq for Ranked {}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == CmpOrdering::Equal
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        ranked_better_cmp(self.score, self.id, other.score, other.id)
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

/// ADR-0256: the bounded top-k collector for the vector and fulltext flat
/// scans — the one CIP-225 result window (score descending, element id
/// ascending, `SKIP`/`LIMIT`). Retains only the best `offset + limit`
/// candidates — worst at the heap root — so the scan costs
/// `O(n log window)` with no full-length collect, and materializes the
/// element `Value` only for the survivors. Owns `(offset, limit)` itself
/// so no caller can let the retained window and the windowed rows drift
/// (stellar round 1, std-5).
pub(crate) struct TopWindow {
    worst_first: BinaryHeap<Reverse<Ranked>>,
    offset: usize,
    limit: usize,
}

impl TopWindow {
    pub(crate) fn new(offset: usize, limit: usize) -> Self {
        let window = offset.saturating_add(limit);
        Self {
            worst_first: BinaryHeap::with_capacity(window.min(1024)),
            offset,
            limit,
        }
    }

    /// Retain `candidate` if it beats the current worst retained entry.
    /// Equal (score, id) pairs never displace each other; ids are unique per
    /// candidate so an equal-ranked push is always the same candidate.
    pub(crate) fn push(&mut self, score: f64, id: i64) {
        let window = self.offset.saturating_add(self.limit);
        if window == 0 {
            return;
        }
        let candidate = Ranked { score, id };
        if self.worst_first.len() < window {
            self.worst_first.push(Reverse(candidate));
        } else if let Some(mut worst) = self.worst_first.peek_mut() {
            if candidate > worst.0 {
                *worst = Reverse(candidate);
            }
        }
    }

    /// The retained candidates best-first, windowed by the constructor's
    /// `offset`/`limit` — the heap emits best-first already
    /// (`Reverse<Ranked>` ascending is `Ranked` best-first).
    pub(crate) fn into_sorted_rows(self) -> Vec<(i64, f64)> {
        self.worst_first
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(r)| (r.id, r.score))
            .skip(self.offset)
            .take(self.limit)
            .collect()
    }
}

/// Bundled CIP-240 vector-search inputs: the decomposed filter
/// predicates (attached at lowering, Rules 1/6 already enforced there)
/// and the row their RHS expressions are evaluated against. (Bundled to
/// avoid `clippy::too_many_arguments`.)
pub(crate) struct VectorSearchFilter<'a> {
    pub(crate) filters: &'a [SearchFilterPred],
    pub(crate) input: &'a Row,
}

/// Materialize one scored survivor (stellar round 2, std-6): the id was
/// just read from this same read-only graph, so a failed re-fetch is an
/// invariant violation — a panic, not a silently short result set. One
/// copy of the contract for the vector and fulltext scans.
pub(crate) fn materialize_candidate(graph: &ExecGraph<'_>, kind: EntityKind, id: i64) -> Value {
    let candidate = if kind.is_relationship() {
        graph.rel(id).map(Value::from_relationship)
    } else {
        graph.node(id).map(Value::from_node)
    };
    candidate
        .unwrap_or_else(|| panic!("SEARCH: scored candidate {id} vanished from the read snapshot"))
}

pub(crate) fn vector_index_candidates(
    ctx: ExecCtx<'_>,
    index_name: &str,
    query: &[f64],
    offset: usize,
    limit: usize,
    filter: VectorSearchFilter<'_>,
) -> Result<(EntityKind, Vec<(Value, f64)>)> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let VectorSearchFilter { filters, input } = filter;
    let (detail, property, metric) = resolve_vector_index(graph, index_name, query)?;
    // The per-search query facts (ADR-0256 hoist, stellar round 2 bundle):
    // norm and zero-guard are shared by every candidate's cosine.
    let prepared = PreparedQuery::prepare(query);
    let evaluated_filters =
        prepare_vector_filters(filters, &detail, graph, input, params, cypher_version)?;
    let kind = EntityKind::from_relationship_flag(detail.relationship);
    // ADR-0256: the unfiltered path never materializes a `Value` per scored
    // candidate — only the ≤ offset+limit survivors become entities below.
    // Filtered (CIP-235 `WITH`) searches still evaluate properties per
    // scored candidate and take the same flat path.
    // The one per-candidate scan body: indexed-property read, coordinate
    // parse, score, filter gate. The entity-kind arms below gate on
    // labels themselves (the rel arm via rel_type, the node arm beside
    // its computation) and drive this once.
    let candidate_score = |properties: &PropertyMap| -> Result<Option<f64>> {
        let Some(value) = properties.get(&property) else {
            return Ok(None);
        };
        let Some(coords) = value_coords(value) else {
            return Ok(None);
        };
        let Some(score) = vector_score(&prepared, coords, metric) else {
            return Ok(None);
        };
        if !vector_filter_passes(properties, &evaluated_filters)? {
            return Ok(None);
        }
        Ok(Some(score))
    };
    let mut top = TopWindow::new(offset, limit);
    if kind.is_relationship() {
        // The rel arm keeps the sweep: the base has no relationship-by-
        // type postings (live_rel_ids_of_type itself is an O(R) walk),
        // so a per-type enumeration would pay k whole-table walks where
        // this sweep pays one.
        for id in graph.all_rel_ids() {
            // The type filter runs before the record clone: a sparse
            // indexed rel type must not pay one Relationship clone per
            // non-matching relationship per SEARCH statement.
            let Some(rel_type) = graph.rel_type(id) else {
                continue;
            };
            if !detail.labels.iter().any(|label| label == &rel_type) {
                continue;
            }
            let Some(rel) = graph.rel(id) else { continue };
            if let Some(score) = candidate_score(&rel.properties)? {
                top.push(score, id);
            }
        }
    } else {
        // The node candidates ride the exact live per-label enumeration
        // (LABS-442: a sparse label skips the other labels' rows and
        // lookups instead of sweeping every id and label-checking each
        // record) — base postings at the pinned snapshot plus every
        // scratch entry by its actual labels, deletions filtered, so an
        // in-statement `SET n:Label` copy-on-write node is enumerated
        // exactly like the sweep found it. The multi-label index is the
        // per-label union; no labels enumerates nothing, as the sweep's
        // label check did. Computed here, in the node arm, so a
        // relationship-index search pays nothing for it.
        let node_candidates: Vec<NodeId> = {
            let mut ids: Vec<NodeId> = detail
                .labels
                .iter()
                .flat_map(|label| graph.live_node_ids_with_label(label))
                .collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        for id in node_candidates {
            // The exact enumeration guarantees the node exists at this
            // snapshot: a miss is an invariant violation, not a skip
            // (the materialize_candidate contract).
            let node = graph.node(id).unwrap_or_else(|| {
                panic!("SEARCH: enumerated candidate {id} vanished from the read snapshot")
            });
            if let Some(score) = candidate_score(&node.properties)? {
                top.push(score, id);
            }
        }
    }
    let rows = top.into_sorted_rows();
    let mut candidates = Vec::with_capacity(rows.len());
    for (id, score) in rows {
        candidates.push((materialize_candidate(graph, kind, id), score));
    }
    Ok((kind, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A duration has no total order, so a range filter over one excludes
    /// the candidate while an equality still compares (CIP-240 value-type
    /// rules) — pinning the guard vector_filter_passes carries.
    #[test]
    fn duration_range_filter_excludes_the_candidate() {
        use cypherlite_core::Temporal;
        let duration = || {
            Value::Temporal(Temporal::Duration {
                months: 0,
                days: 1,
                nanos: 0,
            })
        };
        let mut properties = PropertyMap::new();
        properties.insert("ttl".into(), duration());
        let range = vec![("ttl", SearchOp::Gt, duration())];
        assert!(
            !vector_filter_passes(&properties, &range).unwrap(),
            "a range over a duration filters the candidate out"
        );
        let equality = vec![("ttl", SearchOp::Eq, duration())];
        assert!(vector_filter_passes(&properties, &equality).unwrap());
    }

    /// Deterministic LCG: fixed corpus across runs and platforms.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 11
    }

    /// Brute-force reference for the CIP-225 window: full sort by
    /// `ranked_better_cmp`, drop `offset`, keep `limit`.
    fn reference_window(rows: &[(i64, f64)], offset: usize, limit: usize) -> Vec<(i64, f64)> {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|(left_id, left_score), (right_id, right_score)| {
            ranked_better_cmp(*right_score, *right_id, *left_score, *left_id)
        });
        sorted.drain(..offset.min(sorted.len()));
        sorted.truncate(limit);
        sorted
    }

    /// ADR-0256: the bounded `TopWindow` collector produces exactly the rows
    /// the full-collect `sort_and_window` reference produces — score
    /// descending, element id ascending on ties — across window sizes,
    /// offsets, ties, and NaN scores.
    #[test]
    fn top_window_matches_full_sort_reference() {
        let mut state = 0x5eed_cafe_f00d_bee5u64;
        for trial in 0..50 {
            let n = 1 + (lcg(&mut state) % 200) as usize;
            // Repeated scores force the element-id tiebreak. All scores are
            // finite: `vector_score` yields only finite scores (zero-vector
            // cosine is rejected up front), so the NaN tie path of
            // `ranked_better_cmp` is never exercised by a full sort.
            let rows: Vec<(i64, f64)> = (0..n as i64)
                .map(|id| {
                    let score = match lcg(&mut state) % 10 {
                        1 => 0.5,
                        2 => 1.0,
                        _ => (lcg(&mut state) % 1000) as f64 / 997.0,
                    };
                    (id, score)
                })
                .collect();
            for &(offset, limit) in &[(0usize, 10usize), (0, 1), (5, 7), (3, 0), (0, 500), (17, 3)]
            {
                let mut top = TopWindow::new(offset, limit);
                for &(id, score) in &rows {
                    top.push(score, id);
                }
                assert_eq!(
                    top.into_sorted_rows(),
                    reference_window(&rows, offset, limit),
                    "trial={trial} n={n} offset={offset} limit={limit}"
                );
            }
        }
    }
}
