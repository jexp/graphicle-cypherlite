//! `CALL` procedure execution: registered + built-in procedures,
//! fulltext and vector index query procedures. The dispatch path
//! classifies a `CALL` name once ([`divert_call`]): a specialized executor
//! (ontology, vector, fulltext) or the generic registry fallback. The
//! built-in procedure row producers live in the sibling `procedure_rows`
//! module.

use std::borrow::Cow;
use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Expr, YieldItems};

use crate::ctx::ExecCtx;
use crate::eval::{eval, Row};

use super::ontology::{exec_ontology_call, OntologyProc};
use super::procedure_rows::builtin_procedure;
use super::vector::{vector_index_candidates, vector_operand, VectorSearchFilter};

/// Whether a value satisfies a procedure argument type string (`STRING`,
/// `INTEGER`, `LIST<FLOAT>`, …). Unknown/`ANY`/nullable types match
/// everything — the precise element-type check happens in the procedure
/// body itself. Used by the executor's `CALL` dispatch and re-exported for
/// `cypherlite::Database`'s compile-time call validation.
pub fn value_matches_proc_type(v: &Value, ty: &str) -> bool {
    let ty = ty.trim().trim_end_matches('?').trim().to_ascii_uppercase();
    if v.is_null() || ty.is_empty() || ty == "ANY" {
        return true;
    }
    match ty.as_str() {
        "STRING" => matches!(v, Value::String(_)),
        "INTEGER" | "INT" => matches!(v, Value::Int(_)),
        "FLOAT" => matches!(v, Value::Float(_) | Value::Int(_)),
        "NUMBER" => matches!(v, Value::Int(_) | Value::Float(_)),
        "BOOLEAN" | "BOOL" => matches!(v, Value::Bool(_)),
        "LIST" => matches!(v, Value::List(_)),
        "MAP" => matches!(v, Value::Map(_)),
        _ => true,
    }
}

pub(crate) fn call_args_match(proc_val: &Value, given: &Value) -> bool {
    // `eq_non_null` already covers the Int/Float cross-comparison with the
    // exact rule (`cypherlite_model::canonical_key::eq_i64_f64`); a separate
    // `as f64` fallback here would reintroduce the lossy cast.
    proc_val.eq_non_null(given) || (proc_val.is_null() && given.is_null())
}

/// Static shape of a `CALL` clause (name, arguments, yields, optionality).
#[derive(Clone, Copy)]
pub(crate) struct CallSpec<'a> {
    pub name: &'a str,
    pub args: &'a [Expr],
    pub explicit: bool,
    pub yield_items: &'a YieldItems,
    pub optional: bool,
    pub where_clause: Option<&'a Expr>,
}

/// The registry-driven explicit-arity check shared by the generic `CALL`
/// path and the `db.ontology.*` executor (the divert's consumer): an
/// explicit call passes between `required` and `total` arguments. (The
/// facade's compile-time `validate_calls` already enforces the same
/// contract.)
pub fn check_explicit_arity(name: &str, required: usize, total: usize, given: usize) -> Result<()> {
    if given < required || given > total {
        return Err(Error::syntax(format!(
            "InvalidNumberOfArguments: {name} expected {total}, got {given}"
        )));
    }
    Ok(())
}

/// The passing-mode backstop shared by the generic `CALL` path and the
/// `db.ontology.*` executor (the divert's consumer): an in-query implicit
/// call — a `CALL` with a YIELD, in a clause pipeline — cannot pass
/// arguments; the procedure's arguments resolve only in a standalone unit
/// call. `required_args` is the caller's registry truth: the number of
/// declared parameters *without a default* (LABS-389: a procedure whose
/// parameters all carry defaults is callable implicitly). (The facade's
/// compile-time `validate_calls` enforces the same contract before
/// execution; this is the executor-side backstop.)
pub fn check_implicit_passing_mode(
    explicit: bool,
    required_args: usize,
    has_yield: bool,
) -> Result<()> {
    if !explicit && required_args > 0 && has_yield {
        return Err(Error::syntax(
            "InvalidArgumentPassingMode: in-query CALL cannot pass arguments implicitly",
        ));
    }
    Ok(())
}

/// The generic void-procedure contract: an OPTIONAL `CALL` with no return
/// columns over no input rows yields one unit row. Callers gate on the
/// void shape themselves.
pub(crate) fn void_optional_unit_row(rows_empty: bool, optional: bool) -> bool {
    rows_empty && optional
}

/// Row composition shared by every built-in row-producing procedure
/// executor (the generic registry path, fulltext, vector and
/// `db.ontology.get`): resolve the effective YIELD list (the caller's
/// `yield_items`, or — when empty or `*`-containing — `default_yields`),
/// then per input row and per produced column→value map, clone the input,
/// insert each yielded cell under `alias.unwrap_or(column)` (`Null` for a
/// column the producer did not emit), apply the WHERE truthy filter, and
/// pad one null row when an OPTIONAL call produced nothing for the input
/// row.
pub(crate) fn compose_rows<'a>(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    default_yields: &[&str],
    yield_items: &YieldItems,
    optional: bool,
    where_clause: Option<&Expr>,
    mut produce: impl FnMut(&Row) -> Result<Vec<Cow<'a, BTreeMap<String, Value>>>>,
) -> Result<Vec<Row>> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let yields: Vec<(String, Option<String>)> = yield_items.resolved_pairs(default_yields);
    let mut out = Vec::new();
    for input in rows {
        let mut produced = 0usize;
        for values in produce(&input)? {
            let mut row = input.clone();
            for (column, alias) in &yields {
                let key = alias.clone().unwrap_or_else(|| column.clone());
                row.insert(key, values.get(column).cloned().unwrap_or(Value::Null));
            }
            if where_clause
                .map(|predicate| eval(graph, &row, predicate, params, cypher_version))
                .transpose()?
                .is_none_or(|value| value.truthy() == Some(true))
            {
                out.push(row);
                produced += 1;
            }
        }
        if produced == 0 && optional {
            let mut row = input;
            for (column, alias) in yields.iter() {
                // The alias-precedence rule, the same key computation the
                // main loop states — never a second hand-kept spelling.
                let key = alias
                    .as_deref()
                    .map(str::to_string)
                    .unwrap_or_else(|| column.clone());
                row.insert(key, Value::Null);
            }
            out.push(row);
        }
    }
    Ok(out)
}

/// The one classification step of the `CALL` dispatch path
/// ([`exec_call`]): which specialized executor owns the name — or `None`
/// for the generic registry fallback. The `db.ontology.*` procedures
/// (ADR-0121/ADR-0227) classify by the single [`OntologyProc::try_from`]
/// (their executor's exhaustive dispatch enum); the vector and fulltext
/// query procedures classify by the canonical signature registry — the
/// exact, case-sensitive registered names (CIP-230), the same resolution
/// the generic path's own built-in lookup below uses. This replaces the
/// historical ad-hoc divert schemes (a registry scan plus a
/// `starts_with` prefix plus `try_from` for the ontology divert, a
/// lowercase literal match for the vector pair, a lowercase substring
/// match for fulltext): no parallel name-matching scheme remains in the
/// dispatch path.
enum CallDivert {
    /// One of the four `db.ontology.*` procedures, already classified.
    Ontology(OntologyProc),
    /// One of the two `db.index.vector.query*` procedures;
    /// `relationship` distinguishes `queryRelationships` from `queryNodes`.
    VectorQuery { relationship: bool },
    /// `db.index.fulltext.queryNodes`.
    FulltextQuery,
}

/// The one classification step: decide [`CallDivert`] for one `CALL` name,
/// once, before the generic machinery.
fn divert_call(name: &str) -> Option<CallDivert> {
    if let Ok(proc) = OntologyProc::try_from(name) {
        return Some(CallDivert::Ontology(proc));
    }
    // The registry lookup doubles as the classifier: only exact registered
    // names divert, case-sensitively (CIP-230), exactly like the generic
    // path's built-in resolution below.
    match crate::procedure_registry::builtin_procedure_signature(name)?.name {
        "db.index.vector.queryNodes" => Some(CallDivert::VectorQuery {
            relationship: false,
        }),
        "db.index.vector.queryRelationships" => {
            Some(CallDivert::VectorQuery { relationship: true })
        }
        "db.index.fulltext.queryNodes" => Some(CallDivert::FulltextQuery),
        _ => None,
    }
}

/// The runtime `CALL` contract for the diverted `db.ontology.*`
/// procedures — the SAME checks the generic path applies below, from the
/// ONE registry signature: the passing-mode backstop
/// (`check_implicit_passing_mode`), then the explicit arity
/// (`check_explicit_arity`). Applied at the divert site in `exec_call`
/// so the diverted executor enforces exactly the contract the generic
/// path enforces, visibly, at the same place.
pub(super) fn check_ontology_call_contract(call: &CallSpec<'_>) -> Result<()> {
    // The four names are statically registered and the classification
    // accepts exactly those four (pinned by test), so this lookup cannot
    // miss; the fail-closed arm replaces the divert's historical
    // `ProcedureNotFound` error should the table and the classification
    // ever drift apart.
    let signature = crate::procedure_registry::builtin_procedure_signature(call.name)
        .ok_or_else(|| Error::procedure_not_found(call.name))?;
    // All-default parameters are callable paren-less (LABS-389): the
    // guard keys on required args, not declared ones.
    check_implicit_passing_mode(
        call.explicit,
        signature.required_args(),
        call.yield_items.has_yield(),
    )?;
    if call.explicit {
        check_explicit_arity(
            call.name,
            signature.required_args(),
            signature.args.len(),
            call.args.len(),
        )?;
    }
    Ok(())
}

pub(crate) fn exec_call(ctx: ExecCtx<'_>, rows: Vec<Row>, call: CallSpec<'_>) -> Result<Vec<Row>> {
    // ONE classification step decides divert-or-generic once. The
    // `db.ontology.*` divert runs the same runtime contract (passing
    // mode, arity — `check_ontology_call_contract`) the generic path runs
    // below, at this same site, before handing the call to its executor;
    // the vector executor takes the classified entity kind (node vs
    // relationship) with it. Everything else falls to the generic
    // registry machinery below.
    match divert_call(call.name) {
        Some(CallDivert::Ontology(proc)) => {
            check_ontology_call_contract(&call)?;
            exec_ontology_call(ctx, rows, call, proc)
        }
        Some(CallDivert::VectorQuery { relationship }) => {
            exec_vector_query_call(ctx, rows, relationship, call)
        }
        Some(CallDivert::FulltextQuery) => exec_fulltext_query_call(ctx, rows, call),
        None => exec_registry_call(ctx, rows, call),
    }
}
/// The generic registry path a non-diverted call takes: the registry or
/// builtin signature lookup, the passing-mode and arity contract, per-row
/// argument binding and type check, row matching, composition, and the
/// void-procedure unit row. One unit, so the classifier above stays a
/// classifier.
fn exec_registry_call(ctx: ExecCtx<'_>, rows: Vec<Row>, call: CallSpec<'_>) -> Result<Vec<Row>> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        principal,
        ..
    } = ctx;
    let CallSpec {
        name,
        args,
        explicit,
        yield_items,
        optional,
        where_clause,
    } = call;
    let registered = graph.procedure(name);
    let proc = registered
        .clone()
        .or_else(|| builtin_procedure(graph, name, principal))
        .ok_or_else(|| Error::procedure_not_found(name))?;
    // Minimum arity: a registered procedure requires every argument; a
    // built-in only its parameters without defaults (registry truth — e.g.
    // `db.awaitIndexes()` may omit `timeOutSeconds`).
    let required_args = if registered.is_some() {
        proc.arg_names.len()
    } else {
        // A builtin resolved through the same registry this signature comes
        // from; a miss is an internal inconsistency, not a default.
        crate::procedure_registry::builtin_procedure_signature(name)
            .expect("builtin_procedure resolved from this registry")
            .required_args()
    };
    // Same rule on the unit-call path (LABS-389): required_args already
    // folds the builtin/registered split, so the guard reads it instead
    // of any-args.
    check_implicit_passing_mode(explicit, required_args, yield_items.has_yield())?;
    if explicit {
        check_explicit_arity(name, required_args, proc.arg_names.len(), args.len())?;
    }
    if proc.yield_names.is_empty() && void_optional_unit_row(rows.is_empty(), optional) {
        return Ok(vec![Row::new()]);
    }
    let mut out = Vec::new();
    for row in &rows {
        let mut call_args = BTreeMap::new();
        if explicit {
            for (i, a) in args.iter().enumerate() {
                call_args.insert(
                    proc.arg_names[i].clone(),
                    eval(graph, row, a, params, cypher_version)?,
                );
            }
        } else {
            for n in &proc.arg_names {
                if let Some(v) = params.get(n).cloned().or_else(|| row.get(n).cloned()) {
                    call_args.insert(n.clone(), v);
                }
            }
            // Only parameters without a default are mandatory; defaulted
            // built-in parameters (e.g. `db.awaitIndexes`'s
            // `timeOutSeconds`) may be omitted.
            let missing_required = proc
                .arg_names
                .iter()
                .take(required_args)
                .any(|n| !call_args.contains_key(n));
            if missing_required {
                return Err(Error::syntax(format!(
                    "MissingParameter: implicit args for {name}"
                )));
            }
        }
        for (i, n) in proc.arg_names.iter().enumerate() {
            if let (Some(ty), Some(v)) = (proc.arg_types.get(i), call_args.get(n)) {
                if !value_matches_proc_type(v, ty) {
                    return Err(Error::syntax(format!(
                        "InvalidArgumentType: {name} argument `{n}` expects {ty}"
                    )));
                }
            }
        }
        let matched: Vec<_> = proc
            .rows
            .iter()
            .filter(|pr| {
                proc.arg_names
                    .iter()
                    .all(|n| match (pr.get(n), call_args.get(n)) {
                        (Some(a), Some(b)) => call_args_match(a, b),
                        (None, _) => true,
                        _ => false,
                    })
            })
            .collect();
        if matched.is_empty() && proc.yield_names.is_empty() {
            out.push(row.clone());
            continue;
        }
        let default_yields: Vec<&str> = proc.yield_names.iter().map(String::as_str).collect();
        out.extend(compose_rows(
            ctx,
            vec![row.clone()],
            &default_yields,
            yield_items,
            optional,
            where_clause,
            // The matched rows are BORROWED, not cloned: compose_rows
            // clones only the yielded cells it inserts, so a wide
            // procedure row costs one cell per yielded column — never a
            // full-row deep clone per input row.
            |_| Ok(matched.iter().map(|pr| Cow::Borrowed(*pr)).collect()),
        )?);
    }
    if proc.yield_names.is_empty() && proc.rows.is_empty() {
        // A void procedure is a unit operation in a clause pipeline. The
        // caller decides how a terminal unit query is represented.
        return Ok(rows);
    }
    Ok(out)
}

pub(crate) fn exec_fulltext_query_call(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    call: CallSpec<'_>,
) -> Result<Vec<Row>> {
    let CallSpec {
        args,
        yield_items,
        optional,
        where_clause,
        ..
    } = call;
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    if args.len() != 2 {
        return Err(Error::syntax(format!(
            "InvalidNumberOfArguments: db.index.fulltext.queryNodes expected 2, got {}",
            args.len()
        )));
    }
    compose_rows(
        ctx,
        rows,
        &["node", "score"],
        yield_items,
        optional,
        where_clause,
        |input| {
            let index_name = eval(graph, input, &args[0], params, cypher_version)?
                .as_string()
                .ok_or_else(|| Error::invalid_argument_type("indexName must be STRING"))?
                .to_string();
            let query = eval(graph, input, &args[1], params, cypher_version)?
                .as_string()
                .ok_or_else(|| Error::invalid_argument_type("query must be STRING"))?
                .to_string();
            let detail = graph
                .show_schema()?
                .data
                .index_details
                .get(&index_name)
                .cloned()
                .filter(|detail| detail.kind == crate::redo::IndexKindSpec::Fulltext)
                .ok_or_else(|| Error::execution("IndexNotFound: fulltext index"))?;
            let (property_filter, token) = query
                .split_once(':')
                .map_or((None, query.as_str()), |(property, token)| {
                    (Some(property), token)
                });
            let token = token.to_ascii_lowercase();
            let mut produced = Vec::new();
            for id in graph.all_node_ids() {
                let Some(node) = graph.node(id) else {
                    continue;
                };
                if !detail
                    .labels
                    .iter()
                    .any(|label| node.labels.contains(label))
                {
                    continue;
                }
                let matched = detail
                    .properties
                    .iter()
                    .filter(|property| property_filter.is_none_or(|wanted| wanted == *property))
                    .filter_map(|property| node.properties.get(property))
                    .any(|value| fulltext_value_matches(value, &token));
                if !matched {
                    continue;
                }
                produced.push(BTreeMap::from([
                    ("node".to_string(), Value::from_node(node.clone())),
                    ("score".to_string(), Value::Float(1.0)),
                ]));
            }
            Ok(produced.into_iter().map(Cow::Owned).collect())
        },
    )
}

pub(crate) fn fulltext_value_matches(value: &Value, token: &str) -> bool {
    match value {
        Value::String(value) => value
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|word| word.eq_ignore_ascii_case(token)),
        Value::List(values) => values
            .iter()
            .any(|value| fulltext_value_matches(value, token)),
        _ => false,
    }
}

pub(crate) fn exec_vector_query_call(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    relationship: bool,
    call: CallSpec<'_>,
) -> Result<Vec<Row>> {
    let CallSpec {
        name,
        args,
        yield_items,
        optional,
        where_clause,
        ..
    } = call;
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    if args.len() != 3 {
        return Err(Error::syntax(format!(
            "InvalidNumberOfArguments: {name} expected 3, got {}",
            args.len()
        )));
    }
    // The divert already classified the entity kind (`relationship` is
    // `queryRelationships` vs `queryNodes`); no name re-matching here.
    let entity_column = if relationship { "relationship" } else { "node" };
    compose_rows(
        ctx,
        rows,
        &[entity_column, "score"],
        yield_items,
        optional,
        where_clause,
        |input| {
            let index_name = eval(graph, input, &args[0], params, cypher_version)?
                .as_string()
                .ok_or_else(|| Error::invalid_argument_type("indexName must be STRING"))?
                .to_string();
            let limit = match eval(graph, input, &args[1], params, cypher_version)? {
                Value::Int(value) if (1..=i32::MAX as i64).contains(&value) => value as usize,
                _ => return Err(Error::ty(
                    "InvalidArgumentValue: numberOfNearestNeighbours must be a positive INTEGER",
                )),
            };
            let query = eval(graph, input, &args[2], params, cypher_version)?;
            let Some(query) = vector_operand(&query)? else {
                return Ok(Vec::new());
            };
            let (index_kind, candidates) = vector_index_candidates(
                ctx,
                &index_name,
                &query,
                0,
                limit,
                VectorSearchFilter {
                    filters: &[],
                    input,
                },
            )?;
            if index_kind.is_relationship() != relationship {
                return Err(Error::execution(
                    "WrongIndexType: vector index entity type mismatch",
                ));
            }
            let mut produced = Vec::new();
            for (entity, score) in candidates {
                // Both entity spellings map to the candidate (the
                // historical cell match keyed `"node" | "relationship"`),
                // so an explicit YIELD of either sees it.
                produced.push(BTreeMap::from([
                    ("node".to_string(), entity.clone()),
                    ("relationship".to_string(), entity.clone()),
                    ("score".to_string(), Value::Float(score)),
                ]));
            }
            Ok(produced.into_iter().map(Cow::Owned).collect())
        },
    )
}
