//! MATCH rewrites after bind (ADR-0024).

use std::collections::HashSet;

use cypherlite_ir::{BinaryOp, Clause, Expr, LabelExpr, PathPattern, Projection};

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
            | Clause::Use { .. } => {}
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
                if let Some(labels) = static_labels(label_expr).filter(|labels| !labels.is_empty())
                {
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

/// The flat label set of a static conjunction (`A`, `A&B`, `(A&B)&C`), or
/// `None` when the expression is anything else — a disjunction, negation,
/// wildcard or dynamic leaf anywhere in the tree makes the whole expression
/// unfoldable. Gated on [`LabelExpr::is_static_conjunction`], the one
/// predicate for "a conjunction of plain labels" and the only place it is
/// consulted: callers ask this helper, never the raw predicate.
///
/// Flattening leaf by leaf and comparing the collected labels against the
/// item count used to accept `(A&B)&!(B&C)` as `{A, B}` (the inner `And`
/// contributed two labels for one item), folded the pattern to those two
/// labels and dropped the predicate -- so the negation never applied.
fn static_labels(expr: &LabelExpr) -> Option<Vec<String>> {
    match expr {
        LabelExpr::Label(label) => Some(vec![label.clone()]),
        LabelExpr::And(items) if expr.is_static_conjunction() => {
            // `is_static_conjunction` guarantees every item is static too,
            // so the recursion cannot drop one.
            items
                .iter()
                .map(static_labels)
                .collect::<Option<Vec<_>>>()
                .map(|parts| parts.concat())
        }
        _ => None,
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
            if n.variable.as_deref() != Some(var) {
                continue;
            }
            // The rebuild below replaces `label_expr` with the pattern's
            // own static label set plus the folded conjunct, so it is only
            // faithful when that set holds every label the pattern needs: a
            // disjunction/negation/wildcard/dynamic expression
            // (`MATCH (n:A|B) WHERE n:C`) would be narrowed to `{C}`.
            let mut folded = match n.label_expr.as_ref() {
                None => n.labels.clone(),
                Some(expr) => match static_labels(expr) {
                    Some(labels) => labels,
                    None => continue,
                },
            };
            for l in labels {
                if !folded.iter().any(|x| x == l) {
                    folded.push(l.clone());
                }
            }
            n.labels = folded;
            n.label_expr = Some(LabelExpr::And(
                n.labels.iter().cloned().map(LabelExpr::Label).collect(),
            ));
            return true;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower `cypher`, run the MATCH rewrite, and return the first pattern
    /// node's flat label list plus the WHERE clause that survived.
    fn rewritten(cypher: &str) -> (Vec<String>, Option<Expr>) {
        let mut query = cypherlite_front::parse_and_lower(cypher).unwrap();
        rewrite_part(&mut query.parts[0].clauses, &crate::NoPatternStats);
        let Clause::Match {
            patterns,
            where_clause,
            ..
        } = &query.parts[0].clauses[0]
        else {
            panic!("{cypher}: expected MATCH");
        };
        (patterns[0].nodes[0].labels.clone(), where_clause.clone())
    }

    #[test]
    fn static_label_conjunction_folds_into_the_pattern() {
        let (labels, where_clause) = rewritten("MATCH (n:A) WHERE n:B RETURN n");
        assert_eq!(labels, vec!["A".to_string(), "B".to_string()]);
        assert!(where_clause.is_none());
    }

    /// A negation or disjunction anywhere in the label expression makes it
    /// unfoldable: folding `(A&B)&!(B&C)` to `{A, B}` and dropping the
    /// predicate made it match `(:A:B:C)` as well (TCK
    /// LabelExpressionAcceptance / IsLabeledPredicateAcceptance
    /// `(A&B)&!(B&C)`).
    #[test]
    fn non_conjunctive_label_expression_is_not_folded() {
        for cypher in [
            "MATCH (n) WHERE n:(A&B)&!(B&C) RETURN n",
            "MATCH (n) WHERE n:(A&B)&!(C) RETURN n",
            "MATCH (n) WHERE n:(A|B)&!(C) RETURN n",
            "MATCH (n) WHERE n:(A|B) RETURN n",
            "MATCH (n) WHERE n:!(A) RETURN n",
        ] {
            let (labels, where_clause) = rewritten(cypher);
            assert!(labels.is_empty(), "{cypher}");
            assert!(where_clause.is_some(), "{cypher}");
        }
    }

    /// Folding a conjunct into a pattern that already carries a
    /// disjunction must leave that disjunction in place: rebuilding the
    /// expression from the folded labels alone narrowed
    /// `MATCH (n:A|B) WHERE n:C` to `(:C)`.
    #[test]
    fn fold_keeps_the_patterns_own_label_expression() {
        let (labels, where_clause) = rewritten("MATCH (n:A|B) WHERE n:C RETURN n");
        assert!(labels.is_empty(), "{labels:?}");
        assert!(where_clause.is_some());
    }
}
