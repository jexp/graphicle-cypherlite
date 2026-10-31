//! MATCH rewrites after bind (ADR-0024).

use std::collections::HashSet;

use cypherlite_ir::{BinaryOp, Clause, Expr, PathPattern, Projection};

use crate::order::order_patterns;
use crate::stats::PatternStats;

pub fn rewrite_part(clauses: &mut [Clause], stats: &dyn PatternStats) {
    let mut bound = HashSet::new();
    for clause in clauses {
        match clause {
            Clause::Catalog(_) => {}
            Clause::Match {
                patterns,
                where_clause,
                search,
                ..
            } => {
                let introduced = pattern_introduced_vars(patterns);
                if let Some(w) = where_clause.take() {
                    *where_clause = fold_where(patterns, w, &bound, &introduced);
                }
                bound = order_patterns(patterns, &bound, where_clause.as_ref(), stats);
                if let Some(score_var) = search
                    .as_deref()
                    .and_then(|search| search.score_var.as_ref())
                {
                    bound.insert(score_var.clone());
                }
            }
            Clause::Create { patterns } => {
                for p in patterns {
                    add_pattern_vars(&mut bound, p);
                }
            }
            Clause::Merge { pattern, .. } => add_pattern_vars(&mut bound, pattern),
            Clause::Unwind { alias, .. } => {
                bound.insert(alias.clone());
            }
            Clause::Call { yield_items, .. } => {
                bound.extend(yield_items.effective_names().map(str::to_owned));
            }
            Clause::With { items, .. } => {
                bound = with_bound(items, &bound);
            }
            Clause::Return { .. }
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

fn pattern_introduced_vars(patterns: &[PathPattern]) -> HashSet<String> {
    patterns
        .iter()
        .flat_map(cypherlite_ir::path_pattern_vars)
        .cloned()
        .collect()
}

pub(crate) fn add_pattern_vars(bound: &mut HashSet<String>, p: &PathPattern) {
    if let Some(v) = &p.variable {
        bound.insert(v.clone());
    }
    bound.extend(cypherlite_ir::path_pattern_vars(p).cloned());
}

fn with_bound(items: &[Projection], prev: &HashSet<String>) -> HashSet<String> {
    let has_star = items.iter().any(|i| matches!(i.expr, Expr::Star));
    let mut next = if has_star {
        prev.clone()
    } else {
        HashSet::new()
    };
    for (i, item) in items.iter().enumerate() {
        if matches!(item.expr, Expr::Star) {
            continue;
        }
        next.insert(item.output_name(i));
    }
    next
}

fn fold_where(
    patterns: &mut [PathPattern],
    expr: Expr,
    bound: &HashSet<String>,
    introduced: &HashSet<String>,
) -> Option<Expr> {
    let mut kept = Vec::new();
    for conjunct in flatten_and(expr) {
        if !fold_conjunct(patterns, &conjunct, bound, introduced) {
            kept.push(conjunct);
        }
    }
    combine_and(kept)
}

fn flatten_and(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut v = flatten_and(*left);
            v.extend(flatten_and(*right));
            v
        }
        other => vec![other],
    }
}

fn combine_and(parts: Vec<Expr>) -> Option<Expr> {
    parts.into_iter().reduce(|acc, p| Expr::Binary {
        op: BinaryOp::And,
        left: Box::new(acc),
        right: Box::new(p),
    })
}

fn fold_conjunct(
    patterns: &mut [PathPattern],
    expr: &Expr,
    bound: &HashSet<String>,
    introduced: &HashSet<String>,
) -> bool {
    if let Expr::LabelPredicate {
        expr, label_expr, ..
    } = expr
    {
        if let Expr::Variable(v) = expr.as_ref() {
            if introduced.contains(v) && !bound.contains(v) {
                let labels = static_labels(label_expr);
                if !labels.is_empty() {
                    return add_labels(patterns, v, &labels);
                }
            }
        }
        return false;
    }
    let Some((v, key, val)) = eq_prop(expr) else {
        return false;
    };
    if !introduced.contains(&v) || bound.contains(&v) {
        return false;
    }
    if !val.is_seek_const() {
        return false;
    }
    add_prop(patterns, &v, key, val)
}

fn static_labels(expr: &cypherlite_ir::LabelExpr) -> Vec<String> {
    match expr {
        cypherlite_ir::LabelExpr::Label(label) => vec![label.clone()],
        cypherlite_ir::LabelExpr::And(items) => {
            let labels: Vec<_> = items.iter().flat_map(static_labels).collect();
            if labels.len() == items.len() {
                labels
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn eq_prop(expr: &Expr) -> Option<(String, String, Expr)> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    else {
        return None;
    };
    // Eq is operand-symmetric — no op to mirror, so only the shared
    // side-selection core applies.
    cypherlite_ir::orient_side(cypherlite_ir::var_prop_owned, left, right)
        .map(|((v, k), other)| (v, k, other.clone()))
}

fn add_labels(patterns: &mut [PathPattern], var: &str, labels: &[String]) -> bool {
    // Folding into a pattern is compact-view-only: a GPM pattern's
    // `nodes`/`rels` may be cleared or duplicated beside the `elements`
    // the GPM engine walks, so a label folded into `nodes` could desync
    // from the elements. Such paths are skipped and the conjunct stays
    // in the WHERE clause.
    for p in patterns.iter_mut().filter(|p| !p.needs_gpm()) {
        for n in &mut p.nodes {
            if n.variable.as_deref() == Some(var) {
                for l in labels {
                    if !n.labels.iter().any(|x| x == l) {
                        n.labels.push(l.clone());
                    }
                }
                n.label_expr = Some(cypherlite_ir::LabelExpr::And(
                    n.labels
                        .iter()
                        .cloned()
                        .map(cypherlite_ir::LabelExpr::Label)
                        .collect(),
                ));
                return true;
            }
        }
    }
    false
}

fn add_prop(patterns: &mut [PathPattern], var: &str, key: String, val: Expr) -> bool {
    // Same compact-view-only scope as `add_labels` above.
    for p in patterns.iter_mut().filter(|p| !p.needs_gpm()) {
        for n in &mut p.nodes {
            if n.variable.as_deref() == Some(var) {
                return merge_prop_map(&mut n.properties, key, val);
            }
        }
        for r in &mut p.rels {
            if r.variable.as_deref() == Some(var) {
                return merge_prop_map(&mut r.properties, key, val);
            }
        }
    }
    false
}

fn merge_prop_map(slot: &mut Option<Expr>, key: String, val: Expr) -> bool {
    match slot {
        Some(Expr::Map(pairs)) => {
            if !pairs.iter().any(|(k, _)| k == &key) {
                pairs.push((key, val));
            }
            true
        }
        None => {
            *slot = Some(Expr::Map(vec![(key, val)]));
            true
        }
        Some(_) => false,
    }
}
