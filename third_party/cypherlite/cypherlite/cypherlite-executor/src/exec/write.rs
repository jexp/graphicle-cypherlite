//! `SET`/`REMOVE`/`DELETE` write clauses.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, NodeId, RelId, Result, Value};
use cypherlite_ir::{Direction, Expr, LabelsFunctionOp, RemoveItem, SetItem};

use crate::eval::{dynamic_label_names, dynamic_property_key, eval, resolve_write_labels, Row};
use crate::overlay::ExecGraph;

use super::props::{check_property_value, sanitize_props};

/// CIP-241: SET resolves every RHS value, dynamic key, label list, and
/// target against the *pre-clause* state first, then applies the
/// assignments in order. A dynamic target must not observe an earlier
/// assignment in the same SET clause (verified on Neo4j 2026.09.0:
/// `SET n['x']=n.y, n['y']=n.x` on {x:0, y:1} yields x=1, y=0).
pub(crate) fn apply_sets(
    graph: &ExecGraph<'_>,
    row: &mut Row,
    items: &[SetItem],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<()> {
    let resolved = resolve_sets(graph, row, items, params, cypher_version)?;
    apply_resolved_sets(graph, resolved)
}

/// One resolved assignment's entity-kind dispatch, spelled once for the
/// four property-shaped arms of [`apply_resolved_sets`] and
/// [`apply_resolved_removes`]: a node or relationship runs its verb on
/// the moved payload, a null target is a no-op (Cypher's null-tolerant
/// SET/REMOVE, payload dropped), anything else fails with the arm's own
/// type error.
fn apply_to_entity<T>(
    entity: Value,
    err: &'static str,
    payload: T,
    on_node: impl FnOnce(NodeId, T),
    on_rel: impl FnOnce(RelId, T),
) -> Result<()> {
    match entity {
        Value::Node(n) => {
            on_node(n.id, payload);
            Ok(())
        }
        Value::Relationship(r) => {
            on_rel(r.id, payload);
            Ok(())
        }
        Value::Null => Ok(()),
        _ => Err(Error::ty(err)),
    }
}

enum ResolvedSet {
    /// `SET n = <map>` -- full property replacement.
    ReplaceAllProps(Value, BTreeMap<String, Value>),
    /// `SET n.k = v` / `SET n[expr] = v` with the key pre-resolved.
    Prop(Value, String, Value),
    /// `SET n += <map>`.
    MergeMap(Value, BTreeMap<String, Value>),
    /// `SET LABEL(n) = ...` (CIP-162 dynamic labels).
    LabelsFunction(Value, Vec<String>, LabelsFunctionOp),
}

/// CIP-241: resolve a dynamic property target (`expr[key]`) against the
/// pre-clause state. The entity is evaluated first; a null entity
/// short-circuits key evaluation via the empty-string sentinel, so a null
/// target never raises the dynamic-key type error. Shared by SET and REMOVE
/// so both clauses observe identical target semantics.
fn resolve_dynamic_target(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    key: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<(Value, String)> {
    let entity = eval(graph, row, expr, params, cypher_version)?;
    if entity.is_null() {
        Ok((Value::Null, String::new()))
    } else {
        let key = dynamic_property_key(eval(graph, row, key, params, cypher_version)?)?;
        Ok((entity, key))
    }
}

/// An entity value's current properties: refreshed from the graph when the
/// entity is still live there, otherwise the snapshot the value carries.
/// `None` for non-entity values.
fn current_properties(graph: &ExecGraph<'_>, value: &Value) -> Option<BTreeMap<String, Value>> {
    match value {
        Value::Node(n) => Some(match graph.node(n.id) {
            // The live node is freshly built, so its property map is uniquely
            // owned and moves out. A deleted node's snapshot map is shared.
            // `SET n = map` still wants the Cypher map type.
            Some(live) => Arc::unwrap_or_clone(live.properties).into(),
            None => (*n.properties).clone().into(),
        }),
        Value::Relationship(r) => Some(match graph.rel(r.id) {
            Some(live) => Arc::unwrap_or_clone(live.properties).into(),
            None => (*r.properties).clone().into(),
        }),
        _ => None,
    }
}

fn resolve_sets(
    graph: &ExecGraph<'_>,
    row: &Row,
    items: &[SetItem],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<ResolvedSet>> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SetItem::Property { target, value } => {
                let val = eval(graph, row, value, params, cypher_version)?;
                // A bare-variable target is `SET n = <map>`; every other
                // target stores the single value. Null transparency is the
                // callee's contract (ADR-0339), so no per-site null guard.
                if !matches!(target, Expr::Variable(_)) {
                    check_property_value(&val)?;
                }
                match target {
                    Expr::Variable(_) => {
                        let map = match val {
                            Value::Map(m) => sanitize_props(Arc::unwrap_or_clone(m))?,
                            Value::Null => BTreeMap::new(),
                            v => match current_properties(graph, &v) {
                                Some(props) => sanitize_props(props)?,
                                None => return Err(Error::ty("SET all properties requires a map")),
                            },
                        };
                        resolved.push(ResolvedSet::ReplaceAllProps(
                            eval(graph, row, target, params, cypher_version)?,
                            map,
                        ));
                    }
                    Expr::Property { expr, key } => {
                        let entity = eval(graph, row, expr, params, cypher_version)?;
                        resolved.push(ResolvedSet::Prop(entity, key.clone(), val));
                    }
                    Expr::DynamicProperty { expr, key } => {
                        let (entity, key) =
                            resolve_dynamic_target(graph, row, expr, key, params, cypher_version)?;
                        resolved.push(ResolvedSet::Prop(entity, key, val));
                    }
                    _ => return Err(Error::ty("SET expected property")),
                }
            }
            SetItem::MergeMap { target, value } => {
                let val = eval(graph, row, value, params, cypher_version)?;
                let map = match val {
                    Value::Map(m) => Arc::unwrap_or_clone(m),
                    v => match current_properties(graph, &v) {
                        Some(props) => props,
                        None => return Err(Error::ty("+= requires a map")),
                    },
                };
                for v in map.values() {
                    check_property_value(v)?;
                }
                resolved.push(ResolvedSet::MergeMap(
                    eval(graph, row, target, params, cypher_version)?,
                    map,
                ));
            }
            SetItem::Labels { target, labels } => {
                let labels = resolve_write_labels(graph, row, params, labels, cypher_version)?;
                // `SET n :Label` is exactly LabelsFunction(Extend) — one
                // apply arm instead of two (round-3 review).
                resolved.push(ResolvedSet::LabelsFunction(
                    eval(graph, row, target, params, cypher_version)?,
                    labels,
                    LabelsFunctionOp::Extend,
                ));
            }
            SetItem::LabelsFunction { target, value, op } => {
                let labels = dynamic_label_names(graph, row, params, value, cypher_version)?;
                resolved.push(ResolvedSet::LabelsFunction(
                    eval(graph, row, target, params, cypher_version)?,
                    labels,
                    *op,
                ));
            }
        }
    }
    Ok(resolved)
}

fn apply_resolved_sets(graph: &ExecGraph<'_>, resolved: Vec<ResolvedSet>) -> Result<()> {
    for item in resolved {
        match item {
            ResolvedSet::ReplaceAllProps(entity, map) => apply_to_entity(
                entity,
                "SET map target must be a node or relationship",
                map,
                |id, m| graph.replace_properties_node(id, m),
                |id, m| graph.replace_properties_rel(id, m),
            )?,
            ResolvedSet::Prop(entity, key, val) => apply_to_entity(
                entity,
                "SET property target invalid",
                (key, val),
                |id, (k, v)| graph.set_property_node(id, &k, v),
                |id, (k, v)| graph.set_property_rel(id, &k, v),
            )?,
            ResolvedSet::MergeMap(entity, map) => apply_to_entity(
                entity,
                "+= target must be a node or relationship",
                &map,
                |id, m| graph.merge_properties_node(id, m),
                |id, m| graph.merge_properties_rel(id, m),
            )?,
            ResolvedSet::LabelsFunction(entity, labels, op) => match entity {
                Value::Node(n) if n.id < 0 => return Err(Error::execution("Unable to load NODE")),
                Value::Node(n) => match op {
                    LabelsFunctionOp::Extend => graph.add_labels(n.id, &labels),
                    LabelsFunctionOp::Remove => graph.remove_labels(n.id, &labels),
                    LabelsFunctionOp::Overwrite => {
                        let current = graph.node_labels(n.id).unwrap_or_default();
                        let remove: Vec<_> = current
                            .into_iter()
                            .filter(|label| !labels.contains(label))
                            .collect();
                        graph.remove_labels(n.id, &remove);
                        graph.add_labels(n.id, &labels);
                    }
                },
                Value::Null => {}
                _ => return Err(Error::ty("SET labels() requires a node")),
            },
        }
    }
    Ok(())
}

pub(crate) enum ResolvedRemove {
    Property(Value, String),
    Labels(Value, Vec<String>),
}

pub(crate) fn resolve_removes(
    graph: &ExecGraph<'_>,
    row: &Row,
    items: &[RemoveItem],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<ResolvedRemove>> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        match item {
            RemoveItem::Property { target } => {
                let (entity, key) = match target {
                    Expr::Property { expr, key } => {
                        (eval(graph, row, expr, params, cypher_version)?, key.clone())
                    }
                    Expr::DynamicProperty { expr, key } => {
                        resolve_dynamic_target(graph, row, expr, key, params, cypher_version)?
                    }
                    _ => return Err(Error::ty("REMOVE expected property")),
                };
                resolved.push(ResolvedRemove::Property(entity, key));
            }
            RemoveItem::Labels { target, labels } => {
                let labels = resolve_write_labels(graph, row, params, labels, cypher_version)?;
                resolved.push(ResolvedRemove::Labels(
                    eval(graph, row, target, params, cypher_version)?,
                    labels,
                ));
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn apply_resolved_removes(
    graph: &ExecGraph<'_>,
    resolved: Vec<ResolvedRemove>,
) -> Result<()> {
    for item in resolved {
        match item {
            ResolvedRemove::Property(entity, key) => apply_to_entity(
                entity,
                "REMOVE property target invalid",
                key,
                |id, k| graph.set_property_node(id, &k, Value::Null),
                |id, k| graph.set_property_rel(id, &k, Value::Null),
            )?,
            ResolvedRemove::Labels(entity, labels) => match entity {
                Value::Node(n) if n.id < 0 => return Err(Error::execution("Unable to load NODE")),
                Value::Node(n) => graph.remove_labels(n.id, &labels),
                Value::Null => {}
                _ => return Err(Error::ty("REMOVE labels requires a node")),
            },
        }
    }
    Ok(())
}

pub(crate) fn delete_value(graph: &ExecGraph<'_>, v: &Value, detach: bool) -> Result<()> {
    match v {
        Value::Node(n) => graph.delete_node(n.id, detach)?,
        Value::Relationship(r) => graph.delete_rel(r.id),
        Value::Path(p) => {
            for r in p.rels.iter() {
                graph.delete_rel(r.id);
            }
            for n in p.nodes.iter() {
                let incident = graph.neighbors(n.id, Direction::Both);
                if detach || incident.is_empty() {
                    graph.delete_node(n.id, detach)?;
                }
            }
        }
        Value::Null => {}
        Value::List(xs) => {
            for x in xs.iter() {
                delete_value(graph, x, detach)?;
            }
        }
        _ => return Err(Error::ty("DELETE requires nodes, relationships, or paths")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::test_support::exec_write_uncommitted;
    use crate::graph::Graph;

    /// DELETE of a list bound in the row reads nested lists and paths in
    /// place: every entity is deleted once and the row keeps its list.
    #[test]
    fn delete_list_of_nodes_and_paths_keeps_the_row_list() {
        let g = Graph::default();
        let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
        let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
        g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
        g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
        let r = exec_write_uncommitted(
            &g,
            "MATCH p = (:A)-->(:B), (c:C) WITH [c] + [[p], null] AS xs \
             DELETE xs RETURN size(xs) AS n",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
        assert_eq!(r.stats.nodes_deleted, 3);
        assert_eq!(r.stats.relationships_deleted, 1);
    }
}
