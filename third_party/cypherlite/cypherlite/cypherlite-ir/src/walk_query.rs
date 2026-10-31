//! Clause-tree walkers over the planner IR.
//!
//! [`walk_query`] is the clause-tree counterpart of the expression walkers
//! in [`crate::walk`]: it descends into nested queries (`CALL { … }`,
//! `WHEN` branches, `FOREACH` bodies, `NEXT` stages and the
//! pattern/subquery variants [`Expr::children`] skips) with exhaustive
//! matches, so a new clause or expression variant fails compilation here
//! instead of silently skipping a subtree.

use std::collections::HashSet;

use crate::search::SearchSpec;
use crate::walk::label_expr_child_exprs;
use crate::{
    Clause, Concurrency, Expr, LabelExpr, OnError, OrderItem, PathPattern, Projection, Query,
    RemoveItem, SetItem,
};

mod pattern;

pub use pattern::{walk_pattern_payloads, PatternPayload};

/// The syntactic role an expression occupies for a consumer that must tell
/// a value the statement reads from a name it writes (ADR-0363 decision 13):
/// [`ExprRole::Read`] is every expression evaluated against incoming rows;
/// [`ExprRole::WriteTarget`] is a `SET`/`REMOVE` target and every payload
/// expression of a `CREATE`/`MERGE` target pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExprRole {
    Read,
    WriteTarget,
}

/// Pre-order walk over a statement's clause tree.
///
/// `on_clause` sees every clause and `on_expr` every expression in the
/// statement: UNION-arm `parts`, `NEXT` stages, `CALL { … }` subqueries,
/// `WHEN` branches, `FOREACH` bodies and the queries inside expression
/// subqueries (`EXISTS`/`COUNT`/`COLLECT { … }`) are all descended into.
/// The matches are exhaustive over [`Clause`], [`crate::PatternElement`] and the
/// item enums, so a new variant fails compilation here instead of silently
/// skipping a subtree the way an [`Expr::children`] fold skips the
/// pattern/subquery variants.
pub fn walk_query<'a>(
    query: &'a Query,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr),
) {
    walk_query_with_roles(query, on_clause, &mut |expr, _role| on_expr(expr));
}

/// [`walk_query`] with each expression's [`ExprRole`], for the readers that
/// distinguish read evidence from a write target (ADR-0363 decision 13).
pub(crate) fn walk_query_with_roles<'a>(
    query: &'a Query,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    for part in &query.parts {
        walk_clauses_with_roles(&part.clauses, on_clause, on_expr);
    }
    if let Some(next) = query.next.as_deref() {
        walk_query_with_roles(next, on_clause, on_expr);
    }
}

/// [`walk_query`] over a clause list, for plan shapes that store clauses
/// directly rather than as a [`Query`] (e.g. a logical plan's parts).
pub fn walk_clauses<'a>(
    clauses: &'a [Clause],
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr),
) {
    walk_clauses_with_roles(clauses, on_clause, &mut |expr, _role| on_expr(expr));
}

/// [`walk_clauses`] with each expression's [`ExprRole`].
pub fn walk_clauses_with_roles<'a>(
    clauses: &'a [Clause],
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    for clause in clauses {
        on_clause(clause);
        match clause {
            Clause::Match {
                patterns,
                where_clause,
                search,
                ..
            } => {
                for pattern in patterns {
                    walk_pattern(pattern, ExprRole::Read, on_clause, on_expr);
                }
                if let Some(where_clause) = where_clause {
                    walk_expr(where_clause, ExprRole::Read, on_clause, on_expr);
                }
                if let Some(search) = search {
                    walk_search_spec(&search.spec, on_clause, on_expr);
                }
            }
            // A write target's own payload names what the statement brings
            // into existence, so it is a write target, not read evidence.
            Clause::Create { patterns } => {
                for pattern in patterns {
                    walk_pattern(pattern, ExprRole::WriteTarget, on_clause, on_expr);
                }
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                walk_pattern(pattern, ExprRole::WriteTarget, on_clause, on_expr);
                for item in on_create.iter().chain(on_match) {
                    walk_set_item(item, on_clause, on_expr);
                }
            }
            Clause::Call {
                args, where_clause, ..
            } => {
                for arg in args {
                    walk_expr(arg, ExprRole::Read, on_clause, on_expr);
                }
                if let Some(where_clause) = where_clause {
                    walk_expr(where_clause, ExprRole::Read, on_clause, on_expr);
                }
            }
            Clause::Delete { expressions, .. } => {
                for expr in expressions {
                    walk_expr(expr, ExprRole::Read, on_clause, on_expr);
                }
            }
            Clause::Set { items } => {
                for item in items {
                    walk_set_item(item, on_clause, on_expr);
                }
            }
            Clause::Remove { items } => {
                for item in items {
                    walk_remove_item(item, on_clause, on_expr);
                }
            }
            Clause::With {
                items,
                order,
                skip,
                limit,
                where_clause,
                group_by,
                ..
            } => {
                walk_projection(items, order, skip, limit, group_by, on_clause, on_expr);
                if let Some(where_clause) = where_clause {
                    walk_expr(where_clause, ExprRole::Read, on_clause, on_expr);
                }
            }
            Clause::Return {
                items,
                order,
                skip,
                limit,
                group_by,
                ..
            } => {
                walk_projection(items, order, skip, limit, group_by, on_clause, on_expr);
            }
            Clause::Unwind { expression, .. } => {
                walk_expr(expression, ExprRole::Read, on_clause, on_expr)
            }
            Clause::LoadCsv { url, .. } => walk_expr(url, ExprRole::Read, on_clause, on_expr),
            Clause::CallSubquery {
                query,
                in_transactions,
                ..
            } => {
                walk_query_with_roles(query, on_clause, on_expr);
                if let Some(tx) = in_transactions {
                    if let Some(batch) = &tx.batch_size {
                        walk_expr(batch, ExprRole::Read, on_clause, on_expr);
                    }
                    if let Concurrency::Value(concurrency) = &tx.concurrency {
                        walk_expr(concurrency, ExprRole::Read, on_clause, on_expr);
                    }
                    if let OnError::Retry {
                        duration: Some(duration),
                        ..
                    } = &tx.on_error
                    {
                        walk_expr(duration, ExprRole::Read, on_clause, on_expr);
                    }
                }
            }
            Clause::Conditional {
                branches,
                else_query,
            } => {
                for (predicate, query) in branches {
                    walk_expr(predicate, ExprRole::Read, on_clause, on_expr);
                    walk_query_with_roles(query, on_clause, on_expr);
                }
                if let Some(query) = else_query.as_deref() {
                    walk_query_with_roles(query, on_clause, on_expr);
                }
            }
            Clause::Foreach { list, clauses, .. } => {
                walk_expr(list, ExprRole::Read, on_clause, on_expr);
                walk_clauses_with_roles(clauses, on_clause, on_expr);
            }
            Clause::Use { graph } => walk_expr(graph, ExprRole::Read, on_clause, on_expr),
            Clause::Catalog(_)
            | Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::ShowIndexes
            | Clause::Finish
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. } => {}
        }
    }
}

/// Walk every expression of a CIP-278 `<source specification clause>`.
/// The slot enumeration and its exhaustive-match compile guarantee live
/// in `SearchSpec::exprs` (ADR-0379); the planner consumes the same
/// exactly `SearchSpec::exprs()`'s consumer, so there is no second
/// match here to drift.
fn walk_search_spec<'a>(
    spec: &'a SearchSpec,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    for expr in spec.exprs() {
        walk_expr(expr, ExprRole::Read, on_clause, on_expr);
    }
}

/// Pre-order expression walk used by [`walk_query`]: unlike
/// [`Expr::visit`], descends into the pattern- and subquery-bearing
/// variants that [`Expr::children`] deliberately skips. The enclosing
/// [`ExprRole`] is handed to the callback and inherited by every child.
pub(crate) fn walk_expr<'a>(
    expr: &'a Expr,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    on_expr(expr, role);
    match expr {
        Expr::ExistsSubquery { query }
        | Expr::CountSubquery { query }
        | Expr::CollectSubquery { query } => walk_query_with_roles(query, on_clause, on_expr),
        Expr::PatternPred { path } | Expr::ShortestPath { path, .. } => {
            walk_pattern(path, role, on_clause, on_expr);
        }
        Expr::PatternComp {
            path,
            predicate,
            projection,
        } => {
            walk_pattern(path, role, on_clause, on_expr);
            if let Some(predicate) = predicate.as_deref() {
                walk_expr(predicate, role, on_clause, on_expr);
            }
            walk_expr(projection, role, on_clause, on_expr);
        }
        _ => {
            for child in expr.children() {
                walk_expr(child, role, on_clause, on_expr);
            }
        }
    }
}

/// The shared projection shape of `WITH` and `RETURN`; every position is a
/// read.
fn walk_projection<'a>(
    items: &'a [Projection],
    order: &'a [OrderItem],
    skip: &'a Option<Expr>,
    limit: &'a Option<Expr>,
    group_by: &'a Option<Vec<Expr>>,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    for item in items {
        walk_expr(&item.expr, ExprRole::Read, on_clause, on_expr);
    }
    for item in order {
        walk_expr(&item.expr, ExprRole::Read, on_clause, on_expr);
    }
    for expr in skip.iter().chain(limit) {
        walk_expr(expr, ExprRole::Read, on_clause, on_expr);
    }
    if let Some(group_by) = group_by {
        for expr in group_by {
            walk_expr(expr, ExprRole::Read, on_clause, on_expr);
        }
    }
}

/// A `SET` item: the target is a write target, the value a read (so
/// `SET n.key = n.other` still reads `other` — ADR-0363 decision 13).
fn walk_set_item<'a>(
    item: &'a SetItem,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    match item {
        SetItem::Property { target, value }
        | SetItem::MergeMap { target, value }
        | SetItem::LabelsFunction { target, value, .. } => {
            walk_expr(target, ExprRole::WriteTarget, on_clause, on_expr);
            walk_expr(value, ExprRole::Read, on_clause, on_expr);
        }
        SetItem::Labels { target, labels } => {
            walk_expr(target, ExprRole::WriteTarget, on_clause, on_expr);
            for label in labels {
                walk_label_expr(label, ExprRole::WriteTarget, on_clause, on_expr);
            }
        }
    }
}

/// A `REMOVE` item: every position is a write target.
fn walk_remove_item<'a>(
    item: &'a RemoveItem,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    match item {
        RemoveItem::Property { target } => {
            walk_expr(target, ExprRole::WriteTarget, on_clause, on_expr)
        }
        RemoveItem::Labels { target, labels } => {
            walk_expr(target, ExprRole::WriteTarget, on_clause, on_expr);
            for label in labels {
                walk_label_expr(label, ExprRole::WriteTarget, on_clause, on_expr);
            }
        }
    }
}

fn walk_pattern<'a>(
    pattern: &'a PathPattern,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    walk_pattern_full(pattern, role, on_clause, on_expr, &mut |_| {});
}

/// [`walk_pattern`] plus the pattern's *binding* positions: `on_binding`
/// sees every variable a pattern binds (path variable, node/relationship
/// variables, quantified subpath variables) at the same exclusive
/// [`walk_pattern_payloads`] dispatch, so a caller needing "every name a
/// pattern mentions" does not re-enumerate the payload fields.
fn walk_pattern_full<'a>(
    pattern: &'a PathPattern,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
    on_binding: &mut impl FnMut(&'a str),
) {
    if let Some(variable) = &pattern.variable {
        on_binding(variable);
    }
    walk_pattern_payloads(pattern, &mut |payload| match payload {
        PatternPayload::Node(node) => walk_node_pattern(node, role, on_clause, on_expr, on_binding),
        PatternPayload::Relationship(rel) => {
            walk_rel_pattern(rel, role, on_clause, on_expr, on_binding)
        }
        PatternPayload::Quantified(quantified) => {
            if let Some(variable) = &quantified.subpath_variable {
                on_binding(variable);
            }
            if let Some(where_clause) = &quantified.where_clause {
                walk_expr(where_clause, role, on_clause, on_expr);
            }
        }
    });
    if let Some(where_clause) = pattern.where_clause.as_deref() {
        walk_expr(where_clause, role, on_clause, on_expr);
    }
}

/// The one enumeration of a node pattern's payload — binding, label
/// expression, property map, element predicate — consulted by both the
/// compact view and the elements sequence.
fn walk_node_pattern<'a>(
    node: &'a crate::NodePattern,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
    on_binding: &mut impl FnMut(&'a str),
) {
    if let Some(variable) = &node.variable {
        on_binding(variable);
    }
    if let Some(label_expr) = &node.label_expr {
        walk_label_expr(label_expr, role, on_clause, on_expr);
    }
    if let Some(properties) = &node.properties {
        walk_expr(properties, role, on_clause, on_expr);
    }
    if let Some(where_clause) = &node.where_clause {
        walk_expr(where_clause, role, on_clause, on_expr);
    }
}

/// The relationship counterpart of [`walk_node_pattern`].
fn walk_rel_pattern<'a>(
    rel: &'a crate::RelPattern,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
    on_binding: &mut impl FnMut(&'a str),
) {
    if let Some(variable) = &rel.variable {
        on_binding(variable);
    }
    if let Some(type_expr) = &rel.type_expr {
        walk_label_expr(type_expr, role, on_clause, on_expr);
    }
    if let Some(properties) = &rel.properties {
        walk_expr(properties, role, on_clause, on_expr);
    }
    if let Some(where_clause) = &rel.where_clause {
        walk_expr(where_clause, role, on_clause, on_expr);
    }
}

/// Every variable name a path pattern *mentions*: each bound variable
/// (path, node, relationship, quantified subpath — a bound name equal to
/// an already-bound outer name is a correlation predicate, not a fresh
/// binding) plus every variable its expressions reference (property maps,
/// dynamic label/type expressions, element and path predicates). Built on
/// the canonical [`walk_pattern_payloads`] payload walk so the
/// elements-vs-compact dispatch and the per-field enumeration live exactly
/// once.
pub fn path_pattern_mentions(path: &PathPattern, out: &mut HashSet<String>) {
    let mut bindings = HashSet::new();
    walk_pattern_full(
        path,
        ExprRole::Read,
        &mut |_| {},
        &mut |expr, _role| crate::walk::expr_vars(expr, out),
        &mut |name| {
            bindings.insert(name.to_string());
        },
    );
    out.extend(bindings);
}

/// The expressions inside a label expression's dynamic (`$(…)`) forms.
fn walk_label_expr<'a>(
    label_expr: &'a LabelExpr,
    role: ExprRole,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr, ExprRole),
) {
    let mut children = Vec::new();
    label_expr_child_exprs(label_expr, &mut children);
    for child in children {
        walk_expr(child, role, on_clause, on_expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Direction, DynamicLabelQuantifier, MatchMode, NodePattern, PatternElement,
        QuantifiedPathPattern, RelPattern, SingleQuery,
    };

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    fn use_clause(name: &str) -> Clause {
        Clause::Use {
            graph: Expr::Variable(name.into()),
        }
    }

    fn query(clauses: Vec<Clause>) -> Query {
        Query {
            parts: vec![SingleQuery { clauses }],
            union_all: Vec::new(),
            next: None,
            cypher_version: None,
        }
    }

    /// A dynamic `$(COUNT { USE <name> … })` label expression.
    fn dynamic_label(name: &str) -> LabelExpr {
        LabelExpr::Dynamic {
            expr: Box::new(Expr::CountSubquery {
                query: query(vec![use_clause(name)]),
            }),
            quantifier: DynamicLabelQuantifier::Default,
        }
    }

    /// The `USE` graph names the walk sees, in walk order.
    fn seen_use_graphs(query: &Query) -> Vec<String> {
        let mut seen = Vec::new();
        walk_query(
            query,
            &mut |clause| {
                if let Clause::Use {
                    graph: Expr::Variable(name),
                } = clause
                {
                    seen.push(name.clone());
                }
            },
            &mut |_| {},
        );
        seen
    }

    #[test]
    fn walk_query_descends_into_set_and_remove_label_payloads() {
        // Regression: SetItem::Labels / RemoveItem::Labels carry a
        // Vec<LabelExpr> whose dynamic $(…) forms can hold nested queries;
        // skipping the payload lets a nested USE escape resolution.
        let top = query(vec![
            Clause::Set {
                items: vec![SetItem::Labels {
                    target: Expr::Variable("n".into()),
                    labels: vec![dynamic_label("set_inner")],
                }],
            },
            Clause::Remove {
                items: vec![RemoveItem::Labels {
                    target: Expr::Variable("n".into()),
                    labels: vec![dynamic_label("remove_inner")],
                }],
            },
        ]);
        assert_eq!(
            seen_use_graphs(&top),
            vec!["set_inner".to_string(), "remove_inner".to_string()]
        );
    }

    #[test]
    fn walk_query_descends_into_pattern_element_label_exprs() {
        // Regression: lowering clears the compact `nodes`/`rels` repr for
        // quantified (non-compact) patterns, so the `elements` sequence is
        // the surviving representation — its label_expr/type_expr payloads
        // must be walked too.
        fn node(label: Option<LabelExpr>) -> NodePattern {
            NodePattern {
                variable: None,
                labels: Vec::new(),
                label_expr: label,
                properties: None,
                where_clause: None,
            }
        }
        let pattern = PathPattern {
            variable: None,
            nodes: Vec::new(),
            rels: Vec::new(),
            elements: vec![
                PatternElement::Node(node(Some(dynamic_label("node_label")))),
                PatternElement::Relationship(RelPattern {
                    variable: None,
                    types: Vec::new(),
                    type_expr: Some(dynamic_label("rel_type")),
                    direction: Direction::Outgoing,
                    properties: None,
                    var_length: false,
                    min_hops: 1,
                    max_hops: None,
                    where_clause: None,
                    quantified: false,
                }),
                PatternElement::Quantified(QuantifiedPathPattern {
                    subpath_variable: None,
                    elements: vec![PatternElement::Node(node(Some(dynamic_label(
                        "quantified_node",
                    ))))],
                    where_clause: None,
                    min: 1,
                    max: None,
                }),
            ],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let top = query(vec![Clause::Match {
            optional: false,
            match_mode: MatchMode::DifferentRelationships,
            patterns: vec![pattern],
            where_clause: None,
            search: None,
        }]);
        assert_eq!(
            seen_use_graphs(&top),
            vec![
                "node_label".to_string(),
                "rel_type".to_string(),
                "quantified_node".to_string()
            ]
        );
    }

    #[test]
    fn walk_pattern_visits_compact_pattern_payloads_exactly_once() {
        // Regression: a lowered compact pattern carries both the `elements`
        // sequence and the cloned compact `nodes`/`rels` view (`lower_path`
        // in cypherlite-front); walking both representations double-visits
        // every label_expr/properties payload. The post-rewrite precedence
        // reads the compact view once for a non-GPM shape.
        let node = NodePattern {
            variable: None,
            labels: Vec::new(),
            label_expr: Some(dynamic_label("node_label")),
            properties: None,
            where_clause: None,
        };
        let pattern = PathPattern {
            variable: None,
            nodes: vec![node.clone()],
            rels: Vec::new(),
            elements: vec![PatternElement::Node(node)],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let top = query(vec![Clause::Match {
            optional: false,
            match_mode: MatchMode::DifferentRelationships,
            patterns: vec![pattern],
            where_clause: None,
            search: None,
        }]);
        assert_eq!(seen_use_graphs(&top), vec!["node_label".to_string()]);
    }

    #[test]
    fn walk_query_descends_into_subqueries_and_next_stages() {
        // USE clauses nested in a CALL subquery, a COUNT expression
        // subquery (which `Expr::children` skips) and a NEXT stage are all
        // seen by the clause-tree walk.
        let mut top = query(vec![
            Clause::CallSubquery {
                query: Box::new(query(vec![use_clause("call_inner")])),
                optional: false,
                scope: crate::CallScope::Wildcard,
                in_transactions: None,
            },
            Clause::Return {
                distinct: false,
                group_by: None,
                items: vec![Projection {
                    expr: Expr::CountSubquery {
                        query: query(vec![use_clause("count_inner")]),
                    },
                    alias: None,
                    source_text: None,
                }],
                order: Vec::new(),
                skip: None,
                limit: None,
            },
        ]);
        top.next = Some(Box::new(query(vec![use_clause("next_stage")])));
        let mut seen: Vec<String> = Vec::new();
        walk_query(
            &top,
            &mut |clause| {
                if let Clause::Use { graph } = clause {
                    seen.push(format!("{graph:?}"));
                }
            },
            &mut |_| {},
        );
        assert_eq!(seen.len(), 3, "expected all three USE clauses: {seen:?}");
    }

    #[test]
    fn path_pattern_mentions_covers_bindings_predicates_and_compact_view() {
        let rel = |variable: Option<&str>, where_clause: Option<Expr>| RelPattern {
            variable: variable.map(Into::into),
            types: vec![],
            type_expr: None,
            direction: crate::Direction::Outgoing,
            properties: None,
            var_length: false,
            min_hops: 1,
            max_hops: None,
            where_clause,
            quantified: false,
        };
        let node = |variable: Option<&str>,
                    label_expr: Option<LabelExpr>,
                    properties: Option<Expr>,
                    where_clause: Option<Expr>| NodePattern {
            variable: variable.map(Into::into),
            labels: vec![],
            label_expr,
            properties,
            where_clause,
        };
        let path = PathPattern {
            variable: None,
            elements: vec![],
            nodes: vec![node(
                Some("n"),
                Some(LabelExpr::Dynamic {
                    expr: Box::new(var("lbl")),
                    quantifier: crate::DynamicLabelQuantifier::Default,
                }),
                Some(var("props")),
                Some(var("nw")),
            )],
            rels: vec![rel(Some("r"), Some(var("rw")))],
            where_clause: Some(Box::new(var("pw"))),
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let mut out = HashSet::new();
        path_pattern_mentions(&path, &mut out);
        for expected in ["n", "r", "lbl", "props", "nw", "rw", "pw"] {
            assert!(out.contains(expected), "missing mention {expected}");
        }
        assert!(!out.contains("other"));

        // Elements view: quantified subpaths recurse.
        let path = PathPattern {
            variable: None,
            elements: vec![PatternElement::Quantified(crate::QuantifiedPathPattern {
                subpath_variable: Some("q".into()),
                elements: vec![PatternElement::Node(node(None, None, None, None))],
                where_clause: Some(var("qw")),
                min: 1,
                max: None,
            })],
            nodes: vec![],
            rels: vec![],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let mut out = HashSet::new();
        path_pattern_mentions(&path, &mut out);
        for expected in ["q", "qw"] {
            assert!(out.contains(expected), "missing mention {expected}");
        }
        assert!(!out.contains("n"));
    }
}
