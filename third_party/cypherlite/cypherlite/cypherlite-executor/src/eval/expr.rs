//! Scalar expression operators: unary/binary ops, property access,
//! subscript/slice, and CASE.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{temporal, ArithmeticFailure, Error, Result, Value};
use cypherlite_ir::{BinaryOp, Expr, UnaryOp};

use crate::overlay::ExecGraph;

use super::string::char_range;
use super::{bool_or_null, eval, Row};

pub(crate) fn reject_deleted_access(graph: &ExecGraph<'_>, v: &Value) -> Result<()> {
    match v {
        Value::Node(n) if graph.is_deleted_node(n.id) => Err(Error::execution(
            "DeletedEntityAccess: cannot access properties of a deleted node",
        )),
        Value::Relationship(r) if graph.is_deleted_rel(r.id) => Err(Error::execution(
            "DeletedEntityAccess: cannot access properties of a deleted relationship",
        )),
        _ => Ok(()),
    }
}

pub(crate) fn prop(graph: &ExecGraph<'_>, v: &Value, key: &str) -> Result<Value> {
    Ok(match v {
        Value::Node(n) => graph
            .node_prop(n.id, key)
            .or_else(|| n.properties.get(key).cloned())
            .unwrap_or(Value::Null),
        Value::Relationship(r) => graph
            .rel_prop(r.id, key)
            .or_else(|| r.properties.get(key).cloned())
            .unwrap_or(Value::Null),
        Value::Map(m) => m.get(key).cloned().unwrap_or(Value::Null),
        Value::Temporal(t) => t.component(key)?.unwrap_or(Value::Null),
        Value::Point(p) => match key.to_ascii_lowercase().as_str() {
            "x" | "longitude" => Value::Float(p.x),
            "y" | "latitude" => Value::Float(p.y),
            "z" | "height" => p.z.map(Value::Float).unwrap_or(Value::Null),
            "srid" => Value::Int(i64::from(p.srid)),
            "crs" => Value::String(
                match p.srid {
                    4326 => "wgs-84",
                    4979 => "wgs-84-3d",
                    9157 => "cartesian-3d",
                    _ => "cartesian",
                }
                .into(),
            ),
            _ => Value::Null,
        },
        Value::Null => Value::Null,
        _ => {
            return Err(Error::invalid_argument_type(
                "property access requires a map, node, relationship, or temporal value",
            ))
        }
    })
}

pub(crate) fn unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match op {
        UnaryOp::Not => match bool_or_null(&v, "NOT")? {
            Some(b) => Value::Bool(!b),
            None => Value::Null,
        },
        UnaryOp::Plus => match v {
            Value::Int(i) => Value::Int(i),
            Value::Float(f) => Value::Float(f),
            Value::Temporal(t @ temporal::Temporal::Duration { .. }) => Value::Temporal(t),
            Value::Null => Value::Null,
            _ => return Err(Error::ty("unary + needs a number")),
        },
        UnaryOp::Minus => match v {
            // i64::MIN is reachable (e.g. duration.between(...).months on
            // far-apart civil dates); negate honestly instead of panicking.
            Value::Int(i) => Value::Int(
                i.checked_neg()
                    .ok_or_else(|| Error::ty("integer out of range"))?,
            ),
            Value::Float(f) => Value::Float(-f),
            Value::Temporal(temporal::Temporal::Duration {
                months,
                days,
                nanos,
            }) => Value::Temporal(temporal::Temporal::Duration {
                months: months
                    .checked_neg()
                    .ok_or_else(|| Error::ty("temporal value out of range"))?,
                days: days
                    .checked_neg()
                    .ok_or_else(|| Error::ty("temporal value out of range"))?,
                nanos: nanos
                    .checked_neg()
                    .ok_or_else(|| Error::ty("temporal value out of range"))?,
            }),
            Value::Null => Value::Null,
            _ => return Err(Error::ty("unary - needs a number")),
        },
        UnaryOp::IsNull => Value::Bool(v.is_null()),
        UnaryOp::IsNotNull => Value::Bool(!v.is_null()),
    })
}

pub(crate) fn binary(op: BinaryOp, l: Value, r: Value) -> Result<Value> {
    Ok(match op {
        BinaryOp::Eq => l.cypher_eq(&r),
        BinaryOp::Neq => match l.cypher_eq(&r) {
            Value::Bool(b) => Value::Bool(!b),
            other => other,
        },
        BinaryOp::Lt => vector_cmp_or(l, r, false, |o| o.is_lt()),
        BinaryOp::Gt => vector_cmp_or(l, r, false, |o| o.is_gt()),
        BinaryOp::Lte => vector_cmp_or(l, r, true, |o| !o.is_gt()),
        BinaryOp::Gte => vector_cmp_or(l, r, true, |o| !o.is_lt()),
        BinaryOp::And => match (bool_or_null(&l, "AND")?, bool_or_null(&r, "AND")?) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        },
        BinaryOp::Or => match (bool_or_null(&l, "OR")?, bool_or_null(&r, "OR")?) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        BinaryOp::Xor => match (bool_or_null(&l, "XOR")?, bool_or_null(&r, "XOR")?) {
            (Some(a), Some(b)) => Value::Bool(a ^ b),
            _ => Value::Null,
        },
        BinaryOp::Add => add(l, r)?,
        BinaryOp::Sub => sub(l, r)?,
        BinaryOp::Mul => mul(l, r)?,
        BinaryOp::Div => div(l, r)?,
        BinaryOp::Mod => mod_op(l, r)?,
        BinaryOp::Pow => pow(l, r)?,
        BinaryOp::Concat => concat(l, r)?,
        BinaryOp::In => in_op(l, r)?,
        BinaryOp::StartsWith => str_pred(l, r, |a, b| a.starts_with(b)),
        BinaryOp::EndsWith => str_pred(l, r, |a, b| a.ends_with(b)),
        BinaryOp::Contains => str_pred(l, r, |a, b| a.contains(b)),
        BinaryOp::Regex => match (&l, &r) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(a), Value::String(b)) => match regex_is_match(a, b) {
                Ok(m) => Value::Bool(m),
                Err(_) => return Err(Error::syntax("invalid regular expression")),
            },
            _ => Value::Null,
        },
    })
}

pub(crate) fn regex_is_match(text: &str, pattern: &str) -> std::result::Result<bool, ()> {
    regex::Regex::new(pattern)
        .map(|re| re.is_match(text))
        .map_err(|_| ())
}

pub(crate) fn cmp_bool(l: Value, r: Value, pred: impl Fn(std::cmp::Ordering) -> bool) -> Value {
    let ln = l.as_number();
    let rn = r.as_number();
    if ln.is_some_and(|f| f.is_nan()) || rn.is_some_and(|f| f.is_nan()) {
        return if ln.is_some() && rn.is_some() {
            Value::Bool(false)
        } else {
            Value::Null
        };
    }
    match l.compare(&r) {
        Some(o) => Value::Bool(pred(o)),
        None => Value::Null,
    }
}

pub(crate) fn vector_cmp_or(
    l: Value,
    r: Value,
    equality_allowed: bool,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Value {
    if matches!(l, Value::Vector { .. }) || matches!(r, Value::Vector { .. }) {
        return if equality_allowed && l.eq_non_null(&r) {
            Value::Bool(true)
        } else {
            Value::Null
        };
    }
    cmp_bool(l, r, pred)
}

pub(crate) fn add(l: Value, r: Value) -> Result<Value> {
    Ok(match (l, r) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::String(a), Value::String(b)) => Value::String(format!("{a}{b}").into()),
        (Value::String(a), r @ (Value::Int(_) | Value::Float(_) | Value::Bool(_))) => {
            Value::String(format!("{a}{}", r.to_cypher_literal()).into())
        }
        (l @ (Value::Int(_) | Value::Float(_) | Value::Bool(_)), Value::String(b)) => {
            Value::String(format!("{}{b}", l.to_cypher_literal()).into())
        }
        (Value::List(a), Value::List(b)) => {
            let mut o = Arc::unwrap_or_clone(a);
            o.extend(Arc::unwrap_or_clone(b));
            Value::List(o.into())
        }
        // CIP-273: MAP + MAP is a shallow merge with last-write-wins on
        // duplicate keys (extend overwrites), null values included.
        (Value::Map(a), Value::Map(b)) => {
            let mut o = Arc::unwrap_or_clone(a);
            o.extend(Arc::unwrap_or_clone(b));
            Value::Map(o.into())
        }
        (Value::List(a), r) => {
            let mut o = Arc::unwrap_or_clone(a);
            o.push(r);
            Value::List(o.into())
        }
        (l, Value::List(b)) => {
            let mut o = vec![l];
            o.extend(Arc::unwrap_or_clone(b));
            Value::List(o.into())
        }
        (Value::Temporal(a), Value::Temporal(b)) => {
            if matches!(b, temporal::Temporal::Duration { .. }) {
                Value::Temporal(temporal::add_duration(&a, &b)?)
            } else if matches!(a, temporal::Temporal::Duration { .. }) {
                Value::Temporal(temporal::add_duration(&b, &a)?)
            } else {
                return Err(Error::ty("+ type mismatch"));
            }
        }
        (Value::Int(a), Value::Int(b)) => Value::Int(
            // Checked like Sub/Mul/Div/Mod: integer overflow is an honest
            // range error (Neo4j errors too), never a silent saturation.
            a.checked_add(b)
                .ok_or_else(|| Error::ty("integer out of range"))?,
        ),
        (l, r) => {
            if let (Some(a), Some(b)) = (l.as_number(), r.as_number()) {
                // Both-integer addition is handled by the `(Int, Int)` arm
                // above, so this is the float path.
                Value::Float(a + b)
            } else {
                return Err(Error::ty("+ type mismatch"));
            }
        }
    })
}

pub(crate) fn concat(l: Value, r: Value) -> Result<Value> {
    Ok(match (l, r) {
        (Value::String(a), Value::String(b)) => Value::String(format!("{a}{b}").into()),
        (Value::List(a), Value::List(b)) => {
            let mut out = Arc::unwrap_or_clone(a);
            out.extend(Arc::unwrap_or_clone(b));
            Value::List(out.into())
        }
        (Value::Null, Value::String(_) | Value::List(_))
        | (Value::String(_) | Value::List(_), Value::Null)
        | (Value::Null, Value::Null) => Value::Null,
        _ => {
            return Err(Error::invalid_argument_type(
                "|| requires two strings or two lists",
            ))
        }
    })
}

pub(crate) fn sub(l: Value, r: Value) -> Result<Value> {
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Temporal(a), Value::Temporal(temporal::Temporal::Duration { .. })) => Ok(
            Value::Temporal(temporal::sub_duration(a, r.as_temporal().unwrap())?),
        ),
        (Value::Temporal(a), Value::Temporal(b)) => {
            Ok(Value::Temporal(temporal::duration_between(b, a)?))
        }
        _ => num_op(l, r, i64::checked_sub, |a, b| a - b),
    }
}

pub(crate) fn mul(l: Value, r: Value) -> Result<Value> {
    Ok(match (&l, &r) {
        // Duration * number is commutative, so one or-pattern arm covers
        // both operand orders; a non-number other operand (including
        // duration * duration) takes the same numeric fall-through as any
        // other operand pair.
        (Value::Temporal(d @ temporal::Temporal::Duration { .. }), b)
        | (b, Value::Temporal(d @ temporal::Temporal::Duration { .. })) => match b.as_number() {
            Some(n) => Value::Temporal(temporal::scale_duration(d, n)?),
            None => num_op(l, r, i64::checked_mul, |a, b| a * b)?,
        },
        _ => num_op(l, r, i64::checked_mul, |a, b| a * b)?,
    })
}

pub(crate) fn div(l: Value, r: Value) -> Result<Value> {
    Ok(match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Temporal(d @ temporal::Temporal::Duration { .. }), b) => {
            let n = b.as_number().ok_or_else(|| Error::ty("/ needs numbers"))?;
            if n == 0.0 {
                return Err(Error::arithmetic(ArithmeticFailure::DivisionByZero));
            }
            Value::Temporal(temporal::scale_duration(d, 1.0 / n)?)
        }
        (Value::Int(a), Value::Int(b)) => {
            if *b == 0 {
                return Err(Error::arithmetic(ArithmeticFailure::DivisionByZero));
            }
            // ADR-0175: i64::MIN / -1 wraps to i64::MIN, matching
            // Neo4j's plain Java `/`, which carries no overflow check
            // (unlike +, -, * and unary minus, which keep raising
            // `integer out of range`).
            Value::Int(a.wrapping_div(*b))
        }
        _ => {
            let a = l.as_number().ok_or_else(|| Error::ty("/ needs numbers"))?;
            let b = r.as_number().ok_or_else(|| Error::ty("/ needs numbers"))?;
            Value::Float(a / b)
        }
    })
}

pub(crate) fn mod_op(l: Value, r: Value) -> Result<Value> {
    Ok(match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int(a), Value::Int(b)) => {
            if *b == 0 {
                return Err(Error::arithmetic(ArithmeticFailure::ModuloByZero));
            }
            // ADR-0175: i64::MIN % -1 is 0, matching Neo4j's plain Java
            // `%` (no overflow check on modulo either).
            Value::Int(a.wrapping_rem(*b))
        }
        _ => {
            let a = l.as_number().ok_or_else(|| Error::ty("% needs numbers"))?;
            let b = r.as_number().ok_or_else(|| Error::ty("% needs numbers"))?;
            Value::Float(a % b)
        }
    })
}

pub(crate) fn pow(l: Value, r: Value) -> Result<Value> {
    Ok(if l.is_null() || r.is_null() {
        Value::Null
    } else {
        let a = l.as_number().ok_or_else(|| Error::ty("^ needs numbers"))?;
        let b = r.as_number().ok_or_else(|| Error::ty("^ needs numbers"))?;
        Value::Float(a.powf(b))
    })
}

/// Generic numeric binary op. The integer arm must be a checked op
/// (`None` on overflow) so extreme integers surface an honest range error
/// instead of unwinding into the host; floats never trap.
pub(crate) fn num_op(
    l: Value,
    r: Value,
    on_int: impl Fn(i64, i64) -> Option<i64>,
    on_float: impl Fn(f64, f64) -> f64,
) -> Result<Value> {
    Ok(match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int(a), Value::Int(b)) => {
            Value::Int(on_int(*a, *b).ok_or_else(|| Error::ty("integer out of range"))?)
        }
        _ => {
            let a = l.as_number().ok_or_else(|| Error::ty("numeric op"))?;
            let b = r.as_number().ok_or_else(|| Error::ty("numeric op"))?;
            Value::Float(on_float(a, b))
        }
    })
}

pub(crate) fn in_op(l: Value, r: Value) -> Result<Value> {
    Ok(match r {
        Value::Null => Value::Null,
        Value::List(xs) => super::membership(&l, &xs),
        _ => {
            return Err(Error::invalid_argument_type(
                "IN requires a list on the right-hand side",
            ))
        }
    })
}

pub(crate) fn str_pred(l: Value, r: Value, f: impl Fn(&str, &str) -> bool) -> Value {
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::String(a), Value::String(b)) => Value::Bool(f(a, b)),
        _ => Value::Null,
    }
}

pub(crate) fn subscript(graph: &ExecGraph<'_>, v: &Value, index: Value) -> Result<Value> {
    Ok(match (v, index) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::List(xs), idx) => {
            // ADR-0175: any FLOAT index — including 1.0 and NaN — is a type
            // error (Neo4j 22N01); no truncation.
            let i = match idx {
                Value::Int(i) => i,
                _ => {
                    return Err(Error::invalid_argument_type(
                        "list index must be an integer",
                    ))
                }
            };
            let n = xs.len() as i64;
            let i = if i < 0 { n + i } else { i };
            if i < 0 || i >= n {
                Value::Null
            } else {
                xs[i as usize].clone()
            }
        }
        // A string key rides the same dispatch as `n.k` (property access),
        // so the two paths cannot drift (the Map/Node/Relationship arms used
        // to be a hand-copied, subtly divergent subset of prop).
        (_, Value::String(k)) => prop(graph, v, &k)?,
        (Value::Vector { .. }, _) => {
            return Err(Error::invalid_argument_type(
                "vector indexing is not supported",
            ))
        }
        (Value::Map(_), _) | (Value::Node(_), _) | (Value::Relationship(_), _) => {
            return Err(Error::ty(
                "MapElementAccessByNonString: map index must be a string",
            ))
        }
        _ => {
            return Err(Error::invalid_argument_type(
                "subscript requires a list or map",
            ))
        }
    })
}

pub(crate) fn slice(v: &Value, start: Option<Value>, end: Option<Value>) -> Result<Value> {
    let (start, end) = (start.as_ref(), end.as_ref());
    Ok(match v {
        Value::Null => Value::Null,
        Value::List(xs) => match slice_bounds(xs.len(), start, end)? {
            Some(range) => Value::List(xs[range].to_vec().into()),
            None => Value::Null,
        },
        // A string slices by character; the substring is copied once.
        Value::String(s) => match slice_bounds(s.chars().count(), start, end)? {
            Some(range) => Value::String(char_range(s, range).into()),
            None => Value::Null,
        },
        _ => return Err(Error::ty("slice needs a list")),
    })
}

/// The index range `[start..end]` selects from a sequence of `len` elements,
/// or `None` when either bound is null (the slice is null, even if the other
/// bound is not an integer). A missing bound is the sequence's start or end;
/// a negative bound counts from the end; both clamp to `0..=len`, and a
/// start at or past the end selects nothing.
///
/// ADR-0175: a bound converts like Neo4j's `asIntExact`
/// (CypherFunctions.java L3257–L3296): an integer passes through; a float
/// converts with Java `(long)`-cast semantics (truncate toward zero,
/// NaN → 0, saturate at the i64 limits); the result must fit the int32
/// range — beyond it (±Infinity included) raises NumberOutOfRange. A
/// non-numeric bound keeps the historical bare `what` type error.
fn slice_bounds(
    len: usize,
    start: Option<&Value>,
    end: Option<&Value>,
) -> Result<Option<std::ops::Range<usize>>> {
    if matches!(start, Some(Value::Null)) || matches!(end, Some(Value::Null)) {
        return Ok(None);
    }
    let n = len as i64;
    let bound = |b: Option<&Value>, missing: i64, what: &str| -> Result<usize> {
        let i = match b {
            None => missing,
            Some(v) => {
                let i = super::numeric::int_arg(Some(v)).ok_or_else(|| Error::ty(what))?;
                if i < i64::from(i32::MIN) || i > i64::from(i32::MAX) {
                    return Err(Error::ty(format!("NumberOutOfRange: {what}")));
                }
                i
            }
        };
        Ok((if i < 0 { i + n } else { i }).clamp(0, n) as usize)
    };
    let s = bound(start, 0, "slice start")?;
    let e = bound(end, n, "slice end")?;
    Ok(Some(s..e.max(s)))
}

pub(crate) fn eval_case(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    operand: &Option<Box<Expr>>,
    when_then: &[(Expr, Expr, bool)],
    else_expr: &Option<Box<Expr>>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    if let Some(op) = operand {
        let ov = eval(graph, row, op, params, cypher_version)?;
        for (w, t, is_predicate) in when_then {
            let wv = eval(graph, row, w, params, cypher_version)?;
            if (*is_predicate && wv.truthy() == Some(true))
                || (!*is_predicate && ov.eq_non_null(&wv))
            {
                return eval(graph, row, t, params, cypher_version);
            }
        }
    } else {
        for (w, t, _) in when_then {
            if eval(graph, row, w, params, cypher_version)?.truthy() == Some(true) {
                return eval(graph, row, t, params, cypher_version);
            }
        }
    }
    if let Some(e) = else_expr {
        eval(graph, row, e, params, cypher_version)
    } else {
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }

    fn int(x: i64) -> Value {
        Value::Int(x)
    }

    fn ty_err(msg: &str) -> String {
        Error::ty(msg).to_string()
    }

    /// Five characters of 1, 2, 4, 1 and 2 bytes: slice bounds count
    /// characters, so a byte-indexed slice would split a code point.
    const MB: &str = "añ😀bç";

    fn mb_slice(start: Option<Value>, end: Option<Value>) -> Value {
        slice(&s(MB), start, end).unwrap()
    }

    #[test]
    fn string_slice_indexes_by_character() {
        assert_eq!(mb_slice(Some(int(1)), Some(int(3))), s("ñ😀"));
        assert_eq!(mb_slice(Some(int(2)), None), s("😀bç"));
        assert_eq!(mb_slice(None, Some(int(2))), s("añ"));
        assert_eq!(mb_slice(None, None), s(MB));
        assert_eq!(mb_slice(Some(int(4)), Some(int(5))), s("ç"));
        // Integral floats are integer bounds.
        assert_eq!(
            mb_slice(Some(Value::Float(1.0)), Some(Value::Float(2.0))),
            s("ñ")
        );
    }

    #[test]
    fn string_slice_negative_bounds_count_from_the_end() {
        assert_eq!(mb_slice(Some(int(-2)), None), s("bç"));
        assert_eq!(mb_slice(Some(int(-4)), Some(int(-1))), s("ñ😀b"));
        assert_eq!(mb_slice(None, Some(int(-3))), s("añ"));
    }

    #[test]
    fn string_slice_out_of_range_bounds_clamp() {
        assert_eq!(mb_slice(Some(int(-10)), Some(int(10))), s(MB));
        assert_eq!(mb_slice(Some(int(3)), Some(int(99))), s("bç"));
        assert_eq!(mb_slice(Some(int(-99)), Some(int(2))), s("añ"));
    }

    #[test]
    fn string_slice_empty_range_is_the_empty_string() {
        assert_eq!(mb_slice(Some(int(2)), Some(int(2))), s(""));
        assert_eq!(mb_slice(Some(int(4)), Some(int(1))), s(""));
        assert_eq!(mb_slice(Some(int(7)), None), s(""));
        assert_eq!(mb_slice(None, Some(int(-9))), s(""));
        assert_eq!(slice(&s(""), Some(int(0)), Some(int(1))).unwrap(), s(""));
    }

    #[test]
    fn string_slice_null_and_non_integer_bounds() {
        assert_eq!(mb_slice(Some(Value::Null), Some(int(2))), Value::Null);
        assert_eq!(mb_slice(Some(int(1)), Some(Value::Null)), Value::Null);
        assert_eq!(mb_slice(Some(Value::Null), None), Value::Null);
        // A null bound wins over a non-integer one.
        assert_eq!(mb_slice(Some(s("x")), Some(Value::Null)), Value::Null);
        let err = |start, end| slice(&s(MB), start, end).unwrap_err().to_string();
        assert_eq!(err(Some(s("x")), None), ty_err("slice start"));
        // ADR-0175: a float bound truncates instead of erroring; a
        // non-numeric bound keeps the bare type error.
        assert_eq!(err(None, Some(Value::Bool(true))), ty_err("slice end"));
        // The start bound is checked first.
        assert_eq!(err(Some(s("x")), Some(s("y"))), ty_err("slice start"));
    }

    #[test]
    fn list_slice_follows_the_same_bounds_rules() {
        let list = |xs: &[i64]| Value::List(xs.iter().map(|&x| int(x)).collect::<Vec<_>>().into());
        let xs = list(&[0, 1, 2, 3]);
        let sl = |start, end| slice(&xs, start, end);
        assert_eq!(sl(Some(int(1)), Some(int(3))).unwrap(), list(&[1, 2]));
        assert_eq!(sl(Some(int(-3)), Some(int(-1))).unwrap(), list(&[1, 2]));
        assert_eq!(
            sl(Some(int(-9)), Some(int(9))).unwrap(),
            list(&[0, 1, 2, 3])
        );
        assert_eq!(sl(Some(int(3)), Some(int(1))).unwrap(), list(&[]));
        assert_eq!(sl(None, Some(Value::Null)).unwrap(), Value::Null);
        assert_eq!(
            sl(Some(Value::Bool(true)), None).unwrap_err().to_string(),
            ty_err("slice start")
        );
        assert_eq!(
            sl(None, Some(s("x"))).unwrap_err().to_string(),
            ty_err("slice end")
        );
    }

    #[test]
    fn slice_of_null_or_a_non_sequence() {
        assert_eq!(
            slice(&Value::Null, Some(s("x")), None).unwrap(),
            Value::Null
        );
        assert_eq!(
            slice(&int(5), Some(s("x")), None).unwrap_err().to_string(),
            ty_err("slice needs a list")
        );
    }
}
