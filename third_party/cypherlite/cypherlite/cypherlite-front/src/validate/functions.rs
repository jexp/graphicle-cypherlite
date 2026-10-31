//! The canonical known-function registry: every function name the engine
//! accepts on lowered IR, advertised (`KNOWN_FUNCTIONS`, the `SHOW
//! FUNCTIONS` source) or unadvertised (`UNADVERTISED_FUNCTIONS`, e.g. the
//! lowering's internal renames). The planner binds against
//! [`is_known_function`], so a name `SHOW FUNCTIONS` advertises can no
//! longer be rejected by the binder (and vice versa). Unknown calls are
//! rejected by the planner on the lowered IR (`UnknownFunction`).

/// Known built-in function names.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    "id",
    "elementid",
    "db.namefromelementid",
    "normalize",
    "labels",
    "type",
    "keys",
    "properties",
    "size",
    "length",
    "head",
    "tail",
    "last",
    "reverse",
    "range",
    "coalesce",
    "tostring",
    "uuid",
    "uuid.mostsignificantbits",
    "uuid.leastsignificantbits",
    "randomuuid",
    "tointeger",
    "tofloat",
    "toboolean",
    "tolower",
    "toupper",
    "upper",
    "lower",
    "trim",
    "trim_leading",
    "trim_trailing",
    "ltrim",
    "rtrim",
    "btrim",
    "replace",
    "substring",
    "split",
    "left",
    "right",
    "abs",
    "ceil",
    "floor",
    "round",
    "sqrt",
    "sign",
    "rand",
    "exists",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "stdev",
    "stdevp",
    "percentilecont",
    "percentiledisc",
    "collect_list",
    "stdev_samp",
    "stdev_pop",
    "stddev_samp",
    "stddev_pop",
    "percentile_cont",
    "percentile_disc",
    "nodes",
    "relationships",
    "rels",
    "startnode",
    "endnode",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "log",
    "log10",
    "exp",
    "cot",
    "cosh",
    "coth",
    "sinh",
    "tanh",
    "isnan",
    "timestamp",
    "degrees",
    "radians",
    "typename",
    "date",
    "date.transaction",
    "date.statement",
    "date.realtime",
    "datetime",
    "datetime.transaction",
    "datetime.statement",
    "datetime.realtime",
    "localdatetime",
    "localdatetime.transaction",
    "localdatetime.statement",
    "localdatetime.realtime",
    "localtime",
    "localtime.transaction",
    "localtime.statement",
    "localtime.realtime",
    "time",
    "time.transaction",
    "time.statement",
    "time.realtime",
    "duration",
    "format",
    "point",
    "distance",
    "point.distance",
    "vector",
    "vector_dimension_count",
    "vector.similarity.cosine",
    // NOTE: entries must be lowercase -- `is_known_function` lowercases
    // the call name before `.contains()`, so a mixed-case entry can never
    // match (it would be advertised-but-rejected). `vector.cosinesimilarity`
    // is the documented alias of `vector.similarity.cosine`; it dispatches
    // via the shared arm in eval/function.rs.
    "vector.cosinesimilarity",
    "vector.similarity.euclidean",
    "vector.dimensions",
    "vector.normalize",
    "vector.add",
    "vector.subtract",
    "vector.manhattan",
    "vector.euclideandistance",
    "vector.dotproduct",
    "vector_distance",
    "vector_norm",
    "tofloatlist",
    "tointegerlist",
    "point.withinbbox",
    "point.withindistance",
    "datetime.fromepoch",
    "datetime.fromepochmillis",
    "date.truncate",
    "datetime.truncate",
    "localdatetime.truncate",
    "localtime.truncate",
    "time.truncate",
    "duration.between",
    "duration.inmonths",
    "duration.indays",
    "duration.inseconds",
    "pi",
    "e",
    "none",
    "any",
    "all",
    "single", // quantifiers parsed as functions sometimes
    "reduce",
    "shortestpath",
    "allshortestpaths",
    "tobooleanornull",
    "tofloatornull",
    "tointegerornull",
    "tostringornull",
    "char_length",
    "character_length",
    "nullif",
    "valuetype",
    "linenumber",
    "file",
    "linestring",
    "polygon",
    "spatial.contains",
    "spatial.within",
    "spatial.intersects",
    "spatial.overlaps",
    "spatial.crosses",
    "spatial.touches",
    "spatial.area",
    "spatial.length",
    "spatial.centroid",
    "spatial.towkt",
    "spatial.fromwkt",
    "spatial.togeojson",
    "spatial.fromgeojson",
    "string.join",
    "string.regexreplace",
    "string.indexof",
    // GQL function aliases (CIP-245)
    "ceiling",
    "ln",
    "local_time",
    "local_datetime",
    "zoned_time",
    "zoned_datetime",
    "duration_between",
    "path_length",
    // GQL collection functions (CIP-222)
    "coll.flatten",
    "coll.sort",
    // `apoc.coll.sort` shares the `coll.sort` implementation
    // (eval/function.rs) but exists only in the Cypher 5 dialect; Cypher 25
    // removed it. That restriction is enforced SOLELY by the typed dialect
    // gate (`dialect::validate_no_apoc_coll_sort`, which runs at lowering,
    // before the binder): the registry must know the name so `CYPHER 5`
    // statements bind and run it.
    "apoc.coll.sort",
    "coll.indexof",
    "coll.insert",
    "coll.remove",
    "coll.max",
    "coll.min",
    "coll.distinct",
    // GQL collection functions (CIP-274, CIP-286)
    "coll.zip",
    "coll.hasanyof",
    "coll.hasallof",
    "coll.hasnoneof",
    // GQL compliance (CIP-263, CIP-247)
    "cardinality",
    "property_exists",
];

/// Function names the binder accepts on lowered IR but which are *not* part
/// of the user-facing registry (`KNOWN_FUNCTIONS`, the `SHOW FUNCTIONS`
/// source): a user-written call with one of these spellings is still an
/// UnknownFunction error at parse time. Members:
///
/// - `tostring_full` -- the lowering's internal rename of
///   `toString(local_time(...))` & friends (`lower.rs`); never written by
///   users.
/// - `graph.byname` -- valid only as a `USE` graph selector; it is listed
///   by `SHOW FUNCTIONS` through the executor's catalog-only entries, not
///   through `KNOWN_FUNCTIONS`.
/// - `string.interp` -- the CIP-261 interpolated string literal's lowered
///   form (`s'...{expr}...'`); users write the literal, never the call.
pub const UNADVERTISED_FUNCTIONS: &[&str] = &["tostring_full", "graph.byname", "string.interp"];

/// True when `name` (any casing) is a function the engine accepts: every
/// advertised built-in (`KNOWN_FUNCTIONS`, which includes the aggregate
/// names and their CIP-128 aliases) plus the unadvertised lowered-internal
/// names (`UNADVERTISED_FUNCTIONS`). This is the single known-function
/// registry the planner binds against, so `SHOW FUNCTIONS` can no longer
/// advertise a name the binder rejects.
pub fn is_known_function(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    KNOWN_FUNCTIONS.contains(&lower.as_str()) || UNADVERTISED_FUNCTIONS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `is_known_function` lowercases the call name before `.contains()`,
    // so any KNOWN_FUNCTIONS entry that is not already lowercase can never
    // match a real call -- it would be an advertised-but-always-rejected
    // name (exactly the `vector.cosineSimilarity` regression that shipped a
    // capital `S`). This invariant test forbids that whole class of bug.
    #[test]
    fn every_known_function_entry_is_lowercase() {
        let offenders: Vec<&&str> = KNOWN_FUNCTIONS
            .iter()
            .filter(|name| **name != name.to_lowercase())
            .collect();
        assert!(
            offenders.is_empty(),
            "KNOWN_FUNCTIONS entries must be lowercase (the validator lowercases \
             the call name before lookup); unreachable entries: {offenders:?}"
        );
    }
}
