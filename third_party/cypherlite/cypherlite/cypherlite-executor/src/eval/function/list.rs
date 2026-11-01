//! List and collection builtins: `head`/`last`/`tail`/`reverse`, `range`,
//! `size`/`cardinality`/`length`, and the `coll.*`/`apoc.coll.*` wrappers
//! over the `eval::collection` helpers.

use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::QuantifierKind;

use crate::overlay::ExecGraph;

use super::super::collection::{
    collection_distinct, collection_extreme, collection_flatten, collection_has,
    collection_index_of, collection_insert, collection_remove, collection_sort, collection_zip,
};
use super::super::into_items;

pub(super) fn bi_length(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::Path(p)) => Value::Int(p.rels.len() as i64),
        Some(Value::String(s)) => Value::Int(s.chars().count() as i64),
        Some(Value::List(xs)) => Value::Int(xs.len() as i64),
        Some(Value::Null) | None => Value::Null,
        _ => {
            return Err(Error::invalid_argument_type(
                "length() needs a path, string, or list",
            ))
        }
    })
}

pub(super) fn bi_size(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::List(xs)) => Value::Int(xs.len() as i64),
        Some(Value::String(s)) => Value::Int(s.chars().count() as i64),
        Some(Value::Map(m)) => Value::Int(m.len() as i64),
        Some(Value::Vector { values, .. }) => Value::Int(values.len() as i64),
        Some(Value::Null) | None => Value::Null,
        Some(Value::Path(_)) => {
            return Err(Error::ty("InvalidArgumentValue: size() on paths"));
        }
        _ => return Err(Error::ty("InvalidArgumentValue: size()")),
    })
}

pub(super) fn bi_head(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::List(xs)) => xs.first().cloned().unwrap_or(Value::Null),
        Some(Value::Null) | None => Value::Null,
        Some(Value::Vector { .. }) => {
            return Err(Error::invalid_argument_type("head() requires a list"))
        }
        _ => Value::Null,
    })
}

pub(super) fn bi_last(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::List(xs)) => xs.last().cloned().unwrap_or(Value::Null),
        Some(Value::Null) | None => Value::Null,
        Some(Value::Vector { .. }) => {
            return Err(Error::invalid_argument_type("last() requires a list"))
        }
        _ => Value::Null,
    })
}

pub(super) fn bi_tail(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.into_iter().next() {
        // `into_items` owns the owned-vs-shared split: a unique list moves
        // its tail elements out, a shared one clones only the tail, and an
        // empty list falls out of `skip(1)`.
        Some(Value::List(xs)) => Value::List(into_items(xs).skip(1).collect::<Vec<_>>().into()),
        Some(Value::Null) | None => Value::Null,
        _ => Value::Null,
    })
}

pub(super) fn bi_reverse(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.into_iter().next() {
        Some(Value::List(xs)) => {
            let mut o = Arc::unwrap_or_clone(xs);
            o.reverse();
            Value::List(o.into())
        }
        Some(Value::String(s)) => Value::String(s.chars().rev().collect::<String>().into()),
        Some(Value::Null) | None => Value::Null,
        _ => Value::Null,
    })
}

pub(super) fn bi_cardinality(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::List(xs)) => Value::Int(xs.len() as i64),
        Some(Value::Map(m)) => Value::Int(m.len() as i64),
        Some(Value::Path(p)) => Value::Int((p.nodes.len() + p.rels.len()) as i64),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("CARDINALITY() requires a list, path, or map")),
    })
}

pub(super) fn bi_range(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        // ADR-0175: a float argument converts with Java `(long)`-cast
        // semantics (truncate toward zero, NaN → 0, ±Infinity and
        // out-of-range saturate at the i64 limits); every non-numeric
        // argument stays a type error.
        let int_arg = |v: Option<&Value>, what: &str| {
            super::super::numeric::int_arg(v)
                .ok_or_else(|| Error::invalid_argument_type(format!("range() {what}")))
        };
        let start = int_arg(vals.first(), "start")?;
        let end = int_arg(vals.get(1), "end")?;
        let step = if vals.len() >= 3 {
            int_arg(vals.get(2), "step")?
        } else {
            1
        };
        if step == 0 {
            return Err(Error::ty("NumberOutOfRange: range() step cannot be 0"));
        }
        // ADR-0175: a size computation overflowing i64 is the
        // arithmetic-overflow error (Neo4j 22N28, IntegralRangeListValue's
        // subtractExact/divideExact/addExact) — `range(1, -Infinity)`
        // errors because `end - start` overflows once -Infinity has
        // saturated to i64::MIN, while `range(5, 1, -Infinity)` is `[5]`.
        // A direction-inconsistent range (the end unreachable from the
        // start at this step sign) is empty (TCK List11 [2]/[3]).
        let diff = end
            .checked_sub(start)
            .ok_or_else(|| Error::ty("integer out of range"))?;
        let size = if (diff > 0 && step < 0) || (diff < 0 && step > 0) {
            0
        } else {
            diff.checked_div(step)
                .and_then(|q| q.checked_add(1))
                .ok_or_else(|| Error::ty("integer out of range"))?
        };
        let mut xs = Vec::new();
        if size > 0 {
            xs.push(Value::Int(start));
            let mut i = start;
            for _ in 1..size {
                // In range by construction (size is exact); checked anyway
                // so an extreme input errors honestly instead of wrapping
                // or looping forever.
                i = i
                    .checked_add(step)
                    .ok_or_else(|| Error::ty("integer out of range"))?;
                xs.push(Value::Int(i));
            }
        }
        Value::List(xs.into())
    })
}

pub(super) fn bi_coll_flatten(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_flatten(&vals)
}

pub(super) fn bi_coll_sort(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_sort(vals)
}

pub(super) fn bi_coll_indexof(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_index_of(&vals)
}

pub(super) fn bi_coll_insert(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_insert(vals)
}

pub(super) fn bi_coll_remove(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_remove(vals)
}

pub(super) fn bi_coll_max(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_extreme(&vals, true)
}

pub(super) fn bi_coll_min(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_extreme(&vals, false)
}

pub(super) fn bi_coll_distinct(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    collection_distinct(&vals)
}

pub(super) fn bi_coll_zip(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    collection_zip(&vals)
}

pub(super) fn bi_coll_has_any_of(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    collection_has(QuantifierKind::Any, &vals)
}

pub(super) fn bi_coll_has_all_of(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    collection_has(QuantifierKind::All, &vals)
}

pub(super) fn bi_coll_has_none_of(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    collection_has(QuantifierKind::None, &vals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn tail_drops_the_head_of_a_shared_or_unique_list() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let tail = |v: Value| bi_tail(&graph, "tail", vec![v]).unwrap();
        let list =
            |xs: &[i64]| Value::List(xs.iter().map(|&x| Value::Int(x)).collect::<Vec<_>>().into());

        // Unique payload: moved out and shifted in place.
        assert_eq!(tail(list(&[1, 2, 3])), list(&[2, 3]));
        // Shared payload: only the tail is copied; the input is untouched.
        let input = list(&[1, 2, 3]);
        assert_eq!(tail(input.clone()), list(&[2, 3]));
        assert_eq!(input, list(&[1, 2, 3]));
        // An empty list stays empty, shared or not.
        let empty = list(&[]);
        assert_eq!(tail(empty.clone()), list(&[]));
        assert_eq!(tail(empty), list(&[]));
        assert_eq!(tail(Value::Null), Value::Null);
    }
}
