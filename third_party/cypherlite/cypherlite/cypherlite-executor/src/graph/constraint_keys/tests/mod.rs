//! ADR-0439 fixture helpers and the two sub-suites, mirroring the module's
//! own split: [`seams`] pins the committed map and its charge (install,
//! free, rebuild, graph-type re-derivation, and the entry-growth estimate),
//! [`admission`] pins the commit-loop probe family (the per-slot overlay and
//! its rejections).

use std::collections::BTreeMap;

use cypherlite_core::{PropertyMap, Value};
use cypherlite_model::ConstraintType;

use crate::graph::Graph;
use crate::group_key::GroupKey;
use crate::redo::RedoOp;

mod admission;
mod seams;

/// The one property-map builder: any key/value pairs, owned.
pub(super) fn props(pairs: impl IntoIterator<Item = (impl Into<String>, Value)>) -> PropertyMap {
    pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect::<BTreeMap<_, _>>()
        .into()
}

pub(super) fn create_node(id: i64, label: &str, pairs: &[(&str, Value)]) -> RedoOp {
    RedoOp::create_node(
        id,
        vec![label.to_string()],
        props(pairs.iter().map(|(key, value)| (*key, value.clone()))),
    )
}

pub(super) fn create_rel(
    id: i64,
    start: i64,
    end: i64,
    rel_type: &str,
    pairs: &[(&str, Value)],
) -> RedoOp {
    RedoOp::create_rel(
        id,
        start,
        end,
        rel_type.to_string(),
        props(pairs.iter().map(|(key, value)| (*key, value.clone()))),
    )
}

/// The one constraint builder: `relationship` picks the entity side, the
/// target names the label or relationship type, and the kind is the
/// canonical uniqueness spelling the catalog's closed vocabulary uses.
pub(super) fn constraint(
    name: &str,
    relationship: bool,
    target: &str,
    properties: &[&str],
) -> RedoOp {
    RedoOp::CreateConstraint {
        name: name.to_string(),
        relationship,
        label: target.to_string(),
        properties: properties.iter().map(|p| (*p).to_string()).collect(),
        kind: ConstraintType::Unique.as_str().to_string(),
        property_type: None,
        enforced_label: None,
    }
}

pub(super) fn unique(name: &str, label: &str, properties: &[&str]) -> RedoOp {
    constraint(name, false, label, properties)
}

pub(super) fn unique_rel(name: &str, rel_type: &str, properties: &[&str]) -> RedoOp {
    constraint(name, true, rel_type, properties)
}

/// The one tuple builder: any owned values, in constraint property order.
pub(super) fn tuple(values: impl IntoIterator<Item = Value>) -> Box<[GroupKey]> {
    values
        .into_iter()
        .map(|value| GroupKey::for_property_index(&value).expect("keyable value"))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

pub(super) fn seeded() -> Graph {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(0, "P", &[("email", Value::Int(1))]),
                create_node(1, "P", &[("email", Value::Int(2))]),
                create_node(2, "Q", &[("email", Value::Int(3))]),
            ],
            1,
        )
        .unwrap();
    graph
}

/// A keyable value whose `GroupKey` logical size is exactly the estimate's
/// per-component floor (`COMPONENT_ESTIMATE_BYTES`): `norm_bytes(Str)` is
/// `8 + 16 + len`, so eight bytes of payload make one component cost 32
/// bytes both when charged and when estimated. With single-holder tuples the
/// estimate is therefore exact, not merely conservative.
pub(super) fn key(n: i64) -> Value {
    Value::String(format!("key{n:05}").into())
}

/// Two committed `:P` nodes with distinct keyable emails — the estimate
/// suite's base graph.
pub(super) fn seeded_two_p_emails() -> Graph {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(0, "P", &[("email", key(1))]),
                create_node(1, "P", &[("email", key(2))]),
            ],
            1,
        )
        .unwrap();
    graph
}

/// [`seeded`] plus the `:P.email` uniqueness constraint installed at commit
/// 2 — the constrained-map fixture the seam and admission suites share.
pub(super) fn seeded_constrained() -> Graph {
    let graph = seeded();
    graph
        .apply_redo(vec![unique("c", "P", &["email"])], 2)
        .unwrap();
    graph
}
