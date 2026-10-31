//! Expression evaluation: the `eval` dispatcher, the `Row` type, and the
//! `eval/` submodule wiring.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Expr, MapProjectionItem};

use crate::ctx::EvalCtx;
use crate::overlay::ExecGraph;

mod collection;
mod expr;
mod fold;
mod function;
mod items;
mod labels;
mod numeric;
mod pattern;
mod quantifier;
mod row;
mod shortest;
mod string;
mod temporal;
mod typematch;
mod typename;
mod vector;

pub(crate) use fold::{fold_quantifier, membership};
pub(crate) use items::into_items;
pub(crate) use labels::{
    dynamic_label_names, dynamic_property_key, label_expr_matches, resolve_write_labels,
};
// The expression walkers live in `cypherlite-ir` next to `Expr::children`
// so planner and executor share one traversal.
pub(crate) use cypherlite_ir::{
    expr_vars, is_agg_expr, key_of, label_expr_vars, orient, var_key_side, var_prop,
};

pub(crate) use expr::binary;
use expr::{eval_case, prop, reject_deleted_access, slice, subscript, unary};
use function::function;
pub(crate) use function::is_nondeterministic;
pub(crate) use pattern::bind_trail;
use pattern::collect_pattern_rows;
pub(crate) use pattern::rel_edge_ok;
pub(crate) use pattern::rel_props_ok;
pub(crate) use pattern::{bind_pattern_node, bound_node};
pub(crate) use pattern::{pattern_exists, resolved_endpoints_exist};
use quantifier::{eval_all_reduce, eval_quantifier, eval_reduce};
use shortest::{eval_shortest, ShortestWant};
use string::is_normalized;
pub(crate) use typematch::value_matches_type;
pub(crate) use typename::value_type_name;

/// The one boolean-operand policy of the boolean operators (`expr.rs`'s
/// NOT/AND/OR/XOR) and the list-quantifier WHERE predicate
/// (`quantifier.rs`): a boolean yields its truthiness, null yields the
/// unknown, anything else is an InvalidArgumentType syntax error naming
/// the requiring context.
pub(crate) fn bool_or_null(v: &Value, ctx: &str) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        // The pinned InvalidArgumentType token rides Error::syntax so the
        // site keeps its long-standing SyntaxError class (42N06,
        // ADR-0003); the typed-constructor convergence (executor and
        // planner sites together) is a class-changing, ADR-worthy change
        // (LABS-400), not this one.
        _ => Err(Error::syntax(format!(
            "InvalidArgumentType: {ctx} requires a boolean"
        ))),
    }
}

/// An operand of a container access (`Subscript`/`Property`/
/// `DynamicProperty`/`Slice`): a plain variable is borrowed from the row —
/// evaluating it would deep-clone the whole bound container, and the access
/// only needs the element — while any other expression is evaluated and
/// owned. `Cow` carries the borrowed-or-owned value.
fn eval_operand<'a>(
    graph: &ExecGraph<'a>,
    row: &'a Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<std::borrow::Cow<'a, Value>> {
    if let Expr::Variable(name) = expr {
        if let Some(v) = row.get(name) {
            return Ok(std::borrow::Cow::Borrowed(v));
        }
    }
    let v = eval(graph, row, expr, params, cypher_version)?;
    Ok(std::borrow::Cow::Owned(v))
}

pub use row::{Row, RowNames};

/// The summed logical bytes of a row set — the row-materialization metric
/// (ADR-0344 metric fields): each binding counts its key's string bytes
/// (ADR-0042 string size) plus its value's [`Value::logical_bytes`]. The
/// one canonical spelling of "deep size of these rows", beside the `Row`
/// type it measures; the PROFILE dispatch loop is the caller. A row's
/// map header itself is not counted — only what the row carries, so a
/// column rename shows up as the value's size, not a constant offset.
pub(crate) fn rows_logical_bytes(rows: &[Row]) -> u64 {
    rows.iter()
        .flat_map(|row| row.iter())
        .map(|(key, value)| cypherlite_core::str_logical_bytes(key) + value.logical_bytes())
        .sum()
}

/// Evaluate a scalar expression against a row. `cypher_version` is the
/// enclosing statement's CIP-150 preamble: nested expression subqueries
/// (EXISTS/COUNT/COLLECT) lowered with `Query::cypher_version: None` inherit
/// it (see `exec::subquery_qctx`).
pub fn eval(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let ctx = EvalCtx {
        graph,
        row,
        params,
        cypher_version,
    };
    match expr {
        Expr::RuntimeValue(value) => Ok(value.clone()),
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::String(s) => Ok(Value::String(s.as_str().into())),
        // ADR-0308: the facade rejects a planned statement whose parameter
        // references are absent before dispatch, so this fires only for
        // expressions the plan walk cannot see (catalog commands lower
        // theirs at run time) — with the same typed error, never `null`.
        Expr::Parameter(n) => params
            .get(n)
            .cloned()
            .ok_or_else(|| Error::parameters_missing([n.as_str()])),
        // ADR-0335: the plan-time binding walks reject an undefined
        // variable before execution, so a row miss here is either a
        // planner/executor binding bug or an expression the plan walk
        // cannot see (run-time-lowered catalog predicates) — never `null`.
        Expr::Variable(n) => row
            .get(n)
            .cloned()
            .ok_or_else(|| Error::undefined_variable(n)),
        Expr::Star => Err(Error::semantic("* is not a scalar expression")),
        Expr::Property { expr, key } => {
            // Variable operands are read by reference (see Subscript): a
            // property read must not clone the whole map it reads from.
            let v = eval_operand(graph, row, expr, params, cypher_version)?;
            reject_deleted_access(graph, &v)?;
            prop(graph, &v, key)
        }
        Expr::DynamicProperty { expr, key } => {
            let v = eval_operand(graph, row, expr, params, cypher_version)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            reject_deleted_access(graph, &v)?;
            let key = dynamic_property_key(eval(graph, row, key, params, cypher_version)?)?;
            prop(graph, &v, &key)
        }
        Expr::LabelPredicate {
            expr,
            label_expr,
            strict,
        } => {
            let v = eval(graph, row, expr, params, cypher_version)?;
            Ok(match v {
                Value::Node(n) => Value::Bool(label_expr_matches(
                    graph,
                    row,
                    params,
                    label_expr,
                    &n.labels,
                    false,
                    cypher_version,
                )?),
                Value::Relationship(r) => Value::Bool(label_expr_matches(
                    graph,
                    row,
                    params,
                    label_expr,
                    std::slice::from_ref(&r.rel_type),
                    true,
                    cypher_version,
                )?),
                Value::Null => Value::Null,
                _ if *strict => {
                    return Err(Error::invalid_argument_type(
                        "IS LABELED requires a node or relationship",
                    ))
                }
                _ => Value::Bool(false),
            })
        }
        Expr::Map(entries) => {
            let mut m = BTreeMap::new();
            for (k, e) in entries {
                m.insert(k.clone(), eval(graph, row, e, params, cypher_version)?);
            }
            Ok(Value::Map(m.into()))
        }
        Expr::List(xs) => {
            let mut out = Vec::new();
            for e in xs {
                out.push(eval(graph, row, e, params, cypher_version)?);
            }
            Ok(Value::List(out.into()))
        }
        Expr::Unary { op, expr } => {
            let v = eval(graph, row, expr, params, cypher_version)?;
            unary(*op, v)
        }
        Expr::Binary { op, left, right } => {
            let l = eval(graph, row, left, params, cypher_version)?;
            let r = eval(graph, row, right, params, cypher_version)?;
            binary(*op, l, r)
        }
        Expr::Function {
            name,
            distinct,
            args,
        } => function(graph, row, params, name, *distinct, args, cypher_version),
        Expr::CountStar => Err(Error::semantic("count(*) only valid in aggregation")),
        Expr::Case {
            operand,
            when_then,
            else_expr,
        } => eval_case(
            graph,
            row,
            params,
            operand,
            when_then,
            else_expr,
            cypher_version,
        ),
        Expr::Subscript { expr, index } => {
            // The container is evaluated before the index (the pre-by-reference
            // order, so error precedence is unchanged), and a variable
            // operand is read by reference: evaluating it would deep-clone
            // the whole container, and the subscript only needs the element.
            let v = eval_operand(graph, row, expr, params, cypher_version)?;
            let i = eval(graph, row, index, params, cypher_version)?;
            subscript(graph, &v, i)
        }
        Expr::Slice { expr, start, end } => {
            let v = eval_operand(graph, row, expr, params, cypher_version)?;
            let s = match start {
                Some(e) => Some(eval(graph, row, e, params, cypher_version)?),
                None => None,
            };
            let e = match end {
                Some(e) => Some(eval(graph, row, e, params, cypher_version)?),
                None => None,
            };
            slice(&v, s, e)
        }
        // CIP-272 map comprehension: bind (key, value) or (item) per
        // source element, filter with the optional WHERE, and project
        // `keyExpr : valueExpr` into a map. A null source is null; a
        // non-string key is a type error; duplicate keys take the last
        // write (BTreeMap insert). Each source arm keeps its concrete
        // iterator type; the shared bind/filter/project loop is generic
        // over it (see `eval_map_comp`).
        Expr::MapComp {
            key_ident,
            value_ident,
            source,
            predicate,
            key_expr,
            value_expr,
        } => {
            let src_v = eval(graph, row, source, params, cypher_version)?;
            // (key, value) bindings per source element; the key binding of
            // a list-source form is absent.
            match (src_v, key_ident.as_deref()) {
                (Value::Map(m), Some(k)) => eval_map_comp(
                    ctx,
                    Some(k),
                    value_ident,
                    predicate.as_deref(),
                    key_expr,
                    value_expr,
                    Arc::unwrap_or_clone(m)
                        .into_iter()
                        .map(|(k, v)| (Some(Value::String(k.into())), v)),
                ),
                (Value::List(xs), None) => eval_map_comp(
                    ctx,
                    None,
                    value_ident,
                    predicate.as_deref(),
                    key_expr,
                    value_expr,
                    into_items(xs).map(|x| (None, x)),
                ),
                (Value::Null, _) => Ok(Value::Null),
                (Value::Map(_), None) => Err(Error::ty(
                    "map comprehension with a list binding needs a LIST source",
                )),
                (Value::List(_), Some(_)) => Err(Error::ty(
                    "map comprehension with key/value bindings needs a MAP source",
                )),
                _ => Err(Error::ty("map comprehension needs a MAP or LIST source")),
            }
        }
        Expr::ListComp {
            ident,
            list,
            predicate,
            projection,
        } => {
            let list_v = eval(graph, row, list, params, cypher_version)?;
            let items = match list_v {
                Value::List(xs) => xs,
                Value::Null => return Ok(Value::Null),
                _ => return Err(Error::ty("list comprehension needs a list")),
            };
            let mut out = Vec::new();
            for item in into_items(items) {
                let mut r = row.clone();
                r.insert(ident.clone(), item);
                if let Some(pred) = predicate {
                    if eval(graph, &r, pred, params, cypher_version)?.truthy() != Some(true) {
                        continue;
                    }
                }
                out.push(eval(graph, &r, projection, params, cypher_version)?);
            }
            Ok(Value::List(out.into()))
        }
        Expr::Quantifier {
            kind,
            ident,
            list,
            predicate,
        } => eval_quantifier(ctx, *kind, ident, list, predicate),
        Expr::Reduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
        } => eval_reduce(ctx, accumulator, initial, ident, list, expression),
        Expr::AllReduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
            predicate,
        } => eval_all_reduce(
            ctx,
            accumulator,
            initial,
            ident,
            list,
            expression,
            predicate,
        ),
        Expr::IsType {
            expr,
            type_expr,
            negated,
        } => {
            let value = eval(graph, row, expr, params, cypher_version)?;
            Ok(Value::Bool(
                value_matches_type(&value, type_expr) ^ *negated,
            ))
        }
        Expr::IsNormalized {
            expr,
            form,
            negated,
        } => {
            let value = eval(graph, row, expr, params, cypher_version)?;
            let Value::String(s) = value else {
                return Ok(Value::Null);
            };
            let result = is_normalized(&s, form.as_deref().unwrap_or("NFC"));
            Ok(Value::Bool(result ^ *negated))
        }
        Expr::MapProjection { expr, items } => {
            let target = eval(graph, row, expr, params, cypher_version)?;
            let mut map = BTreeMap::new();
            for item in items {
                match item {
                    MapProjectionItem::KeyValue { key, value } => {
                        map.insert(
                            key.clone(),
                            eval(graph, row, value, params, cypher_version)?,
                        );
                    }
                    MapProjectionItem::Property { key } => {
                        reject_deleted_access(graph, &target)?;
                        map.insert(key.clone(), prop(graph, &target, key)?);
                    }
                    MapProjectionItem::Variable { name } => {
                        // ADR-0335: same policy as `Expr::Variable` — an
                        // unbound name is a typed error, never `null`.
                        map.insert(
                            name.clone(),
                            row.get(name)
                                .cloned()
                                .ok_or_else(|| Error::undefined_variable(name))?,
                        );
                    }
                    MapProjectionItem::AllProperties => match &target {
                        Value::Map(properties) => {
                            map.extend(properties.iter().map(|(k, v)| (k.clone(), v.clone())))
                        }
                        Value::Node(node) => map.extend(
                            node.properties
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        ),
                        Value::Relationship(rel) => map.extend(
                            rel.properties
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        ),
                        _ => {}
                    },
                }
            }
            Ok(Value::Map(map.into()))
        }
        Expr::DistanceMetric(_) => Err(Error::semantic(
            "distance metrics are only valid as a vector_distance/vector_norm argument",
        )),
        Expr::PatternPred { path } => {
            // LABS-193/LABS-246: existence only — the anchored probe, then
            // a limit-1 walk; never a full match collection.
            Ok(Value::Bool(pattern_exists(
                graph,
                row,
                path,
                params,
                cypher_version,
            )?))
        }
        Expr::PatternComp {
            path,
            predicate,
            projection,
        } => {
            let mut out = Vec::new();
            for r in collect_pattern_rows(graph, row, path, params, cypher_version)? {
                if let Some(pred) = predicate {
                    if eval(graph, &r, pred, params, cypher_version)?.truthy() != Some(true) {
                        continue;
                    }
                }
                out.push(eval(graph, &r, projection, params, cypher_version)?);
            }
            Ok(Value::List(out.into()))
        }
        Expr::ShortestPath { all, path } => eval_shortest(
            graph,
            row,
            params,
            *all,
            path,
            ShortestWant::Path,
            cypher_version,
        ),
        Expr::ExistsSubquery { query } => {
            crate::exec::eval_exists_query(graph, row, params, query, cypher_version)
        }
        Expr::CountSubquery { query } => {
            crate::exec::eval_count_query(graph, row, params, query, cypher_version)
        }
        Expr::CollectSubquery { query } => {
            crate::exec::eval_collect_query(graph, row, params, query, cypher_version)
        }
    }
}

/// CIP-272 map-comprehension body: bind (key, value) — or just (item),
/// with no key binding, for a list source — per element, filter with the
/// optional WHERE, and project `keyExpr : valueExpr` into a map. A
/// non-string key is a type error; duplicate keys take the last write
/// (BTreeMap insert). Generic over the bindings iterator so the map-source
/// and list-source arms each keep their concrete iterator type.
fn eval_map_comp(
    ctx: EvalCtx<'_>,
    key_ident: Option<&str>,
    value_ident: &str,
    predicate: Option<&Expr>,
    key_expr: &Expr,
    value_expr: &Expr,
    bindings: impl Iterator<Item = (Option<Value>, Value)>,
) -> Result<Value> {
    let EvalCtx {
        graph,
        row,
        params,
        cypher_version,
    } = ctx;
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for (k, v) in bindings {
        let mut r = row.clone();
        if let (Some(kvar), Some(kv)) = (key_ident, k) {
            r.insert(kvar.to_string(), kv);
        }
        r.insert(value_ident.to_string(), v);
        if let Some(pred) = predicate {
            if eval(graph, &r, pred, params, cypher_version)?.truthy() != Some(true) {
                continue;
            }
        }
        let key_v = eval(graph, &r, key_expr, params, cypher_version)?;
        let key = match key_v {
            Value::String(s) => String::from(&*s),
            Value::Null => continue,
            _ => {
                return Err(Error::ty(
                    "map comprehension key expression must be a STRING",
                ))
            }
        };
        let value_v = eval(graph, &r, value_expr, params, cypher_version)?;
        out.insert(key, value_v);
    }
    Ok(Value::Map(out.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use cypherlite_core::VectorCoordKind;

    /// LABS-156 (ADR-0170): reading a bound variable and carrying a row
    /// share the String/List/Map/Vector payload (a refcount bump) instead of
    /// deep-copying it, so the cost no longer scales with the payload size.
    #[test]
    fn variable_read_and_row_carry_share_payloads() {
        let mut row = Row::new();
        row.insert(
            "list".into(),
            Value::List((0..10_000).map(Value::Int).collect::<Vec<_>>().into()),
        );
        row.insert(
            "map".into(),
            Value::Map(
                BTreeMap::from([
                    ("a".to_string(), Value::Int(1)),
                    ("b".to_string(), Value::String("two".into())),
                ])
                .into(),
            ),
        );
        row.insert("string".into(), Value::String("x".repeat(1_000).into()));
        row.insert(
            "vector".into(),
            Value::Vector {
                coord: VectorCoordKind::Float64,
                values: vec![Value::Float(1.0), Value::Float(2.0), Value::Float(3.0)].into(),
            },
        );

        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let params = BTreeMap::new();
        let read =
            |name: &str| eval(&graph, &row, &Expr::Variable(name.into()), &params, None).unwrap();

        let (Value::List(bound), Value::List(read_back)) = (&row["list"], read("list")) else {
            panic!("list variable did not read back as a list");
        };
        assert_eq!(read_back.len(), 10_000);
        assert!(Arc::ptr_eq(bound, &read_back));

        let (Value::Map(bound), Value::Map(read_back)) = (&row["map"], read("map")) else {
            panic!("map variable did not read back as a map");
        };
        assert!(Arc::ptr_eq(bound, &read_back));

        let (Value::String(bound), Value::String(read_back)) = (&row["string"], read("string"))
        else {
            panic!("string variable did not read back as a string");
        };
        assert!(read_back.len() >= 1_000);
        assert!(Arc::ptr_eq(bound, &read_back));

        let (
            Value::Vector { values: bound, .. },
            Value::Vector {
                values: read_back, ..
            },
        ) = (&row["vector"], read("vector"))
        else {
            panic!("vector variable did not read back as a vector");
        };
        assert!(Arc::ptr_eq(bound, &read_back));

        // Operators clone the input row for every output row (expand,
        // pattern matching, ...); that carry shares the list, not copies it.
        let carried = row.clone();
        let (Value::List(original), Value::List(cloned)) = (&row["list"], &carried["list"]) else {
            panic!("cloned row lost the list binding");
        };
        assert!(Arc::ptr_eq(original, cloned));
    }

    /// ADR-0335: an unbound variable is the typed undefined-variable error,
    /// never a fabricated null, in both the scalar and the map-projection
    /// arms.
    #[test]
    fn unbound_variables_fail_with_undefined_variable() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let params = BTreeMap::new();
        let row = Row::new();

        let error = eval(
            &graph,
            &row,
            &Expr::Variable("missing".into()),
            &params,
            None,
        )
        .unwrap_err();
        assert_eq!(error, Error::undefined_variable("missing"));
        assert_eq!(
            error.to_string(),
            "syntax error: UndefinedVariable: variable 'missing' not defined"
        );

        let projection = Expr::MapProjection {
            expr: Box::new(Expr::Map(Vec::new())),
            items: vec![MapProjectionItem::Variable {
                name: "missing".into(),
            }],
        };
        assert_eq!(
            eval(&graph, &row, &projection, &params, None).unwrap_err(),
            Error::undefined_variable("missing")
        );
    }
}
