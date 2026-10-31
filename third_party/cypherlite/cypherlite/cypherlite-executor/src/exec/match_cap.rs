//! DISTINCT and ordered-limit caps for MATCH expansion.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Result, Value};
use cypherlite_ir::{Clause, Direction, Expr, PathPattern, Projection};

use crate::eval::{eval, is_agg_expr, var_prop, Row};
use crate::group_key::{row_key, GroupKey};
use crate::overlay::ExecGraph;

use super::project::skip_limit_count;

/// Stop MATCH once `RETURN/WITH DISTINCT … LIMIT n` has enough distinct keys.
/// OpenCypher does not specify which rows `LIMIT` keeps without `ORDER BY`.
pub(crate) struct DistinctLimitCap<'a> {
    items: &'a [Projection],
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    seen: HashSet<Vec<GroupKey>>,
    want: usize,
}

impl DistinctLimitCap<'_> {
    fn full(&self) -> bool {
        self.seen.len() >= self.want
    }

    pub(crate) fn offer(&mut self, graph: &ExecGraph<'_>, row: &Row) -> Result<()> {
        if self.full() {
            return Ok(());
        }
        let mut vals = Vec::with_capacity(self.items.len());
        for p in self.items {
            vals.push(eval(graph, row, &p.expr, self.params, self.cypher_version)?);
        }
        self.seen.insert(row_key(&vals));
        Ok(())
    }
}

pub(crate) fn distinct_limit_cap<'a>(
    suffix: &'a [Clause],
    graph: &ExecGraph<'_>,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<DistinctLimitCap<'a>>> {
    let Some(clause) = suffix.first() else {
        return Ok(None);
    };
    let (distinct, group_by, items, order, skip, limit, where_clause) = match clause {
        Clause::Return {
            distinct,
            group_by,
            items,
            order,
            skip,
            limit,
        } => (
            *distinct,
            group_by.as_deref(),
            items,
            order,
            skip,
            limit,
            None,
        ),
        Clause::With {
            distinct,
            group_by,
            items,
            order,
            skip,
            limit,
            where_clause,
            ..
        } => (
            *distinct,
            group_by.as_deref(),
            items,
            order,
            skip,
            limit,
            where_clause.as_ref(),
        ),
        _ => return Ok(None),
    };
    if !distinct
        // CIP-236: an explicit GROUP BY changes what the clause projects
        // (grouped rows, not one row per match), so the per-match cap no
        // longer describes the output.
        || group_by.is_some()
        || !order.is_empty()
        || skip.is_some()
        || where_clause.is_some()
        || items
            .iter()
            .any(|p| matches!(p.expr, Expr::Star) || is_agg_expr(&p.expr))
    {
        return Ok(None);
    }
    let Some(lim) = limit else {
        return Ok(None);
    };
    let empty = Row::new();
    let want = skip_limit_count(&eval(graph, &empty, lim, params, cypher_version)?)?;
    Ok(Some(DistinctLimitCap {
        items,
        params,
        cypher_version,
        seen: HashSet::new(),
        want,
    }))
}

/// Stop MATCH once `ORDER BY rel.ts DESC LIMIT` has `SKIP+LIMIT` visible rows.
/// Walk order is the stored typed-chunk order (ADR-0035).
pub(crate) struct OrderedLimitCap {
    want: usize,
    produced: usize,
}

impl OrderedLimitCap {
    fn full(&self) -> bool {
        self.produced >= self.want
    }
}

pub(crate) enum MatchCap<'a> {
    Distinct(DistinctLimitCap<'a>),
    Ordered(OrderedLimitCap),
}

impl MatchCap<'_> {
    pub(crate) fn full(&self) -> bool {
        match self {
            MatchCap::Distinct(c) => c.full(),
            MatchCap::Ordered(c) => c.full(),
        }
    }

    pub(crate) fn offer(&mut self, graph: &ExecGraph<'_>, row: &Row) -> Result<()> {
        match self {
            MatchCap::Distinct(c) => c.offer(graph, row),
            MatchCap::Ordered(c) => {
                if !c.full() {
                    c.produced += 1;
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn rel_var_of_ts(expr: &Expr) -> Option<&str> {
    var_prop(expr)
        .filter(|(_, key)| *key == "ts")
        .map(|(var, _)| var)
}

pub(crate) fn order_rel_ts_desc_var<'a>(
    order: &'a [cypherlite_ir::OrderItem],
    items: &'a [Projection],
) -> Option<&'a str> {
    let item = order.first()?;
    if order.len() != 1 || item.ascending {
        return None;
    }
    if let Some(v) = rel_var_of_ts(&item.expr) {
        return Some(v);
    }
    if let Expr::Variable(name) = &item.expr {
        for (i, p) in items.iter().enumerate() {
            if p.output_name(i) == *name {
                return rel_var_of_ts(&p.expr);
            }
        }
    }
    None
}

pub(crate) fn ordered_expand_pattern(patterns: &[PathPattern], rel_var: &str) -> bool {
    if patterns.len() != 1 {
        return false;
    }
    let p = &patterns[0];
    if p.rels.len() != 1 {
        return false;
    }
    let rp = &p.rels[0];
    !rp.var_length
        && rp.types.len() == 1
        && rp.direction != Direction::Both
        && rp.variable.as_deref() == Some(rel_var)
}

pub(crate) fn ordered_limit_cap(
    suffix: &[Clause],
    patterns: &[PathPattern],
    graph: &ExecGraph<'_>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    input_rows: usize,
) -> Result<Option<OrderedLimitCap>> {
    if input_rows != 1 || graph.has_write_overlay() {
        return Ok(None);
    }
    let Some(clause) = suffix.first() else {
        return Ok(None);
    };
    let (distinct, group_by, items, order, skip, limit, where_clause) = match clause {
        Clause::Return {
            distinct,
            group_by,
            items,
            order,
            skip,
            limit,
        } => (
            *distinct,
            group_by.as_deref(),
            items,
            order,
            skip,
            limit,
            None,
        ),
        Clause::With {
            distinct,
            group_by,
            items,
            order,
            skip,
            limit,
            where_clause,
            ..
        } => (
            *distinct,
            group_by.as_deref(),
            items,
            order,
            skip,
            limit,
            where_clause.as_ref(),
        ),
        _ => return Ok(None),
    };
    if distinct
        // CIP-236: see `distinct_limit_cap`; grouped output is not one row
        // per match, so an early cap would truncate groups.
        || group_by.is_some()
        || where_clause.is_some()
        || items
            .iter()
            .any(|p| matches!(p.expr, Expr::Star) || is_agg_expr(&p.expr))
    {
        return Ok(None);
    }
    let Some(rel_var) = order_rel_ts_desc_var(order, items) else {
        return Ok(None);
    };
    if !ordered_expand_pattern(patterns, rel_var) {
        return Ok(None);
    }
    let Some(lim) = limit else {
        return Ok(None);
    };
    let empty = Row::new();
    let skip_n = match skip {
        Some(e) => skip_limit_count(&eval(graph, &empty, e, params, cypher_version)?)?,
        None => 0,
    };
    let limit_n = skip_limit_count(&eval(graph, &empty, lim, params, cypher_version)?)?;
    let Some(want) = skip_n.checked_add(limit_n) else {
        return Ok(None);
    };
    if want == 0 {
        return Ok(None);
    }
    Ok(Some(OrderedLimitCap { want, produced: 0 }))
}

pub(crate) fn match_limit_cap<'a>(
    suffix: &'a [Clause],
    patterns: &[PathPattern],
    graph: &ExecGraph<'_>,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    input_rows: usize,
) -> Result<Option<MatchCap<'a>>> {
    if let Some(o) = ordered_limit_cap(suffix, patterns, graph, params, cypher_version, input_rows)?
    {
        return Ok(Some(MatchCap::Ordered(o)));
    }
    Ok(distinct_limit_cap(suffix, graph, params, cypher_version)?.map(MatchCap::Distinct))
}
