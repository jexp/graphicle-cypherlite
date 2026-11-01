//! Temporal function helpers (construction, component access).
//! Full-seconds rendering lives on the value:
//! `Temporal::full_seconds_string` (cypherlite-core, ADR-0448).

use cypherlite_core::{temporal, Error, Result, Value};

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
