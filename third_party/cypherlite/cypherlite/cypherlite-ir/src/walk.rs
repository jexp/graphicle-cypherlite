//! Shared expression walkers over the planner IR.
//!
//! Every expression walker is a fold over [`Expr::children`], so variant
//! coverage can no longer drift between the planner and the executor. Two
//! scope-aware concerns keep explicit arms in [`expr_vars`]:
//! comprehension/reduce variants bind local names (their bound identifiers
//! are subtracted), and the pattern/subquery variants carry their own
//! structure (`PathPattern` / `Query`), which `children()` deliberately
//! does not flatten.
//!
//! The clause-tree counterpart ([`crate::walk_query`]) descends into nested
//! queries (`CALL { … }`, `WHEN` branches, `FOREACH` bodies, `NEXT` stages
//! and the pattern/subquery variants `children()` skips) with exhaustive
//! matches, so a new clause or expression variant fails compilation there
//! instead of silently skipping a subtree.

use std::collections::HashSet;

use crate::{Clause, Expr, LabelExpr, MapProjectionItem, PathPattern, Query};

impl Expr {
    /// Direct child expressions.
    ///
    /// Uniform traversal used by every walker. Label expressions contribute
    /// the expressions inside their dynamic (`$(…)`) forms. Pattern-bearing
    /// variants (`PatternPred`, `PatternComp`, `ShortestPath`) and subquery
    /// variants (`ExistsSubquery`, `CountSubquery`, `CollectSubquery`)
    /// introduce their own scope and structure, so they report no children;
    /// walkers that need their internals (e.g. [`expr_vars`]) handle those
    /// variants explicitly.
    pub fn children(&self) -> Vec<&Expr> {
        let mut out = Vec::new();
        match self {
            Expr::RuntimeValue(_)
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::String(_)
            | Expr::Parameter(_)
            | Expr::Variable(_)
            | Expr::Star
            | Expr::CountStar
            | Expr::DistanceMetric(_) => {}
            Expr::Property { expr, .. }
            | Expr::Unary { expr, .. }
            | Expr::IsType { expr, .. }
            | Expr::IsNormalized { expr, .. } => out.push(expr.as_ref()),
            Expr::LabelPredicate {
                expr, label_expr, ..
            } => {
                out.push(expr.as_ref());
                label_expr_child_exprs(label_expr, &mut out);
            }
            Expr::DynamicProperty { expr, key } => {
                out.push(expr.as_ref());
                out.push(key.as_ref());
            }
            Expr::Map(items) => out.extend(items.iter().map(|(_, value)| value)),
            Expr::List(items) | Expr::Function { args: items, .. } => out.extend(items),
            Expr::MapComp {
                source,
                predicate,
                key_expr,
                value_expr,
                ..
            } => {
                out.push(source.as_ref());
                if let Some(p) = predicate {
                    out.push(p.as_ref());
                }
                out.push(key_expr.as_ref());
                out.push(value_expr.as_ref());
            }
            Expr::Binary { left, right, .. } => {
                out.push(left.as_ref());
                out.push(right.as_ref());
            }
            Expr::Case {
                operand,
                when_then,
                else_expr,
            } => {
                out.extend(operand.as_deref());
                for (when, then, _) in when_then {
                    out.push(when);
                    out.push(then);
                }
                out.extend(else_expr.as_deref());
            }
            Expr::Subscript { expr, index } => {
                out.push(expr.as_ref());
                out.push(index.as_ref());
            }
            Expr::Slice { expr, start, end } => {
                out.push(expr.as_ref());
                out.extend(start.as_deref());
                out.extend(end.as_deref());
            }
            Expr::ListComp {
                list,
                predicate,
                projection,
                ..
            } => {
                out.push(list.as_ref());
                out.extend(predicate.as_deref());
                out.push(projection.as_ref());
            }
            Expr::Quantifier {
                list, predicate, ..
            } => {
                out.push(list.as_ref());
                out.push(predicate.as_ref());
            }
            Expr::Reduce {
                initial,
                list,
                expression,
                ..
            } => {
                out.push(initial.as_ref());
                out.push(list.as_ref());
                out.push(expression.as_ref());
            }
            Expr::AllReduce {
                initial,
                list,
                expression,
                predicate,
                ..
            } => {
                out.push(initial.as_ref());
                out.push(list.as_ref());
                out.push(expression.as_ref());
                out.push(predicate.as_ref());
            }
            Expr::MapProjection { expr, items } => {
                out.push(expr.as_ref());
                for item in items {
                    if let MapProjectionItem::KeyValue { value, .. } = item {
                        out.push(value);
                    }
                }
            }
            Expr::PatternPred { .. }
            | Expr::PatternComp { .. }
            | Expr::ShortestPath { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::CountSubquery { .. }
            | Expr::CollectSubquery { .. } => {}
        }
        out
    }

    /// Whether the variant introduces its own scope/structure that
    /// [`children`](Expr::children) deliberately does not flatten: pattern
    /// predicates/comprehensions, `shortestPath`, and the
    /// `EXISTS`/`COUNT`/`COLLECT` subquery forms. This is the one
    /// vocabulary for that class — `children()`'s skip set is the
    /// compile-forced enumeration; walkers that must refuse or specially
    /// handle these variants consult this predicate instead of re-listing
    /// them. A new `Expr` variant fails compilation in `children()` first,
    /// and the `scope_bearing_variants_match_children_skip_set` test keeps
    /// the two aligned.
    pub fn is_scope_bearing(&self) -> bool {
        matches!(
            self,
            Expr::PatternPred { .. }
                | Expr::PatternComp { .. }
                | Expr::ShortestPath { .. }
                | Expr::ExistsSubquery { .. }
                | Expr::CountSubquery { .. }
                | Expr::CollectSubquery { .. }
        )
    }

    /// Pre-order traversal: `f` sees `self`, then every descendant.
    pub fn visit(&self, f: &mut impl FnMut(&Expr)) {
        f(self);
        for child in self.children() {
            child.visit(f);
        }
    }

    /// Whether `pred` holds for this expression or any descendant expression.
    pub fn any(&self, pred: &mut impl FnMut(&Expr) -> bool) -> bool {
        pred(self) || self.children().iter().any(|child| child.any(pred))
    }
}

/// Collect the variable names an expression references, comprehension- and
/// reduce-bound identifiers excluded.
///
/// For pattern variants the pattern's own variable names are included: the
/// executor's column analysis treats them as values the expression needs
/// from the surrounding row. Subquery variants contribute every variable
/// name mentioned in the subquery (an over-approximation of the outer
/// references, matching the historical executor behaviour).
pub fn expr_vars(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Variable(v) => {
            out.insert(v.clone());
        }
        Expr::ListComp {
            ident,
            list,
            predicate,
            projection,
        } => {
            expr_vars(list, out);
            let mut inner = HashSet::new();
            if let Some(p) = predicate {
                expr_vars(p, &mut inner);
            }
            expr_vars(projection, &mut inner);
            inner.remove(ident);
            out.extend(inner);
        }
        Expr::MapComp {
            key_ident,
            value_ident,
            source,
            predicate,
            key_expr,
            value_expr,
        } => {
            expr_vars(source, out);
            let mut inner = HashSet::new();
            if let Some(p) = predicate {
                expr_vars(p, &mut inner);
            }
            expr_vars(key_expr, &mut inner);
            expr_vars(value_expr, &mut inner);
            inner.remove(value_ident.as_str());
            if let Some(k) = key_ident {
                inner.remove(k.as_str());
            }
            out.extend(inner);
        }
        Expr::Quantifier {
            ident,
            list,
            predicate,
            ..
        } => {
            expr_vars(list, out);
            let mut inner = HashSet::new();
            expr_vars(predicate, &mut inner);
            inner.remove(ident);
            out.extend(inner);
        }
        Expr::Reduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
        } => {
            expr_vars(initial, out);
            expr_vars(list, out);
            let mut inner = HashSet::new();
            expr_vars(expression, &mut inner);
            inner.remove(accumulator);
            inner.remove(ident);
            out.extend(inner);
        }
        Expr::AllReduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
            predicate,
        } => {
            expr_vars(initial, out);
            expr_vars(list, out);
            let mut inner = HashSet::new();
            expr_vars(expression, &mut inner);
            expr_vars(predicate, &mut inner);
            inner.remove(accumulator);
            inner.remove(ident);
            out.extend(inner);
        }
        Expr::PatternPred { path } | Expr::ShortestPath { path, .. } => {
            path_var_names(path, out);
        }
        Expr::PatternComp {
            path,
            predicate,
            projection,
        } => {
            path_var_names(path, out);
            if let Some(p) = predicate {
                expr_vars(p, out);
            }
            expr_vars(projection, out);
        }
        Expr::ExistsSubquery { query }
        | Expr::CountSubquery { query }
        | Expr::CollectSubquery { query } => exists_outer_vars(query, out),
        Expr::MapProjection { items, .. } => {
            for item in items {
                if let MapProjectionItem::Variable { name } = item {
                    out.insert(name.clone());
                }
            }
            for child in expr.children() {
                expr_vars(child, out);
            }
        }
        _ => {
            for child in expr.children() {
                expr_vars(child, out);
            }
        }
    }
}

/// Variables referenced by the dynamic (`$(…)`) parts of a label
/// expression: a fold over `label_expr_child_exprs`, the one enumeration
/// of the `LabelExpr` structure. The variant vocabulary is matched in
/// that helper alone, so a new `LabelExpr` variant fails compilation
/// there and is picked up here without a second match to keep aligned.
pub fn label_expr_vars(expr: &LabelExpr, out: &mut HashSet<String>) {
    let mut children = Vec::new();
    label_expr_child_exprs(expr, &mut children);
    for child in children {
        expr_vars(child, out);
    }
}

pub(crate) fn label_expr_child_exprs<'a>(label_expr: &'a LabelExpr, out: &mut Vec<&'a Expr>) {
    match label_expr {
        LabelExpr::Dynamic { expr, .. } => out.push(expr),
        LabelExpr::Not(inner) => label_expr_child_exprs(inner, out),
        LabelExpr::And(items) | LabelExpr::Or(items) => {
            for item in items {
                label_expr_child_exprs(item, out);
            }
        }
        LabelExpr::Label(_) | LabelExpr::Wildcard => {}
    }
}

/// Node variables, then rel variables, of a path pattern's compact view
/// (`nodes`/`rels`) — the path variable excluded. Compact-view only:
/// lowering clears `nodes`/`rels` when an element is non-alternating or
/// quantified (see `walk_query`'s rule), so such a pattern yields nothing.
pub fn path_pattern_vars(path: &PathPattern) -> impl Iterator<Item = &String> {
    path.nodes
        .iter()
        .filter_map(|n| n.variable.as_ref())
        .chain(path.rels.iter().filter_map(|r| r.variable.as_ref()))
}

fn path_var_names(path: &PathPattern, out: &mut HashSet<String>) {
    if let Some(v) = &path.variable {
        out.insert(v.clone());
    }
    // Node and rel variables both come from the shared compact-view
    // iterator above; the payload loops below carry only the property
    // maps and the label/type expressions.
    out.extend(path_pattern_vars(path).cloned());
    for n in &path.nodes {
        if let Some(p) = &n.properties {
            expr_vars(p, out);
        }
        if let Some(e) = &n.label_expr {
            label_expr_vars(e, out);
        }
    }
    for r in &path.rels {
        if let Some(p) = &r.properties {
            expr_vars(p, out);
        }
        if let Some(e) = &r.type_expr {
            label_expr_vars(e, out);
        }
    }
}

fn exists_outer_vars(query: &Query, out: &mut HashSet<String>) {
    let mut inner = HashSet::new();
    for part in &query.parts {
        for clause in &part.clauses {
            match clause {
                Clause::Match {
                    patterns,
                    where_clause,
                    ..
                } => {
                    for p in patterns {
                        path_var_names(p, &mut inner);
                    }
                    if let Some(w) = where_clause {
                        expr_vars(w, &mut inner);
                    }
                }
                Clause::Unwind { expression, .. } => {
                    expr_vars(expression, &mut inner);
                }
                Clause::With {
                    items,
                    order,
                    skip,
                    limit,
                    where_clause,
                    ..
                } => {
                    for i in items {
                        expr_vars(&i.expr, &mut inner);
                    }
                    for o in order {
                        expr_vars(&o.expr, &mut inner);
                    }
                    if let Some(s) = skip {
                        expr_vars(s, &mut inner);
                    }
                    if let Some(l) = limit {
                        expr_vars(l, &mut inner);
                    }
                    if let Some(w) = where_clause {
                        expr_vars(w, &mut inner);
                    }
                }
                Clause::Return {
                    items,
                    order,
                    skip,
                    limit,
                    ..
                } => {
                    for i in items {
                        expr_vars(&i.expr, &mut inner);
                    }
                    for o in order {
                        expr_vars(&o.expr, &mut inner);
                    }
                    if let Some(s) = skip {
                        expr_vars(s, &mut inner);
                    }
                    if let Some(l) = limit {
                        expr_vars(l, &mut inner);
                    }
                }
                Clause::Call { args, .. } => {
                    for a in args {
                        expr_vars(a, &mut inner);
                    }
                }
                // Clause kinds whose expressions are not part of the
                // outer-reference scan (the historical over-approximation
                // documented on `expr_vars`). Enumerated explicitly rather
                // than through a `_` catch-all so a new `Clause` variant
                // fails compilation here instead of silently skipping its
                // variables.
                Clause::Catalog(_)
                | Clause::Create { .. }
                | Clause::Merge { .. }
                | Clause::Delete { .. }
                | Clause::Set { .. }
                | Clause::Remove { .. }
                | Clause::CreateIndex { .. }
                | Clause::DropIndex { .. }
                | Clause::ShowIndexes
                | Clause::Finish
                | Clause::LoadCsv { .. }
                | Clause::CallSubquery { .. }
                | Clause::Conditional { .. }
                | Clause::Foreach { .. }
                | Clause::Use { .. }
                | Clause::CreateConstraint { .. }
                | Clause::DropConstraint { .. } => {}
            }
        }
    }
    out.extend(inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodePattern, PatternElement, RelPattern};

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    #[test]
    fn scope_bearing_variants_match_children_skip_set() {
        let path = crate::PathPattern {
            variable: None,
            elements: vec![],
            nodes: vec![],
            rels: vec![],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let query = || crate::Query {
            parts: vec![],
            union_all: vec![],
            next: None,
            cypher_version: None,
        };
        let bearing: Vec<Expr> = vec![
            Expr::PatternPred { path: path.clone() },
            Expr::PatternComp {
                path: path.clone(),
                predicate: None,
                projection: Box::new(var("n")),
            },
            Expr::ShortestPath { all: false, path },
            Expr::ExistsSubquery { query: query() },
            Expr::CountSubquery { query: query() },
            Expr::CollectSubquery { query: query() },
        ];
        for expr in &bearing {
            assert!(expr.is_scope_bearing(), "{expr:?} must be scope-bearing");
            assert!(expr.children().is_empty(), "{expr:?} has no children");
        }
        for expr in [var("n"), Expr::CountStar, Expr::Int(1)] {
            assert!(!expr.is_scope_bearing(), "{expr:?} is scope-transparent");
        }
    }

    #[test]
    fn children_covers_every_variant_exactly_once() {
        // A list-comprehension under a subscript: the historical
        // `expr_contains_variable` walker missed the ListComp arm entirely.
        let expr = Expr::Subscript {
            expr: Box::new(Expr::ListComp {
                ident: "x".into(),
                list: Box::new(Expr::List(vec![var("n")])),
                predicate: None,
                projection: Box::new(var("x")),
            }),
            index: Box::new(Expr::Int(0)),
        };
        assert!(expr.any(&mut |e| matches!(e, Expr::Variable(v) if v == "n")));
        assert!(expr.any(&mut |e| matches!(e, Expr::Variable(v) if v == "x")));
    }

    #[test]
    fn children_include_dynamic_label_expressions() {
        let expr = Expr::LabelPredicate {
            expr: Box::new(var("n")),
            label_expr: LabelExpr::And(vec![
                LabelExpr::Label("A".into()),
                LabelExpr::Dynamic {
                    expr: Box::new(var("l")),
                    quantifier: crate::DynamicLabelQuantifier::Default,
                },
            ]),
            strict: false,
        };
        assert_eq!(expr.children().len(), 2);
        assert!(expr.any(&mut |e| matches!(e, Expr::Variable(v) if v == "l")));
    }

    #[test]
    fn label_expr_vars_folds_the_dynamic_child_expressions() {
        // `label_expr_vars` is a fold over `label_expr_child_exprs`, so
        // every dynamic `$(…)` form contributes through any boolean
        // structure, while static labels and wildcards contribute nothing.
        let expr = LabelExpr::And(vec![
            LabelExpr::Label("A".into()),
            LabelExpr::Not(Box::new(LabelExpr::Dynamic {
                expr: Box::new(var("l")),
                quantifier: crate::DynamicLabelQuantifier::Default,
            })),
            LabelExpr::Or(vec![
                LabelExpr::Wildcard,
                LabelExpr::Dynamic {
                    expr: Box::new(var("m")),
                    quantifier: crate::DynamicLabelQuantifier::All,
                },
            ]),
        ]);
        let mut vars = HashSet::new();
        label_expr_vars(&expr, &mut vars);
        assert!(vars.contains("l"));
        assert!(vars.contains("m"));
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn children_do_not_descend_into_patterns_or_subqueries() {
        let node = |variable: &str, properties: Option<Expr>| NodePattern {
            variable: Some(variable.into()),
            labels: Vec::new(),
            label_expr: None,
            properties,
            where_clause: None,
        };
        let rel = || RelPattern {
            variable: Some("r".into()),
            types: Vec::new(),
            type_expr: Some(LabelExpr::Dynamic {
                expr: Box::new(var("t")),
                quantifier: crate::DynamicLabelQuantifier::Default,
            }),
            direction: crate::Direction::Outgoing,
            properties: Some(var("rp")),
            var_length: false,
            min_hops: 1,
            max_hops: None,
            where_clause: None,
            quantified: false,
        };
        let path = PathPattern {
            variable: None,
            elements: vec![
                PatternElement::Node(node("a", Some(var("p")))),
                PatternElement::Relationship(rel()),
                PatternElement::Node(node("b", None)),
            ],
            nodes: vec![node("a", Some(var("p"))), node("b", None)],
            rels: vec![rel()],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let expr = Expr::PatternPred { path };
        assert!(expr.children().is_empty());
        // ...but expr_vars still looks inside the pattern: node and rel
        // variables (both via the shared `path_pattern_vars` iterator),
        // the property maps, and the dynamic type expressions.
        let mut vars = HashSet::new();
        expr_vars(&expr, &mut vars);
        for expected in ["a", "b", "r", "p", "t", "rp"] {
            assert!(vars.contains(expected), "missing variable {expected}");
        }
        assert_eq!(vars.len(), 6);
    }

    #[test]
    fn expr_vars_excludes_comprehension_bound_idents() {
        // `[x IN [n] | x + y]` references `n` and `y`, not the bound `x`.
        let expr = Expr::ListComp {
            ident: "x".into(),
            list: Box::new(Expr::List(vec![var("n")])),
            predicate: None,
            projection: Box::new(Expr::Binary {
                op: crate::BinaryOp::Add,
                left: Box::new(var("x")),
                right: Box::new(var("y")),
            }),
        };
        let mut vars = HashSet::new();
        expr_vars(&expr, &mut vars);
        let mut collected: Vec<&str> = vars.iter().map(String::as_str).collect();
        collected.sort_unstable();
        assert_eq!(collected, vec!["n", "y"]);
        // An outer `x` next to the comprehension is still reported: the
        // historical planner walker removed it depending on traversal order.
        let expr = Expr::Binary {
            op: crate::BinaryOp::Add,
            left: Box::new(var("x")),
            right: Box::new(Expr::ListComp {
                ident: "x".into(),
                list: Box::new(Expr::List(vec![])),
                predicate: None,
                projection: Box::new(var("x")),
            }),
        };
        let mut vars = HashSet::new();
        expr_vars(&expr, &mut vars);
        assert!(vars.contains("x"));
    }

    #[test]
    fn expr_vars_collects_map_projection_variable_items() {
        let expr = Expr::MapProjection {
            expr: Box::new(var("n")),
            items: vec![
                MapProjectionItem::Property { key: "name".into() },
                MapProjectionItem::Variable { name: "v".into() },
                MapProjectionItem::KeyValue {
                    key: "k".into(),
                    value: var("w"),
                },
                MapProjectionItem::AllProperties,
            ],
        };
        let mut vars = HashSet::new();
        expr_vars(&expr, &mut vars);
        assert!(vars.contains("n"));
        assert!(vars.contains("v"));
        assert!(vars.contains("w"));
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn needs_gpm_is_the_single_shared_predicate() {
        let node = || NodePattern {
            variable: None,
            labels: Vec::new(),
            label_expr: None,
            properties: None,
            where_clause: None,
        };
        let rel = || RelPattern {
            variable: None,
            types: Vec::new(),
            type_expr: None,
            direction: crate::Direction::Outgoing,
            properties: None,
            var_length: false,
            min_hops: 1,
            max_hops: None,
            where_clause: None,
            quantified: false,
        };
        let compact = PathPattern {
            variable: None,
            elements: vec![
                PatternElement::Node(node()),
                PatternElement::Relationship(rel()),
                PatternElement::Node(node()),
            ],
            nodes: vec![node(), node()],
            rels: vec![rel()],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        assert!(!compact.needs_gpm());
        // Compact representation cleared at lowering => GPM.
        let mut gpm = compact.clone();
        gpm.nodes.clear();
        gpm.rels.clear();
        assert!(gpm.needs_gpm());
        // GPM-only features on an otherwise compact path.
        let mut with_mode = compact.clone();
        with_mode.mode = Some(crate::PathMode::Walk);
        assert!(with_mode.needs_gpm());
        let mut with_element_pred = compact.clone();
        with_element_pred.elements[1] = PatternElement::Relationship(RelPattern {
            where_clause: Some(Expr::Bool(true)),
            ..rel()
        });
        assert!(with_element_pred.needs_gpm());
        let mut with_qpp = compact.clone();
        with_qpp
            .elements
            .push(PatternElement::Quantified(crate::QuantifiedPathPattern {
                subpath_variable: None,
                elements: vec![PatternElement::Node(node())],
                where_clause: None,
                min: 1,
                max: None,
            }));
        assert!(with_qpp.needs_gpm());
    }
}
