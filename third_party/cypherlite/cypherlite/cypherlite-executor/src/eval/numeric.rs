//! Numeric function helpers (`round`, unary int/float adapters).

use cypherlite_core::{ArithmeticFailure, Error, Result, Value};

/// The largest `round()` precision the f64-backed engine supports: an f64
/// carries at most ~15–17 significant decimal digits, so a larger decimal
/// precision cannot express anything the value does not already have, and
/// the scale computation would overflow (ADR-0336).
const MAX_ROUND_PRECISION: i64 = 15;

/// The `round()` mode vocabulary (Neo4j's eight names), parsed once at the
/// call boundary so the rounding dispatch matches on a typed mode instead of
/// re-classifying the raw strings (the `TruncUnit::parse` shape in
/// cypherlite-core).
#[derive(Clone, Copy)]
enum RoundingMode {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

impl RoundingMode {
    /// Parses a wire spelling; any other spelling is the
    /// `UnsupportedRoundingMode` error.
    fn parse(mode: &str) -> Result<RoundingMode> {
        Ok(match mode {
            "UP" => RoundingMode::Up,
            "DOWN" => RoundingMode::Down,
            "CEILING" => RoundingMode::Ceiling,
            "FLOOR" => RoundingMode::Floor,
            "HALF_UP" => RoundingMode::HalfUp,
            "HALF_DOWN" => RoundingMode::HalfDown,
            "HALF_EVEN" => RoundingMode::HalfEven,
            "UNNECESSARY" => RoundingMode::Unnecessary,
            _ => return Err(Error::execution("UnsupportedRoundingMode")),
        })
    }
}

pub(crate) fn round_function(vals: &[Value]) -> Result<Value> {
    if vals.is_empty() || vals.len() > 3 {
        return Err(Error::syntax(
            "InvalidNumberOfArguments: round() requires one to three arguments",
        ));
    }
    if vals.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let value = vals[0]
        .as_number()
        .ok_or_else(|| Error::invalid_argument_type("round value must be numeric"))?;
    let precision = match vals.get(1) {
        None => 0,
        Some(Value::Int(value)) if (0..=MAX_ROUND_PRECISION).contains(value) => *value as i32,
        Some(Value::Int(value)) if *value < 0 => {
            return Err(Error::execution(
                "NumericValueOutOfRange: round precision must be non-negative",
            ))
        }
        Some(Value::Int(_)) => {
            return Err(Error::execution(format!(
                "NumericValueOutOfRange: round precision must be at most {MAX_ROUND_PRECISION}"
            )))
        }
        _ => {
            return Err(Error::invalid_argument_type(
                "round precision must be INTEGER",
            ))
        }
    };
    let mode = match vals.get(2) {
        None => RoundingMode::HalfUp,
        Some(Value::String(mode)) => RoundingMode::parse(mode)?,
        _ => return Err(Error::invalid_argument_type("round mode must be STRING")),
    };
    if !value.is_finite() {
        return Ok(Value::Float(value));
    }
    let scale = 10_f64.powi(precision);
    let scaled = value * scale;
    let rounded = match mode {
        RoundingMode::Up => {
            if scaled.is_sign_negative() {
                scaled.floor()
            } else {
                scaled.ceil()
            }
        }
        RoundingMode::Down => scaled.trunc(),
        RoundingMode::Ceiling => scaled.ceil(),
        RoundingMode::Floor => scaled.floor(),
        RoundingMode::HalfUp => scaled.round(),
        RoundingMode::HalfEven => scaled.round_ties_even(),
        RoundingMode::HalfDown => {
            let trunc = scaled.trunc();
            if (scaled - trunc).abs() > 0.5 {
                scaled.round()
            } else {
                trunc
            }
        }
        RoundingMode::Unnecessary if scaled.fract() != 0.0 => {
            return Err(Error::arithmetic(ArithmeticFailure::RoundingNecessary))
        }
        RoundingMode::Unnecessary => scaled,
    };
    Ok(Value::Float(rounded / scale))
}

/// Generic numeric unary op. The integer arm must be a checked op (`None`
/// on overflow — e.g. `abs(i64::MIN)`) so extreme integers surface an honest
/// range error instead of unwinding into the host.
pub(crate) fn num_unary(
    vals: &[Value],
    on_int: impl Fn(i64) -> Option<i64>,
    on_float: impl Fn(f64) -> f64,
) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::Int(i)) => {
            Value::Int(on_int(*i).ok_or_else(|| Error::ty("integer out of range"))?)
        }
        Some(Value::Float(f)) => Value::Float(on_float(*f)),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("expected number")),
    })
}

pub(crate) fn float_unary(vals: &[Value], f: impl Fn(f64) -> f64) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::Int(i)) => Value::Float(f(*i as f64)),
        Some(Value::Float(x)) => Value::Float(f(*x)),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("expected number")),
    })
}

/// The shared FLOAT → INTEGER conversion (ADR-0168, ADR-0175), used by the
/// integer-argument sites (`range()`, `substring()` / `left()` /
/// `right()`, list-slice bounds) and the `toInteger()` /
/// `toIntegerOrNull()` / `toIntegerList()` family. It is Java's
/// `(long) d`, which Neo4j applies through `NumberValue.longValue()`, and
/// Rust's saturating `as` cast has exactly those semantics: NaN converts
/// to 0, values at or beyond the `i64` limits (±Infinity included)
/// saturate to those limits, and every other value truncates toward zero.
/// A float never fails to convert.
pub(crate) fn float_to_i64(f: f64) -> i64 {
    f as i64
}

/// ADR-0175's shared integer-argument coercion (`range()`,
/// `substring()` / `left()` / `right()`, list-slice bounds): an INTEGER
/// passes through, a FLOAT converts via [`float_to_i64`], and anything
/// else — null, missing, or non-numeric — is `None` so the call site
/// applies its own policy (type error, range gate, or default fallback).
pub(crate) fn int_arg(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int(i)) => Some(*i),
        Some(Value::Float(f)) => Some(float_to_i64(*f)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0336: the precision is bounded to 0..=15 — a larger one gets the
    /// same NumericValueOutOfRange treatment as a negative one, never the
    /// NaN the overflow-prone scale computation used to produce.
    #[test]
    fn round_precision_is_bounded() {
        let too_large =
            Error::execution("NumericValueOutOfRange: round precision must be at most 15");
        for precision in [16, 400, i64::MAX] {
            assert_eq!(
                round_function(&[Value::Float(2.5), Value::Int(precision)]).unwrap_err(),
                too_large,
                "precision {precision}"
            );
        }
        assert_eq!(
            round_function(&[Value::Float(2.5), Value::Int(-1)]).unwrap_err(),
            Error::execution("NumericValueOutOfRange: round precision must be non-negative")
        );
        assert_eq!(
            round_function(&[Value::Float(2.5), Value::Int(15)]).unwrap(),
            Value::Float(2.5)
        );
        assert_eq!(
            round_function(&[Value::Float(2.5)]).unwrap(),
            Value::Float(3.0)
        );
        assert_eq!(
            round_function(&[Value::Float(-2.5)]).unwrap(),
            Value::Float(-3.0)
        );
        assert_eq!(
            round_function(&[Value::Float(2.5), Value::Int(2)]).unwrap(),
            Value::Float(2.5)
        );
        assert_eq!(
            round_function(&[Value::Null, Value::Int(400)]).unwrap(),
            Value::Null
        );
    }

    /// The `UNNECESSARY` mode keeps its typed failure at a bounded
    /// precision.
    #[test]
    fn round_unnecessary_keeps_its_typed_failure() {
        assert_eq!(
            round_function(&[
                Value::Float(2.5),
                Value::Int(0),
                Value::String("UNNECESSARY".into())
            ])
            .unwrap_err(),
            Error::arithmetic(ArithmeticFailure::RoundingNecessary)
        );
        assert_eq!(
            round_function(&[
                Value::Float(2.0),
                Value::Int(0),
                Value::String("UNNECESSARY".into())
            ])
            .unwrap(),
            Value::Float(2.0)
        );
    }

    /// The mode vocabulary parses once at the boundary: every accepted
    /// spelling reaches a typed variant, and any other spelling keeps the
    /// `UnsupportedRoundingMode` error.
    #[test]
    fn round_mode_parses_the_wire_vocabulary_once() {
        for mode in [
            "UP",
            "DOWN",
            "CEILING",
            "FLOOR",
            "HALF_UP",
            "HALF_DOWN",
            "HALF_EVEN",
            "UNNECESSARY",
        ] {
            assert!(RoundingMode::parse(mode).is_ok(), "{mode}");
        }
        assert_eq!(
            round_function(&[
                Value::Float(2.5),
                Value::Int(0),
                Value::String("half_up".into())
            ])
            .unwrap_err(),
            Error::execution("UnsupportedRoundingMode")
        );
    }
}
