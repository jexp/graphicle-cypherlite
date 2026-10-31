//! The WAL's on-disk JSON codec for redo property values (ADR-0078).
//!
//! The tagged `{"t":..., "v":...}` encoding itself is canonical on
//! [`Value`] in `cypherlite-core` (`Value: Serialize`/`Deserialize`), shared
//! with the Parquet checkpoint codec. This module is the redo-side glue: the
//! `props_json` / `props_from_json` helpers for the one cold path that still
//! carries a raw `serde_json::Value` (`CatalogIndexDetail.options`). The
//! Parquet checkpoint loader decodes its properties cells through
//! `props_from_json` too, so both refuse a non-object (ADR-0183).
//!
//! A future binary WAL format replaces this module; `redo` itself is
//! format-agnostic.

use std::collections::BTreeMap;

use cypherlite_core::Value;

/// Tagged-JSON object form of a property map. Retained for the cold
/// `CatalogIndexDetail.options` field (a raw `serde_json::Value`); the hot
/// create/set paths serialize typed values directly via `Value: Serialize`.
pub fn props_json(map: &BTreeMap<String, Value>) -> serde_json::Value {
    let obj: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), v.to_json_tagged()))
        .collect();
    serde_json::Value::Object(obj)
}

/// Decode a tagged-JSON object into a typed property map. Strict: a non-object
/// payload is an error, never a silently dropped value.
pub fn props_from_json(v: &serde_json::Value) -> cypherlite_core::Result<BTreeMap<String, Value>> {
    let serde_json::Value::Object(map) = v else {
        return Err(cypherlite_core::Error::storage(
            "property JSON must be an object",
        ));
    };
    let mut out = BTreeMap::new();
    for (k, val) in map {
        out.insert(k.clone(), Value::from_json_tagged(val)?);
    }
    Ok(out)
}

/// Tagged-JSON form of a single value. Production serialization uses
/// `Value: Serialize` directly; this remains for tests and for the
/// legacy-shape DTOs in the byte-compat golden test.
#[cfg(test)]
pub fn value_json(v: &Value) -> serde_json::Value {
    v.to_json_tagged()
}
