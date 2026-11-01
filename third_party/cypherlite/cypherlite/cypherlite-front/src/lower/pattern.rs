use super::expr::*;
use super::*;

pub(super) fn lower_paths(paths: &[cl::PatternPath]) -> Result<Vec<mdb::PathPattern>> {
    paths.iter().map(lower_path).collect()
}

pub(super) fn lower_path(p: &cl::PatternPath) -> Result<mdb::PathPattern> {
    let elements = lower_pattern_elements(&p.elements)?;
    // A non-compact path (a quantified path part or a broken
    // alternation) lowers to empty `nodes`/`rels` projections: the
    // planner matches it through `elements` instead.
    let (nodes, rels) = compact_projections(&elements).unwrap_or_default();
    Ok(mdb::PathPattern {
        variable: p.variable.clone(),
        elements,
        nodes,
        rels,
        where_clause: lower_boxed(p.where_expr.as_ref(), lower_expr)?,
        mode: p.mode.map(|mode| match mode {
            cl::PathMode::Walk => mdb::PathMode::Walk,
            cl::PathMode::Trail => mdb::PathMode::Trail,
            cl::PathMode::Acyclic => mdb::PathMode::Acyclic,
            cl::PathMode::Simple => mdb::PathMode::Simple,
        }),
        selector: p.selector.as_ref().map(lower_path_selector),
        is_shortest_path_function: p.is_shortest_path_function,
    })
}

/// The `nodes`/`rels` split of a compact path — a plain node/relationship
/// alternation (nodes at even positions, relationships at odd ones) with
/// no quantified path part. `None` for a non-compact path.
fn compact_projections(
    elements: &[mdb::PatternElement],
) -> Option<(Vec<mdb::NodePattern>, Vec<mdb::RelPattern>)> {
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    for (i, el) in elements.iter().enumerate() {
        match el {
            mdb::PatternElement::Node(n) if i % 2 == 0 => nodes.push(n.clone()),
            mdb::PatternElement::Relationship(r) if i % 2 == 1 => rels.push(r.clone()),
            _ => return None,
        }
    }
    Some((nodes, rels))
}

pub(super) fn lower_pattern_elements(
    items: &[cl::PatternElement],
) -> Result<Vec<mdb::PatternElement>> {
    items
        .iter()
        .map(|item| {
            Ok(match item {
                cl::PatternElement::Node(node) => mdb::PatternElement::Node(lower_node(node)?),
                cl::PatternElement::Relationship(rel) => {
                    mdb::PatternElement::Relationship(lower_rel(rel)?)
                }
                cl::PatternElement::Quantified(qpp) => {
                    mdb::PatternElement::Quantified(mdb::QuantifiedPathPattern {
                        subpath_variable: qpp.subpath_variable.clone(),
                        elements: lower_pattern_elements(&qpp.elements)?,
                        where_clause: qpp.where_expr.as_ref().map(lower_expr).transpose()?,
                        min: qpp.quantifier.min,
                        max: qpp.quantifier.max,
                    })
                }
            })
        })
        .collect()
}

pub(super) fn lower_path_count(count: &cl::PathCount) -> mdb::PathCount {
    match count {
        cl::PathCount::Literal(value) => mdb::PathCount::Literal(*value),
        cl::PathCount::Parameter(name) => mdb::PathCount::Parameter(name.clone()),
    }
}

pub(super) fn lower_path_selector(selector: &cl::PathSelector) -> mdb::PathSelector {
    match selector {
        cl::PathSelector::All => mdb::PathSelector::All,
        cl::PathSelector::Any(count) => {
            mdb::PathSelector::Any(count.as_ref().map(lower_path_count))
        }
        cl::PathSelector::AnyShortest => mdb::PathSelector::AnyShortest,
        cl::PathSelector::AllShortest => mdb::PathSelector::AllShortest,
        cl::PathSelector::Shortest { count, group } => mdb::PathSelector::Shortest {
            count: count.as_ref().map(lower_path_count),
            group: *group,
        },
    }
}

pub(super) fn lower_node(n: &cl::NodePattern) -> Result<mdb::NodePattern> {
    let labels = match &n.label_expr {
        None => Vec::new(),
        Some(le) => le.as_label_list().unwrap_or_default(),
    };
    let properties = if let Some(parameter) = &n.properties_param {
        Some(lower_expr(parameter)?)
    } else if n.has_properties {
        Some(mdb::Expr::Map(
            n.properties
                .iter()
                .map(|(k, v)| Ok((k.clone(), lower_expr(v)?)))
                .collect::<Result<Vec<_>>>()?,
        ))
    } else {
        None
    };
    Ok(mdb::NodePattern {
        variable: n.variable.clone(),
        labels,
        label_expr: n.label_expr.as_ref().map(lower_label_expr).transpose()?,
        properties,
        where_clause: n.where_expr.as_ref().map(lower_expr).transpose()?,
    })
}

pub(super) fn lower_rel(r: &cl::RelPattern) -> Result<mdb::RelPattern> {
    let types = match &r.type_expr {
        None => Vec::new(),
        Some(cl::LabelExpr::Label(t)) => vec![t.clone()],
        Some(cl::LabelExpr::Or(items)) => items
            .iter()
            .map_while(|i| match i {
                cl::LabelExpr::Label(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|_| items.iter().all(|i| matches!(i, cl::LabelExpr::Label(_))))
            .collect(),
        Some(_) => Vec::new(),
    };
    let properties = if let Some(parameter) = &r.properties_param {
        Some(lower_expr(parameter)?)
    } else if !r.properties.is_empty() {
        Some(mdb::Expr::Map(
            r.properties
                .iter()
                .map(|(k, v)| Ok((k.clone(), lower_expr(v)?)))
                .collect::<Result<Vec<_>>>()?,
        ))
    } else {
        None
    };
    let var_length = r.min_hops.is_some() || r.max_hops.is_some();
    let max_hops = if var_length { r.max_hops } else { Some(1) };
    Ok(mdb::RelPattern {
        variable: r.variable.clone(),
        types,
        type_expr: r.type_expr.as_ref().map(lower_label_expr).transpose()?,
        direction: match r.direction {
            cl::Direction::Right => mdb::Direction::Outgoing,
            cl::Direction::Left => mdb::Direction::Incoming,
            cl::Direction::Both => mdb::Direction::Both,
        },
        properties,
        var_length,
        min_hops: r.min_hops.unwrap_or(1),
        max_hops,
        where_clause: r.where_expr.as_ref().map(lower_expr).transpose()?,
        quantified: r.quantified,
    })
}
