//! CIP-121/CIP-214 vector similarity, matching Neo4j float32 rounding.

use cypherlite_core::{Error, Result, Value};
use std::collections::BTreeMap;

fn round_f32(x: f64) -> f64 {
    x as f32 as f64
}
/// Eight independent dot-product accumulators. The split into lanes is the
/// only reordering versus a single-accumulator loop; it breaks the
/// serial dependency chain so the compiler can vectorize the loop. The
/// `round_f32` results are contract-identical to the sequential loop
/// (see the equivalence tests below, ADR-0256).
fn dot_unrolled(a: &[f64], b: &[f64]) -> f64 {
    let mut lanes = [0.0f64; 8];
    let mut i = 0;
    while i + 8 <= a.len() {
        lanes[0] += a[i] * b[i];
        lanes[1] += a[i + 1] * b[i + 1];
        lanes[2] += a[i + 2] * b[i + 2];
        lanes[3] += a[i + 3] * b[i + 3];
        lanes[4] += a[i + 4] * b[i + 4];
        lanes[5] += a[i + 5] * b[i + 5];
        lanes[6] += a[i + 6] * b[i + 6];
        lanes[7] += a[i + 7] * b[i + 7];
        i += 8;
    }
    let mut dot = lanes.iter().sum();
    while i < a.len() {
        dot += a[i] * b[i];
        i += 1;
    }
    dot
}

/// The vector index's similarity function — the two-function subset of
/// the metric-name table [`VectorMetric`] owns (the vector *functions*
/// vocabulary): `from_name` narrows that one table, and the canonical
/// spelling `name` returns is what the DDL defaults insert and
/// `from_name` parses back. `Cosine` is the documented default (an
/// absent key — catalogs written before the DDL defaulted it).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    Cosine,
    Euclidean,
}

impl Metric {
    /// Narrow the one metric-name table: only the two index-config
    /// functions are storable.
    fn from_name(name: &str) -> Option<Self> {
        match cypherlite_ir::VectorMetric::from_name(name) {
            Some(cypherlite_ir::VectorMetric::Cosine) => Some(Metric::Cosine),
            Some(cypherlite_ir::VectorMetric::Euclidean) => Some(Metric::Euclidean),
            _ => None,
        }
    }

    /// The canonical stored spelling.
    fn name(self) -> &'static str {
        match self {
            Metric::Cosine => cypherlite_ir::VectorMetric::Cosine.name(),
            Metric::Euclidean => cypherlite_ir::VectorMetric::Euclidean.name(),
        }
    }
}

// The stored vector-index option schema, one home: the key table, the
// dimension bound, the one parse both the CREATE validation and the
// stored read consult, and the DDL defaults.

/// The outer OPTIONS keys the DDL admits, named once (the allowlist,
/// the navigations, and the inserts all consult them).
pub(crate) const KEY_INDEX_CONFIG: &str = "indexConfig";
pub(crate) const KEY_INDEX_PROVIDER: &str = "indexProvider";

const KEY_DIMENSIONS: &str = "vector.dimensions";
const KEY_SIMILARITY: &str = "vector.similarity_function";
const KEY_QUANTIZATION: &str = "vector.quantization.enabled";
const KEY_HNSW_M: &str = "vector.hnsw.m";
const KEY_HNSW_EF: &str = "vector.hnsw.ef_construction";

/// One navigation of the `indexConfig` option — Map or absent,
/// anything else is rejected at this boundary, never silently dropped.
/// A borrow: the one mutating consumer (the vector CREATE defaults)
/// clones at its point of mutation.
pub(crate) fn index_config(
    options: &BTreeMap<String, Value>,
) -> Result<Option<&BTreeMap<String, Value>>> {
    match options.get(KEY_INDEX_CONFIG) {
        None => Ok(None),
        Some(Value::Map(map)) => Ok(Some(map.as_ref())),
        Some(_) => Err(Error::semantic("InvalidIndexOption")),
    }
}

/// The five `vector.*` option keys the DDL admits — the allowlist, the
/// defaults, and the parse all consult this one table.
const CONFIG_KEYS: [&str; 5] = [
    KEY_DIMENSIONS,
    KEY_SIMILARITY,
    KEY_QUANTIZATION,
    KEY_HNSW_M,
    KEY_HNSW_EF,
];

/// The DDL bound on `vector.dimensions`.
const VECTOR_DIMENSIONS_MAX: i64 = 4096;

fn valid_vector_dimension(value: i64) -> bool {
    (1..=VECTOR_DIMENSIONS_MAX).contains(&value)
}

/// One drift contract for every invalid stored index option: the
/// create-time validation rejects these shapes on input, so a stored
/// entry that still carries one is catalog drift, never a silently
/// unconfigured index.
fn invalid_index_option(index_name: &str) -> Error {
    Error::execution(format!(
        "InvalidIndexDefinition: vector index {index_name} has an invalid indexConfig option"
    ))
}

/// The parse of a stored vector index's `indexConfig` — the allowlist and
/// both value-carrying leaves' shapes and domains stated once. The
/// CREATE validation calls this directly (its typed errors are the DDL's
/// user-facing codes); the stored read maps any error onto the drift
/// contract.
pub(crate) fn parse_config(config: &BTreeMap<String, Value>) -> Result<StoredVectorConfig> {
    if config
        .keys()
        .any(|key| !CONFIG_KEYS.contains(&key.as_str()))
    {
        return Err(Error::ty("InvalidInputValue"));
    }
    // The CREATE-side vocabulary: every user-input failure is a typed
    // DDL error; the stored read folds them all onto the drift
    // contract.
    let dimension = match config.get(KEY_DIMENSIONS) {
        None => None,
        Some(Value::Int(value)) if valid_vector_dimension(*value) => Some(*value as usize),
        Some(Value::Int(_)) => return Err(Error::ty("NumberOutOfRange")),
        Some(_) => return Err(Error::ty("InvalidInputValue")),
    };
    let metric = match config.get(KEY_SIMILARITY) {
        None => Metric::Cosine,
        Some(Value::String(name)) => {
            Metric::from_name(name).ok_or_else(|| Error::ty("InvalidInputValue"))?
        }
        Some(_) => return Err(Error::ty("InvalidInputValue")),
    };
    Ok(StoredVectorConfig { dimension, metric })
}

/// The stored vector index's parsed options.
pub(crate) struct StoredVectorConfig {
    pub dimension: Option<usize>,
    pub metric: Metric,
}

/// The create side of the option schema, the read side's mirror: one
/// adjacent pipeline — navigate (Map or absent, anything else is
/// rejected), parse (the typed DDL errors), apply the DDL defaults,
/// and return the canonical `indexConfig` value to store under
/// KEY_INDEX_CONFIG.
pub(crate) fn create_config(options: &BTreeMap<String, Value>) -> Result<Value> {
    let mut config = index_config(options)?.cloned().unwrap_or_default();
    let parsed = parse_config(&config)?;
    apply_defaults(&mut config, parsed.metric);
    Ok(Value::Map(config.into()))
}

/// The stored read: navigate the options' `indexConfig` and reuse the
/// one parse, mapping any failure onto the drift contract (a stored
/// entry a CREATE would have rejected is catalog drift).
pub(crate) fn stored_config(
    options: &BTreeMap<String, Value>,
    index_name: &str,
) -> Result<StoredVectorConfig> {
    // One navigation, shared with the create side; absent is the parse
    // over the empty config (the documented defaults), and every
    // failure folds onto the drift contract.
    let empty = BTreeMap::new();
    let config = index_config(options)
        .map_err(|_| invalid_index_option(index_name))?
        .unwrap_or(&empty);
    parse_config(config).map_err(|_| invalid_index_option(index_name))
}

/// The DDL defaults: the three pass-through HNSW/quantization values and
/// the parsed metric's canonical spelling — the parse established the
/// shapes, so this only inserts.
pub(crate) fn apply_defaults(config: &mut BTreeMap<String, Value>, metric: Metric) {
    config
        .entry(KEY_QUANTIZATION.into())
        .or_insert(Value::Bool(true));
    config.entry(KEY_HNSW_M.into()).or_insert(Value::Int(16));
    config.entry(KEY_HNSW_EF.into()).or_insert(Value::Int(100));
    // Canonical spelling, present or defaulted: the parse established
    // the String-ness, so the uppercase normalization folds here.
    config.insert(KEY_SIMILARITY.into(), Value::String(metric.name().into()));
}

/// The CIP-121 cosine score for the scan path (ADR-0256), with the
/// float32-rounded `(cos + 1) / 2` chain the pre-ADR-0256 kernel applied.
pub(crate) fn cosine_similarity_values(
    query: &[f64],
    query_norm: f64,
    candidate: &[Value],
) -> Option<f64> {
    let cos = round_f32(cosine_raw_values(query, query_norm, candidate)?);
    Some(round_f32((cos + 1.0) / 2.0))
}

/// The CIP-214 euclidean score for the scan path (ADR-0256), with the
/// float32-rounded `1 / (1 + d^2)` chain the pre-ADR-0256 kernel applied.
pub(crate) fn euclidean_similarity_values(query: &[f64], candidate: &[Value]) -> Option<f64> {
    Some(round_f32(
        1.0 / (1.0 + euclidean_squared_raw_values(query, candidate)?),
    ))
}

/// One `Value` list coordinate. `None` when the entry is not a finite
/// number — the candidate-skip half of `vector_operand`'s contract
/// (ADR-0256: scan candidates are never converted to a fresh `Vec<f64>`).
pub(crate) fn value_coord(value: &Value) -> Option<f64> {
    match value {
        Value::Int(x) => Some(*x as f64),
        Value::Float(x) if x.is_finite() => Some(*x),
        _ => None,
    }
}

/// The `Value` coordinate list of a candidate property: a VECTOR or a list.
/// `None` skips the candidate, exactly as `vector_operand` does.
pub(crate) fn value_coords(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Vector { values, .. } | Value::List(values) => Some(values),
        _ => None,
    }
}

/// The query's L2 norm — a per-search constant hoisted out of the scan
/// (ADR-0256). Computed by the same unrolled kernel the per-candidate
/// cosine uses, so the norm is bit-consistent within one search.
pub(crate) fn query_norm(query: &[f64]) -> f64 {
    dot_unrolled(query, query).sqrt()
}

/// Raw cosine between an f64 query (norm precomputed) and a `Value`
/// coordinate list, in one pass over the candidate: eight dot lanes and
/// eight candidate-norm lanes.
///
/// `None` when any candidate coordinate is not a finite number, or every
/// candidate coordinate is zero — the skip contracts of `vector_operand`
/// and the zero-vector guard.
fn cosine_raw_values(query: &[f64], query_norm: f64, candidate: &[Value]) -> Option<f64> {
    let mut dot = [0.0f64; 8];
    let mut norm = [0.0f64; 8];
    let mut any_nonzero = false;
    let mut i = 0;
    while i + 8 <= candidate.len() {
        for lane in 0..8 {
            let c = value_coord(&candidate[i + lane])?;
            dot[lane] += query[i + lane] * c;
            norm[lane] += c * c;
            any_nonzero |= c != 0.0;
        }
        i += 8;
    }
    let mut dot_sum: f64 = dot.iter().sum();
    let mut norm_sum: f64 = norm.iter().sum();
    while i < candidate.len() {
        let c = value_coord(&candidate[i])?;
        dot_sum += query[i] * c;
        norm_sum += c * c;
        any_nonzero |= c != 0.0;
        i += 1;
    }
    if !any_nonzero {
        return None;
    }
    Some(dot_sum / (query_norm * norm_sum.sqrt()))
}

/// Raw squared euclidean distance between an f64 query and a `Value`
/// coordinate list. `None` when any candidate coordinate is not a finite
/// number (the `vector_operand` skip contract).
fn euclidean_squared_raw_values(query: &[f64], candidate: &[Value]) -> Option<f64> {
    let mut dist = [0.0f64; 8];
    let mut i = 0;
    while i + 8 <= candidate.len() {
        for lane in 0..8 {
            let c = value_coord(&candidate[i + lane])?;
            let d = query[i + lane] - c;
            dist[lane] += d * d;
        }
        i += 8;
    }
    let mut dist_sum: f64 = dist.iter().sum();
    while i < candidate.len() {
        let c = value_coord(&candidate[i])?;
        let d = query[i] - c;
        dist_sum += d * d;
        i += 1;
    }
    Some(dist_sum)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Sequential single-accumulator reference, the pre-ADR-0256 kernel.
    fn cosine_raw_sequential(a: &[f64], b: &[f64]) -> f64 {
        let mut dot = 0.0;
        let mut na = 0.0;
        let mut nb = 0.0;
        for i in 0..a.len() {
            dot += a[i] * b[i];
            na += a[i] * a[i];
            nb += b[i] * b[i];
        }
        dot / (na.sqrt() * nb.sqrt())
    }

    fn euclidean_squared_sequential(a: &[f64], b: &[f64]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    /// Deterministic LCG so the equivalence corpus is fixed across runs and
    /// platforms.
    fn lcg(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*state >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    }

    fn value_list(coords: &[f64]) -> Arc<Vec<Value>> {
        Arc::new(coords.iter().map(|&x| Value::Float(x)).collect())
    }

    /// ADR-0256: the unrolled kernels reorder only the f64 partial sums, so
    /// every float32-rounded score — the CIP-121/CIP-214 user-visible
    /// contract — matches the sequential reference bit-for-bit over a fixed
    /// corpus of random, aligned, tail-carrying, and scale-varied inputs.
    /// The `Value`-list kernels (the scan path) and the `Vec<f64>` kernels
    /// (the reference path) round identically too.
    #[test]
    fn unrolled_kernels_round_identically_to_sequential() {
        let dims = [1, 2, 7, 8, 9, 15, 16, 17, 63, 64, 65, 127, 128, 1536];
        let mut state = 0x5eed_cafe_f00d_bee5u64;
        for &d in &dims {
            for trial in 0..200 {
                let scale = [1e-8, 1e-3, 1.0, 1e3][trial % 4];
                let a: Vec<f64> = (0..d).map(|_| lcg(&mut state) * scale).collect();
                let b: Vec<f64> = (0..d).map(|_| lcg(&mut state) * scale).collect();
                // The scan path: candidate as a Value coordinate list, query
                // norm hoisted per search, compared against the sequential
                // single-accumulator reference with the same rounding chain.
                let b_values = Value::List(value_list(&b));
                let coords = value_coords(&b_values).expect("list coordinates");
                assert_eq!(
                    cosine_similarity_values(&a, query_norm(&a), coords),
                    Some(round_f32(
                        (round_f32(cosine_raw_sequential(&a, &b)) + 1.0) / 2.0
                    )),
                    "cosine d={d} trial={trial}"
                );
                assert_eq!(
                    euclidean_similarity_values(&a, coords),
                    Some(round_f32(
                        1.0 / (1.0 + euclidean_squared_sequential(&a, &b))
                    )),
                    "euclidean d={d} trial={trial}"
                );
            }
        }
    }

    /// The `Value` kernels keep `vector_operand`'s skip contracts: a
    /// non-number or non-finite coordinate, a non-list property, and an
    /// all-zero cosine candidate all skip the candidate (None); euclidean
    /// has no zero guard.
    #[test]
    fn value_kernels_keep_the_skip_contracts() {
        let query = [1.0, 0.0];
        let qn = query_norm(&query);
        let values = |coords: &[Value]| Value::List(Arc::new(coords.to_vec()));

        // (The query/candidate length check lives in `vector_score`.)
        assert!(
            cosine_raw_values(&query, qn, &[Value::Int(1), Value::Null]).is_none(),
            "non-number coordinate"
        );
        assert!(
            cosine_raw_values(&query, qn, &[Value::Float(f64::NAN), Value::Float(0.0)]).is_none(),
            "non-finite coordinate"
        );
        assert!(
            cosine_raw_values(&query, qn, &[Value::Float(0.0), Value::Float(0.0)]).is_none(),
            "zero-vector candidate"
        );
        assert!(
            euclidean_squared_raw_values(&query, &[Value::Float(0.0), Value::Float(0.0)]).is_some(),
            "euclidean has no zero guard"
        );
        assert!(value_coords(&Value::Null).is_none(), "null property");
        assert!(value_coords(&Value::Int(3)).is_none(), "scalar property");
        assert_eq!(
            value_coords(&values(&[Value::Float(1.0)])).map(|c| c.len()),
            Some(1)
        );
    }
}
