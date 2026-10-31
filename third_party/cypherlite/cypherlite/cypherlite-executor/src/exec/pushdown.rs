//! MATCH limit pushdown caps (DISTINCT/ORDERED), one-hop aggregate
//! folding (ADR-0036), residual predicates, and liveness trimming.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use cypherlite_core::{Result, Value};
use cypherlite_ir::{BinaryOp, Clause, Expr, PathPattern, PatternElement, RemoveItem, SetItem};

use crate::eval::{expr_vars, label_expr_vars, Row};
use crate::overlay::ExecGraph;

pub(crate) use super::agg_fold::{try_fold_one_hop_aggs, try_fold_path_aggs, FoldedReturn};
pub(crate) use super::match_cap::{match_limit_cap, MatchCap};
use super::match_clause::eval_match_predicate;
use super::scope::{call_scope_reads, has_star_item, is_internal_column, ProjectionFields};

static ANON: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Residual<'a> {
    pub conjuncts: &'a [Expr],
    pub scope: &'a HashSet<String>,
}

/// The degenerate residual — no conjuncts, empty scope: the one spelling
/// of "read with no residual pushdown", used by every caller that reads
/// candidates without a MATCH predicate of its own (the eval walk engine,
/// the agg folds, the exists probe, the shortest-route start).
impl Residual<'_> {
    /// The degenerate case of [`Self::with_conjuncts`]: no conjuncts
    /// either.
    pub(crate) fn empty() -> Residual<'static> {
        Self::with_conjuncts(&[])
    }

    /// Seek material without a scope: conjuncts to consider, no names
    /// bound — the one spelling of that shape, so no caller fabricates a
    /// dead local scope beside its conjuncts.
    pub(crate) fn with_conjuncts(conjuncts: &[Expr]) -> Residual<'_> {
        static EMPTY_SCOPE: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        Residual {
            conjuncts,
            scope: EMPTY_SCOPE.get_or_init(HashSet::new),
        }
    }
}

pub(crate) fn residual_ok(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Result<bool> {
    for c in residual.conjuncts {
        let mut vars = HashSet::new();
        expr_vars(c, &mut vars);
        vars.retain(|v| residual.scope.contains(v));
        if !vars.iter().all(|v| row.contains_key(v)) {
            continue;
        }
        if eval_match_predicate(graph, row, c, params, cypher_version)?.truthy() != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn flatten_and_owned(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut v = flatten_and_owned(left);
            v.extend(flatten_and_owned(right));
            v
        }
        other => vec![other.clone()],
    }
}

/// The variables a projection (`RETURN` / `WITH`) still needs from the
/// carried row: its item expressions, its explicit grouping keys (CIP-236:
/// evaluated *before* projection), its sort keys, its `SKIP`/`LIMIT`, and —
/// for `WITH` — its `WHERE`. One helper for both clause arms, so a new
/// projection field lands in one place.
fn projection_needs(fields: &ProjectionFields<'_>, needed: &mut HashSet<String>) {
    for i in fields.items {
        expr_vars(&i.expr, needed);
    }
    if let Some(keys) = fields.group_by {
        for k in keys {
            expr_vars(k, needed);
        }
    }
    for o in fields.order {
        expr_vars(&o.expr, needed);
    }
    if let Some(s) = fields.skip {
        expr_vars(s, needed);
    }
    if let Some(l) = fields.limit {
        expr_vars(l, needed);
    }
    if let Some(w) = fields.where_clause {
        expr_vars(w, needed);
    }
}

/// A `RETURN`/`WITH` projection's live set: `None` under a `*` item (the
/// star expands the whole row), otherwise the names its fields need.
fn projection_live(
    fields: &ProjectionFields<'_>,
    needed: &mut HashSet<String>,
) -> Option<HashSet<String>> {
    if has_star_item(fields.items) {
        return None;
    }
    projection_needs(fields, needed);
    Some(std::mem::take(needed))
}

pub(crate) fn live_from(suffix: &[Clause]) -> Option<HashSet<String>> {
    let mut needed = HashSet::new();
    for c in suffix {
        match c {
            Clause::Catalog(_) => return None,
            Clause::Return {
                items,
                group_by,
                order,
                skip,
                limit,
                ..
            } => {
                let fields = ProjectionFields {
                    items,
                    group_by,
                    order,
                    skip,
                    limit,
                    where_clause: &None,
                };
                return projection_live(&fields, &mut needed);
            }
            Clause::With {
                items,
                group_by,
                order,
                skip,
                limit,
                where_clause,
                ..
            } => {
                let fields = ProjectionFields {
                    items,
                    group_by,
                    order,
                    skip,
                    limit,
                    where_clause,
                };
                return projection_live(&fields, &mut needed);
            }
            Clause::Match {
                patterns,
                where_clause,
                search,
                ..
            } => {
                add_pattern_names(patterns, &mut needed);
                if let Some(w) = where_clause {
                    expr_vars(w, &mut needed);
                }
                if let Some(search) = search {
                    for expr in search.spec.exprs() {
                        expr_vars(expr, &mut needed);
                    }
                }
            }
            Clause::Create { patterns } => add_pattern_names(patterns, &mut needed),
            Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::ShowIndexes
            | Clause::Finish
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. } => {}
            Clause::LoadCsv { url, .. } => expr_vars(url, &mut needed),
            Clause::CallSubquery {
                in_transactions,
                scope,
                ..
            } => {
                // A whole-row scope keeps everything live; an import list
                // keeps the names it names (the one `call_scope_reads` rule).
                let vars = call_scope_reads(scope)?;
                needed.extend(vars.iter().cloned());
                if let Some(tx) = in_transactions {
                    if let Some(n) = &tx.batch_size {
                        expr_vars(n, &mut needed);
                    }
                }
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                add_pattern_names(std::slice::from_ref(pattern), &mut needed);
                for s in on_create.iter().chain(on_match.iter()) {
                    set_item_vars(s, &mut needed);
                }
            }
            Clause::Delete { expressions, .. } => {
                for e in expressions {
                    expr_vars(e, &mut needed);
                }
            }
            Clause::Set { items } => {
                for i in items {
                    set_item_vars(i, &mut needed);
                }
            }
            Clause::Remove { items } => {
                for i in items {
                    remove_item_vars(i, &mut needed);
                }
            }
            Clause::Unwind { expression, .. } => expr_vars(expression, &mut needed),
            Clause::Call { args, .. } => {
                for a in args {
                    expr_vars(a, &mut needed);
                }
            }
            Clause::Conditional { branches, .. } => {
                for (predicate, _) in branches {
                    expr_vars(predicate, &mut needed);
                }
                return None;
            }
            Clause::Foreach { list, .. } => {
                expr_vars(list, &mut needed);
                return None;
            }
            // A bare USE graph name is a catalog identifier, not a row variable.
            Clause::Use { .. } => {}
        }
    }
    None
}

pub(crate) fn add_pattern_names(patterns: &[PathPattern], out: &mut HashSet<String>) {
    for p in patterns {
        if let Some(v) = &p.variable {
            out.insert(v.clone());
        }
        if let Some(predicate) = p.where_clause.as_deref() {
            expr_vars(predicate, out);
        }
        for n in &p.nodes {
            if let Some(v) = &n.variable {
                out.insert(v.clone());
            }
            if let Some(props) = &n.properties {
                expr_vars(props, out);
            }
            if let Some(expr) = &n.label_expr {
                label_expr_vars(expr, out);
            }
        }
        for r in &p.rels {
            if let Some(v) = &r.variable {
                out.insert(v.clone());
            }
            if let Some(props) = &r.properties {
                expr_vars(props, out);
            }
            if let Some(expr) = &r.type_expr {
                label_expr_vars(expr, out);
            }
        }
        add_gpm_element_names(&p.elements, out);
    }
}

pub(crate) fn add_gpm_element_names(elements: &[PatternElement], out: &mut HashSet<String>) {
    for element in elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(name) = &node.variable {
                    out.insert(name.clone());
                }
                if let Some(predicate) = &node.where_clause {
                    expr_vars(predicate, out);
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(name) = &rel.variable {
                    out.insert(name.clone());
                }
                if let Some(predicate) = &rel.where_clause {
                    expr_vars(predicate, out);
                }
            }
            PatternElement::Quantified(qpp) => {
                if let Some(name) = &qpp.subpath_variable {
                    out.insert(name.clone());
                }
                if let Some(predicate) = &qpp.where_clause {
                    expr_vars(predicate, out);
                }
                add_gpm_element_names(&qpp.elements, out);
            }
        }
    }
}

pub(crate) fn set_item_vars(item: &SetItem, out: &mut HashSet<String>) {
    match item {
        SetItem::Property { target, value } | SetItem::MergeMap { target, value } => {
            expr_vars(target, out);
            expr_vars(value, out);
        }
        SetItem::Labels { target, labels } => {
            expr_vars(target, out);
            for label in labels {
                label_expr_vars(label, out);
            }
        }
        SetItem::LabelsFunction { target, value, .. } => {
            expr_vars(target, out);
            expr_vars(value, out);
        }
    }
}

pub(crate) fn remove_item_vars(item: &RemoveItem, out: &mut HashSet<String>) {
    match item {
        RemoveItem::Property { target } => expr_vars(target, out),
        RemoveItem::Labels { target, labels } => {
            expr_vars(target, out);
            for label in labels {
                label_expr_vars(label, out);
            }
        }
    }
}

pub(crate) fn trim_rows(rows: &mut [Row], live: &HashSet<String>) {
    for row in rows {
        row.retain(|k, _| live.contains(k) || is_internal_column(k));
    }
}

pub(crate) fn match_scope(row: &Row, patterns: &[PathPattern]) -> HashSet<String> {
    let mut s: HashSet<String> = row
        .keys()
        .filter(|k| !is_internal_column(k))
        .cloned()
        .collect();
    for p in patterns {
        if let Some(v) = &p.variable {
            s.insert(v.clone());
        }
        for n in &p.nodes {
            if let Some(v) = &n.variable {
                s.insert(v.clone());
            }
        }
        for r in &p.rels {
            if let Some(v) = &r.variable {
                s.insert(v.clone());
            }
        }
        add_gpm_element_names(&p.elements, &mut s);
    }
    s
}

pub(crate) fn anon_name() -> String {
    format!("__a{}", ANON.fetch_add(1, Ordering::Relaxed))
}
