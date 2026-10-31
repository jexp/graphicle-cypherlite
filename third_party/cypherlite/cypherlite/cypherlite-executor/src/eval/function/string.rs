//! String builtins: case mapping, the trim family, replace/split/substring,
//! `string.*`, `normalize`, and the length variants.

use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use unicode_normalization::UnicodeNormalization;

use crate::overlay::ExecGraph;

use super::super::string::{char_range, str_unary, string_arg, trim_function, TrimMode};
use super::write_scalar;

pub(super) fn bi_string_interp(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    // A lone STRING part (`f"{s}"`) is the result itself, shared.
    if let [Value::String(s)] = vals.as_slice() {
        return Ok(Value::String(Arc::clone(s)));
    }
    let mut out = String::new();
    for v in &vals {
        match v {
            Value::Null => return Ok(Value::Null),
            Value::Vector { .. } => out.push_str(&v.to_display_string()),
            other => {
                if !write_scalar(&mut out, other) {
                    return Err(Error::ty(
                        "InvalidArgumentValue: string interpolation cannot coerce value to STRING",
                    ));
                }
            }
        }
    }
    Ok(Value::String(out.into()))
}

pub(super) fn bi_normalize(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let Some(value) = vals.first() else {
            return Err(Error::semantic("normalize() requires an argument"));
        };
        match value {
            Value::Null => Value::Null,
            Value::String(s) => {
                let form = vals.get(1).and_then(Value::as_string).unwrap_or("NFC");
                let normalized: String = match form.to_ascii_uppercase().as_str() {
                    "NFD" => s.nfd().collect(),
                    "NFKC" => s.nfkc().collect(),
                    "NFKD" => s.nfkd().collect(),
                    _ => s.nfc().collect(),
                };
                Value::String(normalized.into())
            }
            _ => return Err(Error::ty("normalize() requires a string")),
        }
    })
}

pub(super) fn bi_char_length(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::String(s)) => Value::Int(s.chars().count() as i64),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("char_length() needs a string")),
    })
}

pub(super) fn bi_octet_length(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::String(s)) => Value::Int(s.len() as i64),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("octet_length() needs a string")),
    })
}

pub(super) fn bi_lower(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    str_unary(&vals, |s| s.to_lowercase())
}

pub(super) fn bi_upper(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    str_unary(&vals, |s| s.to_uppercase())
}

pub(super) fn bi_trim(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    trim_function(&vals, TrimMode::Both, true)
}

pub(super) fn bi_trim_leading(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    trim_function(&vals, TrimMode::Leading, true)
}

pub(super) fn bi_trim_trailing(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    trim_function(&vals, TrimMode::Trailing, true)
}

pub(super) fn bi_btrim(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    trim_function(&vals, TrimMode::Both, false)
}

pub(super) fn bi_ltrim(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    trim_function(&vals, TrimMode::Leading, false)
}

pub(super) fn bi_rtrim(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    trim_function(&vals, TrimMode::Trailing, false)
}

pub(super) fn bi_replace(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let s = string_arg(&vals, 0)?;
        let from = string_arg(&vals, 1)?;
        let to = string_arg(&vals, 2)?;
        match (s, from, to) {
            (Some(s), Some(f), Some(t)) => {
                if let Some(limit) = vals.get(3) {
                    let count = match limit {
                        Value::Int(value) if *value >= 0 => *value as usize,
                        Value::Null => return Ok(Value::Null),
                        Value::Int(_) => {
                            return Err(Error::execution(
                                "InvalidArgumentValue: replace limit must be non-negative",
                            ))
                        }
                        _ => {
                            return Err(Error::invalid_argument_type(
                                "replace limit must be INTEGER",
                            ))
                        }
                    };
                    Value::String(s.replacen(f, t, count).into())
                } else {
                    Value::String(s.replace(f, t).into())
                }
            }
            _ => Value::Null,
        }
    })
}
pub(super) fn bi_split(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let Some(source) = vals.first() else {
            return Err(Error::invalid_argument_type("split requires two arguments"));
        };
        if source.is_null() {
            return Ok(Value::Null);
        }
        let Value::String(source) = source else {
            return Err(Error::invalid_argument_type("split input must be STRING"));
        };
        match vals.get(1) {
            Some(Value::Null) | None => Value::Null,
            Some(Value::String(delimiter)) if delimiter.is_empty() => Value::List(
                source
                    .char_indices()
                    .map(|(i, ch)| Value::String(source[i..i + ch.len_utf8()].into()))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Some(Value::String(delimiter)) => Value::List(
                source
                    .split(&**delimiter)
                    .map(|part| Value::String(part.into()))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Some(Value::List(delimiters)) => {
                if delimiters.iter().any(Value::is_null) {
                    Value::Null
                } else {
                    let mut unique: Vec<&str> = Vec::new();
                    for delimiter in delimiters.iter() {
                        let Value::String(delimiter) = delimiter else {
                            return Err(Error::invalid_argument_type(
                                "split delimiters must be STRING",
                            ));
                        };
                        if !unique.contains(&&**delimiter) {
                            unique.push(delimiter);
                        }
                    }
                    if unique.is_empty() {
                        Value::List(vec![Value::String(source.clone())].into())
                    } else {
                        let pattern = unique
                            .iter()
                            .map(|delimiter| regex::escape(delimiter))
                            .collect::<Vec<_>>()
                            .join("|");
                        let splitter = regex::Regex::new(&pattern)
                            .map_err(|_| Error::ty("InvalidArgumentValue: delimiter"))?;
                        Value::List(
                            splitter
                                .split(source)
                                .map(|part| Value::String(part.into()))
                                .collect::<Vec<_>>()
                                .into(),
                        )
                    }
                }
            }
            _ => return Err(Error::invalid_argument_type("split delimiter")),
        }
    })
}
pub(super) fn bi_string_indexof(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(match (vals.first(), vals.get(1)) {
        (Some(Value::Null), _) | (_, Some(Value::Null)) => Value::Null,
        (Some(Value::String(input)), Some(Value::String(needle))) => {
            Value::Int(input.find(&**needle).map_or(-1, |index| index as i64))
        }
        _ => return Err(Error::invalid_argument_type("string.indexOf")),
    })
}
pub(super) fn bi_string_join(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match (vals.first(), vals.get(1)) {
        (Some(Value::Null), _) | (_, Some(Value::Null)) => Value::Null,
        (Some(Value::List(items)), Some(Value::String(delimiter))) => {
            let strings = items
                .iter()
                .filter(|item| !item.is_null())
                .map(|item| match item {
                    Value::String(value) => Ok(&**value),
                    _ => Err(Error::invalid_argument_type("string.join list item")),
                })
                .collect::<Result<Vec<_>>>()?;
            Value::String(strings.join(&**delimiter).into())
        }
        _ => return Err(Error::invalid_argument_type("string.join")),
    })
}
pub(super) fn bi_string_regexreplace(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(match (vals.first(), vals.get(1), vals.get(2)) {
        (Some(Value::Null), _, _) | (_, Some(Value::Null), _) | (_, _, Some(Value::Null)) => {
            Value::Null
        }
        (
            Some(Value::String(input)),
            Some(Value::String(pattern)),
            Some(Value::String(replacement)),
        ) => {
            let pattern = regex::Regex::new(pattern)
                .map_err(|error| Error::execution(format!("InvalidRegex: {error}")))?;
            Value::String(pattern.replace_all(input, &**replacement).into())
        }
        _ => return Err(Error::invalid_argument_type("string.regexReplace")),
    })
}
/// ADR-0175: a `substring()` / `left()` / `right()` start/length argument
/// (Neo4j's `asIntExact(allowFloats=true)`, CypherFunctions.java L1748):
/// an integer passes through; a float converts with Java `(long)`-cast
/// semantics (truncate toward zero, NaN → 0, saturate at the i64 limits);
/// the result must lie in [0, 2147483647] — a negative or beyond-int32
/// value (±Infinity included) raises NumberOutOfRange. A null, missing, or
/// non-numeric argument keeps the historical `default`.
fn string_index_arg(v: Option<&Value>, default: i64, what: &str) -> Result<i64> {
    let Some(n) = super::super::numeric::int_arg(v) else {
        return Ok(default);
    };
    if !(0..=i64::from(i32::MAX)).contains(&n) {
        return Err(Error::ty(format!("NumberOutOfRange: {what}")));
    }
    Ok(n)
}

pub(super) fn bi_substring(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    let s = match vals.first() {
        Some(Value::String(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => return Err(Error::ty("substring() needs a string")),
    };
    let start = string_index_arg(vals.get(1), 0, "substring() start")? as usize;
    // Without an integer length the substring runs to the end of the string.
    let len = string_index_arg(vals.get(2), i64::MAX, "substring() length")? as usize;
    Ok(Value::String(
        char_range(s, start..start.saturating_add(len)).into(),
    ))
}
pub(super) fn bi_left(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    let s = string_arg(&vals, 0)?;
    let n = string_index_arg(vals.get(1), 0, "left() length")? as usize;
    Ok(match s {
        Some(s) => Value::String(char_range(s, 0..n).into()),
        None => Value::Null,
    })
}
pub(super) fn bi_right(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    let s = string_arg(&vals, 0)?;
    let n = string_index_arg(vals.get(1), 0, "right() length")? as usize;
    Ok(match s {
        Some(s) => {
            Value::String(char_range(s, s.chars().count().saturating_sub(n)..usize::MAX).into())
        }
        None => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::super::coerce_scalar_to_string;
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn string_interp_concatenates_every_coerced_part() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let interp = |vals: Vec<Value>| bi_string_interp(&graph, "string.interp", vals);
        let s = |v: &str| Value::String(v.into());

        assert_eq!(
            interp(vec![
                s("n="),
                Value::Int(1),
                s(", s="),
                s("añ😀"),
                s(", f="),
                Value::Float(2.0),
                s(", "),
                Value::Bool(true),
            ])
            .unwrap(),
            s("n=1, s=añ😀, f=2.0, true")
        );
        assert_eq!(interp(vec![]).unwrap(), s(""));
        assert_eq!(interp(vec![s("a"), Value::Null]).unwrap(), Value::Null);
        assert!(interp(vec![s("a"), Value::List(vec![Value::Int(1)].into())]).is_err());
    }

    #[test]
    fn string_interp_renders_every_scalar_kind_in_order() {
        use cypherlite_core::{PointValue, Temporal, VectorCoordKind};
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let interp = |vals: Vec<Value>| bi_string_interp(&graph, "string.interp", vals);
        let s = |v: &str| Value::String(v.into());
        let uuid = Value::Uuid(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let date = Value::Temporal(Temporal::Date(
            chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
        ));
        let point = Value::Point(PointValue::cartesian_2d(1.5, -2.0));
        let vector = Value::Vector {
            coord: VectorCoordKind::Int64,
            values: vec![Value::Int(1), Value::Int(2)].into(),
        };

        // Each part renders as its toString() form, except a Vector, which
        // uses its display form.
        let parts = vec![
            Value::Int(-42),
            s("|"),
            Value::Float(0.5),
            s("|"),
            Value::Bool(false),
            s("|ñ|"),
            uuid.clone(),
            s("|"),
            date.clone(),
            s("|"),
            point.clone(),
            s("|"),
            vector.clone(),
        ];
        let expected = format!(
            "-42|0.5|false|ñ|{}|{}|{}|{}",
            coerce_scalar_to_string(&uuid).unwrap(),
            coerce_scalar_to_string(&date).unwrap(),
            coerce_scalar_to_string(&point).unwrap(),
            vector.to_display_string(),
        );
        assert_eq!(interp(parts).unwrap(), s(&expected));
        assert_eq!(
            interp(vec![uuid.clone()]).unwrap(),
            s("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(interp(vec![date]).unwrap(), s("2024-02-29"));

        // A lone non-STRING part renders on its own.
        assert_eq!(interp(vec![Value::Int(7)]).unwrap(), s("7"));
        assert_eq!(interp(vec![Value::Null]).unwrap(), Value::Null);
        // The first null or uncoercible part, in order, decides the result.
        let list = Value::List(vec![Value::Int(1)].into());
        assert_eq!(
            interp(vec![s("a"), Value::Null, list.clone()]).unwrap(),
            Value::Null
        );
        let err = interp(vec![s("a"), list.clone(), Value::Null]).unwrap_err();
        assert!(
            err.to_string().contains("cannot coerce value to STRING"),
            "{err}"
        );
        assert!(interp(vec![Value::Map(Default::default())]).is_err());
    }

    /// Five characters of 1, 2, 4, 1 and 2 bytes: positions count
    /// characters, so a byte-indexed cut would split a code point.
    const MB: &str = "añ😀bç";

    #[test]
    fn substring_counts_characters_and_clamps_at_the_boundaries() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let s = |v: &str| Value::String(v.into());
        let int = Value::Int;
        let sub = |vals: Vec<Value>| bi_substring(&graph, "substring", vals).unwrap();

        assert_eq!(sub(vec![s(MB), int(0)]), s(MB));
        assert_eq!(sub(vec![s(MB), int(0), int(2)]), s("añ"));
        assert_eq!(sub(vec![s(MB), int(1), int(2)]), s("ñ😀"));
        assert_eq!(sub(vec![s(MB), int(2)]), s("😀bç"));
        assert_eq!(sub(vec![s(MB), int(4), int(1)]), s("ç"));
        // A length beyond the end stops at the end.
        assert_eq!(sub(vec![s(MB), int(0), int(99)]), s(MB));
        assert_eq!(sub(vec![s(MB), int(3), int(i64::from(i32::MAX))]), s("bç"));
        assert_eq!(
            sub(vec![
                s(MB),
                int(i64::from(i32::MAX)),
                int(i64::from(i32::MAX))
            ]),
            s("")
        );
        // A zero length, or a start at or past the end, is empty.
        assert_eq!(sub(vec![s(MB), int(0), int(0)]), s(""));
        assert_eq!(sub(vec![s(MB), int(2), int(0)]), s(""));
        assert_eq!(sub(vec![s(MB), int(5)]), s(""));
        assert_eq!(sub(vec![s(MB), int(9), int(2)]), s(""));
        assert_eq!(sub(vec![s(""), int(0), int(1)]), s(""));
        // ADR-0175: a negative or beyond-int32 position is NumberOutOfRange;
        // a float truncates; null or non-numeric positions are ignored.
        let oor = |vals: Vec<Value>| {
            bi_substring(&graph, "substring", vals)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            oor(vec![s(MB), int(-3), int(2)]),
            Error::ty("NumberOutOfRange: substring() start").to_string()
        );
        assert_eq!(
            oor(vec![s(MB), int(1), int(-1)]),
            Error::ty("NumberOutOfRange: substring() length").to_string()
        );
        assert_eq!(
            oor(vec![s(MB), int(i64::MAX)]),
            Error::ty("NumberOutOfRange: substring() start").to_string()
        );
        assert_eq!(
            sub(vec![s(MB), Value::Float(1.0), Value::Float(1.0)]),
            s("ñ")
        );
        assert_eq!(sub(vec![s(MB), Value::Null, Value::Null]), s(MB));
        // Null input is null; any other non-string input is an error.
        assert_eq!(sub(vec![Value::Null, int(1)]), Value::Null);
        assert_eq!(sub(vec![]), Value::Null);
        assert_eq!(
            bi_substring(&graph, "substring", vec![int(1), int(0)])
                .unwrap_err()
                .to_string(),
            Error::ty("substring() needs a string").to_string()
        );
    }

    #[test]
    fn left_and_right_count_characters_and_clamp_at_the_boundaries() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let s = |v: &str| Value::String(v.into());
        let int = Value::Int;
        let left = |vals: Vec<Value>| bi_left(&graph, "left", vals);
        let right = |vals: Vec<Value>| bi_right(&graph, "right", vals);

        assert_eq!(left(vec![s(MB), int(3)]).unwrap(), s("añ😀"));
        assert_eq!(right(vec![s(MB), int(3)]).unwrap(), s("😀bç"));
        assert_eq!(left(vec![s(MB), int(1)]).unwrap(), s("a"));
        assert_eq!(right(vec![s(MB), int(1)]).unwrap(), s("ç"));
        // A length beyond the end is the whole string.
        assert_eq!(left(vec![s(MB), int(5)]).unwrap(), s(MB));
        assert_eq!(right(vec![s(MB), int(99)]).unwrap(), s(MB));
        assert_eq!(left(vec![s(MB), int(i64::from(i32::MAX))]).unwrap(), s(MB));
        assert_eq!(right(vec![s(MB), int(i64::from(i32::MAX))]).unwrap(), s(MB));
        // A zero or missing length is empty.
        for n in [vec![int(0)], vec![], vec![Value::Null]] {
            let mut vals = vec![s(MB)];
            vals.extend(n);
            assert_eq!(left(vals.clone()).unwrap(), s(""));
            assert_eq!(right(vals).unwrap(), s(""));
        }
        // ADR-0175: a negative or beyond-int32 length is NumberOutOfRange.
        for n in [int(-2), int(i64::MAX)] {
            assert_eq!(
                left(vec![s(MB), n.clone()]).unwrap_err().to_string(),
                Error::ty("NumberOutOfRange: left() length").to_string()
            );
            assert_eq!(
                right(vec![s(MB), n]).unwrap_err().to_string(),
                Error::ty("NumberOutOfRange: right() length").to_string()
            );
        }
        assert_eq!(left(vec![s(""), int(2)]).unwrap(), s(""));
        assert_eq!(right(vec![s(""), int(2)]).unwrap(), s(""));
        // Null input is null; any other non-string input is an error.
        assert_eq!(left(vec![Value::Null, int(2)]).unwrap(), Value::Null);
        assert_eq!(right(vec![Value::Null, int(2)]).unwrap(), Value::Null);
        let ty = Error::ty("expected string argument").to_string();
        assert_eq!(left(vec![int(1), int(2)]).unwrap_err().to_string(), ty);
        assert_eq!(right(vec![int(1), int(2)]).unwrap_err().to_string(), ty);
    }

    #[test]
    fn split_on_the_empty_delimiter_yields_each_character() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let s = |v: &str| Value::String(v.into());
        assert_eq!(
            bi_split(&graph, "split", vec![s("añ😀b"), s("")]).unwrap(),
            Value::List(vec![s("a"), s("ñ"), s("😀"), s("b")].into())
        );
        assert_eq!(
            bi_split(&graph, "split", vec![s(""), s("")]).unwrap(),
            Value::List(Vec::new().into())
        );
    }
}
