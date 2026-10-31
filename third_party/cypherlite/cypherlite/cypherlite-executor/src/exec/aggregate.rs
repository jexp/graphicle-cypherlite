//! Grouped aggregation (`aggregate` + `agg_eval`).

use std::borrow::Cow;
use std::collections::BTreeMap;

use cypherlite_core::temporal::{self, Temporal};
use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Expr, Projection};
use indexmap::map::Entry;
use indexmap::IndexMap;

use crate::eval::{eval, is_agg_expr, Row};
use crate::group_key::GroupKey;
use crate::overlay::ExecGraph;

use super::scope::{has_star_item, is_internal_column, is_star_item, visible_keys};

/// CIP-236: `group_by` carries explicit grouping keys when a GROUP BY
/// clause is present. That is the *only* difference from the implicit
/// form: the keys come from those expressions instead of being inferred
/// from the non-aggregating projection items. `Some(&[])` is the empty
/// grouping set (`GROUP BY ()`): one global group.
pub(crate) fn aggregate_ex(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    items: &[Projection],
    group_by: Option<&[Expr]>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    // The grouping strategy is loop-invariant: decide once whether the key
    // comes from the explicit `group_by` expressions or from the inferred
    // non-aggregating projection items, then loop over rows.
    let key_exprs = desugar_star_keys(
        match group_by {
            Some(keys) => keys.iter().map(Cow::Borrowed).collect(),
            None => items
                .iter()
                .filter(|p| !is_agg_expr(&p.expr))
                .map(|p| Cow::Borrowed(&p.expr))
                .collect(),
        },
        &rows,
    );
    // One row for an empty input only when there are no grouping keys at
    // all: implicit grouping over an all-aggregate projection, an explicit
    // empty grouping set (`GROUP BY ()`), or an explicit empty `ALL` (which
    // lowers to no keys). An explicit non-empty key set has no group to emit
    // for an empty input, so it yields zero rows -- emitting the phantom
    // global group there would invent a row with no key values. A `*` item
    // counts as a key: it carries every in-scope variable (ADR-0198).
    let keyless = key_exprs.is_empty();
    // ADR-0198 ruling 1: with aggregation in play, a `*` item groups by all
    // of the row's carried variables and projects them through to the
    // output row alongside the aggregate aliases.
    let star = has_star_item(items);
    let mut groups: IndexMap<Vec<GroupKey>, Vec<Row>> = IndexMap::new();
    let mut groups_charge = cypherlite_core::tokens::WorkspaceCharge::empty();
    let account = cypherlite_core::tokens::workspace_accounting();
    let mut meter = crate::workspace::GroupMapMeter::default();
    // The input vector stays allocated until this loop finishes. Bill it up
    // front, then hand each row's payload to the group map as it moves.
    let mut retained = account.then(|| crate::workspace::RetainedRows::measure(&rows));
    let mut input_charge = match &retained {
        Some(retained) => {
            let charge = cypherlite_core::tokens::WorkspaceCharge::retain(retained.bytes())?;
            cypherlite_core::tokens::peak_hold("aggregation");
            charge
        }
        None => cypherlite_core::tokens::WorkspaceCharge::empty(),
    };
    for row in rows {
        graph.cancel_tick()?;
        // Star-free after the desugar above: one `eval` per key, and the
        // capacity is exact.
        let mut key = Vec::with_capacity(key_exprs.len());
        for k in &key_exprs {
            key.push(GroupKey::new(&eval(
                graph,
                &row,
                k,
                params,
                cypher_version,
            )?));
        }
        if let Some(retained) = retained.as_mut() {
            retained.release_row(&row);
            input_charge.resize(retained.bytes())?;
        }
        if account {
            match groups.entry(key) {
                Entry::Occupied(mut entry) => {
                    meter.add_row(entry.index(), &row);
                    groups_charge.resize(meter.bytes)?;
                    cypherlite_core::tokens::peak_hold("aggregation");
                    entry.get_mut().push(row);
                }
                Entry::Vacant(entry) => {
                    meter.add_group(entry.key(), &row);
                    groups_charge.resize(meter.bytes)?;
                    cypherlite_core::tokens::peak_hold("aggregation");
                    entry.insert(vec![row]);
                }
            }
        } else {
            groups.entry(key).or_default().push(row);
        }
    }
    drop(input_charge);
    if groups.is_empty() && keyless {
        groups.insert(Vec::new(), Vec::new());
        if account {
            meter.add_empty_group();
            groups_charge.resize(meter.bytes)?;
            cypherlite_core::tokens::peak_hold("aggregation");
        }
    }
    let mut out = Vec::new();
    for (_, group) in groups {
        let mut n = group.first().cloned().unwrap_or_default();
        if star {
            // The variables `*` carries are already in the first-row seed;
            // keep the internal `__*` names out of the projected row, the
            // same filter the non-aggregating `*` path applies.
            n.retain(|name, _| !is_internal_column(name));
        }
        for (i, p) in items.iter().enumerate() {
            if is_star_item(p) {
                continue;
            }
            let name = p.output_name(i);
            let val = if is_agg_expr(&p.expr) {
                agg_eval(graph, &group, &p.expr, params, cypher_version)?
            } else if let Some(row) = group.first() {
                eval(graph, row, &p.expr, params, cypher_version)?
            } else {
                Value::Null
            };
            n.insert(name, val);
        }
        out.push(n);
    }
    drop(groups_charge);
    Ok(out)
}

/// ADR-0198 ruling 1: desugar a `*` key ONCE, before the row loop, into
/// one `Expr::Variable` key per visible column of the FIRST row, sorted
/// by name so the key does not depend on column insertion order (the
/// same expansion the non-aggregating projection path does). Pipeline
/// rows carry a uniform scope per clause, so every row in one
/// aggregation input has the same visible column set -- the positional
/// key vector already relies on that homogeneity. Expanding here keeps
/// the hot loop on the plain `eval` path: no per-key Star arm, no
/// per-row collect+sort. Empty input has no first row to expand from;
/// the unexpanded `*` then only keeps `keyless` false (a star key means
/// keys exist), and the row loop never runs.
pub(crate) fn desugar_star_keys<'a>(
    key_exprs: Vec<Cow<'a, Expr>>,
    rows: &[Row],
) -> Vec<Cow<'a, Expr>> {
    let Some(first) = rows.first() else {
        return key_exprs;
    };
    if !key_exprs.iter().any(|k| matches!(&**k, Expr::Star)) {
        return key_exprs;
    }
    let mut carried = visible_keys(first);
    carried.sort();
    let star_keys: Vec<Expr> = carried.into_iter().map(Expr::Variable).collect();
    let mut expanded = Vec::with_capacity(key_exprs.len() + star_keys.len());
    for k in key_exprs {
        if matches!(&*k, Expr::Star) {
            expanded.extend(star_keys.iter().cloned().map(Cow::Owned));
        } else {
            expanded.push(k);
        }
    }
    expanded
}

/// ADR-0205: the one sum()/avg() accumulator shared by the grouped path
/// ([`agg_eval`]), the horizontal rewrite
/// ([`super::match_clause::horizontal_aggregate`]), and the one-hop fold
/// ([`super::agg_fold::StreamAgg`]) — three call sites, one behavior:
/// nulls are skipped, numbers accumulate int/float, durations accumulate
/// component-wise, and mixing numbers with durations is a type error
/// (F-3-1: the copies had already drifted — `sum([0, duration])` was
/// silently accepted on the horizontal path but rejected elsewhere).
/// The accumulator mode (F-6-1): empty until the first non-null offer,
/// then either numeric or duration for the rest of its life. The
/// no-mixing invariant (F-3-2) is the match on this state at each
/// transition — a numeric offer against `Dur` errors, a duration offer
/// against `Num` errors — not a separate guard probing side fields.
#[derive(Clone)]
enum SumState {
    Empty,
    Num {
        int: i64,
        float: f64,
        used_float: bool,
        /// Non-null numeric values seen (avg needs the divisor).
        n: usize,
    },
    Dur {
        acc: Temporal,
        /// Non-null duration values seen (avg needs the divisor).
        n: usize,
    },
}

#[derive(Clone)]
pub(crate) struct SumAcc {
    state: SumState,
}

impl SumAcc {
    pub(crate) fn new() -> Self {
        Self {
            state: SumState::Empty,
        }
    }

    /// Offer one value with the strict sum() contract: nulls skipped,
    /// anything that is neither number nor duration is a type error.
    /// `name` ("sum"/"avg") labels the error.
    pub(crate) fn offer(&mut self, v: &Value, name: &str) -> Result<()> {
        match v {
            Value::Int(i) => self.offer_int(*i, name),
            Value::Float(f) => self.offer_float(*f, name),
            Value::Temporal(t @ Temporal::Duration { .. }) => self.offer_duration(t, name),
            Value::Null => Ok(()),
            _ => Err(Error::ty(format!("{name}() needs numbers"))),
        }
    }

    /// Offer one value with the lenient avg() contract: nulls and
    /// non-numeric non-duration values are skipped (the historical
    /// behavior), numbers and durations go through the strict arms, so
    /// mixing numbers and durations still errors.
    pub(crate) fn offer_lenient(&mut self, v: &Value, name: &str) -> Result<()> {
        match v {
            Value::Int(i) => self.offer_int(*i, name),
            Value::Float(f) => self.offer_float(*f, name),
            Value::Temporal(t @ Temporal::Duration { .. }) => self.offer_duration(t, name),
            _ => Ok(()),
        }
    }

    fn offer_int(&mut self, i: i64, name: &str) -> Result<()> {
        match &mut self.state {
            state @ SumState::Empty => {
                *state = SumState::Num {
                    int: i,
                    float: 0.0,
                    used_float: false,
                    n: 1,
                };
            }
            SumState::Num { int, n, .. } => {
                *int += i;
                *n += 1;
            }
            SumState::Dur { .. } => return Err(Self::mix(name)),
        }
        Ok(())
    }

    fn offer_float(&mut self, f: f64, name: &str) -> Result<()> {
        match &mut self.state {
            state @ SumState::Empty => {
                *state = SumState::Num {
                    int: 0,
                    float: f,
                    used_float: true,
                    n: 1,
                };
            }
            SumState::Num {
                float,
                used_float,
                n,
                ..
            } => {
                *float += f;
                *used_float = true;
                *n += 1;
            }
            SumState::Dur { .. } => return Err(Self::mix(name)),
        }
        Ok(())
    }

    /// A duration value: one accumulation arm for both offers (F-1-11/12/16:
    /// the strict and lenient offers shared one copy of this arm each; the
    /// take-add-replace dance would drift the way the three
    /// pre-consolidation copies did).
    fn offer_duration(&mut self, t: &Temporal, name: &str) -> Result<()> {
        match &mut self.state {
            state @ SumState::Empty => {
                *state = SumState::Dur {
                    acc: t.clone(),
                    n: 1,
                };
            }
            SumState::Dur { acc, n } => {
                *acc = temporal::add_duration(acc, t)?;
                *n += 1;
            }
            SumState::Num { .. } => return Err(Self::mix(name)),
        }
        Ok(())
    }

    fn mix(name: &str) -> Error {
        Error::invalid_argument_type(format!("{name}() cannot mix numbers and durations"))
    }

    /// sum(): durations accumulate component-wise; over zero non-null
    /// values the result is 0, matching the reference engine.
    pub(crate) fn finish_sum(self) -> Value {
        match self.state {
            SumState::Dur { acc, .. } => Value::Temporal(acc),
            SumState::Empty => Value::Int(0),
            SumState::Num {
                int,
                float,
                used_float: true,
                ..
            } => Value::Float(float + int as f64),
            SumState::Num { int, .. } => Value::Int(int),
        }
    }

    /// avg(): durations average via the accumulated duration scaled by
    /// 1/n; numbers average as a float; over zero non-null values the
    /// result is null.
    pub(crate) fn finish_avg(self) -> Result<Value> {
        match self.state {
            SumState::Dur { acc, n } => Ok(Value::Temporal(temporal::scale_duration(
                &acc,
                1.0 / n as f64,
            )?)),
            SumState::Empty => Ok(Value::Null),
            SumState::Num { int, float, n, .. } => {
                Ok(Value::Float((float + int as f64) / n as f64))
            }
        }
    }
}

pub(crate) fn agg_eval(
    graph: &ExecGraph<'_>,
    group: &[Row],
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    match expr {
        Expr::CountStar => Ok(Value::Int(group.len() as i64)),
        Expr::Function {
            name,
            distinct,
            args,
        } => {
            let n = name.to_ascii_lowercase();
            // The canonical aggregate set lives in `cypherlite-ir` (single
            // definition, re-exported by `cypherlite-front::agg`; `n` is
            // already lowercased here, and lowering has already
            // canonicalized alias spellings).
            if !cypherlite_ir::AGGREGATES.contains(&n.as_str()) {
                return agg_eval_function(graph, group, name, args, params, cypher_version);
            }
            // LABS-276 / ADR-0225/0217: every aggregate's arity contract
            // comes from the single `aggregate_arity` table in
            // `cypherlite-ir`, checked before the row loop so an empty
            // group cannot slip a bad call through and the loop's
            // `args[0]` index cannot panic. The contract class owns the
            // error shape: the single-input class matches the front's
            // `check_aggregate_arity` (the shared `SINGLE_INPUT_AGGREGATES`
            // table); `count` is guarded here too — a backtick-escaped
            // `` `count`() `` bypasses both the parser's keyword path and
            // front validation — and the percentiles keep their own
            // two-argument message, now exact: over-arity calls raise the
            // same error instead of silently dropping args[2..].
            if let Some(arity) = cypherlite_ir::aggregate_arity(&n) {
                if args.len() != arity.required() {
                    let rejection = arity.rejection(name, args.len());
                    return Err(match arity {
                        cypherlite_ir::AggregateArity::SingleInput => Error::syntax(rejection),
                        cypherlite_ir::AggregateArity::PercentilePair => Error::ty(rejection),
                    });
                }
            }
            let mut vals = Vec::new();
            for row in group {
                graph.cancel_tick()?;
                vals.push(eval(graph, row, &args[0], params, cypher_version)?);
            }
            if *distinct {
                // First-seen emission: the GroupKey decides set membership;
                // the first equivalent value is the one kept. The set is
                // charged for its own entries; a duplicate does not grow it.
                let mut seen = std::collections::HashSet::new();
                let mut distinct_charge = cypherlite_core::tokens::WorkspaceCharge::empty();
                let account = cypherlite_core::tokens::workspace_accounting();
                let mut u = Vec::new();
                for v in vals {
                    let key = GroupKey::new(&v);
                    let delta = if account {
                        crate::workspace::group_key_insert_delta(seen.len(), &key)
                    } else {
                        0
                    };
                    if seen.insert(key) {
                        if account {
                            distinct_charge
                                .resize(distinct_charge.bytes().saturating_add(delta))?;
                            cypherlite_core::tokens::peak_hold("distinct");
                        }
                        u.push(v);
                    }
                }
                drop(distinct_charge);
                vals = u;
            }
            Ok(match n.as_str() {
                "count" => {
                    let c = vals.iter().filter(|v| !v.is_null()).count();
                    Value::Int(c as i64)
                }
                "collect" => Value::List(
                    vals.into_iter()
                        .filter(|v| !v.is_null())
                        .collect::<Vec<_>>()
                        .into(),
                ),
                "sum" => {
                    // ADR-0205: durations accumulate component-wise; mixing
                    // numbers and durations in one sum() is a type error
                    // (the shared accumulator — three call sites, one
                    // behavior).
                    let mut acc = SumAcc::new();
                    let flat = vals.iter().flat_map(|value| match value {
                        Value::List(values) => values.as_slice(),
                        value => std::slice::from_ref(value),
                    });
                    for v in flat {
                        acc.offer(v, "sum")?;
                    }
                    acc.finish_sum()
                }
                "min" => vals
                    .into_iter()
                    .filter(|v| !v.is_null())
                    .min_by(|a, b| a.order_by_cmp(b))
                    .unwrap_or(Value::Null),
                "max" => vals
                    .into_iter()
                    .filter(|v| !v.is_null())
                    .max_by(|a, b| a.order_by_cmp(b))
                    .unwrap_or(Value::Null),
                "avg" => {
                    // ADR-0205: durations average via the accumulated
                    // duration scaled by 1/n; mixing numbers and durations
                    // is a type error. Non-numeric non-duration values keep
                    // their historical lenient skip (the lenient offer).
                    let mut acc = SumAcc::new();
                    for v in &vals {
                        acc.offer_lenient(v, "avg")?;
                    }
                    acc.finish_avg()?
                }
                "stdev" | "stdevp" => {
                    let nums: Vec<f64> = vals.iter().filter_map(Value::as_number).collect();
                    let divisor = if n == "stdev" {
                        nums.len().saturating_sub(1)
                    } else {
                        nums.len()
                    };
                    if divisor == 0 {
                        Value::Float(0.0)
                    } else {
                        let mean = nums.iter().sum::<f64>() / nums.len() as f64;
                        let variance =
                            nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / divisor as f64;
                        Value::Float(variance.sqrt())
                    }
                }
                "percentilecont" | "percentiledisc" => {
                    // Arity (two arguments) is checked ahead of the row
                    // loop, above; the percentile itself must be a number
                    // in [0, 1] — one rejection site covers both failure
                    // modes, so the message cannot drift between them.
                    let p = eval(
                        graph,
                        group.first().unwrap_or(&Row::new()),
                        &args[1],
                        params,
                        cypher_version,
                    )?;
                    let Some(p) = p.as_number().filter(|p| (0.0..=1.0).contains(p)) else {
                        return Err(Error::ty("NumberOutOfRange: percentile"));
                    };
                    let mut nums: Vec<f64> = vals.iter().filter_map(|v| v.as_number()).collect();
                    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    if nums.is_empty() {
                        Value::Null
                    } else if n == "percentiledisc" {
                        if p == 0.0 {
                            Value::Float(nums[0])
                        } else {
                            let idx = ((nums.len() as f64) * p).ceil() as usize - 1;
                            Value::Float(nums[idx.min(nums.len() - 1)])
                        }
                    } else if nums.len() == 1 {
                        Value::Float(nums[0])
                    } else {
                        let h = p * (nums.len() - 1) as f64;
                        let lo = h.floor() as usize;
                        let hi = h.ceil() as usize;
                        let f = h - lo as f64;
                        Value::Float(nums[lo] * (1.0 - f) + nums[hi] * f)
                    }
                }
                // Unreachable: the early return above diverts every name not
                // in `cypherlite_ir::AGGREGATES` to `agg_eval_function`, and
                // the arms above cover every name that list contains.
                other => unreachable!("unknown aggregate function: {other}"),
            })
        }
        Expr::Map(es) => {
            let mut m = BTreeMap::new();
            for (k, e) in es {
                m.insert(
                    k.clone(),
                    agg_eval(graph, group, e, params, cypher_version)?,
                );
            }
            Ok(Value::Map(m.into()))
        }
        Expr::List(xs) => {
            let mut out = Vec::new();
            for e in xs {
                out.push(agg_eval(graph, group, e, params, cypher_version)?);
            }
            Ok(Value::List(out.into()))
        }
        Expr::ListComp {
            ident,
            list,
            predicate,
            projection,
        } => agg_eval_combinator(
            graph,
            group,
            &[("__agg_list", list)],
            params,
            cypher_version,
            || Expr::ListComp {
                ident: ident.clone(),
                list: Box::new(Expr::Variable("__agg_list".into())),
                predicate: predicate.clone(),
                projection: projection.clone(),
            },
        ),
        Expr::MapComp {
            key_ident,
            value_ident,
            source,
            predicate,
            key_expr,
            value_expr,
        } => agg_eval_combinator(
            graph,
            group,
            &[("__agg_map", source)],
            params,
            cypher_version,
            || Expr::MapComp {
                key_ident: key_ident.clone(),
                value_ident: value_ident.clone(),
                source: Box::new(Expr::Variable("__agg_map".into())),
                predicate: predicate.clone(),
                key_expr: key_expr.clone(),
                value_expr: value_expr.clone(),
            },
        ),
        Expr::Quantifier {
            kind,
            ident,
            list,
            predicate,
        } => agg_eval_combinator(
            graph,
            group,
            &[("__agg_list", list)],
            params,
            cypher_version,
            || Expr::Quantifier {
                kind: *kind,
                ident: ident.clone(),
                list: Box::new(Expr::Variable("__agg_list".into())),
                predicate: predicate.clone(),
            },
        ),
        Expr::Case {
            operand,
            when_then,
            else_expr,
        } => agg_eval_case(
            graph,
            group,
            operand,
            when_then,
            else_expr,
            params,
            cypher_version,
        ),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::String(s) => Ok(Value::String(s.as_str().into())),
        Expr::Null => Ok(Value::Null),
        // The rebuilt expression references only the synthetic `__l`/`__r`
        // variables (both operands were already `agg_eval`'d), so the
        // combinator's first-row seed is a harmless superset of the empty
        // row this arm used to pass explicitly.
        Expr::Binary { op, left, right } => agg_eval_combinator(
            graph,
            group,
            &[("__l", left), ("__r", right)],
            params,
            cypher_version,
            || Expr::Binary {
                op: *op,
                left: Box::new(Expr::Variable("__l".into())),
                right: Box::new(Expr::Variable("__r".into())),
            },
        ),
        Expr::Subscript { expr, index } => agg_eval_combinator(
            graph,
            group,
            &[("__agg_base", expr)],
            params,
            cypher_version,
            || Expr::Subscript {
                expr: Box::new(Expr::Variable("__agg_base".into())),
                index: index.clone(),
            },
        ),
        Expr::Slice { expr, start, end } => agg_eval_combinator(
            graph,
            group,
            &[("__agg_base", expr)],
            params,
            cypher_version,
            || Expr::Slice {
                expr: Box::new(Expr::Variable("__agg_base".into())),
                start: start.clone(),
                end: end.clone(),
            },
        ),
        _ => {
            if let Some(row) = group.first() {
                eval(graph, row, expr, params, cypher_version)
            } else {
                Ok(Value::Null)
            }
        }
    }
}

/// Shared shape of the transparent-combinator arms: recursively `agg_eval`
/// each child, bind the results under their synthetic `__agg_*` names in a
/// row seeded from the group's first row (cloned, or default for an empty
/// group), and delegate the `rebuild`'d combinator — which references those
/// names via `Expr::Variable` — to scalar `eval`.
fn agg_eval_combinator<S: AsRef<str>>(
    graph: &ExecGraph<'_>,
    group: &[Row],
    children: &[(S, &Expr)],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    rebuild: impl FnOnce() -> Expr,
) -> Result<Value> {
    let mut row = group.first().cloned().unwrap_or_default();
    for (name, child) in children {
        row.insert(
            name.as_ref().into(),
            agg_eval(graph, group, child, params, cypher_version)?,
        );
    }
    eval(graph, &row, &rebuild(), params, cypher_version)
}

/// Non-aggregate function wrapper over aggregate arguments: bind each
/// argument under `__agg_arg{i}` and re-run the call as scalar `eval`.
fn agg_eval_function(
    graph: &ExecGraph<'_>,
    group: &[Row],
    name: &str,
    args: &[Expr],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let children: Vec<(String, &Expr)> = args
        .iter()
        .enumerate()
        .map(|(i, a)| (format!("__agg_arg{i}"), a))
        .collect();
    agg_eval_combinator(graph, group, &children, params, cypher_version, || {
        Expr::Function {
            name: name.into(),
            distinct: false,
            args: children
                .iter()
                .map(|(n, _)| Expr::Variable(n.clone()))
                .collect(),
        }
    })
}

/// ADR-0198 ruling 2: an aggregate call in a CASE arm belongs to the
/// enclosing aggregating projection. Every part that can contain an
/// aggregate (operand, WHEN conditions, THEN results, ELSE) goes through
/// `agg_eval`; the rebuilt CASE references the results via `__agg_*`
/// variables, so scalar `eval_case` picks the arm.
fn agg_eval_case<'e>(
    graph: &ExecGraph<'_>,
    group: &[Row],
    operand: &'e Option<Box<Expr>>,
    when_then: &'e [(Expr, Expr, bool)],
    else_expr: &'e Option<Box<Expr>>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let mut children: Vec<(String, &Expr)> = Vec::new();
    // Bind every part under its synthetic name and capture the matching
    // variable in the same pass: one name stream, no rebuild-time cursor
    // that could drift.
    let mut bind = |name: String, child: &'e Expr| {
        children.push((name.clone(), child));
        Expr::Variable(name)
    };
    let operand = operand
        .as_deref()
        .map(|op| Box::new(bind("__agg_operand".into(), op)));
    let when_then: Vec<(Expr, Expr, bool)> = when_then
        .iter()
        .enumerate()
        .map(|(i, (when, then, is_predicate))| {
            let when = bind(format!("__agg_when{i}"), when);
            let then = bind(format!("__agg_then{i}"), then);
            (when, then, *is_predicate)
        })
        .collect();
    let else_expr = else_expr
        .as_deref()
        .map(|e| Box::new(bind("__agg_else".into(), e)));
    agg_eval_combinator(graph, group, &children, params, cypher_version, || {
        Expr::Case {
            operand,
            when_then,
            else_expr,
        }
    })
}
