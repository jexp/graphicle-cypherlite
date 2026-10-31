//! `CALL` validation of logical plans.
//!
//! Validation checks `CALL` clauses at compile time against registered
//! procedures and the executor's canonical built-in signature registry.
//! The ADR-0116 namespace classifiers (read/write routing, the §2
//! database-administration classifier, the §4 `system` gate) live in
//! `cypherlite_planner::namespaces` — the one home for the namespace rules,
//! next to the §3 `USE` rules; the `system_ontologies` gate is ADR-0229's
//! fixed rejection (one text, no classifier), raised in
//! `crate::gate::Connection::namespace_gate_error`. The
//! executor-registry predicate [`call_is_write`] lives in the executor
//! beside the registry it reads (`cypherlite_executor::call_is_write`), so
//! the executor's ADR-0402 statement write-attempt walk and this crate's
//! namespace gate share it.

use std::collections::BTreeMap;

use cypherlite_executor::Graph;
use cypherlite_ir::{Clause, Expr};
use cypherlite_planner::LogicalPlan;

use crate::{Error, Result, Value};

/// Compile-time validation of `CALL` clauses against registered procedures
/// and the executor's canonical built-in procedure signature registry
/// (`cypherlite_executor::procedure_registry`) — the same table the
/// executor's `CALL` dispatch and the `SHOW PROCEDURES` catalog read, so
/// validation can no longer disagree with execution or the catalog (e.g. the
/// vector `query` argument is `LIST<FLOAT>` everywhere).
pub(crate) fn validate_calls(
    graph: &Graph,
    logical: &LogicalPlan,
    params: &BTreeMap<String, Value>,
) -> Result<()> {
    // Full-descent walk: EVERY `CALL` in the clause tree — top-level
    // clauses, `NEXT` stages, `CALL { … }` bodies, `WHEN`/`ELSE` branches,
    // `FOREACH` bodies and expression subqueries (via `walk_plan_clauses`)
    // — is validated. Without the full descent a `CALL` in a nested
    // position skipped every check below and an undeclared `YIELD` name
    // silently bound null.
    //
    // Deliberate contrast: the §4 42N17 offender collection
    // (`cypherlite_planner::unsupported_system_clause_names`) stays
    // part-granular, because 42N17 judges the statement's top-level shape
    // while CALL validation must cover every call site. Do not "unify"
    // the two walks.
    let mut result = Ok(());
    cypherlite_planner::walk_plan_clauses(logical, &mut |clause| {
        if result.is_ok() {
            result = validate_call_clause(graph, clause, params);
        }
    });
    result
}

/// The per-clause validation body of [`validate_calls`]: a no-op for
/// non-`CALL` clauses.
fn validate_call_clause(
    graph: &Graph,
    clause: &Clause,
    params: &BTreeMap<String, Value>,
) -> Result<()> {
    let Clause::Call {
        name,
        args,
        explicit,
        yield_items,
        ..
    } = clause
    else {
        return Ok(());
    };
    let registered = graph.procedure(name);
    let builtin = if registered.is_none() {
        cypherlite_executor::builtin_procedure_signature(name)
            .filter(|signature| signature.executable)
    } else {
        None
    };
    let (arg_names, arg_types, required_args, output_names) = if let Some(proc) = registered {
        let required_args = proc.arg_names.len();
        (
            proc.arg_names,
            proc.arg_types,
            required_args,
            proc.yield_names,
        )
    } else if let Some(signature) = builtin {
        let (arg_names, arg_types): (Vec<_>, Vec<_>) = signature
            .parameters()
            .map(|(name, type_str)| (name.to_string(), type_str.to_string()))
            .unzip();
        (
            arg_names,
            arg_types,
            signature.required_args(),
            signature
                .return_columns()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    } else {
        return Err(Error::procedure_not_found(name));
    };
    // A procedure whose parameters all carry defaults is callable
    // paren-less in-query too (LABS-389): only parameters without a
    // default are mandatory, so the implicit-pass guard keys on
    // required_args, not any-args.
    // The two contract guards, one implementation: the executor's own
    // predicates own the policy and the error text for the runtime path;
    // this compile-time facade consults the same two (never a second
    // spelling of either message).
    cypherlite_executor::check_implicit_passing_mode(
        *explicit,
        required_args,
        yield_items.has_yield(),
    )?;
    if *explicit {
        cypherlite_executor::check_explicit_arity(
            name,
            required_args,
            arg_names.len(),
            args.len(),
        )?;
    }
    if !*explicit && required_args > 0 && !yield_items.has_yield() {
        // Only parameters without a default are mandatory.
        let missing = arg_names
            .iter()
            .take(required_args)
            .any(|n| !params.contains_key(n));
        if missing {
            return Err(Error::syntax(format!(
                "MissingParameter: implicit args for {name}"
            )));
        }
    }
    for (i, arg) in args.iter().enumerate() {
        let Some(ty) = arg_types.get(i) else {
            continue;
        };
        let Some(value) = static_expr_value(arg) else {
            continue;
        };
        if !cypherlite_executor::value_matches_proc_type(&value, ty) {
            return Err(Error::syntax(format!(
                "InvalidArgumentType: {name} argument expects {ty}"
            )));
        }
    }
    // Every explicit named YIELD item must be a column the procedure
    // actually declares. `YIELD *` binds the full output set and is
    // always allowed; a name the procedure does not return is a
    // compile-time error rather than a silently-null binding.
    for item in yield_items.names() {
        if !output_names.iter().any(|declared| declared == item) {
            return Err(Error::syntax(format!(
                "UnknownProcedureOutput: {name} does not return `{item}`"
            )));
        }
    }
    Ok(())
}

fn static_expr_value(expr: &Expr) -> Option<Value> {
    Some(match expr {
        Expr::Null => Value::Null,
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Int(i) => Value::Int(*i),
        Expr::Float(f) => Value::Float(*f),
        Expr::String(s) => Value::String(s.as_str().into()),
        Expr::List(xs) => Value::List(
            xs.iter()
                .map(static_expr_value)
                .collect::<Option<Vec<_>>>()?
                .into(),
        ),
        _ => return None,
    })
}

/// The executor's one write-procedure predicate, re-exported for the
/// namespace gate (`crate::gate`); its definition and rationale live beside
/// the registry it reads (`cypherlite_executor::call_is_write`).
pub(crate) use cypherlite_executor::call_is_write;

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(cypher: &str) -> LogicalPlan {
        let ast = cypherlite_front::parse_cypher(cypher).unwrap();
        cypherlite_planner::plan(ast, &mut |_| false).unwrap()
    }

    #[test]
    fn validate_calls_covers_next_stages() {
        // Regression: `validate_calls` must recurse into `NEXT` stages —
        // before it did, a `CALL` in a later stage skipped compile-time
        // validation entirely and an undeclared YIELD name silently bound
        // null.
        let graph = Graph::default();
        let no_params = BTreeMap::new();

        let error = validate_calls(
            &graph,
            &plan_of("CREATE (:L) RETURN 1 AS x NEXT CALL db.labels() YIELD nope RETURN nope"),
            &no_params,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("UnknownProcedureOutput"),
            "{error}"
        );

        // A valid NEXT-stage CALL still validates.
        validate_calls(
            &graph,
            &plan_of("CREATE (:L) RETURN 1 AS x NEXT CALL db.labels() YIELD label RETURN label"),
            &no_params,
        )
        .unwrap();

        // Arity and parameter-mode checks apply in NEXT stages too.
        let error = validate_calls(
            &graph,
            &plan_of("RETURN 1 AS x NEXT CALL db.labels(1)"),
            &no_params,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("InvalidNumberOfArguments"),
            "{error}"
        );

        let error = validate_calls(
            &graph,
            &plan_of("RETURN 1 AS x NEXT CALL db.createLabel"),
            &no_params,
        )
        .unwrap_err();
        assert!(error.to_string().contains("MissingParameter"), "{error}");
    }

    #[test]
    fn validate_calls_covers_nested_call_positions() {
        // Regression: `validate_calls` walks the FULL clause tree
        // (`walk_plan_clauses`) — a `CALL` nested in a `CALL { … }`
        // subquery, a `WHEN` branch or a `FOREACH` body is validated too;
        // before the full descent an undeclared YIELD there silently bound
        // null.
        let graph = Graph::default();
        let no_params = BTreeMap::new();
        for cypher in [
            "CALL { CALL db.labels() YIELD nope RETURN nope } RETURN 1 AS x",
            "WHEN true THEN { CALL db.labels() YIELD nope RETURN nope }",
            "FOREACH (x IN [1] | CALL db.labels() YIELD nope)",
        ] {
            let error = validate_calls(&graph, &plan_of(cypher), &no_params).unwrap_err();
            assert!(
                error.to_string().contains("UnknownProcedureOutput"),
                "{cypher}: {error}"
            );
        }
        // Valid nested CALLs still validate.
        for cypher in [
            "CALL { CALL db.labels() YIELD label RETURN label } RETURN 1 AS x",
            "WHEN true THEN { CALL db.labels() YIELD label RETURN label }",
        ] {
            validate_calls(&graph, &plan_of(cypher), &no_params).unwrap();
        }
    }
}
