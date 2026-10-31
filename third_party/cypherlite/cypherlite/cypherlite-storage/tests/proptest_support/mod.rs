//! Reusable `proptest` strategies for the `cypherlite-storage` property tests.
//!
//! Included by the integration tests in this directory with
//! `mod proptest_support;`. The strategies live here, once, so every WAL /
//! checkpoint / snapshot property test draws from the same alphabet instead of
//! re-declaring generators inline (ADR-0268).
//!
//! # What the `RedoOp` strategy generates
//!
//! [`cypherlite_executor::RedoOp`] is the real WAL redo-op type; every variant
//! and every variant's fields are `pub`, so it *is* directly constructible
//! from a `cypherlite-storage` integration test (the same way `wal.rs`'s own
//! doctest builds `RedoOp::CreateNode`). [`redo_op`] therefore generates actual
//! `RedoOp` values directly. The alphabet is the graph-mutation subset a WAL
//! commit carries — `CreateNode`, `CreateRel`, `DeleteNode`, `DeleteRel`,
//! `SetNodeProp`, `RemoveNodeProp`, `SetRelProp`, `RemoveRelProp`, `AddLabels`,
//! `RemoveLabels` — and intentionally omits the catalog/DDL variants
//! (`PutGraphType`, `CatalogIndexDetail`, …) whose payload models
//! (`GraphTypeSpec`, `IndexDetailSpec`) are not what the WAL replay properties
//! need. IDs stay in a small range so generated ops form graphs rather than
//! pointing at ids the engine never allocates.
//!
//! The alphabet is kept closed at [`ModelGraph::appliable_op`], which matches
//! [`RedoOp`] exhaustively with **no catch-all**: a new `RedoOp` variant fails
//! to compile there until its applicability policy is written, so it cannot
//! silently flow into a "keep" default — the bug class a catch-all over
//! `RedoOp` would reintroduce.
//!
//! [`redo_op_sequence`] wraps a bounded `Vec` of those ops — the unit
//! [`cypherlite_storage::Wal::append_commit`] persisting and replaying.
//!
//! # Checkpoint-bearing properties
//!
//! The WAL codec round-trips every generated value (non-finite floats via
//! tagged encodings), but a checkpoint persists property values through the
//! active *snapshot* codec, whose contract is narrower: the v2 JSON snapshot
//! codec (cypherlite-model/src/serde_value.rs) encodes by "rejecting values
//! that cannot round-trip" — a non-finite float anywhere in a property fails
//! the checkpoint closed (ADR-0087), and a `Uuid` reloads as its canonical
//! string (the CIP-246 deviation: `CypherValue` has no UUID variant).
//! [`checkpoint_persistable_ops`] is the single value-representability rule
//! for checkpoint-bearing properties, in the spirit of
//! [`ModelGraph::appliable_op`]'s single applicability filter (ADR-0268):
//! identity under the Parquet codec, a finite-stand-in / canonical-string
//! rewrite under the JSON codec.
//!
//! # Property values
//!
//! The property values WAL ops carry are [`cypherlite_core::Value`] — that is
//! the type in `RedoOp`'s fields, and it is what `cypherlite-core` names the
//! runtime `CypherValue` model. [`property_value`] covers the value kinds a
//! WAL op can carry: `Null`, `Bool`, `Int`, `Float` (including the tagged
//! `NaN`/`±Infinity`/`-0.0` encodings), `String`, `Uuid`, `List`, `Map`,
//! `Temporal`, `Point`, and `Vector`. It deliberately does not generate the
//! graph-entity variants (`Node`, `Relationship`, `Path`) — those are query
//! results, never property payloads.

// Each integration-test binary compiles this module on its own and uses only
// the helpers it needs, so a shared helper is legitimately unused in at least
// one binary. There is deliberately no blanket `#![allow(dead_code)]`; instead
// each helper that is not used by *every* binary including this module carries
// a targeted `#[allow(dead_code)]` naming why.

/// The reference WAL model, consumed only by the state-machine suite; compiled
/// (and therefore dead) in the WAL and recovery suites.
#[allow(dead_code)]
pub mod wal_model;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cypherlite_core::temporal::parse_iso;
use cypherlite_core::{uuid_hyphenated, PointValue, Value, VectorCoordKind};
use cypherlite_executor::{Graph, RedoOp};
use cypherlite_storage::SnapshotCodecKind;
use proptest::prelude::*;
use proptest::test_runner::Config;
use serde_json::{json, Value as JsonValue};

/// File name of the single WAL segment within the store's WAL directory.
///
/// A filesystem store keeps it directly under [`cypherlite_storage::wal_dir`];
/// a `:memory:` store keeps it under the `wal/` prefix. Shared by the WAL and
/// recovery property suites so the segment name lives in one place. Unused by
/// the state-machine suite, hence the targeted `allow`.
#[allow(dead_code)]
pub const WAL_SEGMENT_FILE: &str = "00000001.log";

/// Directory prefix a store keeps its WAL segment under.
///
/// A `:memory:` store opens its `Wal` under this prefix (`Wal::memory` uses the
/// same), and the WAL and recovery property suites open their real `Wal`s with
/// it, so the literal lives in one place. The state-machine suite reaches it
/// through [`wal_model`], hence no `allow`.
pub const WAL_PREFIX: &str = "wal";

/// The segment path within a `:memory:` store: [`WAL_PREFIX`] joined with
/// [`WAL_SEGMENT_FILE`]. Both suites that read the raw segment bytes build the
/// path here rather than open-coding `format!("wal/…")`. Unused by the
/// state-machine suite, hence the targeted `allow`.
#[allow(dead_code)]
pub fn wal_segment_path() -> String {
    format!("{WAL_PREFIX}/{WAL_SEGMENT_FILE}")
}

/// Encode a slice of [`RedoOp`]s exactly as the WAL codec writes them —
/// `serde_json`'s tagged encoding, the bytes `Wal::append_record` persists and
/// `scan_seg` decodes.
///
/// The one place the encode-and-`expect` incantation lives: [`wal_model`]'s
/// digest and codec round-trip and the WAL suite's record comparison all encode
/// through it. Serialization is infallible for `RedoOp`, so a failure is a
/// harness bug, not a data condition.
pub fn encode_ops(ops: &[RedoOp]) -> Vec<u8> {
    serde_json::to_vec(ops).expect("RedoOp serializes exactly as the WAL encodes it")
}

/// The shared `proptest` configuration for every property test in the crate.
///
/// It is [`Config::default`], which already applies `proptest`'s 256-case
/// default **and** the `PROPTEST_*` environment overrides — the default config
/// is built by reading `PROPTEST_CASES`, `PROPTEST_RNG_SEED`, and the rest.
/// Wrapping it here keeps one config for the whole crate and one place to
/// document the knobs. Do **not** pin `cases` explicitly: naming it would
/// shadow `PROPTEST_CASES` and stop a scheduled job from scaling the load.
///
/// Use from a `proptest!` block with
/// `#![proptest_config(proptest_support::config())]` so the whole suite runs
/// under one configuration.
pub fn config() -> Config {
    Config::default()
}

/// The upper bound on a generated op sequence's length. Small enough to keep a
/// single case cheap, long enough for a replay to exercise multi-commit state.
pub const MAX_SEQUENCE_LEN: usize = 16;

/// Reference model of the graph ids a generated [`RedoOp`] sequence touches,
/// used to reduce the raw alphabet to ops the engine's redo preflight accepts.
///
/// One model instance is fed every commit of a case in order, so the reduction
/// tracks state across commits (the WAL suite's contract) as well as within a
/// flat sequence (the recovery suite's). This is the *single* applicability
/// filter for both suites (ADR-0268): it implements the stricter WAL policy —
/// a `CreateNode` is dropped when its id is already live, and a `CreateRel`
/// needs a fresh id **and** both endpoints live.
///
/// The engine itself is more permissive at the id level (`install_node` /
/// `install_rel` push another version for an occupied id, and the redo
/// preflight in `cypherlite-executor` does not reject a duplicate create), so
/// the reduction is stricter than the engine. That is deliberate: the suites
/// measure graph *state* across replay, and dropping id reuse keeps the
/// reference and the recovered state aligned without relying on versioned
/// duplicate-create semantics that the properties do not model.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ModelGraph {
    nodes: BTreeSet<i64>,
    rels: BTreeMap<i64, (i64, i64)>,
}

#[allow(dead_code)]
impl ModelGraph {
    /// An empty model (no live nodes or relationships).
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether `op` applies on top of the state modelled so far, and
    /// update the model when it does.
    ///
    /// The policy:
    ///
    /// - `CreateNode` applies while its id is fresh (dropped otherwise).
    /// - `CreateRel` applies while its id is fresh **and** both endpoints are
    ///   live; a kept relationship detaches nothing.
    /// - `DeleteNode` applies while its node is live, and detaches the node's
    ///   incident relationships from the model.
    /// - `DeleteRel` applies while its relationship is live.
    /// - property/label mutations apply while their target entity is live.
    ///
    /// The match is exhaustive over [`RedoOp`] with **no catch-all**, so a
    /// generated variant cannot silently fall into a "keep" default: adding a
    /// `RedoOp` variant is a compile error here until its policy is written.
    ///
    /// The catalog / DDL variants of [`RedoOp`] (`PutGraphType`,
    /// `CreateIndex`, …) are outside the alphabet [`redo_op`] generates, so
    /// they are never appliable and share a single `false` arm — present only
    /// to keep the match exhaustive. This is **not** a claim that the engine
    /// rejects such ops: `RedoPreflight::check_op` accepts well-formed ones.
    /// Any future addition of a catalog variant to the alphabet must therefore
    /// have its policy validated against `check_op` first.
    pub fn appliable_op(&mut self, op: &RedoOp) -> bool {
        match op {
            RedoOp::CreateNode { id, .. } => self.nodes.insert(*id),
            RedoOp::CreateRel { id, start, end, .. } => {
                if !self.rels.contains_key(id)
                    && self.nodes.contains(start)
                    && self.nodes.contains(end)
                {
                    self.rels.insert(*id, (*start, *end));
                    true
                } else {
                    false
                }
            }
            RedoOp::DeleteNode { id } => {
                if self.nodes.remove(id) {
                    self.rels.retain(|_, (start, end)| start != id && end != id);
                    true
                } else {
                    false
                }
            }
            RedoOp::DeleteRel { id } => self.rels.remove(id).is_some(),
            RedoOp::SetNodeProp { id, .. }
            | RedoOp::RemoveNodeProp { id, .. }
            | RedoOp::AddLabels { id, .. }
            | RedoOp::RemoveLabels { id, .. } => self.nodes.contains(id),
            RedoOp::SetRelProp { id, .. } | RedoOp::RemoveRelProp { id, .. } => {
                self.rels.contains_key(id)
            }
            RedoOp::CreateIndex { .. }
            | RedoOp::DropIndex { .. }
            | RedoOp::CreateConstraint { .. }
            | RedoOp::DropConstraint { .. }
            | RedoOp::CatalogIndexDetail { .. }
            | RedoOp::CatalogDerivedConstraint { .. }
            | RedoOp::PutDatabase { .. }
            | RedoOp::DropDatabase { .. }
            | RedoOp::PutGraphType { .. }
            | RedoOp::SetCurrentGraphType { .. }
            | RedoOp::PullIdentityPut { .. } => false,
            // Store-level ontology ops (ADR-0120 §3) mutate no graph state;
            // the model never generates them, but the match stays total.
            RedoOp::OntologySet { .. } | RedoOp::OntologyDrop { .. } => false,
        }
    }

    /// Reduce one commit's ops to the appliable ones, in order, advancing the
    /// model. The single reduction entry point both suites call; it returns the
    /// [`RedoOp`]s directly (the alphabet is already `RedoOp`), so callers do
    /// not widen.
    pub fn apply_commit(&mut self, ops: &[RedoOp]) -> Vec<RedoOp> {
        ops.iter()
            .filter(|op| self.appliable_op(op))
            .cloned()
            .collect()
    }
}

/// Canonical fingerprint of the observable graph state — live nodes and
/// relationships (with their tagged property maps), the commit timestamp, and
/// the allocation counters — in the engine's own tagged value encoding
/// ([`Value::to_json_tagged`]).
///
/// This is the single state representation for both suites. The tagged value
/// encoding is the stronger of the two historical forms: it renders every
/// [`Value`] canonically (normalising `NaN`, ±Infinity, signed zero) so values
/// the WAL/checkpoint codecs legitimately normalise still compare by
/// representation, whereas a `Debug` string depends on the value types'
/// `Debug` impls.
///
/// Used by the WAL and recovery suites only; dead in the state-machine suite.
#[allow(dead_code)]
pub fn state_json(graph: &Graph) -> JsonValue {
    let nodes: Vec<JsonValue> = graph
        .all_node_ids()
        .into_iter()
        .map(|id| graph.node(id).expect("listed node is live"))
        .map(|n| {
            json!({
                "id": n.id,
                "labels": n.labels,
                "props": Value::Map(std::sync::Arc::new((*n.properties).clone().into())).to_json_tagged(),
            })
        })
        .collect();
    let rels: Vec<JsonValue> = graph
        .all_rel_ids()
        .into_iter()
        .map(|id| graph.rel(id).expect("listed relationship is live"))
        .map(|r| {
            json!({
                "id": r.id,
                "start": r.start,
                "end": r.end,
                "type": r.rel_type,
                "props": Value::Map(std::sync::Arc::new((*r.properties).clone().into())).to_json_tagged(),
            })
        })
        .collect();
    json!({
        "commit_ts": graph.commit_ts(),
        "next_node_id": graph.next_node_id(),
        "next_rel_id": graph.next_rel_id(),
        "nodes": nodes,
        "rels": rels,
    })
}

/// A `RedoOp` sequence of bounded length drawn from the graph-mutation
/// alphabet described in the [module docs](self). This is the input a WAL
/// append/replay property persists and replays as one commit.
pub fn redo_op_sequence() -> impl Strategy<Value = Vec<RedoOp>> {
    prop::collection::vec(redo_op(), 0..=MAX_SEQUENCE_LEN)
}

/// Adapt a drawn op sequence to what the active snapshot codec can persist in
/// a checkpoint: the single value-representability rule for checkpoint-bearing
/// properties, in the spirit of [`ModelGraph::appliable_op`]'s single
/// applicability filter (ADR-0268).
///
/// Why: a checkpoint persists property values through the store's *snapshot*
/// codec, whose contract is narrower than the WAL's. The v2 JSON snapshot
/// codec (cypherlite-model/src/serde_value.rs, `cypher_value_to_json`)
/// encodes a persistence value by "rejecting values that cannot round-trip":
/// a non-finite float (`NaN`, `±Infinity`) — scalar, nested in a list/map, a
/// point coordinate, or a float vector coordinate — fails the checkpoint with
/// `expected finite number` rather than silently publishing bytes the strict
/// decoder would refuse. That is ADR-0087's fail-closed rationale: the writer
/// refuses a value it cannot round-trip instead of substituting `null`. The
/// same JSON path also cannot represent a `Uuid` *as* a `Uuid`:
/// `value_to_cypher` crosses it as its canonical hyphenated string (the
/// documented CIP-246 deviation — `CypherValue` has no UUID variant), so a
/// checkpointed `Uuid` reloads as a `String`. The Parquet codec stores
/// properties in the engine's tagged encoding and the WAL codec round-trips
/// them the same way, so both keep `NaN`/`±Infinity`/`Uuid` exactly; the
/// narrowing is the JSON codec's contract, not an engine bug, and the fix
/// belongs in this harness, not in the codec.
///
/// Codec detection: integration tests cannot `#[cfg]` on a dependency's
/// `parquet` feature, so the active codec is read through the public
/// [`SnapshotCodecKind::default`] API — the codec every fresh store the
/// recovery suite opens ([`cypherlite_storage::memory_store`] /
/// [`cypherlite_storage::parse_uri`]) uses: Parquet when it is compiled in
/// (ADR-0105), JSON otherwise.
///
/// Under Parquet this is the identity: the full alphabet — `NaN`, `±Infinity`,
/// `-0.0`, `Uuid` — is retained. Under the JSON codec each non-finite float is
/// rewritten to a finite stand-in (`NaN` → `0.0`, `+Infinity` → `f64::MAX`,
/// `-Infinity` → `f64::MIN`) and each `Uuid` to its canonical hyphenated
/// string — exactly the form the codec would persist it as — before the op is
/// persisted, so the properties still checkpoint every generated graph shape;
/// `-0.0` is finite and JSON-representable, so it stays in every build.
/// Rewriting rather than `prop_filter`-rejecting keeps the case count
/// deterministic: three of the five [`float_value`] branches are non-finite,
/// so rejection sampling would discard most generated sequences.
///
/// The `RedoOp` match is exhaustive with **no catch-all**, like
/// [`ModelGraph::appliable_op`]: a new variant is a compile error here until
/// its property-payload policy is written.
///
/// Used by the recovery suite's two checkpoint-bearing properties
/// (`checkpoint_reload_equivalence`, `wal_checkpoint_joint_replay`); the
/// fsync-prefix property truncates the WAL without checkpointing and keeps
/// the full alphabet in every build. Dead in the WAL and state-machine
/// suites, hence the targeted `allow`.
#[allow(dead_code)]
pub fn checkpoint_persistable_ops(ops: Vec<RedoOp>) -> Vec<RedoOp> {
    /// The finite stand-in for a float the JSON snapshot codec cannot
    /// round-trip; finite values (including `-0.0`) pass through unchanged.
    fn persistable_f64(f: f64) -> f64 {
        if f.is_nan() {
            0.0
        } else if f == f64::INFINITY {
            f64::MAX
        } else if f == f64::NEG_INFINITY {
            f64::MIN
        } else {
            f
        }
    }

    /// Rewrite every value the JSON snapshot codec cannot persist as-is:
    /// non-finite floats become finite stand-ins and `Uuid`s become their
    /// canonical hyphenated strings (the exact form the codec persists),
    /// recursing through lists, maps, and vector elements — the positions
    /// serde_value.rs's `finite_number()` and the `value_to_cypher` bridge
    /// act on.
    fn persistable_value(value: &mut Value) {
        match value {
            Value::Float(f) => *f = persistable_f64(*f),
            Value::Uuid(u) => *value = Value::String(uuid_hyphenated(*u).into()),
            Value::Point(p) => {
                p.x = persistable_f64(p.x);
                p.y = persistable_f64(p.y);
                if let Some(z) = &mut p.z {
                    *z = persistable_f64(*z);
                }
            }
            Value::Vector { values, .. } | Value::List(values) => {
                let mut owned = Arc::unwrap_or_clone(std::mem::take(values));
                owned.iter_mut().for_each(persistable_value);
                *values = owned.into();
            }
            Value::Map(entries) => {
                let mut owned = Arc::unwrap_or_clone(std::mem::take(entries));
                owned.values_mut().for_each(persistable_value);
                *entries = owned.into();
            }
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::String(_)
            | Value::Temporal(_) => {}
            // Graph entities are query results, never property payloads (see
            // the module docs), and byte arrays are statement-scoped
            // (ADR-0327), so the alphabet never carries them.
            Value::Node(_) | Value::Relationship(_) | Value::Path(_) | Value::Bytes(_) => {}
        }
    }

    /// Rewrite the property payloads of `op`; ops without one are no-ops.
    fn persistable_op(op: &mut RedoOp) {
        match op {
            RedoOp::CreateNode { properties, .. } | RedoOp::CreateRel { properties, .. } => {
                std::sync::Arc::make_mut(properties)
                    .values_mut()
                    .for_each(persistable_value);
            }
            RedoOp::SetNodeProp { value, .. } | RedoOp::SetRelProp { value, .. } => {
                persistable_value(value);
            }
            RedoOp::DeleteNode { .. }
            | RedoOp::DeleteRel { .. }
            | RedoOp::RemoveNodeProp { .. }
            | RedoOp::RemoveRelProp { .. }
            | RedoOp::AddLabels { .. }
            | RedoOp::RemoveLabels { .. }
            | RedoOp::CreateIndex { .. }
            | RedoOp::DropIndex { .. }
            | RedoOp::CreateConstraint { .. }
            | RedoOp::DropConstraint { .. }
            | RedoOp::CatalogIndexDetail { .. }
            | RedoOp::CatalogDerivedConstraint { .. }
            | RedoOp::PutDatabase { .. }
            | RedoOp::DropDatabase { .. }
            | RedoOp::PutGraphType { .. }
            | RedoOp::SetCurrentGraphType { .. }
            | RedoOp::PullIdentityPut { .. }
            | RedoOp::OntologySet { .. }
            | RedoOp::OntologyDrop { .. } => {}
        }
    }

    if SnapshotCodecKind::default() != SnapshotCodecKind::Json {
        return ops;
    }
    let mut ops = ops;
    for op in &mut ops {
        persistable_op(op);
    }
    ops
}

/// A single [`RedoOp`] from the graph-mutation subset of the WAL alphabet
/// (see the [module docs](self)). Building block of [`redo_op_sequence`], and
/// the value [`ModelGraph::appliable_op`] matches exhaustively.
pub fn redo_op() -> impl Strategy<Value = RedoOp> {
    prop_oneof![
        (node_id(), labels(), properties()).prop_map(|(id, labels, properties)| {
            RedoOp::CreateNode {
                id,
                labels: labels.into(),
                properties: cypherlite_core::PropertyMap::share(properties),
            }
        }),
        (rel_id(), node_id(), node_id(), rel_type(), properties()).prop_map(
            |(id, start, end, rel_type, properties)| RedoOp::CreateRel {
                id,
                start,
                end,
                rel_type,
                properties: cypherlite_core::PropertyMap::share(properties),
            }
        ),
        node_id().prop_map(|id| RedoOp::DeleteNode { id }),
        rel_id().prop_map(|id| RedoOp::DeleteRel { id }),
        (node_id(), key(), property_value()).prop_map(|(id, key, value)| RedoOp::SetNodeProp {
            id,
            key,
            value,
        }),
        (node_id(), key()).prop_map(|(id, key)| RedoOp::RemoveNodeProp { id, key }),
        (rel_id(), key(), property_value()).prop_map(|(id, key, value)| RedoOp::SetRelProp {
            id,
            key,
            value,
        }),
        (rel_id(), key()).prop_map(|(id, key)| RedoOp::RemoveRelProp { id, key }),
        (node_id(), labels()).prop_map(|(id, labels)| RedoOp::AddLabels { id, labels }),
        (node_id(), labels()).prop_map(|(id, labels)| RedoOp::RemoveLabels { id, labels }),
    ]
}

/// A [`cypherlite_core::Value`] of any kind a WAL op can carry (see the
/// [module docs](self)): scalars, tagged floats, strings, UUIDs, temporal and
/// point values, vectors, and nested lists/maps.
pub fn property_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        float_value(),
        "[a-zA-Z0-9_ ]{0,8}".prop_map(|s| Value::String(s.into())),
        any::<u128>().prop_map(Value::Uuid),
    ];
    leaf.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(|xs| Value::List(xs.into())),
            prop::collection::btree_map(key(), inner.clone(), 0..4)
                .prop_map(|m| Value::Map(m.into())),
            vector_value(),
            temporal_value(),
            point_value(),
        ]
    })
}

/// A property map as `RedoOp`'s `Create*`/`Set*` fields carry it.
fn properties() -> impl Strategy<Value = BTreeMap<String, Value>> {
    prop::collection::btree_map(key(), property_value(), 0..4)
}

/// A WAL node id: small and non-negative, matching what the engine allocates.
fn node_id() -> impl Strategy<Value = cypherlite_core::NodeId> {
    0i64..8
}

/// A WAL relationship id: small and non-negative.
fn rel_id() -> impl Strategy<Value = cypherlite_core::RelId> {
    0i64..8
}

/// A property key / map key.
fn key() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,4}".prop_map(String::from)
}

/// A node label / relationship type.
fn labels() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[A-Z][A-Za-z0-9_]{0,4}".prop_map(String::from), 0..3)
}

/// A relationship type name.
fn rel_type() -> impl Strategy<Value = String> {
    "[A-Z][A-Z0-9_]{0,4}".prop_map(String::from)
}

/// A float `Value`, including the tagged special encodings (`NaN`, `±Infinity`,
/// `-0.0`) that the WAL codec renders as strings / signed zero.
fn float_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<f64>(),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-0.0),
    ]
    .prop_map(Value::Float)
}

/// A `Temporal` `Value` built through the public `parse_iso` entry point, over
/// one fixed valid sample per kind.
fn temporal_value() -> impl Strategy<Value = Value> {
    const SAMPLES: &[(&str, &str)] = &[
        ("date", "2020-01-02"),
        ("datetime", "2020-01-02T03:04:05Z"),
        ("localdatetime", "2020-01-02T03:04:05"),
        ("time", "03:04:05Z"),
        ("localtime", "03:04:05"),
        ("duration", "P1Y2M3DT4H5M6S"),
    ];
    let values: Vec<Value> = SAMPLES
        .iter()
        .map(|(kind, text)| Value::Temporal(parse_iso(kind, text).expect("temporal sample parses")))
        .collect();
    prop::sample::select(values)
}

/// A `Point` `Value` in 2D or 3D Cartesian form.
fn point_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        (any::<f64>(), any::<f64>())
            .prop_map(|(x, y)| Value::Point(PointValue::cartesian_2d(x, y))),
        (any::<f64>(), any::<f64>(), any::<f64>())
            .prop_map(|(x, y, z)| Value::Point(PointValue::cartesian_3d(x, y, z))),
    ]
}

/// A `Vector` `Value`: an integer-coordinate vector of ints or a
/// float-coordinate vector of floats, matching the CIP-200 homogeneity rule.
fn vector_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        prop::collection::vec(any::<i64>().prop_map(Value::Int), 1..4).prop_map(|values| {
            Value::Vector {
                coord: VectorCoordKind::Int64,
                values: values.into(),
            }
        }),
        prop::collection::vec(any::<f64>().prop_map(Value::Float), 1..4).prop_map(|values| {
            Value::Vector {
                coord: VectorCoordKind::Float64,
                values: values.into(),
            }
        }),
    ]
}
