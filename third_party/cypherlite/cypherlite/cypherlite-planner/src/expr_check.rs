//! Expression checking: variable resolution against the binding
//! environment, predicate shape rules, and the per-variant semantic rules
//! (function arity/argument types, comprehension scopes, subquery
//! expressions, ...).

use cypherlite_core::{Error, Result};
// The aggregate-name set is owned by `cypherlite-ir` and re-exported by
// `cypherlite-front::agg` (which adds the CIP-128 alias spellings); the
// known-function registry is owned by `cypherlite-front`. The planner binds
// against these single definitions instead of keeping local copies.
use cypherlite_front::agg::is_aggregate_name as is_agg_fn;
use cypherlite_ir::{BinaryOp, Clause, Expr, MapProjectionItem, UnaryOp};

use crate::binding::{Binding, Env};
use crate::names::query_output_names_with_seed;
use crate::pattern::{check_path_exprs, check_pattern_pred_vars};
use crate::use_target::graph_call_text;
use crate::validate::{clause_is_write, query_with_persistent_imports, validate_part_with_env};

pub(crate) fn check_expr(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    expr: &Expr,
) -> Result<()> {
    check_expr_ctx(env, is_write_procedure, expr, false)
}

pub(crate) fn check_pred(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    expr: &Expr,
) -> Result<()> {
    if is_static_non_bool(expr) {
        return Err(Error::syntax(
            "InvalidArgumentType: predicate requires a boolean",
        ));
    }
    if let Expr::Variable(v) = expr {
        if matches!(
            env.get(v),
            Some(Binding::Node | Binding::Rel | Binding::Path)
        ) {
            return Err(Error::syntax(
                "InvalidArgumentType: an entity or path is not a boolean predicate",
            ));
        }
    }
    check_expr_ctx(env, is_write_procedure, expr, true)
}

pub(crate) fn is_delete_target(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Variable(_)
            | Expr::Property { .. }
            | Expr::Subscript { .. }
            | Expr::Function { .. }
            | Expr::Case { .. }
    )
}

fn is_static_non_bool(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Int(_)
            | Expr::Float(_)
            | Expr::String(_)
            | Expr::List(_)
            | Expr::Map(_)
            | Expr::CountStar
    )
}

pub(crate) fn check_nonneg_int_arg(expr: &Expr) -> Result<()> {
    match expr {
        Expr::Int(i) if *i < 0 => Err(Error::syntax(
            "NegativeIntegerArgument: SKIP/LIMIT must be non-negative",
        )),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Int(i) if *i > 0) => Err(Error::syntax(
            "NegativeIntegerArgument: SKIP/LIMIT must be non-negative",
        )),
        _ => Ok(()),
    }
}

/// ADR-0175 (statically-known floats): a float *literal*, sign included —
/// the parser splits `-1.5` into a unary minus over the literal, and both
/// spellings are the same float-literal mistake Neo4j's static FLOAT gate
/// rejects at compile time (22N27 / 22NB1). Constant float *expressions*
/// (`0.0/0.0`, `head([1.0, 'x'])`, …) are not literals and are handled at
/// runtime — the ADR's recorded compile-vs-runtime deviation.
fn is_float_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Float(_) => true,
        Expr::Unary {
            op: UnaryOp::Minus,
            expr,
        } => matches!(expr.as_ref(), Expr::Float(_)),
        _ => false,
    }
}

/// ADR-0175: reject a float literal at an integer-argument site with the
/// same `InvalidArgumentType` kind the site raises at runtime
/// (`Error::invalid_argument_type`); `what` names the site ("range()
/// start", "list index must be an integer", …).
fn reject_float_literal(expr: &Expr, what: &str) -> Result<()> {
    if is_float_literal(expr) {
        return Err(Error::invalid_argument_type(what));
    }
    Ok(())
}

/// ADR-0175: reject a float literal at an integer-argument site at plan
/// time, with the same `InvalidArgumentType` kind the site raises at
/// runtime (`Error::invalid_argument_type`). For `range()` and list index the rendered
/// message matches the runtime failure; for `substring()`/`left()`/
/// `right()`/slice the wording is plan-time-only — at runtime those sites
/// coerce floats (truncate toward zero, NaN → 0, saturate) and only raise
/// `NumberOutOfRange` with different wording, per ADR-0175. CypherLite has
/// no static float typing, so every non-literal float is still handled by
/// the runtime rules.
fn check_int_site_float_literals(name: &str, args: &[Expr]) -> Result<()> {
    let sites: &[(usize, &str)] = match name {
        "range" => &[
            (0, "range() start"),
            (1, "range() end"),
            (2, "range() step"),
        ],
        "substring" => &[
            (1, "substring() start must be an integer"),
            (2, "substring() length must be an integer"),
        ],
        "left" => &[(1, "left() length must be an integer")],
        "right" => &[(1, "right() length must be an integer")],
        _ => &[],
    };
    for &(i, what) in sites {
        if let Some(arg) = args.get(i) {
            reject_float_literal(arg, what)?;
        }
    }
    Ok(())
}

/// Fixed-arity function policy as data: (name set, expected arity,
/// message) — the same idiom as `check_int_site_float_literals`' sites
/// table. The name sets are disjoint, so at most one row fires.
const FIXED_ARITY_FUNCTIONS: &[(&[&str], usize, &str)] = &[
    (
        &["timestamp"],
        0,
        "InvalidNumberOfArguments: timestamp() takes no arguments",
    ),
    (
        &["valuetype"],
        1,
        "InvalidNumberOfArguments: valueType() requires one argument",
    ),
    (
        &["tofloatlist", "tointegerlist"],
        1,
        "InvalidNumberOfArguments: list conversion requires one argument",
    ),
    (
        &[
            "char_length",
            "character_length",
            "octet_length",
            "upper",
            "lower",
            "toupper",
            "tolower",
            "sinh",
            "cosh",
            "tanh",
            "coth",
        ],
        1,
        "InvalidNumberOfArguments: function requires one argument",
    ),
];

pub(crate) fn check_positive_int_arg(expr: &Expr, what: &str) -> Result<()> {
    match expr {
        Expr::Int(i) if *i <= 0 => Err(Error::syntax(format!(
            "InvalidArgument: {what} must be a positive integer"
        ))),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Int(_) | Expr::Float(_)) => Err(Error::syntax(format!(
            "InvalidArgument: {what} must be a positive integer"
        ))),
        Expr::Float(_) | Expr::String(_) | Expr::Bool(_) | Expr::List(_) | Expr::Map(_) => Err(
            Error::syntax(format!("InvalidArgumentType: {what} must be an integer")),
        ),
        _ => Ok(()),
    }
}

/// Whether the expression is non-static for CIP-34 concurrency / CIP-198
/// retry-duration purposes: it references a variable, a map projection (its
/// base is a row value), a pattern, or a subquery — at any nesting depth.
/// This is a single fold over `Expr::children`, so no variant (e.g. a list
/// comprehension under a subscript) can smuggle a variable past the check.
pub(crate) fn expr_non_static(expr: &Expr) -> bool {
    expr.any(&mut |e| {
        matches!(
            e,
            Expr::Variable(_)
                | Expr::MapProjection { .. }
                | Expr::PatternPred { .. }
                | Expr::PatternComp { .. }
                | Expr::ShortestPath { .. }
                | Expr::ExistsSubquery { .. }
                | Expr::CountSubquery { .. }
                | Expr::CollectSubquery { .. }
        )
    })
}

fn expr_contains_rand(expr: &Expr) -> bool {
    expr.any(&mut |e| matches!(e, Expr::Function { name, .. } if name.eq_ignore_ascii_case("rand")))
}

/// The known-function check the binder applies to lowered IR. Delegates to
/// the single registry owned by `cypherlite-front` (`KNOWN_FUNCTIONS` plus
/// the unadvertised lowered-internal names such as `tostring_full`), so a
/// function advertised by `SHOW FUNCTIONS` can no longer be rejected here
/// (and vice versa).
fn is_known_function(name: &str) -> bool {
    cypherlite_front::is_known_function(name)
}

fn check_expr_ctx(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    expr: &Expr,
    allow_pattern_pred: bool,
) -> Result<()> {
    match expr {
        Expr::Variable(v) => {
            if !env.contains_key(v) {
                return Err(Error::syntax(format!("UndefinedVariable: `{v}`")));
            }
        }
        Expr::Star => {}
        Expr::Function { name, args, .. } => {
            let n = name.to_ascii_lowercase();
            // ADR-0116 §3: every composite-database `graph.*` selector
            // (`cypherlite_core::GRAPH_SELECTOR_FUNCTIONS`,
            // `graph.byElementId` included) fails with CE's 42N72 text in
            // every expression position (the `USE` clause itself is
            // resolved by `use_target`). They stay listed by
            // `SHOW FUNCTIONS` through the executor's catalog-only entries.
            if cypherlite_core::is_graph_selector_function_name(name) {
                return Err(cypherlite_core::ce_messages::composite_only_graph_function(
                    &graph_call_text(name, args),
                ));
            }
            if !is_known_function(&n) {
                return Err(Error::syntax(format!("UnknownFunction: `{name}`")));
            }
            // ADR-0175: float literals at the integer-argument sites fail
            // at plan time (Neo4j's 22N27/22NB1 static gate fires at the
            // same phase); non-literal floats are handled at runtime.
            check_int_site_float_literals(&n, args)?;
            for &(names, arity, message) in FIXED_ARITY_FUNCTIONS {
                if names.contains(&n.as_str()) && args.len() != arity {
                    return Err(Error::syntax(message));
                }
            }
            if n == "type" {
                if let Some(Expr::Variable(v)) = args.first() {
                    if env.get(v) == Some(&Binding::Node) {
                        return Err(Error::syntax(
                            "InvalidArgumentType: type() needs a relationship",
                        ));
                    }
                }
            }
            if n == "length" {
                if let Some(Expr::Variable(v)) = args.first() {
                    if matches!(
                        env.get(v),
                        Some(&Binding::Node | &Binding::Rel | &Binding::Scalar)
                    ) {
                        return Err(Error::syntax(
                            "InvalidArgumentType: length() needs a path, string, or list",
                        ));
                    }
                }
            }
            if n == "properties" {
                if let Some(Expr::Variable(v)) = args.first() {
                    if matches!(
                        env.get(v),
                        Some(&Binding::Scalar | &Binding::List | &Binding::Path)
                    ) {
                        return Err(Error::syntax(
                            "InvalidArgumentType: properties() needs a node, relationship, or map",
                        ));
                    }
                }
            }
            if n == "size" {
                match args.first() {
                    Some(Expr::PatternPred { .. }) => {
                        return Err(Error::syntax(
                            "UnexpectedSyntax: size() cannot be applied to a pattern predicate",
                        ));
                    }
                    Some(Expr::Variable(v)) if env.get(v) == Some(&Binding::Path) => {
                        return Err(Error::syntax(
                            "InvalidArgumentType: size() cannot be applied to a path",
                        ));
                    }
                    _ => {}
                }
            }
            let child_pred = n == "exists";
            if is_agg_fn(&n) && args.iter().any(expr_contains_rand) {
                return Err(Error::syntax(
                    "NonConstantExpression: aggregations cannot contain rand()",
                ));
            }
            for (index, a) in args.iter().enumerate() {
                if n == "property_exists" && index == 1 {
                    continue;
                }
                check_expr_ctx(env, is_write_procedure, a, child_pred)?;
            }
        }
        Expr::Property { expr: inner, .. } => {
            if let Expr::Variable(v) = inner.as_ref() {
                if matches!(
                    env.get(v),
                    Some(&Binding::Path | &Binding::Scalar | &Binding::List)
                ) {
                    return Err(Error::syntax(
                        "InvalidArgumentType: property access on a non-map value",
                    ));
                }
            }
            for child in expr.children() {
                check_expr(env, is_write_procedure, child)?;
            }
        }
        Expr::Unary { op, .. } => {
            let child = allow_pattern_pred && matches!(op, UnaryOp::Not);
            for c in expr.children() {
                check_expr_ctx(env, is_write_procedure, c, child)?;
            }
        }
        Expr::Binary { op, left, right } => {
            let child =
                allow_pattern_pred && matches!(op, BinaryOp::And | BinaryOp::Or | BinaryOp::Xor);
            for c in expr.children() {
                check_expr_ctx(env, is_write_procedure, c, child)?;
            }
            if matches!(op, BinaryOp::And | BinaryOp::Or | BinaryOp::Xor)
                && (is_static_non_bool(left) || is_static_non_bool(right))
            {
                let name = match op {
                    BinaryOp::And => "AND",
                    BinaryOp::Or => "OR",
                    BinaryOp::Xor => "XOR",
                    _ => "boolean operator",
                };
                return Err(Error::syntax(format!(
                    "InvalidArgumentType: {name} requires a boolean"
                )));
            }
            if *op == BinaryOp::In {
                if let Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::String(_) =
                    right.as_ref()
                {
                    return Err(Error::syntax("InvalidArgumentType: IN requires a list"));
                }
            }
        }
        Expr::Case {
            operand,
            when_then,
            else_expr,
        } => {
            if let Some(o) = operand {
                check_expr(env, is_write_procedure, o)?;
            }
            for (a, b, is_predicate) in when_then {
                if operand.is_some() && !is_predicate {
                    check_expr(env, is_write_procedure, a)?;
                } else {
                    check_pred(env, is_write_procedure, a)?;
                }
                check_expr_ctx(env, is_write_procedure, b, allow_pattern_pred)?;
            }
            if let Some(e) = else_expr {
                check_expr_ctx(env, is_write_procedure, e, allow_pattern_pred)?;
            }
        }
        Expr::ListComp {
            ident,
            list,
            predicate,
            projection,
        } => {
            check_expr(env, is_write_procedure, list)?;
            let mut inner = env.clone();
            inner.insert(ident.clone(), Binding::Value);
            if let Some(p) = predicate {
                check_pred(&inner, is_write_procedure, p)?;
            }
            check_expr(&inner, is_write_procedure, projection)?;
        }
        // CIP-272: the key/value (map source) or item (list source)
        // bindings scope the predicate and both projection expressions.
        Expr::MapComp {
            key_ident,
            value_ident,
            source,
            predicate,
            key_expr,
            value_expr,
        } => {
            check_expr(env, is_write_procedure, source)?;
            let mut inner = env.clone();
            if let Some(k) = key_ident {
                inner.insert(k.clone(), Binding::Value);
            }
            inner.insert(value_ident.clone(), Binding::Value);
            if let Some(p) = predicate {
                check_pred(&inner, is_write_procedure, p)?;
            }
            check_expr(&inner, is_write_procedure, key_expr)?;
            check_expr(&inner, is_write_procedure, value_expr)?;
        }
        Expr::Quantifier {
            ident,
            list,
            predicate,
            ..
        } => {
            check_expr(env, is_write_procedure, list)?;
            let mut inner = env.clone();
            inner.insert(ident.clone(), Binding::Value);
            check_pred(&inner, is_write_procedure, predicate)?;
        }
        Expr::Reduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
        } => {
            check_expr(env, is_write_procedure, initial)?;
            check_expr(env, is_write_procedure, list)?;
            let mut inner = env.clone();
            inner.insert(accumulator.clone(), Binding::Value);
            inner.insert(ident.clone(), Binding::Value);
            check_expr(&inner, is_write_procedure, expression)?;
        }
        Expr::AllReduce {
            accumulator,
            initial,
            ident,
            list,
            expression,
            predicate,
        } => {
            check_expr(env, is_write_procedure, initial)?;
            check_expr(env, is_write_procedure, list)?;
            let mut inner = env.clone();
            inner.insert(accumulator.clone(), Binding::Value);
            inner.insert(ident.clone(), Binding::Value);
            check_expr(&inner, is_write_procedure, expression)?;
            check_pred(&inner, is_write_procedure, predicate)?;
        }
        Expr::PatternPred { path } => {
            if !allow_pattern_pred {
                return Err(Error::syntax(
                    "UnexpectedSyntax: pattern predicates are only valid in WHERE",
                ));
            }
            check_pattern_pred_vars(env, is_write_procedure, path)?;
            if path.rels.is_empty() {
                return Err(Error::syntax(
                    "InvalidArgumentType: a node pattern is not a valid predicate",
                ));
            }
        }
        Expr::ShortestPath { path, .. } => {
            check_pattern_pred_vars(env, is_write_procedure, path)?;
        }
        Expr::PatternComp {
            path,
            predicate,
            projection,
        } => {
            check_path_exprs(env, is_write_procedure, path)?;
            let mut inner = env.clone();
            if let Some(v) = &path.variable {
                inner.insert(v.clone(), Binding::Path);
            }
            for n in &path.nodes {
                if let Some(v) = &n.variable {
                    inner.insert(v.clone(), Binding::Node);
                }
            }
            for r in &path.rels {
                if let Some(v) = &r.variable {
                    inner.insert(v.clone(), Binding::Rel);
                }
            }
            if let Some(p) = predicate {
                check_pred(&inner, is_write_procedure, p)?;
            }
            check_expr(&inner, is_write_procedure, projection)?;
        }
        Expr::MapProjection {
            expr: target,
            items,
        } => {
            check_expr(env, is_write_procedure, target)?;
            for item in items {
                match item {
                    MapProjectionItem::KeyValue { value, .. } => {
                        check_expr(env, is_write_procedure, value)?
                    }
                    MapProjectionItem::Property { .. } => {
                        // The `.key` shorthand reads a property of the
                        // projection target: same check as `Expr::Property`.
                        if let Expr::Variable(v) = target.as_ref() {
                            if matches!(
                                env.get(v),
                                Some(&Binding::Path | &Binding::Scalar | &Binding::List)
                            ) {
                                return Err(Error::syntax(
                                    "InvalidArgumentType: property access on a non-map value",
                                ));
                            }
                        }
                    }
                    MapProjectionItem::Variable { name } => {
                        if !env.contains_key(name) {
                            return Err(Error::syntax(format!("UndefinedVariable: `{name}`")));
                        }
                    }
                    MapProjectionItem::AllProperties => {}
                }
            }
        }
        Expr::ExistsSubquery { query }
        | Expr::CountSubquery { query }
        | Expr::CollectSubquery { query } => {
            let imported: Vec<String> = env.keys().cloned().collect();
            let scoped_query = query_with_persistent_imports(query, &imported);
            if query
                .parts
                .iter()
                .any(|part| matches!(part.clauses.last(), Some(Clause::Finish)))
            {
                return Err(Error::syntax(
                    "InvalidClauseComposition: FINISH is not allowed in a subquery expression",
                ));
            }
            for part in &scoped_query.parts {
                for clause in &part.clauses {
                    let introduced = match clause {
                        Clause::Unwind { alias, .. }
                        | Clause::LoadCsv {
                            variable: alias, ..
                        } => Some(alias),
                        _ => None,
                    };
                    if introduced.is_some_and(|name| env.contains_key(name)) {
                        return Err(Error::syntax(
                            "VariableAlreadyBound: inner variable shadows outer scope",
                        ));
                    }
                }
                if part
                    .clauses
                    .iter()
                    .any(|clause| clause_is_write(clause, is_write_procedure))
                {
                    return Err(Error::syntax(
                        "InvalidClauseComposition: writes are not allowed in subquery expressions",
                    ));
                }
                validate_part_with_env(part, env.clone(), is_write_procedure)?;
            }
            if matches!(expr, Expr::CollectSubquery { .. }) {
                let names = query_output_names_with_seed(&scoped_query, imported)?;
                if names.len() != 1 {
                    return Err(Error::syntax(
                        "CollectExpressionMustReturnSingleColumn: COLLECT { } requires one column",
                    ));
                }
            }
        }
        // ADR-0175: a float-literal list index or slice bound fails at plan
        // time with the same `InvalidArgumentType` kind the site raises at
        // runtime (Neo4j's 22N27/22NB1 static gate fires at the same phase);
        // non-literal floats are handled at runtime.
        Expr::Subscript { expr: inner, index } => {
            reject_float_literal(index, "list index must be an integer")?;
            check_expr(env, is_write_procedure, inner)?;
            check_expr(env, is_write_procedure, index)?;
        }
        Expr::Slice { start, end, .. } => {
            if let Some(s) = start {
                reject_float_literal(s, "slice start must be an integer")?;
            }
            if let Some(e) = end {
                reject_float_literal(e, "slice end must be an integer")?;
            }
            for child in expr.children() {
                check_expr(env, is_write_procedure, child)?;
            }
        }
        // Every other variant (literals, parameters, label predicates,
        // type checks, lists, maps, distance-metric literals, ...) has no
        // bespoke validation: just check each child.
        other => {
            for child in other.children() {
                check_expr(env, is_write_procedure, child)?;
            }
        }
    }
    Ok(())
}
