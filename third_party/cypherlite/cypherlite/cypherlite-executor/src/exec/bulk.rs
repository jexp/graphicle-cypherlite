//! Batched execution of global bulk writes (ADR-0209, LABS-263).
//!
//! A statement like `MATCH (n) DETACH DELETE n` or
//! `MATCH (n) MATCH (m) WITH n AS a, m AS b CREATE (a)-[:TAGGED]->(b)`
//! has no selective anchor: it touches every matched entity, and running
//! it as one giant transaction materializes the whole redo in a single
//! scratch and lands it as a single WAL record (which can cross the
//! 64 MiB payload limit or exhaust memory). This module detects those
//! shapes at plan time — conservatively, only when per-row effects are
//! independent of batch boundaries — and provides the pieces the
//! auto-commit path (`cypherlite::commit`) needs to run them in bounded
//! batches, one WAL commit per batch:
//!
//! - [`analyze_bulk_write`] — plan-shape detection (this file);
//! - [`BulkWrite::collect_targets`] — phase 1: snapshot the matched
//!   entity id space (memory O(entities), never O(cross product));
//! - [`BulkWrite::execute_chunk`] — phase 2: apply up to `budget` work
//!   units starting at `cursor`, on the caller's scratch.
//!
//! The two phases live in [`run`]; this file holds the plan analysis.
//!
//! Semantics: the matched set is fixed before the first batch writes,
//! and every eligible write's per-row effect is independent of when
//! sibling rows ran, so the final state and the summed write statistics
//! equal the unbatched run. On the error path earlier batches stay
//! committed — the documented `CALL { } IN TRANSACTIONS` trade-off. One
//! error is preflighted before the first commit: a non-`DETACH` node
//! `DELETE`'s `DeleteConnectedNode` check. That keeps the statement
//! atomic for this error only as long as no concurrent writer adds a
//! relationship after the preflight snapshot; such a race surfaces as a
//! mid-run error with earlier batches committed, like any other
//! mid-statement failure.

use std::collections::HashSet;

use cypherlite_ir::{Clause, Expr, PathPattern, RemoveItem, SetItem};

use crate::graph::Graph;
use cypherlite_planner::LogicalPlan;

use super::label_expr_ok;

mod run;

pub use run::{BulkCursor, BulkTargets};

#[cfg(test)]
mod tests;

/// Default work estimate at or above which an eligible statement is
/// batched instead of running as one transaction.
pub const DEFAULT_BATCH_THRESHOLD: u64 = 10_000;
/// Default per-batch work budget (estimated touched entities).
pub const DEFAULT_BATCH_BUDGET: u64 = 25_000;

/// One matched variable's identity source: a bare node pattern
/// `(<var> [:labels…])` or a bare relationship pattern
/// `()-[<var> [:types…]]->()`.
#[derive(Debug, Clone)]
pub(super) enum FactorSource {
    Nodes { labels: Vec<String> },
    Rels { types: Vec<String> },
}

/// A matched variable and where its entities come from.
#[derive(Debug, Clone)]
pub(super) struct Factor {
    var: String,
    source: FactorSource,
}

/// The write clause of an eligible statement, plus what it may
/// reference: one variant per write flavor, carrying the payload the
/// execution arm needs so execution matches one enum exactly once.
#[derive(Debug, Clone)]
pub(super) enum BulkKind {
    Delete {
        detach: bool,
        /// The variables the `DELETE` names; factors not named keep
        /// their entities.
        targets: HashSet<String>,
    },
    Set {
        items: Vec<SetItem>,
    },
    Remove {
        items: Vec<RemoveItem>,
    },
    Create {
        patterns: Vec<PathPattern>,
    },
}

/// A plan-time eligible global bulk write (ADR-0209 §1).
#[derive(Debug, Clone)]
pub struct BulkWrite {
    pub(super) kind: BulkKind,
    pub(super) factors: Vec<Factor>,
    pub(super) cypher_version: Option<u32>,
}

/// Whether `pattern` is a bare node pattern `(<var> [:labels…])`: named,
/// no relationship, no properties, no predicates anywhere, no label
/// expression, no path variable, no GPM-only features.
fn bare_node_factor(pattern: &PathPattern) -> Option<Factor> {
    if pattern.variable.is_some()
        || !pattern.rels.is_empty()
        || pattern.nodes.len() != 1
        || pattern.needs_gpm()
    {
        return None;
    }
    let node = &pattern.nodes[0];
    if !label_expr_ok(&node.label_expr, &node.labels)
        || node.properties.is_some()
        || node.where_clause.is_some()
    {
        return None;
    }
    Some(Factor {
        var: node.variable.clone()?,
        source: FactorSource::Nodes {
            labels: node.labels.clone(),
        },
    })
}

/// Whether `pattern` is a bare relationship pattern
/// `()-[<var> [:types…]]->()`: both endpoints anonymous and bare, the
/// relationship named, fixed length, no properties or predicates.
fn bare_rel_factor(pattern: &PathPattern) -> Option<Factor> {
    if pattern.variable.is_some()
        || pattern.nodes.len() != 2
        || pattern.rels.len() != 1
        || pattern.needs_gpm()
    {
        return None;
    }
    for node in &pattern.nodes {
        if node.variable.is_some()
            || !node.labels.is_empty()
            || node.label_expr.is_some()
            || node.properties.is_some()
            || node.where_clause.is_some()
        {
            return None;
        }
    }
    let rel = &pattern.rels[0];
    if rel.var_length
        || rel.quantified
        || !label_expr_ok(&rel.type_expr, &rel.types)
        || rel.properties.is_some()
        || rel.where_clause.is_some()
    {
        return None;
    }
    Some(Factor {
        var: rel.variable.clone()?,
        source: FactorSource::Rels {
            types: rel.types.clone(),
        },
    })
}

/// The variable a `SET`/`REMOVE` target expression roots at:
/// `n`, `n.k`, or `n[k]` with `n` a plain variable.
fn target_root_var(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Variable(name) => Some(name),
        Expr::Property { expr, .. } | Expr::DynamicProperty { expr, .. } => match expr.as_ref() {
            Expr::Variable(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

/// The variables `expr` references, via the IR walker.
fn referenced_vars(expr: &Expr) -> HashSet<String> {
    let mut out = HashSet::new();
    cypherlite_ir::expr_vars(expr, &mut out);
    out
}

/// `expr` references no variable outside `allowed`.
fn refs_within(expr: &Expr, allowed: &HashSet<String>) -> bool {
    referenced_vars(expr).is_subset(allowed)
}

/// Shared per-item eligibility rule for `SET`/`REMOVE`: every target
/// roots at a matched variable, and every value/dynamic-key expression
/// references at most that same variable (per-row effects then cannot
/// depend on when sibling rows ran — ADR-0209 §1). Each item arrives as
/// its target expression plus the value/dynamic-key expressions to
/// scope-check.
fn per_item_eligible<'a>(
    items: impl IntoIterator<Item = (&'a Expr, Vec<&'a Expr>)>,
    scope: &HashSet<String>,
) -> bool {
    items.into_iter().all(|(target, value_exprs)| {
        let Some(root) = target_root_var(target) else {
            return false;
        };
        if !scope.contains(root) {
            return false;
        }
        let allowed = HashSet::from([root.to_string()]);
        value_exprs.iter().all(|expr| refs_within(expr, &allowed))
    })
}

/// Whether the `SET` items are batch-safe (see [`per_item_eligible`]).
fn set_items_eligible(items: &[SetItem], scope: &HashSet<String>) -> bool {
    !items.is_empty()
        && per_item_eligible(
            items.iter().map(|item| match item {
                SetItem::Property { target, value } => {
                    let mut exprs = vec![value];
                    if let Expr::DynamicProperty { key, .. } = target {
                        exprs.push(key);
                    }
                    (target, exprs)
                }
                SetItem::MergeMap { target, value } => (target, vec![value]),
                SetItem::Labels { target, .. } => (target, Vec::new()),
                SetItem::LabelsFunction { target, value, .. } => (target, vec![value]),
            }),
            scope,
        )
}

/// Whether the `REMOVE` items are batch-safe (same target rule; the only
/// expressions are dynamic property keys).
fn remove_items_eligible(items: &[RemoveItem], scope: &HashSet<String>) -> bool {
    !items.is_empty()
        && per_item_eligible(
            items.iter().map(|item| match item {
                RemoveItem::Property { target } => {
                    let exprs = match target {
                        Expr::DynamicProperty { key, .. } => vec![key.as_ref()],
                        _ => Vec::new(),
                    };
                    (target, exprs)
                }
                RemoveItem::Labels { target, .. } => (target, Vec::new()),
            }),
            scope,
        )
}

/// Whether the `CREATE` patterns are batch-safe: no graph-sensitive
/// expressions, compact (non-GPM) patterns, matched variables appear
/// only as bare node endpoints, and property expressions reference only
/// matched variables.
fn create_patterns_eligible(patterns: &[PathPattern], scope: &HashSet<String>) -> bool {
    !patterns.is_empty()
        && !super::create::patterns_have_graph_sensitive_expr(patterns)
        && patterns.iter().all(|pattern| {
            if pattern.variable.is_some() || pattern.needs_gpm() {
                return false;
            }
            pattern.nodes.iter().all(|node| {
                let endpoint_ok = match &node.variable {
                    // A matched variable is a bare endpoint: no labels,
                    // properties, or predicates of its own.
                    Some(var) if scope.contains(var) => {
                        node.labels.is_empty()
                            && node.label_expr.is_none()
                            && node.properties.is_none()
                            && node.where_clause.is_none()
                    }
                    // A fresh (or anonymous) variable: created here.
                    _ => {
                        node.properties
                            .as_ref()
                            .is_none_or(|p| refs_within(p, scope))
                            && label_expr_ok(&node.label_expr, &node.labels)
                            && node.where_clause.is_none()
                    }
                };
                endpoint_ok
            }) && pattern.rels.iter().all(|rel| {
                !rel.var_length
                    && !rel.quantified
                    && label_expr_ok(&rel.type_expr, &rel.types)
                    && rel.where_clause.is_none()
                    && rel
                        .properties
                        .as_ref()
                        .is_none_or(|p| refs_within(p, scope))
            })
        })
}

/// Whether the `DELETE` clause is batch-safe, and its kind when it is:
/// every expression must be a plain variable naming a matched factor
/// (one walk collects the named factors and classifies their sources),
/// and — unless `DETACH` — the targets may not mix nodes and
/// relationships: the connectedness preflight would have to ignore the
/// statement's own relationship targets, so that shape stays on the
/// unbatched path.
fn delete_kind(detach: bool, expressions: &[Expr], factors: &[Factor]) -> Option<BulkKind> {
    if expressions.is_empty() {
        return None;
    }
    let mut named = HashSet::with_capacity(expressions.len());
    let mut touches_nodes = false;
    let mut touches_rels = false;
    for expr in expressions {
        let Expr::Variable(name) = expr else {
            return None;
        };
        let factor = factors.iter().find(|f| f.var == *name)?;
        match factor.source {
            FactorSource::Nodes { .. } => touches_nodes = true,
            FactorSource::Rels { .. } => touches_rels = true,
        }
        named.insert(name.clone());
    }
    if !detach && touches_nodes && touches_rels {
        return None;
    }
    Some(BulkKind::Delete {
        detach,
        targets: named,
    })
}

/// A pure-rename `WITH` clause: whether it carries `*`, plus the
/// old → new name pair of each explicit variable item.
struct RenameProjection {
    star: bool,
    renames: Vec<(String, String)>,
}

/// Whether a `WITH` clause is a pure rename projection: no aggregation,
/// distinct, ordering, paging, or filter, and every item a variable or
/// `*`. `None` when the clause is not a pure rename.
fn with_rename_projection(clause: &Clause) -> Option<RenameProjection> {
    let Clause::With {
        distinct,
        group_by,
        items,
        order,
        skip,
        limit,
        where_clause,
        ..
    } = clause
    else {
        return None;
    };
    if *distinct
        || group_by.is_some()
        || !order.is_empty()
        || skip.is_some()
        || limit.is_some()
        || where_clause.is_some()
    {
        return None;
    }
    let mut star = false;
    let mut renames = Vec::new();
    for item in items {
        match &item.expr {
            Expr::Star => star = true,
            Expr::Variable(name) => {
                renames.push((
                    name.clone(),
                    item.alias.clone().unwrap_or_else(|| name.clone()),
                ));
            }
            _ => return None,
        }
    }
    Some(RenameProjection { star, renames })
}

/// Apply one pure-rename `WITH` to the surviving factors, following the
/// clause's simultaneous-assignment semantics: every item reads the
/// incoming scope, so each factor's name is rewritten by at most one
/// item (its first match), and a factor no item mentions survives only
/// under `*`. Returns `None` when the projection would make the scope
/// ambiguous for batched targeting: `*` mixed with a real rename binds
/// two names to one entity, and an alias colliding with another
/// surviving factor's name shadows it.
fn apply_rename_projection(factors: &mut Vec<Factor>, projection: &RenameProjection) -> Option<()> {
    if projection.star && projection.renames.iter().any(|(old, new)| old != new) {
        return None;
    }
    let mut kept = Vec::with_capacity(factors.len());
    for mut factor in factors.drain(..) {
        match projection
            .renames
            .iter()
            .find(|(old, _)| *old == factor.var)
        {
            Some((_, new)) => factor.var = new.clone(),
            None if projection.star => {}
            None => continue,
        }
        kept.push(factor);
    }
    *factors = kept;
    let names: HashSet<&str> = factors.iter().map(|f| f.var.as_str()).collect();
    (names.len() == factors.len()).then_some(())
}

/// Plan-time detection of a batchable global bulk write (ADR-0209 §1).
///
/// Conservative by construction: any deviation from the recognized
/// shapes — a `WHERE`, an aggregating `WITH`, a graph-sensitive `CREATE`
/// expression, a `RETURN` after the write, multiple parts, `UNION`,
/// `NEXT` — yields `None` and the statement runs unbatched.
impl BulkWrite {
    /// An O(1) upper bound on the touched-entity count, from the label
    /// index (plan-time counts, ADR-0218): product of per-factor upper
    /// bounds for row writes, sum over the named factors for a delete.
    /// `None` when a factor has no cheap bound (unlabeled rels) — the
    /// caller then pays the real collection. Overestimates are safe
    /// (the cost is collecting anyway); underestimates would wrongly
    /// skip batching, so every arm errs upward.
    pub fn estimate_upper_bound(&self, graph: &Graph) -> Option<u64> {
        let factor_bound = |f: &Factor| -> Option<u64> {
            match &f.source {
                FactorSource::Nodes { labels } if !labels.is_empty() => {
                    // intersection ≤ smallest label count
                    labels.iter().map(|l| graph.label_count(l)).min()
                }
                FactorSource::Nodes { .. } => Some(graph.live_node_count() as u64),
                // no cheap rel-type count index: decline to estimate
                FactorSource::Rels { .. } => None,
            }
        };
        match &self.kind {
            // detach work is entities + their incident rels (1 + degree per
            // node) — label counts give no cheap bound, so decline and let
            // phase 1's exact total_work() decide.
            BulkKind::Delete { detach: true, .. } => None,
            BulkKind::Delete { targets, .. } => self
                .factors
                .iter()
                .filter(|f| targets.contains(&f.var))
                .try_fold(0u64, |acc, f| {
                    factor_bound(f).map(|b| acc.saturating_add(b))
                }),
            BulkKind::Set { .. } | BulkKind::Remove { .. } | BulkKind::Create { .. } => {
                self.factors.iter().try_fold(1u64, |acc, f| {
                    factor_bound(f).map(|b| acc.saturating_mul(b))
                })
            }
        }
    }
}

pub fn analyze_bulk_write(plan: &LogicalPlan) -> Option<BulkWrite> {
    if plan.parts.len() != 1 || !plan.union_all.is_empty() || plan.next.is_some() {
        return None;
    }
    let clauses = &plan.parts[0];
    let mut factors: Vec<Factor> = Vec::new();
    let mut i = 0;
    while let Some(Clause::Match {
        optional,
        patterns,
        where_clause,
        search,
        ..
    }) = clauses.get(i)
    {
        if *optional || where_clause.is_some() || search.is_some() || patterns.is_empty() {
            return None;
        }
        for pattern in patterns {
            factors.push(bare_node_factor(pattern).or_else(|| bare_rel_factor(pattern))?);
        }
        i += 1;
    }
    if factors.is_empty() {
        return None;
    }
    // A repeated variable across patterns is a self-join in the
    // unbatched engine, not a cross product of independent factors.
    if factors.iter().map(|f| &f.var).collect::<HashSet<_>>().len() != factors.len() {
        return None;
    }
    // Pure-rename WITHs only; the write clause references the final names.
    while let Some(clause @ Clause::With { .. }) = clauses.get(i) {
        let projection = with_rename_projection(clause)?;
        apply_rename_projection(&mut factors, &projection)?;
        i += 1;
    }
    // Exactly one terminal write clause; nothing may follow it.
    if i + 1 != clauses.len() {
        return None;
    }
    let scope: HashSet<String> = factors.iter().map(|f| f.var.clone()).collect();
    let kind = match &clauses[i] {
        Clause::Delete {
            detach,
            expressions,
        } => delete_kind(*detach, expressions, &factors)?,
        Clause::Set { items } if set_items_eligible(items, &scope) => BulkKind::Set {
            items: items.clone(),
        },
        Clause::Remove { items } if remove_items_eligible(items, &scope) => BulkKind::Remove {
            items: items.clone(),
        },
        Clause::Create { patterns } if create_patterns_eligible(patterns, &scope) => {
            BulkKind::Create {
                patterns: patterns.clone(),
            }
        }
        _ => return None,
    };
    Some(BulkWrite {
        kind,
        factors,
        cypher_version: plan.cypher_version,
    })
}
