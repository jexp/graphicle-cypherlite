//! Projection (`RETURN`/`WITH`): expressions, ORDER BY, DISTINCT,
//! SKIP/LIMIT with top-k selection.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Clause, Expr, Projection};

use crate::ctx::ExecCtx;
use crate::eval::{eval, expr_vars, is_agg_expr, Row, RowNames};
use crate::group_key::{row_key, GroupKey};
use crate::overlay::{ExecGraph, MaterializeCache};

use super::aggregate::aggregate_ex;
use super::scope::{
    has_star_item, is_internal_column, is_star_item, parked_key_name,
    projection_collides_with_internal_names, return_columns, star_scope_names, visible_keys,
};

pub(crate) fn projection_names(items: &[Projection]) -> Vec<String> {
    items
        .iter()
        .enumerate()
        .filter(|(_, p)| !is_star_item(p))
        .map(|(i, p)| p.output_name(i))
        .collect()
}

/// The statement's result columns for a `RETURN`/`WITH`: the shared
/// [`return_columns`] item set, with a `RETURN *`/`WITH *` item expanded
/// from the first row's visible keys — or, when the table is empty, from
/// the declared scope `star_scope_names` walks. This answers the *result*
/// shape the dispatcher materializes; the front's compile-time admission
/// set for the following clause is a different question, asked by
/// `clause_introduced_names` / `call_subquery_exports` in `cypherlite-front`.
pub(crate) fn projection_result_columns(
    items: &[Projection],
    rows: &[Row],
    clauses: &[Clause],
) -> Vec<String> {
    return_columns(items, || match rows.first() {
        None => star_scope_names(clauses),
        Some(row) => {
            let mut keys = visible_keys(row);
            keys.sort();
            keys
        }
    })
}

/// A result column of a rectangular projection row: every projection path
/// binds each result column name on every row by contract, so a missing
/// name is a bug, not a null. The one place that invariant is asserted —
/// shared by the result materialization ([`super::dispatch`]) and the
/// DISTINCT key extraction in [`project_rows`].
pub(crate) fn result_column<'r>(row: &'r Row, name: &str) -> &'r Value {
    row.get(name)
        .expect("projection rows carry every result column")
}

pub(crate) fn order_value(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    items: &[Projection],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Value {
    for (i, p) in items.iter().enumerate() {
        if p.expr == *expr {
            if let Some(v) = row.get(&p.output_name(i)) {
                return v.clone();
            }
        }
    }
    let v = eval(graph, row, expr, params, cypher_version).unwrap_or(Value::Null);
    if v.is_null() {
        if let Expr::Variable(name) = expr {
            return row.get(name).cloned().unwrap_or(Value::Null);
        }
    }
    v
}

/// A following `RETURN`/`WITH` the match sink can evaluate per path.
///
/// Eager `SKIP`/`LIMIT` (no `ORDER BY`, `DISTINCT`, or `WITH` filter) stays
/// on the ordinary projection so a later row's error is not raised (CIP-27).
pub(crate) struct SinkProjection<'a> {
    pub items: &'a [Projection],
    pub order: &'a [cypherlite_ir::OrderItem],
    names: RowNames,
}

impl<'a> SinkProjection<'a> {
    fn new(items: &'a [Projection], order: &'a [cypherlite_ir::OrderItem]) -> Self {
        let mut names = Vec::with_capacity(items.len() + order.len());
        for (index, item) in items.iter().enumerate() {
            names.push(item.output_name(index));
        }
        for index in 0..order.len() {
            names.push(parked_key_name(index));
        }
        Self {
            items,
            order,
            names: RowNames::new(&names),
        }
    }
}

/// The projection a `MATCH` may finish in its sink, when the next clause
/// is a non-aggregate `RETURN` or `WITH`.
pub(crate) fn sinkable_projection(clause: &Clause) -> Option<SinkProjection<'_>> {
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
            items.as_slice(),
            order.as_slice(),
            skip.as_ref(),
            limit.as_ref(),
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
            items.as_slice(),
            order.as_slice(),
            skip.as_ref(),
            limit.as_ref(),
            where_clause.as_ref(),
        ),
        _ => return None,
    };
    if group_by.is_some()
        || items
            .iter()
            .any(|item| is_star_item(item) || is_agg_expr(&item.expr))
        // A user alias in the `__` namespace would collide with the parked
        // `__ord*` sort keys `SinkProjection` writes — the sink declines and
        // the ordinary projection (which parks nothing) keeps the column.
        || projection_collides_with_internal_names(items)
    {
        return None;
    }
    // A `WITH` filter runs later, on the projected row. When it names a
    // variable the projection does not produce, the ordinary path has to
    // keep that binding (pre-projection scope).
    if let Some(predicate) = where_clause {
        let mut named = HashSet::new();
        expr_vars(predicate, &mut named);
        let produced: HashSet<String> = items
            .iter()
            .enumerate()
            .map(|(index, item)| item.output_name(index))
            .collect();
        if named.iter().any(|name| !produced.contains(name)) {
            return None;
        }
    }
    let eager_page = !distinct
        && order.is_empty()
        && where_clause.is_none()
        && (skip.is_some() || limit.is_some());
    if eager_page {
        return None;
    }
    Some(SinkProjection::new(items, order))
}

/// Evaluate `projection` against `row` and keep only those values, plus the
/// order keys the later sort reads. `row` itself is not retained.
pub(crate) fn finish_match_row(
    graph: &ExecGraph<'_>,
    row: &Row,
    projection: &SinkProjection<'_>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Row> {
    let mut values = Vec::with_capacity(projection.names_len());
    // Order keys can name a projected alias (`ORDER BY answer.kind`), so
    // they are read from a row that already holds those values. The clone
    // dies with this call; the stored row is only the projected cells.
    let mut visible = row.clone();
    for (index, item) in projection.items.iter().enumerate() {
        let value = eval(graph, row, &item.expr, params, cypher_version)?;
        visible.insert(item.output_name(index), value.clone());
        values.push(value);
    }
    for item in projection.order {
        values.push(order_value(
            graph,
            &visible,
            &item.expr,
            projection.items,
            params,
            cypher_version,
        ));
    }
    Ok(Row::from_names(&projection.names, values))
}

impl SinkProjection<'_> {
    fn names_len(&self) -> usize {
        self.items.len() + self.order.len()
    }
}

/// Where a `RETURN`/`WITH` projection's incoming rows come from. The sink's
/// caller is the only authority for the projected case: a row carries no
/// marker a user alias could forge, and an ordinary batch whose bindings
/// happen to bind every output name is still evaluated, not passed through.
pub(crate) enum RowSource {
    /// Bindings the projection must evaluate its items against.
    Bindings,
    /// The match sink already evaluated this clause's items on every row
    /// (their columns plus the parked `__ord*` sort keys), so only the
    /// sort, page, DISTINCT, and column-extraction stages are left.
    SinkProjected,
}

/// Field references of a `RETURN`/`WITH` projection clause.
#[derive(Clone, Copy)]
pub(crate) struct ProjectSpec<'a> {
    /// CIP-236 explicit grouping keys (None = infer from non-aggregating
    /// items; empty = `GROUP BY ()` global aggregation).
    pub(crate) group_by: Option<&'a [Expr]>,
    pub items: &'a [Projection],
    pub distinct: bool,
    pub order: &'a [cypherlite_ir::OrderItem],
    pub skip: &'a Option<Expr>,
    pub limit: &'a Option<Expr>,
    pub where_clause: Option<&'a Expr>,
}

pub(crate) fn project_rows(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    spec: &ProjectSpec<'_>,
    inflate: bool,
    source: RowSource,
) -> Result<Vec<Row>> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let ProjectSpec {
        items,
        distinct,
        group_by,
        order,
        skip,
        limit,
        where_clause,
    } = *spec;
    let star = has_star_item(items);
    let has_agg = items.iter().any(|p| is_agg_expr(&p.expr));
    // CIP-236: an explicit GROUP BY takes the aggregation path even without
    // aggregate items (the non-aggregating case acts as DISTINCT).
    let has_agg = has_agg || group_by.is_some();
    let out_names = projection_names(items);
    // Without sorting, filtering, aggregation, or DISTINCT, paging may stop
    // row evaluation. In particular a LIMIT 1 must not evaluate a later row
    // whose projection raises an error (CIP-27).
    let eager_paged = !has_agg
        && !distinct
        && order.is_empty()
        && where_clause.is_none()
        && (skip.is_some() || limit.is_some());
    // The match sink already evaluated these items. Eager paging never takes
    // that path, so this bag is the full input the sort and the page see.
    let preprojected = matches!(source, RowSource::SinkProjected);
    let mut rows = if eager_paged {
        let empty = Row::new();
        let skip_n = skip
            .as_ref()
            .map(|expr| {
                eval(graph, &empty, expr, params, cypher_version).and_then(|v| skip_limit_count(&v))
            })
            .transpose()?
            .unwrap_or(0);
        let limit_n = limit
            .as_ref()
            .map(|expr| {
                eval(graph, &empty, expr, params, cypher_version).and_then(|v| skip_limit_count(&v))
            })
            .transpose()?;
        rows.into_iter()
            .skip(skip_n)
            .take(limit_n.unwrap_or(usize::MAX))
            .collect()
    } else {
        rows
    };
    // One cache for the whole projection: its expressions never write, so
    // the graph state, and with it each payload's refresh, is fixed here.
    let mut cache = MaterializeCache::default();
    let mut refresh = |v: Value| {
        if inflate {
            graph.materialize_cached(v, &mut cache)
        } else {
            v
        }
    };
    let mut merged = if preprojected {
        if inflate {
            for row in &mut rows {
                let keys: Vec<String> = row
                    .keys()
                    .filter(|key| !is_internal_column(key))
                    .cloned()
                    .collect();
                for key in keys {
                    let Some(value) = row.get(&key).cloned() else {
                        continue;
                    };
                    let fresh = refresh(value);
                    if let Some(slot) = row.get_mut(&key) {
                        *slot = fresh;
                    }
                }
            }
        }
        rows
    } else if star && !has_agg {
        rows.into_iter()
            .map(|r| -> Result<Row> {
                let mut n = Row::new();
                for (k, v) in &r {
                    if !is_internal_column(k) {
                        n.insert(k.clone(), refresh(v.clone()));
                    }
                }
                for (index, projection) in items.iter().enumerate() {
                    if is_star_item(projection) {
                        continue;
                    }
                    let value = eval(graph, &r, &projection.expr, params, cypher_version)?;
                    n.insert(projection.output_name(index), refresh(value));
                }
                Ok(n)
            })
            .collect::<Result<Vec<_>>>()?
    } else if has_agg {
        let mut agg = aggregate_ex(graph, rows, items, group_by, params, cypher_version)?;
        if inflate {
            for row in &mut agg {
                let keys: Vec<_> = row.keys().cloned().collect();
                for k in keys {
                    if let Some(v) = row.shift_remove(&k) {
                        row.insert(k, refresh(v));
                    }
                }
            }
        }
        agg
    } else {
        let mut keep = HashSet::new();
        if let Some(w) = where_clause {
            expr_vars(w, &mut keep);
        }
        for o in order {
            expr_vars(&o.expr, &mut keep);
        }
        let mut out = Vec::new();
        for mut row in rows {
            let mut row2 = Row::new();
            let projected: Vec<(String, Value)> = items
                .iter()
                .enumerate()
                .filter(|(_, projection)| !is_star_item(projection))
                .map(|(i, projection)| {
                    let name = projection.output_name(i);
                    let val = eval(graph, &row, &projection.expr, params, cypher_version)?;
                    Ok((name, refresh(val)))
                })
                .collect::<Result<Vec<_>>>()?;
            let kept: Vec<String> = row
                .keys()
                .filter(|name| keep.contains(name.as_str()))
                .cloned()
                .collect();
            for name in kept {
                if let Some(value) = row.take_value(&name) {
                    row2.insert(name, value);
                }
            }
            for (name, value) in projected {
                row2.insert(name, value);
            }
            out.push(row2);
        }
        out
    };

    if let Some(w) = where_clause {
        merged.retain(|r| {
            eval(graph, r, w, params, cypher_version)
                .ok()
                .and_then(|v| v.truthy())
                == Some(true)
        });
    }

    let empty = Row::new();
    let skip_n = match skip.as_ref().filter(|_| !eager_paged) {
        Some(e) => skip_limit_count(&eval(graph, &empty, e, params, cypher_version)?)?,
        None => 0,
    };
    let limit_n = match limit.as_ref().filter(|_| !eager_paged) {
        Some(e) => Some(skip_limit_count(&eval(
            graph,
            &empty,
            e,
            params,
            cypher_version,
        )?)?),
        None => None,
    };

    if !order.is_empty() {
        let top_k = match (distinct, limit_n) {
            (false, Some(lim)) => skip_n.checked_add(lim),
            _ => None,
        };
        if preprojected {
            if let Some(keep) = top_k {
                if keep == 0 {
                    merged.clear();
                } else if keep < merged.len() {
                    merged = take_parked_top_k(order, merged, keep)?;
                } else {
                    sort_rows_by_parked_keys(order, &mut merged)?;
                }
            } else {
                sort_rows_by_parked_keys(order, &mut merged)?;
            }
        } else if let Some(keep) = top_k {
            if keep == 0 {
                merged.clear();
            } else if keep < merged.len() {
                merged =
                    take_ordered_top_k(graph, items, params, order, merged, keep, cypher_version)?;
            } else {
                sort_rows_by_order(graph, items, params, order, &mut merged, cypher_version)?;
            }
        } else {
            sort_rows_by_order(graph, items, params, order, &mut merged, cypher_version)?;
        }
    }

    let mut retained_rows = cypherlite_core::tokens::WorkspaceCharge::empty();
    if distinct {
        retained_rows = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
            crate::workspace::row_buffer_bytes(&merged)
        })?;
        let mut seen: HashSet<Vec<GroupKey>> = HashSet::new();
        let mut distinct_charge = cypherlite_core::tokens::WorkspaceCharge::empty();
        let account = cypherlite_core::tokens::workspace_accounting();
        let mut kept = Vec::new();
        for row in merged {
            let names = if star {
                visible_keys(&row)
            } else {
                out_names.clone()
            };
            let key = row_key(
                &names
                    .iter()
                    .map(|n| result_column(&row, n).clone())
                    .collect::<Vec<_>>(),
            );
            let delta = if account {
                crate::workspace::distinct_row_insert_delta(seen.len(), &key)
            } else {
                0
            };
            if seen.insert(key) {
                if account {
                    distinct_charge.resize(distinct_charge.bytes().saturating_add(delta))?;
                    cypherlite_core::tokens::peak_hold("distinct");
                }
                kept.push(row);
            }
        }
        drop(distinct_charge);
        merged = kept;
        retained_rows.resize_with(|| crate::workspace::row_buffer_bytes(&merged))?;
    }

    if skip_n > 0 {
        if skip_n >= merged.len() {
            merged.clear();
        } else {
            merged = merged.split_off(skip_n);
        }
    }
    if let Some(n) = limit_n {
        merged.truncate(n);
    }
    if distinct {
        retained_rows.resize_with(|| crate::workspace::row_buffer_bytes(&merged))?;
    }

    if star {
        drop(retained_rows);
        return Ok(merged);
    }
    let names = RowNames::new(&out_names);
    let mut out = Vec::with_capacity(merged.len());
    for mut row in merged {
        let values = out_names
            .iter()
            .map(|name| {
                row.take_value(name)
                    .expect("projection rows carry every result column")
            })
            .collect();
        out.push(Row::from_names(&names, values));
    }
    if distinct {
        retained_rows.resize_with(|| crate::workspace::row_buffer_bytes(&out))?;
    }
    drop(retained_rows);
    Ok(out)
}

pub(crate) fn cmp_sort_keys(
    order: &[cypherlite_ir::OrderItem],
    ka: &[Value],
    ia: usize,
    kb: &[Value],
    ib: usize,
) -> CmpOrdering {
    for (i, o) in order.iter().enumerate() {
        match ka[i].order_by_cmp(&kb[i]) {
            CmpOrdering::Equal => {}
            ord => return if o.ascending { ord } else { ord.reverse() },
        }
    }
    ia.cmp(&ib)
}

pub(crate) fn row_sort_keys(
    graph: &ExecGraph<'_>,
    items: &[Projection],
    params: &BTreeMap<String, Value>,
    order: &[cypherlite_ir::OrderItem],
    row: &Row,
    cypher_version: Option<u32>,
) -> Vec<Value> {
    order
        .iter()
        .map(|o| order_value(graph, row, &o.expr, items, params, cypher_version))
        .collect()
}

/// Apply a catalog clause's own `YIELD` paging stages (`ORDER BY`/`SKIP`/
/// `LIMIT`), in written order, to the composed rows. The stage interpreter
/// is the same one ordinary projections use for their paging sub-grammar;
/// it lives here beside those helpers so `project_catalog` stays lookup →
/// prefill → project → filter.
pub(crate) fn apply_yield_page_stages(
    graph: &ExecGraph<'_>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    stages: &[cypherlite_parser::PageStage],
    output: &mut Vec<Row>,
) -> Result<()> {
    for stage in stages {
        match stage {
            cypherlite_parser::PageStage::OrderBy(items) => {
                let order = items
                    .iter()
                    .map(|item| {
                        Ok(cypherlite_ir::OrderItem {
                            expr: cypherlite_front::lower_expr(&item.expr)?,
                            ascending: !item.descending,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                sort_rows_by_order(
                    graph,
                    &[],
                    params,
                    &order,
                    output.as_mut_slice(),
                    cypher_version,
                )?;
            }
            cypherlite_parser::PageStage::Skip(expr) => {
                let count = skip_limit_count(&eval(
                    graph,
                    &Row::new(),
                    &cypherlite_front::lower_expr(expr)?,
                    params,
                    cypher_version,
                )?)?;
                *output = std::mem::take(output).into_iter().skip(count).collect();
            }
            cypherlite_parser::PageStage::Limit(expr) => {
                let count = skip_limit_count(&eval(
                    graph,
                    &Row::new(),
                    &cypherlite_front::lower_expr(expr)?,
                    params,
                    cypher_version,
                )?)?;
                output.truncate(count);
            }
        }
    }
    Ok(())
}

pub(crate) fn sort_rows_by_order(
    graph: &ExecGraph<'_>,
    items: &[Projection],
    params: &BTreeMap<String, Value>,
    order: &[cypherlite_ir::OrderItem],
    rows: &mut [Row],
    cypher_version: Option<u32>,
) -> Result<()> {
    let _rows = hold_sort_rows(rows)?;
    let mut keyed: Vec<(Vec<Value>, usize)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            (
                row_sort_keys(graph, items, params, order, r, cypher_version),
                i,
            )
        })
        .collect();
    let _keys = hold_sort_keys(keyed.iter().map(|(keys, _)| keys.as_slice()))?;
    keyed.sort_by(|a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    let mut slot: Vec<Option<Row>> = rows.iter_mut().map(|r| Some(std::mem::take(r))).collect();
    for (dest, (_, src)) in keyed.into_iter().enumerate() {
        rows[dest] = slot[src].take().expect("row");
    }
    Ok(())
}

/// Reserve the row buffer, then sample RSS while those rows are still the
/// live workspace and before the key vector is built.
fn hold_sort_rows(rows: &[Row]) -> Result<cypherlite_core::tokens::WorkspaceCharge> {
    let charge = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
        crate::workspace::row_buffer_bytes(rows)
    })?;
    cypherlite_core::tokens::peak_hold("sort");
    Ok(charge)
}

fn hold_sort_keys<'a>(
    keys: impl IntoIterator<Item = &'a [Value]>,
) -> Result<cypherlite_core::tokens::WorkspaceCharge> {
    let charge = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
        crate::workspace::sort_key_columns(keys)
    })?;
    cypherlite_core::tokens::peak_hold("sort");
    Ok(charge)
}

fn parked_keys(row: &Row, order: &[cypherlite_ir::OrderItem]) -> Vec<Value> {
    order
        .iter()
        .enumerate()
        .map(|(index, _)| {
            row.get(&parked_key_name(index))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect()
}

fn sort_rows_by_parked_keys(order: &[cypherlite_ir::OrderItem], rows: &mut [Row]) -> Result<()> {
    let _rows = hold_sort_rows(rows)?;
    let mut keyed: Vec<(Vec<Value>, usize)> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (parked_keys(row, order), index))
        .collect();
    let _keys = hold_sort_keys(keyed.iter().map(|(keys, _)| keys.as_slice()))?;
    keyed.sort_by(|a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    let mut slot: Vec<Option<Row>> = rows
        .iter_mut()
        .map(|row| Some(std::mem::take(row)))
        .collect();
    for (dest, (_, src)) in keyed.into_iter().enumerate() {
        rows[dest] = slot[src].take().expect("row");
    }
    Ok(())
}

fn take_parked_top_k(
    order: &[cypherlite_ir::OrderItem],
    rows: Vec<Row>,
    keep: usize,
) -> Result<Vec<Row>> {
    let mut rows_charge = hold_sort_rows(&rows)?;
    let mut keyed: Vec<(Vec<Value>, usize, Row)> = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let keys = parked_keys(&row, order);
            (keys, index, row)
        })
        .collect();
    let mut keys_charge = hold_sort_keys(keyed.iter().map(|(keys, _, _)| keys.as_slice()))?;
    keyed.select_nth_unstable_by(keep - 1, |a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    keyed.truncate(keep);
    keyed.sort_by(|a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    rows_charge
        .resize_with(|| crate::workspace::row_refs_bytes(keyed.iter().map(|(_, _, row)| row)))?;
    keys_charge.resize_with(|| {
        crate::workspace::sort_key_columns(keyed.iter().map(|(keys, _, _)| keys.as_slice()))
    })?;
    drop(keys_charge);
    drop(rows_charge);
    Ok(keyed.into_iter().map(|(_, _, row)| row).collect())
}

pub(crate) fn take_ordered_top_k(
    graph: &ExecGraph<'_>,
    items: &[Projection],
    params: &BTreeMap<String, Value>,
    order: &[cypherlite_ir::OrderItem],
    rows: Vec<Row>,
    keep: usize,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    let mut rows_charge = hold_sort_rows(&rows)?;
    let mut keyed: Vec<(Vec<Value>, usize, Row)> = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let keys = row_sort_keys(graph, items, params, order, &r, cypher_version);
            (keys, i, r)
        })
        .collect();
    let mut keys_charge = hold_sort_keys(keyed.iter().map(|(keys, _, _)| keys.as_slice()))?;
    keyed.select_nth_unstable_by(keep - 1, |a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    keyed.truncate(keep);
    keyed.sort_by(|a, b| cmp_sort_keys(order, &a.0, a.1, &b.0, b.1));
    rows_charge
        .resize_with(|| crate::workspace::row_refs_bytes(keyed.iter().map(|(_, _, row)| row)))?;
    keys_charge.resize_with(|| {
        crate::workspace::sort_key_columns(keyed.iter().map(|(keys, _, _)| keys.as_slice()))
    })?;
    drop(keys_charge);
    drop(rows_charge);
    Ok(keyed.into_iter().map(|(_, _, row)| row).collect())
}

pub(crate) fn skip_limit_count(v: &Value) -> Result<usize> {
    // ADR-0175: every FLOAT value is rejected — including 1.0, NaN and
    // ±Infinity (Neo4j's runtime gate rejects every float, 22N03); no
    // truncation.
    let n = match v {
        Value::Int(i) => *i,
        _ => {
            return Err(Error::invalid_argument_type(
                "SKIP/LIMIT must be an integer",
            ))
        }
    };
    if n < 0 {
        return Err(Error::execution(
            "NegativeIntegerArgument: SKIP/LIMIT must be non-negative",
        ));
    }
    Ok(n as usize)
}
