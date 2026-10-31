//! `CREATE`/`MERGE` pattern materialization and property sanitization.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use cypherlite_core::{Error, NodeId, Path, RelId, Result, Value};
use cypherlite_ir::{
    Direction, DynamicLabelQuantifier, Expr, LabelExpr, NodePattern, PathPattern, RelPattern,
};

use crate::eval::{dynamic_label_names, eval, Row};
use crate::overlay::ExecGraph;

use super::props::sanitize_props;
use super::pushdown::anon_name;

pub(crate) fn create_patterns(
    graph: &ExecGraph<'_>,
    row: &mut Row,
    patterns: &[PathPattern],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<()> {
    for p in patterns {
        let mut node_ids: Vec<NodeId> = Vec::new();
        for np in &p.nodes {
            let id = create_or_bind_node(graph, row, np, params, cypher_version)?;
            node_ids.push(id);
        }
        let mut rel_ids = Vec::new();
        for (i, rp) in p.rels.iter().enumerate() {
            let start = node_ids[i];
            let end = node_ids[i + 1];
            rel_ids.push(create_rel(
                graph,
                row,
                rp,
                start,
                end,
                params,
                cypher_version,
            )?);
        }
        if let Some(name) = &p.variable {
            let nodes: Vec<_> = node_ids.iter().filter_map(|id| graph.node(*id)).collect();
            let rels: Vec<_> = rel_ids.iter().filter_map(|id| graph.rel(*id)).collect();
            row.insert(name.clone(), Value::from_path(Path { nodes, rels }));
        }
    }
    Ok(())
}

pub(crate) fn resolve_create_patterns(
    graph: &ExecGraph<'_>,
    row: &Row,
    patterns: &[PathPattern],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<PathPattern>> {
    let mut patterns = patterns.to_vec();
    for path in &mut patterns {
        for node in &mut path.nodes {
            if let Some(properties) = &node.properties {
                node.properties = Some(Expr::RuntimeValue(eval(
                    graph,
                    row,
                    properties,
                    params,
                    cypher_version,
                )?));
            }
            if let Some(labels) = &node.label_expr {
                let labels =
                    resolve_creation_labels(graph, row, params, Some(labels), cypher_version)?;
                node.labels = labels.clone();
                node.label_expr = Some(LabelExpr::And(
                    labels.into_iter().map(LabelExpr::Label).collect(),
                ));
            }
        }
        for rel in &mut path.rels {
            if let Some(properties) = &rel.properties {
                rel.properties = Some(Expr::RuntimeValue(eval(
                    graph,
                    row,
                    properties,
                    params,
                    cypher_version,
                )?));
            }
            if let Some(types) = &rel.type_expr {
                let types =
                    resolve_creation_labels(graph, row, params, Some(types), cypher_version)?;
                rel.types = types.clone();
                rel.type_expr = Some(LabelExpr::And(
                    types.into_iter().map(LabelExpr::Label).collect(),
                ));
            }
        }
    }
    Ok(patterns)
}

pub(crate) fn patterns_have_graph_sensitive_expr(patterns: &[PathPattern]) -> bool {
    patterns.iter().any(|path| {
        path.nodes
            .iter()
            .any(|node| node.properties.as_ref().is_some_and(expr_graph_sensitive))
            || path
                .rels
                .iter()
                .any(|rel| rel.properties.as_ref().is_some_and(expr_graph_sensitive))
    })
}

pub(crate) fn expr_graph_sensitive(expr: &Expr) -> bool {
    let mut stack = vec![expr];
    while let Some(expr) = stack.pop() {
        if matches!(
            expr,
            Expr::ExistsSubquery { .. }
                | Expr::CountSubquery { .. }
                | Expr::CollectSubquery { .. }
                | Expr::PatternComp { .. }
                | Expr::PatternPred { .. }
                | Expr::ShortestPath { .. }
        ) {
            return true;
        }
        stack.extend(expr.children());
    }
    false
}

pub(crate) fn resolve_creation_labels(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    expr: Option<&LabelExpr>,
    cypher_version: Option<u32>,
) -> Result<Vec<String>> {
    fn append(
        graph: &ExecGraph<'_>,
        row: &Row,
        params: &BTreeMap<String, Value>,
        expr: &LabelExpr,
        out: &mut Vec<String>,
        cypher_version: Option<u32>,
    ) -> Result<()> {
        match expr {
            LabelExpr::Label(name) => out.push(name.clone()),
            LabelExpr::And(items) => {
                for item in items {
                    append(graph, row, params, item, out, cypher_version)?;
                }
            }
            LabelExpr::Dynamic { expr, quantifier } => {
                if *quantifier == DynamicLabelQuantifier::Any {
                    return Err(Error::semantic(
                        "InvalidLabelExpression: $any() is not allowed in CREATE/MERGE",
                    ));
                }
                out.extend(dynamic_label_names(
                    graph,
                    row,
                    params,
                    expr,
                    cypher_version,
                )?);
            }
            _ => {
                return Err(Error::semantic(
                    "InvalidLabelExpression: CREATE/MERGE only supports conjunctions",
                ))
            }
        }
        Ok(())
    }
    let mut labels = Vec::new();
    if let Some(expr) = expr {
        append(graph, row, params, expr, &mut labels, cypher_version)?;
    }
    let mut seen = HashSet::new();
    labels.retain(|label| seen.insert(label.clone()));
    Ok(labels)
}

pub(crate) fn map_from_expr(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<BTreeMap<String, Value>> {
    match eval(graph, row, expr, params, cypher_version)? {
        Value::Map(m) => sanitize_props(Arc::unwrap_or_clone(m)),
        Value::Null => Ok(BTreeMap::new()),
        _ => Err(Error::ty("pattern properties must be a map")),
    }
}

pub(crate) fn reject_null_merge_props(
    graph: &ExecGraph<'_>,
    row: &Row,
    pattern: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<()> {
    let maps = pattern
        .nodes
        .iter()
        .filter_map(|n| n.properties.as_ref())
        .chain(pattern.rels.iter().filter_map(|r| r.properties.as_ref()));
    for expr in maps {
        match eval(graph, row, expr, params, cypher_version)? {
            Value::Map(m) if m.values().any(Value::is_null) => {
                return Err(Error::execution(
                    "MergeReadOwnWrites: cannot MERGE with null properties",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn create_or_bind_node(
    graph: &ExecGraph<'_>,
    row: &mut Row,
    np: &NodePattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<NodeId> {
    let labels =
        resolve_creation_labels(graph, row, params, np.label_expr.as_ref(), cypher_version)?;
    let props = match &np.properties {
        Some(e) => map_from_expr(graph, row, e, params, cypher_version)?,
        None => BTreeMap::new(),
    };
    if let Some(v) = &np.variable {
        if let Some(Value::Node(n)) = row.get(v) {
            let id = n.id;
            graph.add_labels(id, &labels);
            graph.merge_properties_node(id, &props);
            row.insert(v.clone(), graph.node_value(id));
            return Ok(id);
        }
    }
    let id = graph.create_node(labels, props)?;
    let name = np.variable.clone().unwrap_or_else(anon_name);
    row.insert(name, graph.node_value(id));
    Ok(id)
}

pub(crate) fn create_rel(
    graph: &ExecGraph<'_>,
    row: &mut Row,
    rp: &RelPattern,
    start: NodeId,
    end: NodeId,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<RelId> {
    let (s, e) = match rp.direction {
        Direction::Outgoing | Direction::Both => (start, end),
        Direction::Incoming => (end, start),
    };
    let types = resolve_creation_labels(graph, row, params, rp.type_expr.as_ref(), cypher_version)?;
    if types.len() != 1 {
        return Err(Error::syntax("CREATE relationship requires a type"));
    }
    let rel_type = types[0].clone();
    let props = match &rp.properties {
        Some(ex) => map_from_expr(graph, row, ex, params, cypher_version)?,
        None => BTreeMap::new(),
    };
    let id = graph.create_rel(s, e, rel_type, props)?;
    if let Some(v) = &rp.variable {
        row.insert(v.clone(), graph.rel_value(id));
    }
    Ok(id)
}
