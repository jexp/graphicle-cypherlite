//! Wire adapters and error metadata shared by the language bindings (the C ABI
//! clients, Python, and the browser surface). [`binding_error_metadata`] is the single
//! source of truth for binding error classification — including OpenCypher
//! error names nested inside an outer message — so bindings must not re-scan
//! error strings themselves.

use std::collections::BTreeMap;

use indexmap::IndexMap;

use cypherlite_model::serde_value::try_json_to_param_value;
use cypherlite_model::wire;
use cypherlite_storage::{cypher_to_value, value_to_cypher};

use crate::{ArithmeticFailure, Connection, Error, QueryResult, QueryStats, Value};

/// Convert positional planner rows to the named CypherLite value rows used by
/// language bindings and Arrow conversion.
pub fn query_result_to_cypher_rows(
    result: &QueryResult,
) -> Vec<IndexMap<String, cypherlite_model::CypherValue>> {
    result
        .rows
        .iter()
        .map(|row| {
            result
                .header
                .columns
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    // Engine rows are rectangular by contract (the same
                    // invariant `stamp_column_types` enforces); a short row
                    // is a contract break, not a null.
                    let value = row
                        .get(i)
                        .cloned()
                        .expect("engine rows are rectangular: one value per column");
                    (column.clone(), value_to_cypher(&value))
                })
                .collect()
        })
        .collect()
}

/// Stable CypherLite wire stats: the one rendering of a statement's counters
/// for every surface (HTTP, FFI, Python, MCP, WASM and Bolt, ADR-0309), with
/// Neo4j's update-counter semantics: labels count per node,
/// `properties_set` counts every property write (a `REMOVE n.p` included),
/// and the schema counters are the engine's index and constraint DDL counts.
pub fn query_stats_to_wire(stats: &QueryStats) -> cypherlite_model::wire::WireStatsSnapshot {
    // Every counter is invariantly non-negative — the executor only
    // accumulates counts (an `IF NOT EXISTS` no-op yields 0, never a
    // negative). The i64 → u64 boundary states that invariant instead of
    // clamping it, so a counter that ever went negative fails loudly here
    // rather than reporting a silent 0.
    let count = |n: i64| u64::try_from(n).expect("update counters are non-negative");
    cypherlite_model::wire::WireStatsSnapshot {
        nodes_created: count(stats.nodes_created),
        nodes_deleted: count(stats.nodes_deleted),
        relationships_created: count(stats.relationships_created),
        relationships_deleted: count(stats.relationships_deleted),
        properties_set: count(stats.properties_written),
        labels_added: count(stats.node_labels_added),
        labels_removed: count(stats.node_labels_removed),
        indexes_added: count(stats.indexes_added),
        indexes_removed: count(stats.indexes_removed),
        constraints_added: count(stats.constraints_added),
        constraints_removed: count(stats.constraints_removed),
    }
}

/// Decode a JSON object of query parameters, as the bindings receive them.
///
/// Every nested JSON object, including one containing `__type`, is an
/// ordinary parameter map (`try_json_to_param_value`); typed values are
/// constructed in Cypher, e.g. `point($coords)` or `date($iso)`. Invalid
/// JSON, a non-object, and unrepresentable numbers are errors that name the
/// parameter.
///
/// ```
/// use cypherlite::{params_from_json, Value};
///
/// let params = params_from_json(r#"{"n": 1.0, "p": {"__type": "point"}}"#).unwrap();
/// assert_eq!(params["n"], Value::Float(1.0));
/// assert!(matches!(params["p"], Value::Map(_)));
/// assert!(params_from_json("[1]").unwrap_err().contains("JSON object"));
/// ```
pub fn params_from_json(text: &str) -> Result<BTreeMap<String, Value>, String> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => params_from_value(&value),
        Err(e) => Err(format!("invalid params JSON: {e}")),
    }
}

/// Decode query parameters from an already-parsed JSON value, as surfaces
/// that hold a parsed request body (the HTTP Query API, the MCP server)
/// receive them. The typed sibling of [`params_from_json`], sharing its
/// conversion: the value must be an object; every nested object, including
/// one containing `__type`, is an ordinary parameter map
/// (`try_json_to_param_value`); typed values are constructed in Cypher,
/// e.g. `point($coords)` or `date($iso)`. A non-object and unrepresentable
/// numbers are errors that name the parameter.
///
/// ```
/// use cypherlite::{params_from_value, Value};
///
/// let params = params_from_value(&serde_json::json!({"n": 1.0, "p": {"__type": "point"}})).unwrap();
/// assert_eq!(params["n"], Value::Float(1.0));
/// assert!(matches!(params["p"], Value::Map(_)));
/// assert!(params_from_value(&serde_json::json!([1])).unwrap_err().contains("JSON object"));
/// ```
pub fn params_from_value(value: &serde_json::Value) -> Result<BTreeMap<String, Value>, String> {
    match value {
        serde_json::Value::Object(map) => params_from_map(map),
        _ => Err("params must be a JSON object".to_string()),
    }
}

/// The map-taking fold `params_from_value` wraps: callers that already
/// hold the parameters as a JSON map skip the `Value` re-wrap (and its
/// clone) — the HTTP plain fold and the MCP tools (ADR-0425).
///
/// ```
/// use cypherlite::{params_from_map, Value};
///
/// let map = serde_json::json!({"n": 1.0, "p": {"__type": "point"}});
/// let params = params_from_map(map.as_object().unwrap()).unwrap();
/// assert_eq!(params["n"], Value::Float(1.0));
/// assert!(matches!(params["p"], Value::Map(_)));
/// ```
pub fn params_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<BTreeMap<String, Value>, String> {
    map.iter()
        .map(|(k, v)| {
            let value = try_json_to_param_value(v).map_err(|e| format!("parameter {k}: {e}"))?;
            Ok((k.clone(), cypher_to_value(&value)))
        })
        .collect()
}

/// Run one statement and return its wire envelope: its result, or an error
/// envelope for an engine error or parameters that do not decode.
/// `params_json` is decoded by [`params_from_json`]; `None` or blank means
/// none. The WASM `execute` is this function.
///
/// ```
/// use cypherlite::{execute_wire, Database};
///
/// let db = Database::open(":memory:")?;
/// let mut conn = db.connect()?;
/// let envelope = execute_wire(&mut conn, "RETURN $n AS n", Some(r#"{"n": 1.0}"#));
/// assert_eq!(envelope["rows"][0]["n"], 1.0);
/// assert!(execute_wire(&mut conn, "RETURN 1", Some("[1]"))["error"].is_string());
/// # Ok::<(), cypherlite::Error>(())
/// ```
pub fn execute_wire(
    conn: &mut Connection,
    query: &str,
    params_json: Option<&str>,
) -> serde_json::Value {
    match optional_params(params_json) {
        Ok(params) => statement_envelope(conn.query_with_params(query, params)),
        Err(envelope) => envelope,
    }
}

/// Parameters from optional JSON text (`None` or blank means none), or the
/// error envelope for text that does not decode.
fn optional_params(
    params_json: Option<&str>,
) -> Result<BTreeMap<String, Value>, serde_json::Value> {
    match params_json.filter(|text| !text.trim().is_empty()) {
        None => Ok(BTreeMap::new()),
        Some(text) => params_from_json(text).map_err(|message| wire::error_envelope(&message)),
    }
}

/// A statement's wire envelope: its result, or an error envelope.
fn statement_envelope(result: Result<QueryResult, Error>) -> serde_json::Value {
    match result {
        Ok(result) => query_result_to_wire(&result),
        Err(e) => wire::error_envelope(&e.to_string()),
    }
}

/// Run a script of one or more statements and return one wire envelope per
/// statement run, in order.
///
/// Statements come from `cypherlite_splitter::split_executable_statements`,
/// so comment-only pieces are skipped. Each runs with the same parameters
/// (`params_json`, decoded by [`params_from_json`]; `None` or blank means
/// none) and commits on its own. The run stops after the first envelope with
/// an error, so the statements before it stay applied. A blank or
/// comment-only script returns no envelopes; parameters that do not decode
/// return one error envelope and run nothing.
///
/// ```
/// use cypherlite::{run_script_wire, Database};
///
/// let db = Database::open(":memory:")?;
/// let mut conn = db.connect()?;
/// let envelopes = run_script_wire(
///     &mut conn,
///     "CREATE (:City {name: $name}); // a note\nMATCH (c:City) RETURN c.name AS name",
///     Some(r#"{"name": "Oslo"}"#),
/// );
/// assert_eq!(envelopes.len(), 2);
/// assert_eq!(envelopes[1]["rows"][0]["name"], "Oslo");
/// # Ok::<(), cypherlite::Error>(())
/// ```
pub fn run_script_wire(
    conn: &mut Connection,
    script: &str,
    params_json: Option<&str>,
) -> Vec<serde_json::Value> {
    let params = match optional_params(params_json) {
        Ok(params) => params,
        Err(envelope) => return vec![envelope],
    };
    let mut envelopes = Vec::new();
    for statement in cypherlite_splitter::split_executable_statements(script) {
        let result = conn.query_with_params(&statement, params.clone());
        let failed = result.is_err();
        envelopes.push(statement_envelope(result));
        if failed {
            break;
        }
    }
    envelopes
}

/// Canonical JSON envelope shared by the C ABI language bindings.
///
/// The envelope carries the ADR-0224 `columnTypes` beside `columns`, fed
/// from the engine-stamped [`QueryResult::header`]`.
pub fn query_result_to_wire(result: &QueryResult) -> serde_json::Value {
    let rows = query_result_to_cypher_rows(result);
    let wire_rows = cypherlite_model::wire::rows_for_result(&result.header.columns, &rows);
    let stats = cypherlite_model::wire::stats_to_wire_json(&query_stats_to_wire(&result.stats));
    let elapsed_ms = result.elapsed_ms();
    // The structured plan document (LABS-418, ADR-0381) rides the envelope
    // as `plan` — null on a plain run — so a wire consumer (the WASM surface,
    // any C-ABI envelope reader) gets the plan beside the metadata. The
    // canonical builder owns the key.
    cypherlite_model::wire::response_envelope(
        &result.header.columns,
        &result.header.column_types,
        wire_rows,
        stats,
        result.notifications(),
        elapsed_ms,
        plan_wire_json(result),
    )
}

/// The statement's structured plan document (`cypherlite.plan/v1`,
/// LABS-418/ADR-0381) as a JSON value, or `None` when the statement carried
/// no plan. The one projection the surfaces hand out: the model's
/// [`cypherlite_model::wire::plan_document`] over the engine's plan root,
/// with the result's remedy-bearing notifications joined per operator.
pub fn plan_wire_json(result: &QueryResult) -> Option<serde_json::Value> {
    let root = result.plan_root()?;
    Some(cypherlite_model::wire::plan_document(
        &value_to_cypher(&root.to_value()),
        root.statement_echo_opt(),
        result.notifications(),
    ))
}

/// Stable error metadata for Python's DB-API exception adapter.
///
/// The typed variants classify in the match below — the class travels as
/// data, never as rendered text to scan back. The one surviving message
/// scan (`stringly_error_metadata`) recognizes the still-stringly-typed
/// constraint classes (requires-properties, property-type, label-presence),
/// which the executor nests as `Name:` tokens inside outer messages; it
/// scans the stringly payload only, so a string that merely resembles a
/// typed class's token no longer adopts that class.
pub fn binding_error_metadata(error: &Error) -> (&'static str, &'static str) {
    match error {
        Error::UniquenessViolation { .. } => (
            "ConstraintVerificationFailed",
            "ConstraintVerificationFailed",
        ),
        Error::Type(_) | Error::InvalidArgumentType(_) | Error::TypeError(_) => {
            ("TypeError", "TypeError")
        }
        Error::ParameterMissing(_) => ("ParameterMissing", "ParameterMissing"),
        // The ADR-0307 typed variants keep the classes their legacy
        // renderings had: division/modulo by zero were `Type`, rounding was
        // a nameless `Execution` token, an unknown database the
        // `DatabaseNotFound:` CE token, cancellation a bare `Execution`.
        Error::Arithmetic(ArithmeticFailure::DivisionByZero | ArithmeticFailure::ModuloByZero) => {
            ("TypeError", "TypeError")
        }
        Error::Arithmetic(ArithmeticFailure::RoundingNecessary) | Error::Cancelled => {
            ("SemanticError", "SemanticError")
        }
        Error::DatabaseNotFound { .. } => ("DatabaseNotFound", "DatabaseNotFound"),
        Error::Conflict => ("WriteConflict", "SemanticError"),
        Error::Capacity { .. } => ("MemoryLimitExceeded", "ArgumentError"),
        Error::ResourceLimit { .. } => ("ResourceLimitExceeded", "ClientError"),
        Error::Storage(_) | Error::Io(_) | Error::Locked(_) => ("StorageError", "SemanticError"),
        Error::Internal(_) => ("InternalError", "InternalError"),
        // The typed procedure-not-found (ADR-0138) keeps the class its
        // legacy `Syntax("ProcedureNotFound: <name>")` rendering classified
        // as below.
        Error::ProcedureNotFound { .. } => ("SyntaxError", "SyntaxError"),
        Error::Syntax(s) => stringly_error_metadata(s, ("SyntaxError", "SyntaxError")),
        Error::Semantic(s) | Error::Execution(s) => {
            stringly_error_metadata(s, ("SemanticError", "SemanticError"))
        }
    }
}

/// Classify a still-stringly `Syntax`/`Semantic`/`Execution` payload: the
/// legacy constraint-class tokens first (a uniqueness violation never
/// reaches here — the typed [`Error::UniquenessViolation`] variant already
/// classified it), then the nested `Name:` tokens.
fn stringly_error_metadata(
    detail: &str,
    fallback: (&'static str, &'static str),
) -> (&'static str, &'static str) {
    if detail.contains("ConstraintValidationFailed:")
        || detail.contains("ConstraintVerificationFailed:")
    {
        return (
            "ConstraintVerificationFailed",
            "ConstraintVerificationFailed",
        );
    }
    let named = detail.split_once(':').map(|(name, _)| name.trim());
    match named {
        Some("ArgumentError") => ("ArgumentError", "ArgumentError"),
        Some("EntityNotFound") => ("EntityNotFound", "EntityNotFound"),
        Some("ProcedureError") => ("ProcedureError", "ProcedureError"),
        Some("SyntaxError") => ("SyntaxError", "SyntaxError"),
        Some("SemanticError") => ("SemanticError", "SemanticError"),
        // Fixed-namespace errors (ADR-0116 §9, ADR-0117 §6): the CE-text
        // formatters in `cypherlite_core::ce_messages` embed the kind name as
        // a `"<Kind>:"` token, and it survives to the bindings.
        Some("UnsupportedAdministrationCommand") => (
            "UnsupportedAdministrationCommand",
            "UnsupportedAdministrationCommand",
        ),
        Some("WriteOnReadOnlyAccessDatabase") => (
            "WriteOnReadOnlyAccessDatabase",
            "WriteOnReadOnlyAccessDatabase",
        ),
        Some("DatabaseNotFound") => ("DatabaseNotFound", "DatabaseNotFound"),
        Some("TransactionError") => ("TransactionError", "TransactionError"),
        Some(_) | None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn binding_wire_adapter_preserves_rows_and_stats_shape() {
        let db = Database::open(":memory:").unwrap();
        let mut c = db.connect().unwrap();
        let result = c.query("CREATE (:N {v: 1}) RETURN 7 AS n").unwrap();
        let wire = query_result_to_wire(&result);
        assert_eq!(wire["columns"], serde_json::json!(["n"]));
        assert_eq!(wire["rows"][0]["n"], 7);
        assert_eq!(wire["stats"]["nodes_created"], 1);
        assert_eq!(wire["stats"]["indexes_added"], 0);
        assert_eq!(wire["stats"]["constraints_removed"], 0);
    }

    /// The counters are invariantly non-negative, so the i64 → u64 boundary
    /// states that instead of clamping: a counter that ever went negative is
    /// a contract break and fails loudly here rather than reporting a
    /// silent 0.
    #[test]
    #[should_panic(expected = "update counters are non-negative")]
    fn wire_stats_reject_a_negative_counter() {
        let stats = QueryStats {
            nodes_created: -1,
            ..QueryStats::default()
        };
        query_stats_to_wire(&stats);
    }

    /// ADR-0309: the wire snapshot reports the engine's schema DDL counters
    /// (an `IF [NOT] EXISTS` no-op counts 0, a constraint's backing index is
    /// not an index) and Neo4j's update counters: labels per node, one
    /// property write per write (a removal or an overwrite counts once), and
    /// nothing for what a delete takes with it.
    #[test]
    fn wire_stats_are_neo4j_update_counters_and_schema_ddl() {
        let db = Database::open(":memory:").unwrap();
        let mut c = db.connect().unwrap();
        let wire = |c: &mut crate::Connection, q: &str| {
            query_stats_to_wire(&c.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).stats)
        };
        assert_eq!(
            wire(&mut c, "CREATE INDEX i FOR (n:N) ON (n.p)").indexes_added,
            1
        );
        assert_eq!(
            wire(&mut c, "CREATE INDEX i IF NOT EXISTS FOR (n:N) ON (n.p)").indexes_added,
            0
        );
        assert_eq!(wire(&mut c, "DROP INDEX i").indexes_removed, 1);
        assert_eq!(wire(&mut c, "DROP INDEX i IF EXISTS").indexes_removed, 0);
        let unique = wire(
            &mut c,
            "CREATE CONSTRAINT u FOR (n:N) REQUIRE n.k IS UNIQUE",
        );
        assert_eq!((unique.constraints_added, unique.indexes_added), (1, 0));
        let dropped = wire(&mut c, "DROP CONSTRAINT u");
        assert_eq!(
            (dropped.constraints_removed, dropped.indexes_removed),
            (1, 0)
        );
        let create = wire(&mut c, "CREATE (:A {a: 1, b: 2})-[:R {w: 1}]->(:A)");
        assert_eq!((create.labels_added, create.properties_set), (2, 3));
        let set = wire(
            &mut c,
            "MATCH (n:A) WHERE n.a = 1 SET n:X, n.a = 2, n.c = 3 REMOVE n.b",
        );
        assert_eq!((set.labels_added, set.properties_set), (1, 3));
        assert_eq!(wire(&mut c, "MATCH (n:X) REMOVE n:X").labels_removed, 1);
        let delete = wire(&mut c, "MATCH (n:A) DETACH DELETE n");
        assert_eq!((delete.nodes_deleted, delete.relationships_deleted), (2, 1));
        assert_eq!((delete.labels_removed, delete.properties_set), (0, 0));
    }

    #[test]
    fn binding_error_metadata_maps_dbapi_classes() {
        assert_eq!(
            binding_error_metadata(&Error::syntax("bad query")),
            ("SyntaxError", "SyntaxError")
        );
        assert_eq!(
            binding_error_metadata(&Error::parameter_missing("x")),
            ("ParameterMissing", "ParameterMissing")
        );
        assert_eq!(
            binding_error_metadata(&Error::execution("ConstraintVerificationFailed: duplicate")),
            (
                "ConstraintVerificationFailed",
                "ConstraintVerificationFailed"
            )
        );
    }

    /// ADR-0307: each new typed variant classifies exactly as the stringly
    /// error it replaced did.
    #[test]
    fn binding_error_metadata_keeps_the_legacy_class_of_adr_0307_variants() {
        let cases = [
            (
                Error::arithmetic(ArithmeticFailure::DivisionByZero),
                Error::ty("/ by zero"),
            ),
            (
                Error::arithmetic(ArithmeticFailure::ModuloByZero),
                Error::ty("% by zero"),
            ),
            (
                Error::arithmetic(ArithmeticFailure::RoundingNecessary),
                Error::execution("ArithmeticError: rounding necessary"),
            ),
            (
                Error::database_not_found("nosuchdb"),
                Error::semantic("DatabaseNotFound: Graph not found: nosuchdb"),
            ),
            (Error::cancelled(), Error::execution("query cancelled")),
        ];
        for (typed, legacy) in cases {
            assert_eq!(
                binding_error_metadata(&typed),
                binding_error_metadata(&legacy),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn binding_error_metadata_prefers_nested_specific_names() {
        // The executor nests typed details inside outer messages; the nested
        // `Name:` token wins over the outer variant default for the
        // still-stringly constraint classes.
        assert_eq!(
            binding_error_metadata(&Error::semantic(
                "outer context: ConstraintVerificationFailed: duplicate key"
            )),
            (
                "ConstraintVerificationFailed",
                "ConstraintVerificationFailed"
            )
        );
        assert_eq!(
            binding_error_metadata(&Error::semantic("outer: ConstraintValidationFailed: bad")),
            (
                "ConstraintVerificationFailed",
                "ConstraintVerificationFailed"
            )
        );
        // The typed type-error/parameter classes classify from the variant,
        // wherever the detail text goes.
        assert_eq!(
            binding_error_metadata(&Error::invalid_argument_type("q")),
            ("TypeError", "TypeError")
        );
        assert_eq!(
            binding_error_metadata(&Error::type_error("q")),
            ("TypeError", "TypeError")
        );
        assert_eq!(
            binding_error_metadata(&Error::parameter_missing("p")),
            ("ParameterMissing", "ParameterMissing")
        );
        // The Type variant itself classifies as TypeError.
        assert_eq!(
            binding_error_metadata(&Error::ty("EntityNotFound: x")),
            ("TypeError", "TypeError")
        );
    }

    /// A stringly error whose text merely resembles a typed class's
    /// `Name:` token keeps its own variant's generic class: classification
    /// comes from the type, never from rendered text.
    #[test]
    fn binding_error_metadata_ignores_lookalike_tokens_in_strings() {
        assert_eq!(
            binding_error_metadata(&Error::execution("while running: InvalidArgumentType: q")),
            ("SemanticError", "SemanticError")
        );
        assert_eq!(
            binding_error_metadata(&Error::execution("nested ParameterMissing: p")),
            ("SemanticError", "SemanticError")
        );
        assert_eq!(
            binding_error_metadata(&Error::syntax("TypeError: not the typed class")),
            ("SyntaxError", "SyntaxError")
        );
        // Engine-internal failures (ADR-0259 worker spawn/panic) classify as
        // InternalError and never reach the name scan.
        assert_eq!(
            binding_error_metadata(&Error::internal("query worker thread panicked: boom")),
            ("InternalError", "InternalError")
        );
    }

    /// Every CE-text formatter (ADR-0116 §9, ADR-0117 §6) produces an error
    /// whose `"<Kind>:"` token the bindings recover as the kind name.
    #[test]
    fn binding_error_metadata_recovers_ce_message_kinds() {
        use cypherlite_core::ce_messages as ce;
        let cases: &[(Error, &'static str)] = &[
            (
                ce::unsupported_administration_command("CREATE DATABASE foo"),
                "UnsupportedAdministrationCommand",
            ),
            (ce::graph_not_found("movies"), "DatabaseNotFound"),
            (
                ce::composite_only_graph_function("graph.byName('neo4j')"),
                "SemanticError",
            ),
            (ce::multiple_graphs("system"), "SemanticError"),
            (
                ce::element_id_no_namespace("4:00000000-0000-0000-0000-000000000000:n1"),
                "SemanticError",
            ),
            (ce::unsupported_system_clauses("MATCH"), "SemanticError"),
            (ce::system_ontologies_not_queryable(), "SemanticError"),
            (ce::read_only_write(), "WriteOnReadOnlyAccessDatabase"),
            (ce::alter_unknown_database("movies"), "DatabaseNotFound"),
            (ce::database_does_not_exist("movies"), "DatabaseNotFound"),
            (ce::namespace_change_in_transaction(), "TransactionError"),
        ];
        for (error, kind) in cases {
            assert_eq!(
                binding_error_metadata(error),
                (*kind, *kind),
                "wrong metadata for {error}"
            );
        }
    }
}
