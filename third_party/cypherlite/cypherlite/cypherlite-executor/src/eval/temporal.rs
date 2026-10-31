//! Temporal function helpers (construction, full-seconds rendering).

use cypherlite_core::{temporal, Error, Result, Value};

pub(crate) fn temporal_full_seconds(value: &temporal::Temporal) -> String {
    let mut rendered = value.iso_string();
    if matches!(
        value,
        temporal::Temporal::LocalTime(_)
            | temporal::Temporal::Time { .. }
            | temporal::Temporal::LocalDateTime(_)
            | temporal::Temporal::DateTime { .. }
    ) {
        let start = rendered.find('T').map_or(0, |i| i + 1);
        let boundary = start + 5;
        if rendered
            .as_bytes()
            .get(boundary)
            .is_none_or(|c| matches!(*c, b'Z' | b'+' | b'-' | b'['))
        {
            rendered.insert_str(boundary, ":00");
        }
    }
    rendered
}

pub(crate) fn temporal_construct(name: &str, arg: Option<&Value>) -> Result<Value> {
    if matches!(arg, Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let t = match name {
        "date" => temporal::construct_date(arg)?,
        "time" => temporal::construct_time(arg)?,
        "localtime" => temporal::construct_localtime(arg)?,
        "datetime" => temporal::construct_datetime(arg)?,
        "localdatetime" => temporal::construct_localdatetime(arg)?,
        "duration" => temporal::construct_duration(arg)?,
        _ => return Err(Error::ty("unknown temporal constructor")),
    };
    Ok(Value::Temporal(t))
}
