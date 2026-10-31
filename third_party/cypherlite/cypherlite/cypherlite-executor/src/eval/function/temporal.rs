//! Temporal and duration builtins: date/time/datetime construction,
//! `*.truncate`, `datetime.fromepoch[Millis]`, `duration.between`/`in*`, and
//! `format`.

use cypherlite_core::{temporal, Error, Result, Value};

use crate::overlay::ExecGraph;

use super::super::temporal::temporal_construct;

pub(super) fn bi_format(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        if vals.is_empty() || vals.len() > 2 {
            return Err(Error::semantic("format() requires one or two arguments"));
        }
        if vals.iter().any(Value::is_null) {
            Value::Null
        } else {
            let Value::Temporal(t) = &vals[0] else {
                return Err(Error::ty("format() requires a temporal value"));
            };
            match vals.get(1) {
                None => Value::String(t.iso_string().into()),
                Some(Value::String(pattern)) => {
                    Value::String(temporal::format_pattern(t, pattern)?.into())
                }
                _ => return Err(Error::ty("format() pattern must be a string")),
            }
        }
    })
}

pub(super) fn bi_date(_graph: &ExecGraph<'_>, n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let canon = match n {
            "local_time" => "localtime",
            "local_datetime" | "local_date_time" => "localdatetime",
            "zoned_time" => "time",
            "zoned_datetime" | "zoned_date_time" => "datetime",
            other => other,
        };
        if vals.len() == 2 {
            match (&vals[0], &vals[1]) {
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                (Value::String(value), Value::String(pattern)) => {
                    Value::Temporal(temporal::parse_pattern(canon, value, pattern)?)
                }
                (Value::String(_), _) => {
                    return Err(Error::invalid_argument_type("temporal pattern"))
                }
                _ => return Err(Error::invalid_argument_type("temporal value")),
            }
        } else {
            temporal_construct(canon, vals.first())?
        }
    })
}
pub(super) fn bi_datetime_fromepoch(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok({
        let secs = vals
            .first()
            .and_then(|v| v.as_int())
            .ok_or_else(|| Error::ty("datetime.fromepoch seconds"))?;
        let nanos = vals.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Value::Temporal(temporal::datetime_from_epoch(secs, nanos)?)
    })
}
pub(super) fn bi_datetime_fromepochmillis(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok({
        let ms = vals
            .first()
            .and_then(|v| v.as_int())
            .ok_or_else(|| Error::ty("datetime.fromepochmillis"))?;
        Value::Temporal(temporal::datetime_from_epoch_millis(ms)?)
    })
}
/// `timestamp()`: the statement-pinned instant as epoch milliseconds.
pub(super) fn bi_timestamp(_graph: &ExecGraph<'_>, _n: &str, _vals: Vec<Value>) -> Result<Value> {
    Ok(Value::Int(temporal::now_epoch_millis()))
}
pub(super) fn bi_date_truncate(_graph: &ExecGraph<'_>, n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let kind = n.split('.').next().unwrap();
        let unit = match vals.first() {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => return Ok(Value::Null),
            _ => return Err(Error::ty("truncate unit must be a string")),
        };
        let value = match vals.get(1) {
            Some(Value::Temporal(t)) => t,
            Some(Value::Null) | None => return Ok(Value::Null),
            _ => return Err(Error::ty("truncate value must be temporal")),
        };
        Value::Temporal(temporal::truncate(kind, &unit, value, vals.get(2))?)
    })
}
pub(super) fn bi_duration_indays(
    _graph: &ExecGraph<'_>,
    n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok({
        match (vals.first(), vals.get(1)) {
            (Some(Value::Null), _) | (_, Some(Value::Null)) | (None, _) | (_, None) => Value::Null,
            (Some(Value::Temporal(a)), Some(Value::Temporal(b))) => {
                let unit = match n {
                    "duration.inmonths" => temporal::BetweenUnit::Months,
                    "duration.indays" => temporal::BetweenUnit::Days,
                    "duration.inseconds" => temporal::BetweenUnit::Seconds,
                    _ => temporal::BetweenUnit::Full,
                };
                Value::Temporal(temporal::duration_between_unit(a, b, unit)?)
            }
            _ => return Err(Error::ty("duration.between needs temporal values")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn timestamp_reads_the_pinned_statement_clock() {
        let _clock = cypherlite_core::QueryClockGuard::new();
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let result = bi_timestamp(&graph, "timestamp", vec![]).unwrap();
        let Value::Int(ms) = result else {
            panic!("timestamp() must return an INTEGER, got {result:?}");
        };
        assert_eq!(ms, temporal::now_epoch_millis());
    }
}
