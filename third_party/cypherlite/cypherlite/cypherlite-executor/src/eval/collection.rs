//! Collection function helpers (`coll.*`).

use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::QuantifierKind;

use crate::group_key::GroupKey;

use super::{fold_quantifier, membership};

pub(crate) fn collection_flatten(vals: &[Value]) -> Result<Value> {
    let Some(value) = vals.first() else {
        return Err(Error::semantic("coll.flatten() requires an argument"));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    let Value::List(items) = value else {
        return Err(Error::ty("coll.flatten() requires a list"));
    };
    let depth = match vals.get(1) {
        None => 1,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int(i)) if (0..=i32::MAX as i64).contains(i) => *i,
        Some(Value::Int(_)) => return Err(Error::ty("InvalidArgumentValue: flatten depth")),
        Some(_) => return Err(Error::invalid_argument_type("flatten depth")),
    };
    fn flatten(items: &[Value], depth: i64, out: &mut Vec<Value>) {
        for item in items {
            if depth > 0 {
                if let Value::List(nested) = item {
                    flatten(nested, depth - 1, out);
                    continue;
                }
            }
            out.push(item.clone());
        }
    }
    let mut out = Vec::new();
    flatten(items, depth.max(0), &mut out);
    Ok(Value::List(out.into()))
}

pub(crate) fn collection_sort(vals: Vec<Value>) -> Result<Value> {
    match vals.into_iter().next() {
        Some(Value::List(items)) => {
            let mut out = Arc::unwrap_or_clone(items);
            out.sort_by(Value::order_by_cmp);
            Ok(Value::List(out.into()))
        }
        Some(Value::Null) => Ok(Value::Null),
        _ => Err(Error::ty("coll.sort() requires a list")),
    }
}

pub(crate) fn collection_index_of(vals: &[Value]) -> Result<Value> {
    match vals.first() {
        Some(Value::List(_)) if vals.get(1).is_none_or(Value::is_null) => Ok(Value::Null),
        Some(Value::List(items)) => {
            let search = &vals[1];
            Ok(Value::Int(
                items
                    .iter()
                    .position(|v| v.eq_non_null(search))
                    .map(|i| i as i64)
                    .unwrap_or(-1),
            ))
        }
        Some(Value::Null) => Ok(Value::Null),
        _ => Err(Error::ty("coll.indexOf() requires a list")),
    }
}

/// The shared (list, index) argument policy of `coll.insert` and
/// `coll.remove`: null propagates from either position, the index must be
/// an integer within bounds (`end_inclusive` also admits `len`, the
/// append position, for insertion), and `op` names the function in the
/// pinned denial texts. The index returns as `usize` so the bounds
/// invariant the checks establish survives in the type and callers never
/// re-cast. `None` is the null-propagation exit; the caller keeps only its
/// one-line operation.
fn list_index_args(
    args: &mut std::vec::IntoIter<Value>,
    op: &str,
    end_inclusive: bool,
) -> Result<Option<(Arc<Vec<Value>>, usize)>> {
    let items = match args.next() {
        Some(Value::List(items)) => items,
        Some(Value::Null) => return Ok(None),
        _ => return Err(Error::ty(format!("{op} requires a list"))),
    };
    let i = match args.next() {
        Some(Value::Null) | None => return Ok(None),
        Some(Value::Int(i)) => i,
        _ => return Err(Error::invalid_argument_type(format!("{op} index"))),
    };
    if i < 0 {
        return Err(Error::ty(format!("InvalidArgumentValue: {op} index")));
    }
    let i =
        usize::try_from(i).map_err(|_| Error::ty(format!("InvalidArgumentValue: {op} index")))?;
    let past_end = if end_inclusive {
        i > items.len()
    } else {
        i >= items.len()
    };
    if past_end {
        return Err(Error::ty(format!("InvalidArgumentValue: {op} index")));
    }
    Ok(Some((items, i)))
}

pub(crate) fn collection_insert(vals: Vec<Value>) -> Result<Value> {
    let mut args = vals.into_iter();
    let Some((items, i)) = list_index_args(&mut args, "coll.insert()", true)? else {
        return Ok(Value::Null);
    };
    let mut out = Arc::unwrap_or_clone(items);
    out.insert(i, args.next().unwrap_or(Value::Null));
    Ok(Value::List(out.into()))
}

pub(crate) fn collection_remove(vals: Vec<Value>) -> Result<Value> {
    let mut args = vals.into_iter();
    let Some((items, i)) = list_index_args(&mut args, "coll.remove()", false)? else {
        return Ok(Value::Null);
    };
    let mut out = Arc::unwrap_or_clone(items);
    out.remove(i);
    Ok(Value::List(out.into()))
}

pub(crate) fn collection_extreme(vals: &[Value], max: bool) -> Result<Value> {
    let Some(Value::List(items)) = vals.first() else {
        return if vals.first().is_some_and(Value::is_null) {
            Ok(Value::Null)
        } else {
            Err(Error::ty("collection extreme requires a list"))
        };
    };
    Ok(items
        .iter()
        .filter(|v| !v.is_null())
        .cloned()
        .reduce(|a, b| {
            let order = collection_value_cmp(&a, &b);
            if (max && order.is_lt()) || (!max && order.is_gt()) {
                b
            } else {
                a
            }
        })
        .unwrap_or(Value::Null))
}

pub(crate) fn collection_value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Temporal(_) => 0,
            Value::String(_) => 1,
            Value::Bool(_) => 2,
            Value::Int(_) | Value::Float(_) => 3,
            _ => 4,
        }
    }
    rank(a).cmp(&rank(b)).then_with(|| a.order_by_cmp(b))
}

/// The `coll.zip()` name as its type and arity errors spell it — one owner,
/// consulted by [`collection_zip`]'s checks and [`has_arg`]'s type error.
const ZIP_OP: &str = "coll.zip()";

/// `coll.zip(list1, list2, ...)` (CIP-274): zips two or more lists into a
/// list of per-index lists, to the length of the longest input, padding
/// missing positions with null. A null argument propagates null; a non-list
/// argument is a type error.
pub(crate) fn collection_zip(vals: &[Value]) -> Result<Value> {
    if vals.len() < 2 {
        return Err(Error::syntax(format!(
            "InvalidNumberOfArguments: {} requires at least two arguments, got {}",
            ZIP_OP,
            vals.len()
        )));
    }
    let mut lists: Vec<&Arc<Vec<Value>>> = Vec::with_capacity(vals.len());
    for v in vals {
        let Some(items) = has_arg(v, ZIP_OP)? else {
            return Ok(Value::Null);
        };
        lists.push(items);
    }
    // The arity check above guarantees at least two list arguments, so the
    // max is invariant-bound — expect, never a fabricated 0 default.
    let len = lists
        .iter()
        .map(|l| l.len())
        .max()
        .expect("coll.zip() arity check guarantees at least two lists");
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(Value::List(
            lists
                .iter()
                .map(|l| l.get(i).cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>()
                .into(),
        ));
    }
    Ok(Value::List(out.into()))
}

/// The one mapping of the existing quantifier vocabulary
/// (`cypherlite_ir::QuantifierKind`) onto the `coll.has*Of` function names
/// (CIP-286) — the names travel with the variants, never as a second
/// stringly argument. No `coll.has*Of` adapter constructs the `Single`
/// quantifier, so that arm is unreachable.
fn coll_op(kind: QuantifierKind) -> &'static str {
    match kind {
        QuantifierKind::All => "coll.hasAllOf()",
        QuantifierKind::Any => "coll.hasAnyOf()",
        QuantifierKind::None => "coll.hasNoneOf()",
        QuantifierKind::Single => {
            unreachable!("no coll.has*Of function constructs the Single quantifier")
        }
    }
}

/// The shared list-argument policy of the `coll.*` functions (zip and the
/// `coll.has*Of` family): a null argument (either position) propagates
/// null, and a non-list argument is a type error. `None` is the
/// null-propagation exit.
fn has_arg<'a>(v: &'a Value, op: &str) -> Result<Option<&'a Arc<Vec<Value>>>> {
    match v {
        Value::List(items) => Ok(Some(items)),
        Value::Null => Ok(None),
        _ => Err(Error::ty(format!("{op} requires a list"))),
    }
}

/// `coll.hasAnyOf` / `coll.hasAllOf` / `coll.hasNoneOf` (CIP-286): the
/// [`QuantifierKind`] reduction over the canonical membership predicate
/// [`membership`], via the one shared three-valued quantifier fold
/// (`eval::fold_quantifier`). Each quantifier exits early on
/// its decided value — `Any` on the first member, `All` on the first
/// non-member, `None` on the first member — and otherwise returns null
/// when any membership was unknown, never a fabricated boolean.
pub(crate) fn collection_has(kind: QuantifierKind, vals: &[Value]) -> Result<Value> {
    if vals.len() != 2 {
        return Err(Error::syntax(format!(
            "InvalidNumberOfArguments: {} requires exactly two arguments, got {}",
            coll_op(kind),
            vals.len()
        )));
    }
    // Left-to-right, like collection_zip: a null argument propagates null
    // before the other argument's type is checked.
    let Some(list) = has_arg(&vals[0], coll_op(kind))? else {
        return Ok(Value::Null);
    };
    let Some(values) = has_arg(&vals[1], coll_op(kind))? else {
        return Ok(Value::Null);
    };
    fold_quantifier(kind, values.iter(), |x| {
        // A null element of `list` could equal `x`: the membership is
        // unknown, never decided for this quantifier's exit.
        Ok(membership(x, list).truthy())
    })
}

pub(crate) fn collection_distinct(vals: &[Value]) -> Result<Value> {
    let Some(Value::List(items)) = vals.first() else {
        return if vals.first().is_some_and(Value::is_null) {
            Ok(Value::Null)
        } else {
            Err(Error::ty("coll.distinct() requires a list"))
        };
    };
    // First-seen emission under the canonical equivalence relation: the
    // GroupKey decides set membership; the first equivalent value is kept.
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    for item in items.iter() {
        if seen.insert(GroupKey::new(item)) {
            out.push(item.clone());
        }
    }
    Ok(Value::List(out.into()))
}
