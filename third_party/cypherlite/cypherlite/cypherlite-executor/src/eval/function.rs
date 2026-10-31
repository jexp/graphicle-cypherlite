//! The scalar function dispatcher (`function()`), the BUILTINS
//! name-to-implementation table, and the helpers shared by the per-cluster
//! builtin modules under `function/`.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::Expr;

use crate::overlay::ExecGraph;

use super::shortest::{eval_shortest, ShortestWant};
use super::{eval, Row};

mod convert;
mod entity;
mod list;
mod math;
mod property;
mod scalar;
mod spatial;
mod string;
mod temporal;
mod vector;

/// The dispatch-canonical builtin name: lowercased, with the temporal
/// suffix spellings (`.realtime`/`.statement`/`.transaction`) rewritten to
/// their base. Both the dispatch (`function`) and the determinism
/// classification (`is_nondeterministic`) normalize through here, so the
/// spelling rules live once.
fn canonical_builtin_name(name: &str) -> String {
    let raw = name.to_ascii_lowercase();
    if let Some(base) = raw
        .strip_suffix(".realtime")
        .or_else(|| raw.strip_suffix(".statement"))
        .or_else(|| raw.strip_suffix(".transaction"))
    {
        if TEMPORAL_ACCESS_MODE_BASES.contains(&base) {
            return base.to_string();
        }
    }
    raw
}

pub(crate) fn function(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    name: &str,
    _distinct: bool,
    args: &[Expr],
    cypher_version: Option<u32>,
) -> Result<Value> {
    let n = canonical_builtin_name(name);
    if n == "length" {
        if let Some(Expr::ShortestPath {
            all: false, path, ..
        }) = args.first()
        {
            return eval_shortest(
                graph,
                row,
                params,
                false,
                path,
                ShortestWant::Length,
                cypher_version,
            );
        }
    }
    if matches!(n.as_str(), "isnan" | "elementid" | "db.namefromelementid") && args.len() != 1 {
        return Err(Error::syntax(
            "InvalidNumberOfArguments: function requires exactly one argument",
        ));
    }
    if let Some(result) =
        property::property_exists_form(graph, row, params, n.as_str(), args, cypher_version)
    {
        return result;
    }
    if let Some(result) =
        vector::vector_metric_form(graph, row, params, n.as_str(), args, cypher_version)
    {
        return result;
    }
    let mut vals = Vec::new();
    for a in args {
        vals.push(eval(graph, row, a, params, cypher_version)?);
    }
    let Some(builtin) = lookup_builtin(n.as_str()) else {
        if cypherlite_front::agg::is_aggregate_name(&n) {
            return Err(Error::semantic(format!("{n}() is an aggregating function")));
        }
        return Err(Error::semantic(format!("unknown function {n}()")));
    };
    (builtin.implementation)(graph, n.as_str(), vals)
}

/// Whether a builtin samples the clock or entropy on each call, so two
/// evaluations in one statement can return different values. This is the
/// one classification site: it mirrors the dispatch normalization above
/// (case-insensitive, `.realtime`/`.statement`/`.transaction` suffix
/// rewrite) and the `BUILTINS` vocabulary it describes, and callers must
/// not re-derive the list. Name-only and deliberately conservative: the
/// clock names appear here even though their argument-taking forms parse a
/// value deterministically — per-call clock reads are the common case and
/// the callers treat a false positive as "no fast path", never as an
/// error.
pub(crate) fn is_nondeterministic(name: &str) -> bool {
    lookup_builtin(&canonical_builtin_name(name)).is_some_and(|row| row.nondeterministic)
}

/// Name-to-implementation table for scalar builtins. The `.realtime` /
/// `.statement` / `.transaction` suffix rewrite happens once, before lookup.
type Builtin = fn(&ExecGraph<'_>, &str, Vec<Value>) -> Result<Value>;

/// One builtin row: implementation plus its per-call determinism
/// classification (LABS-198: the shared `CALL { }` fast path refuses any
/// body calling a nondeterministic builtin, since it would sample once
/// instead of per outer row). The flag lives on the row so a new builtin
/// forces the decision where the implementation is registered.
struct BuiltinRow {
    name: &'static str,
    implementation: Builtin,
    nondeterministic: bool,
}

/// The base names whose `.realtime`/`.statement`/`.transaction` suffix
/// spellings `canonical_builtin_name` rewrites to the base — the
/// clock-reading temporal builtins (their rows carry the flag above).
const TEMPORAL_ACCESS_MODE_BASES: &[&str] =
    &["date", "time", "localtime", "datetime", "localdatetime"];

/// Row constructor keeping the table a flat name/implementation/flag
/// listing.
const fn row(name: &'static str, implementation: Builtin, nondeterministic: bool) -> BuiltinRow {
    BuiltinRow {
        name,
        implementation,
        nondeterministic,
    }
}

const BUILTINS: &[BuiltinRow] = &[
    row("abs", math::bi_abs, false),
    row("acos", math::bi_acos, false),
    row("apoc.coll.sort", list::bi_coll_sort, false),
    row("asin", math::bi_asin, false),
    row("atan", math::bi_atan, false),
    row("atan2", math::bi_atan2, false),
    row("btrim", string::bi_btrim, false),
    row("cardinality", list::bi_cardinality, false),
    row("ceil", math::bi_ceil, false),
    row("ceiling", math::bi_ceil, false),
    row("char_length", string::bi_char_length, false),
    row("character_length", string::bi_char_length, false),
    row("coalesce", scalar::bi_coalesce, false),
    row("coll.distinct", list::bi_coll_distinct, false),
    row("coll.flatten", list::bi_coll_flatten, false),
    row("coll.hasallof", list::bi_coll_has_all_of, false),
    row("coll.hasanyof", list::bi_coll_has_any_of, false),
    row("coll.hasnoneof", list::bi_coll_has_none_of, false),
    row("coll.indexof", list::bi_coll_indexof, false),
    row("coll.insert", list::bi_coll_insert, false),
    row("coll.max", list::bi_coll_max, false),
    row("coll.min", list::bi_coll_min, false),
    row("coll.remove", list::bi_coll_remove, false),
    row("coll.sort", list::bi_coll_sort, false),
    row("coll.zip", list::bi_coll_zip, false),
    row("cos", math::bi_cos, false),
    row("cosh", math::bi_cosh, false),
    row("cot", math::bi_cot, false),
    row("coth", math::bi_coth, false),
    row("date", temporal::bi_date, true),
    row("date.truncate", temporal::bi_date_truncate, false),
    row("datetime", temporal::bi_date, true),
    row("datetime.fromepoch", temporal::bi_datetime_fromepoch, false),
    row(
        "datetime.fromepochmillis",
        temporal::bi_datetime_fromepochmillis,
        false,
    ),
    row("datetime.truncate", temporal::bi_date_truncate, false),
    row(
        "db.namefromelementid",
        entity::bi_db_namefromelementid,
        false,
    ),
    row("degrees", math::bi_degrees, false),
    row("distance", spatial::bi_distance, false),
    row("duration", temporal::bi_date, false),
    row("duration.between", temporal::bi_duration_indays, false),
    row("duration.indays", temporal::bi_duration_indays, false),
    row("duration.inmonths", temporal::bi_duration_indays, false),
    row("duration.inseconds", temporal::bi_duration_indays, false),
    row("e", math::bi_e, false),
    row("elementid", entity::bi_elementid, false),
    row("endnode", entity::bi_endnode, false),
    row("exists", scalar::bi_exists, false),
    row("exp", math::bi_exp, false),
    row("floor", math::bi_floor, false),
    row("format", temporal::bi_format, false),
    row("head", list::bi_head, false),
    row("id", entity::bi_id, false),
    row("isnan", math::bi_isnan, false),
    row("keys", entity::bi_keys, false),
    row("labels", entity::bi_labels, false),
    row("last", list::bi_last, false),
    row("left", string::bi_left, false),
    row("length", list::bi_length, false),
    row("ln", math::bi_ln, false),
    row("local_date_time", temporal::bi_date, true),
    row("local_datetime", temporal::bi_date, true),
    row("local_time", temporal::bi_date, true),
    row("localdatetime", temporal::bi_date, true),
    row("localdatetime.truncate", temporal::bi_date_truncate, false),
    row("localtime", temporal::bi_date, true),
    row("localtime.truncate", temporal::bi_date_truncate, false),
    row("log", math::bi_ln, false),
    row("log10", math::bi_log10, false),
    row("lower", string::bi_lower, false),
    row("ltrim", string::bi_ltrim, false),
    row("nodes", entity::bi_nodes, false),
    row("normalize", string::bi_normalize, false),
    row("nullif", scalar::bi_nullif, false),
    row("octet_length", string::bi_octet_length, false),
    row("pi", math::bi_pi, false),
    row("point", spatial::bi_point, false),
    row("point.distance", spatial::bi_distance, false),
    row("point.withinbbox", spatial::bi_point_withinbbox, false),
    row(
        "point.withindistance",
        spatial::bi_point_withindistance,
        false,
    ),
    row("properties", entity::bi_properties, false),
    row("radians", math::bi_radians, false),
    row("rand", math::bi_rand, true),
    row("randomuuid", entity::bi_randomuuid, true),
    row("range", list::bi_range, false),
    row("relationships", entity::bi_relationships, false),
    row("replace", string::bi_replace, false),
    row("reverse", list::bi_reverse, false),
    row("right", string::bi_right, false),
    row("round", math::bi_round, false),
    row("rtrim", string::bi_rtrim, false),
    row("sign", math::bi_sign, false),
    row("sin", math::bi_sin, false),
    row("sinh", math::bi_sinh, false),
    row("size", list::bi_size, false),
    row("split", string::bi_split, false),
    row("sqrt", math::bi_sqrt, false),
    row("startnode", entity::bi_startnode, false),
    row("string.indexof", string::bi_string_indexof, false),
    row("string.interp", string::bi_string_interp, false),
    row("string.join", string::bi_string_join, false),
    row("string.regexreplace", string::bi_string_regexreplace, false),
    row("substring", string::bi_substring, false),
    row("tail", list::bi_tail, false),
    row("tan", math::bi_tan, false),
    row("tanh", math::bi_tanh, false),
    row("time", temporal::bi_date, true),
    row("time.truncate", temporal::bi_date_truncate, false),
    row("timestamp", temporal::bi_timestamp, true),
    row("tobool", convert::bi_tobool, false),
    row("toboolean", convert::bi_tobool, false),
    row("tobooleanornull", convert::bi_tobooleanornull, false),
    row("tofloat", convert::bi_tofloat, false),
    row("tofloatlist", convert::bi_tofloatlist, false),
    row("tofloatornull", convert::bi_tofloatornull, false),
    row("toint", convert::bi_toint, false),
    row("tointeger", convert::bi_toint, false),
    row("tointegerlist", convert::bi_tointegerlist, false),
    row("tointegerornull", convert::bi_tointegerornull, false),
    row("tolower", string::bi_lower, false),
    row("tostring", convert::bi_tostring, false),
    row("tostring_full", convert::bi_tostring_full, false),
    row("tostringornull", convert::bi_tostringornull, false),
    row("toupper", string::bi_upper, false),
    row("trim", string::bi_trim, false),
    row("trim_leading", string::bi_trim_leading, false),
    row("trim_trailing", string::bi_trim_trailing, false),
    row("type", entity::bi_type, false),
    row("typename", scalar::bi_typename, false),
    row("upper", string::bi_upper, false),
    row("uuid", entity::bi_uuid, true),
    row(
        "uuid.leastsignificantbits",
        entity::bi_uuid_leastsignificantbits,
        false,
    ),
    row(
        "uuid.mostsignificantbits",
        entity::bi_uuid_mostsignificantbits,
        false,
    ),
    row("valuetype", scalar::bi_typename, false),
    row("vector", vector::bi_vector, false),
    row("vector.add", vector::bi_vector_add, false),
    row(
        "vector.cosinesimilarity",
        vector::bi_vector_cosinesimilarity,
        false,
    ),
    row("vector.dimensions", vector::bi_vector_dimensions, false),
    row("vector.dotproduct", vector::bi_vector_dotproduct, false),
    row(
        "vector.euclideandistance",
        vector::bi_vector_euclideandistance,
        false,
    ),
    row("vector.manhattan", vector::bi_vector_manhattan, false),
    row("vector.normalize", vector::bi_vector_normalize, false),
    row(
        "vector.similarity.cosine",
        vector::bi_vector_cosinesimilarity,
        false,
    ),
    row(
        "vector.similarity.euclidean",
        vector::bi_vector_similarity_euclidean,
        false,
    ),
    row("vector.subtract", vector::bi_vector_subtract, false),
    row(
        "vector_dimension_count",
        vector::bi_vector_dimension_count,
        false,
    ),
    row("zoned_date_time", temporal::bi_date, true),
    row("zoned_datetime", temporal::bi_date, true),
    row("zoned_time", temporal::bi_date, true),
];

fn lookup_builtin(name: &str) -> Option<&'static BuiltinRow> {
    BUILTINS
        .binary_search_by_key(&name, |row| row.name)
        .ok()
        .map(|i| &BUILTINS[i])
}

/// The scalar half of the `toString` coercion, shared by CIP-271
/// `toString()` and CIP-261 `string.interp`: every scalar goes through here.
/// Appends `v`'s display form to `out` and returns `true`, or returns
/// `false` (leaving `out` untouched) for the container/graph values each
/// caller handles on its own (a list/map/node/relationship/path, or a
/// `Vector`, which the two callers render differently). Temporals render as
/// their bare ISO string unless the caller overrides them.
fn write_scalar(out: &mut String, v: &Value) -> bool {
    use std::fmt::Write as _;
    match v {
        Value::String(s) => out.push_str(s),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(_) => out.push_str(&v.to_cypher_literal()),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Temporal(t) => out.push_str(&t.iso_string()),
        Value::Point(_) => out.push_str(&v.to_display_string()),
        // CIP-246: bare canonical string (verified on 2026.09.0).
        Value::Uuid(u) => out.push_str(&cypherlite_core::uuid_hyphenated(*u)),
        _ => return false,
    }
    true
}

/// [`write_scalar`] as a value: a STRING passes through as the same shared
/// payload (no copy); any other coercible scalar renders into a new one.
fn coerce_scalar_to_string(v: &Value) -> Option<Arc<str>> {
    if let Value::String(s) = v {
        return Some(Arc::clone(s));
    }
    let mut rendered = String::new();
    write_scalar(&mut rendered, v).then(|| rendered.into())
}

#[cfg(test)]
mod tests {
    use super::convert::{bi_tostring, bi_tostring_full, bi_tostringornull};
    use super::string::bi_string_interp;
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn builtins_table_is_strictly_sorted() {
        assert!(
            BUILTINS.windows(2).all(|w| w[0].name < w[1].name),
            "BUILTINS must stay strictly sorted for binary_search_by_key"
        );
    }

    #[test]
    fn to_string_and_interp_of_a_string_share_its_payload() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let payload: Arc<str> = "añ😀".into();
        let input = Value::String(Arc::clone(&payload));
        let shared = |out: Value| matches!(out, Value::String(s) if Arc::ptr_eq(&s, &payload));

        assert!(Arc::ptr_eq(
            &coerce_scalar_to_string(&input).unwrap(),
            &payload
        ));
        assert!(shared(
            bi_tostring(&graph, "toString", vec![input.clone()]).unwrap()
        ));
        assert!(shared(
            bi_tostringornull(&graph, "toStringOrNull", vec![input.clone()]).unwrap()
        ));
        assert!(shared(
            bi_tostring_full(&graph, "tostring_full", vec![input.clone()]).unwrap()
        ));
        assert!(shared(
            bi_string_interp(&graph, "string.interp", vec![input.clone()]).unwrap()
        ));
    }
}
