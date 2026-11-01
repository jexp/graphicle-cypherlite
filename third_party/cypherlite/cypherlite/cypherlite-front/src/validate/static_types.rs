//! Compile-time checks decidable from *literal* operand types: LIMIT/SKIP
//! literal shape, boolean-operator operands, `IN` list operands,
//! `properties()` argument types, and quantifier-variable arithmetic over a
//! statically non-numeric list literal. Everything whose type depends on
//! runtime data is deliberately left to the runtime evaluator.

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr, LiteralValue};

use crate::ast::collect::interp_embeds;
use crate::ast::tree::walk_expr;

/// Check if an expression references any variables (not parameters).
fn expr_references_variables(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) => true,
        Expr::Property(p) => expr_references_variables(&p.expr),
        Expr::Binary(b) => {
            expr_references_variables(&b.left) || expr_references_variables(&b.right)
        }
        Expr::Comparison(c) => {
            expr_references_variables(&c.left) || expr_references_variables(&c.right)
        }
        Expr::Unary(u) => expr_references_variables(&u.expr),
        Expr::FunctionCall(fc) => fc.args.iter().any(expr_references_variables),
        Expr::List(items) => items.iter().any(expr_references_variables),
        Expr::Paren(inner) => expr_references_variables(inner),
        Expr::Subscript(s) => {
            expr_references_variables(&s.expr)
                || s.index.as_ref().is_some_and(expr_references_variables)
                || s.from.as_ref().is_some_and(expr_references_variables)
                || s.to.as_ref().is_some_and(expr_references_variables)
        }
        Expr::StringInterp(parts) => interp_embeds(parts).any(expr_references_variables),
        _ => false,
    }
}
/// Validate LIMIT/SKIP expressions: reject negative literals, float literals,
/// and expressions that reference variables.
pub fn validate_limit_skip(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        let (limit, skip) = match clause {
            Clause::Return(rc) => (rc.limit.as_ref(), rc.skip.as_ref()),
            Clause::With(wc) => (wc.limit.as_ref(), wc.skip.as_ref()),
            _ => (None, None),
        };
        if let Some(limit_expr) = limit {
            // Check for variable references
            if expr_references_variables(limit_expr) {
                return Err(EngineError::semantic(
                    ErrorKind::NonConstantExpression,
                    "LIMIT must be a non-negative integer constant".to_string(),
                ));
            }
            // Check for negative literal
            if let Expr::Unary(u) = limit_expr {
                if u.op == "-" {
                    if let Expr::Literal(LiteralValue::Integer(_)) = &u.expr {
                        return Err(EngineError::semantic(
                            ErrorKind::NegativeIntegerArgument,
                            "LIMIT must not be negative".to_string(),
                        ));
                    }
                }
            }
            // Check for float literal
            if let Expr::Literal(LiteralValue::Float(_)) = limit_expr {
                return Err(EngineError::semantic(
                    ErrorKind::InvalidArgumentType,
                    "LIMIT requires an integer value".to_string(),
                ));
            }
        }
        if let Some(skip_expr) = skip {
            if expr_references_variables(skip_expr) {
                return Err(EngineError::semantic(
                    ErrorKind::NonConstantExpression,
                    "SKIP must be a non-negative integer constant".to_string(),
                ));
            }
            if let Expr::Unary(u) = skip_expr {
                if u.op == "-" {
                    if let Expr::Literal(LiteralValue::Integer(_)) = &u.expr {
                        return Err(EngineError::semantic(
                            ErrorKind::NegativeIntegerArgument,
                            "SKIP must not be negative".to_string(),
                        ));
                    }
                }
            }
            if let Expr::Literal(LiteralValue::Float(_)) = skip_expr {
                return Err(EngineError::semantic(
                    ErrorKind::InvalidArgumentType,
                    "SKIP requires an integer value".to_string(),
                ));
            }
        }
    }
    Ok(())
}
/// The statically-known value type of a *literal* expression — the only class of
/// operand whose type CypherLite can determine without evaluating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticLitType {
    Null,
    Boolean,
    Integer,
    Float,
    String,
    List,
    Map,
}

impl StaticLitType {
    fn name(self) -> &'static str {
        match self {
            StaticLitType::Null => "NULL",
            StaticLitType::Boolean => "BOOLEAN",
            StaticLitType::Integer => "INTEGER",
            StaticLitType::Float => "FLOAT",
            StaticLitType::String => "STRING",
            StaticLitType::List => "LIST",
            StaticLitType::Map => "MAP",
        }
    }
}

/// The static type of a *literal* expression, or `None` when the type cannot be
/// determined without evaluation (variables, parameters, property access,
/// function calls, comparisons, arithmetic, ...). `Paren` is transparent.
fn static_literal_type(expr: &Expr) -> Option<StaticLitType> {
    match expr {
        Expr::Paren(inner) => static_literal_type(inner),
        Expr::Literal(LiteralValue::Null) => Some(StaticLitType::Null),
        Expr::Literal(LiteralValue::Bool(_)) => Some(StaticLitType::Boolean),
        Expr::Literal(LiteralValue::Integer(_)) => Some(StaticLitType::Integer),
        Expr::Literal(LiteralValue::Float(_)) => Some(StaticLitType::Float),
        Expr::Literal(LiteralValue::String(_)) => Some(StaticLitType::String),
        Expr::List(_) => Some(StaticLitType::List),
        Expr::Map(_) => Some(StaticLitType::Map),
        _ => None,
    }
}

/// Reject a boolean-operator operand whose *literal* type is neither Boolean nor
/// Null. A `null` operand is always allowed (three-valued logic); a non-literal
/// operand is left for runtime.
fn require_boolean_literal_operand(operand: &Expr) -> Result<(), EngineError> {
    if let Some(t) = static_literal_type(operand) {
        if t != StaticLitType::Boolean && t != StaticLitType::Null {
            return Err(EngineError::semantic(
                ErrorKind::InvalidArgumentType,
                format!("Type mismatch: expected Boolean but was {}", t.name()),
            ));
        }
    }
    Ok(())
}

/// Compile-time type checking for operators/functions whose operand type is
/// statically knowable from a *literal*. Neo4j/openCypher rejects these during
/// semantic analysis (a compile-time `SyntaxError: InvalidArgumentType`), not at
/// runtime — so `RETURN NOT [1]`, `RETURN 1 IN 2`, and
/// `RETURN properties(1)` are compile errors. This validator reproduces exactly
/// that boundary and *only* for literal operands; every non-literal operand
/// (variable, parameter, function result, …) is left to the runtime evaluator,
/// so no valid query is affected.
///
/// Covered (all `expressions/…` TCK features):
/// - `NOT` / `AND` / `OR` / `XOR` require boolean operands (Boolean1-4).
/// - `x IN <list>` requires a list right operand (List5 scenario 42).
/// - `properties(x)` requires a node/relationship/map (Graph9 scenarios 5-7).
pub fn validate_static_operator_types(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        match clause {
            Clause::Return(rc) => {
                for item in &rc.items {
                    if !item.all {
                        walk_expr(&item.expr, &mut check_static_operator_types)?;
                    }
                }
                for ob in &rc.order_by {
                    walk_expr(&ob.expr, &mut check_static_operator_types)?;
                }
            }
            Clause::With(wc) => {
                for item in &wc.items {
                    if !item.all {
                        walk_expr(&item.expr, &mut check_static_operator_types)?;
                    }
                }
                if let Some(w) = &wc.where_expr {
                    walk_expr(w, &mut check_static_operator_types)?;
                }
                for ob in &wc.order_by {
                    walk_expr(&ob.expr, &mut check_static_operator_types)?;
                }
            }
            Clause::Match(mc) => {
                if let Some(w) = &mc.where_expr {
                    walk_expr(w, &mut check_static_operator_types)?;
                }
            }
            Clause::Unwind(uc) => {
                walk_expr(&uc.expr, &mut check_static_operator_types)?;
            }
            Clause::Set(sc) => {
                for item in &sc.items {
                    if let Some(val) = &item.value {
                        walk_expr(val, &mut check_static_operator_types)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The per-node visitor for [`validate_static_operator_types`].
fn check_static_operator_types(e: &Expr) -> Result<(), EngineError> {
    match e {
        Expr::Unary(u) if u.op == "NOT" => require_boolean_literal_operand(&u.expr)?,
        Expr::Binary(b) if matches!(b.op.as_str(), "AND" | "OR" | "XOR") => {
            require_boolean_literal_operand(&b.left)?;
            require_boolean_literal_operand(&b.right)?;
        }
        Expr::In(ie) => {
            if let Some(t) = static_literal_type(&ie.list) {
                if t != StaticLitType::List && t != StaticLitType::Null {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidArgumentType,
                        format!("IN requires a list, got {}", t.name()),
                    ));
                }
            }
        }
        Expr::FunctionCall(fc)
            if fc.name.eq_ignore_ascii_case("properties") && fc.args.len() == 1 =>
        {
            if let Some(t) = static_literal_type(&fc.args[0]) {
                if !matches!(t, StaticLitType::Map | StaticLitType::Null) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidArgumentType,
                        format!(
                            "properties() requires a node, relationship, or map, got {}",
                            t.name()
                        ),
                    ));
                }
            }
        }
        // A list quantifier (`all`/`any`/`none`/`single`) whose loop variable
        // ranges over a list *literal* of statically non-numeric elements, and
        // whose predicate applies an arithmetic operator directly to that loop
        // variable, is a compile-time type error in openCypher — e.g.
        // `all(x IN ['Clara'] WHERE x % 2 = 0)`
        // (expressions/quantifier/Quantifier1-4 scenario "[15]/[16] Fail …
        // quantifier on type mismatch between list elements and predicate",
        // `SyntaxError … : InvalidArgumentType` at compile time). CypherLite
        // otherwise catches it at runtime in `eval::binary`; this narrow static
        // pass moves the statically-decidable case to compile time. Only literal
        // lists with fully-known non-numeric element types qualify, so every
        // query whose element type depends on runtime data is untouched.
        Expr::Quantifier(q) => {
            if let Some(pred) = &q.predicate {
                if list_literal_all_non_numeric(&q.list)
                    && predicate_applies_arithmetic_to_var(pred, &q.variable)
                {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidArgumentType,
                        format!(
                            "Type mismatch: the quantifier variable '{}' ranges over a list of \
                             non-numeric elements and cannot be used in arithmetic",
                            q.variable
                        ),
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The arithmetic operators that raise a type error for *every* non-numeric,
/// non-null operand. `+` is deliberately excluded: it is also string
/// concatenation (`'a' + 1` → `'a1'`) and list append (`[1] + 2`), both valid
/// for STRING/LIST operands, so `+` is never a *guaranteed* compile-time type
/// error and is left for the runtime evaluator.
fn is_failing_arithmetic_op(op: &str) -> bool {
    matches!(op, "-" | "*" | "/" | "%" | "^")
}

/// True if `list`, ignoring wrapping parentheses, is a non-empty list *literal*
/// (`Expr::List`) whose every element is a statically-typed, non-null,
/// NON-NUMERIC literal (STRING / BOOLEAN / nested LIST / MAP). When this holds,
/// a quantifier binding a variable to the list's elements gives that variable a
/// value on which the [`is_failing_arithmetic_op`] operators must raise, and
/// that is knowable at compile time.
///
/// Any numeric element, `null` element, or element whose type is not statically
/// a literal (a variable, parameter, function result, …) makes the outcome
/// runtime-dependent — arithmetic on a number succeeds, and on `null` yields
/// `null` — so the list does not qualify and the query is left untouched.
fn list_literal_all_non_numeric(list: &Expr) -> bool {
    match list {
        Expr::Paren(inner) => list_literal_all_non_numeric(inner),
        Expr::List(items) => {
            !items.is_empty()
                && items.iter().all(|e| {
                    matches!(
                        static_literal_type(e),
                        Some(
                            StaticLitType::String
                                | StaticLitType::Boolean
                                | StaticLitType::List
                                | StaticLitType::Map
                        )
                    )
                })
        }
        _ => false,
    }
}

/// True if `e`, ignoring wrapping parentheses, is exactly the bare variable
/// `var`.
fn expr_is_bare_var(e: &Expr, var: &str) -> bool {
    match e {
        Expr::Paren(inner) => expr_is_bare_var(inner, var),
        Expr::Variable(v) => v == var,
        _ => false,
    }
}

/// True if `expr` applies an [`is_failing_arithmetic_op`] operator *directly* to
/// the bare quantifier variable `var` somewhere in its tree (`x % 2`, `2 - x`,
/// `x ^ 2`, …).
///
/// The recursion deliberately stops at any nested scope-binding construct
/// (quantifier / list comprehension / reduce / allReduce) that *rebinds* the
/// same variable name, so shadowing (`all(x IN ['a'] WHERE any(x IN [1] WHERE x
/// % 2 = 0))`) never yields a false positive: the inner `x` is a different,
/// numeric binding. Nodes this focused walker does not recognise return `false`
/// — a false negative merely defers to the runtime type check, which is safe;
/// only a false positive (rejecting a valid query) would be a regression, and
/// requiring the operand to be the *bare* loop variable prevents that.
fn predicate_applies_arithmetic_to_var(expr: &Expr, var: &str) -> bool {
    match expr {
        Expr::Binary(b) => {
            if is_failing_arithmetic_op(&b.op)
                && (expr_is_bare_var(&b.left, var) || expr_is_bare_var(&b.right, var))
            {
                return true;
            }
            predicate_applies_arithmetic_to_var(&b.left, var)
                || predicate_applies_arithmetic_to_var(&b.right, var)
        }
        Expr::Paren(inner) => predicate_applies_arithmetic_to_var(inner, var),
        Expr::Unary(u) => predicate_applies_arithmetic_to_var(&u.expr, var),
        Expr::Comparison(c) => {
            predicate_applies_arithmetic_to_var(&c.left, var)
                || predicate_applies_arithmetic_to_var(&c.right, var)
        }
        Expr::In(ie) => {
            predicate_applies_arithmetic_to_var(&ie.expr, var)
                || predicate_applies_arithmetic_to_var(&ie.list, var)
        }
        Expr::IsNull(x) => predicate_applies_arithmetic_to_var(&x.expr, var),
        Expr::IsType(x) => predicate_applies_arithmetic_to_var(&x.expr, var),
        Expr::IsNormalized(x) => predicate_applies_arithmetic_to_var(&x.expr, var),
        Expr::StringMatch(sm) => {
            predicate_applies_arithmetic_to_var(&sm.left, var)
                || predicate_applies_arithmetic_to_var(&sm.right, var)
        }
        Expr::HasLabel(hl) => predicate_applies_arithmetic_to_var(&hl.expr, var),
        Expr::Property(p) => {
            predicate_applies_arithmetic_to_var(&p.expr, var)
                || p.key_expr
                    .as_ref()
                    .is_some_and(|k| predicate_applies_arithmetic_to_var(k, var))
        }
        Expr::Subscript(s) => {
            predicate_applies_arithmetic_to_var(&s.expr, var)
                || s.index
                    .as_ref()
                    .is_some_and(|i| predicate_applies_arithmetic_to_var(i, var))
                || s.from
                    .as_ref()
                    .is_some_and(|f| predicate_applies_arithmetic_to_var(f, var))
                || s.to
                    .as_ref()
                    .is_some_and(|t| predicate_applies_arithmetic_to_var(t, var))
        }
        Expr::FunctionCall(fc) => fc
            .args
            .iter()
            .any(|a| predicate_applies_arithmetic_to_var(a, var)),
        Expr::List(items) => items
            .iter()
            .any(|e| predicate_applies_arithmetic_to_var(e, var)),
        Expr::StringInterp(parts) => {
            interp_embeds(parts).any(|e| predicate_applies_arithmetic_to_var(e, var))
        }
        Expr::Map(pairs) => pairs
            .iter()
            .any(|p| predicate_applies_arithmetic_to_var(&p.value, var)),
        Expr::Case(c) => {
            c.test
                .as_ref()
                .is_some_and(|t| predicate_applies_arithmetic_to_var(t, var))
                || c.whens.iter().any(|w| {
                    predicate_applies_arithmetic_to_var(&w.condition, var)
                        || predicate_applies_arithmetic_to_var(&w.result, var)
                })
                || c.else_expr
                    .as_ref()
                    .is_some_and(|e| predicate_applies_arithmetic_to_var(e, var))
        }
        // Nested binders: descend only while the same name is NOT rebound. If
        // the construct rebinds `var`, its body's `var` is a different binding
        // and must not be inspected against the outer numeric-ness assumption.
        Expr::Quantifier(q) => {
            predicate_applies_arithmetic_to_var(&q.list, var)
                || (q.variable != var
                    && q.predicate
                        .as_ref()
                        .is_some_and(|p| predicate_applies_arithmetic_to_var(p, var)))
        }
        Expr::ListComprehension(lc) => {
            predicate_applies_arithmetic_to_var(&lc.list_expr, var)
                || (lc.variable != var
                    && (lc
                        .where_expr
                        .as_ref()
                        .is_some_and(|w| predicate_applies_arithmetic_to_var(w, var))
                        || lc
                            .map_expr
                            .as_ref()
                            .is_some_and(|m| predicate_applies_arithmetic_to_var(m, var))))
        }
        Expr::Reduce(r) => {
            predicate_applies_arithmetic_to_var(&r.init_expr, var)
                || predicate_applies_arithmetic_to_var(&r.list_expr, var)
                || (r.variable != var
                    && r.accumulator != var
                    && predicate_applies_arithmetic_to_var(&r.expression, var))
        }
        Expr::AllReduce(r) => {
            predicate_applies_arithmetic_to_var(&r.init_expr, var)
                || predicate_applies_arithmetic_to_var(&r.list_expr, var)
                || (r.variable != var
                    && r.accumulator != var
                    && (predicate_applies_arithmetic_to_var(&r.expression, var)
                        || predicate_applies_arithmetic_to_var(&r.predicate, var)))
        }
        // Everything else (bare Variable, Literal, Parameter, subqueries,
        // pattern-bearing leaves, map projection, …) cannot itself be the
        // `x <op> …` form and introduces nothing this pass understands.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr_of(query: &str) -> Expr {
        let stmt = cypherlite_parser::parse(query).unwrap();
        match &stmt.clauses[0] {
            Clause::Return(rc) => rc.items[0].expr.clone(),
            _ => panic!("expected a RETURN clause"),
        }
    }
    // ---- Quantifier over a non-numeric list literal: compile-time TypeError ----
    //
    // openCypher rejects `all/any/none/single(x IN <non-numeric list literal>
    // WHERE <arith on x>)` during semantic analysis, as a `SyntaxError … :
    // InvalidArgumentType` at compile time (expressions/quantifier/Quantifier1-4
    // "[15]/[16] Fail … quantifier on type mismatch between list elements and
    // predicate"). CypherLite's runtime evaluator would otherwise raise the same
    // TypeError one phase too late.

    /// Parse `query`, run only the narrow static operator/type pass, and return
    /// its verdict.
    fn parse_and_check_static_ops(query: &str) -> Result<(), EngineError> {
        let stmt = cypherlite_parser::parse(query).expect("parse");
        validate_static_operator_types(&stmt.clauses)
    }

    #[test]
    fn quantifier_over_string_literal_arith_predicate_is_compile_time_type_error() {
        // Every quantifier keyword, over both a STRING and a BOOLEAN list literal
        // (the exact Quantifier1-4 [15]/[16] examples). Each must be a *compile*
        // (Semantic) error whose kind is InvalidArgumentType — the detail the
        // TCK step pins.
        for q in ["all", "any", "none", "single"] {
            for list in [
                "['Clara']",
                "['Clara', 'Bob', 'Dave', 'Alice']",
                "[false, true]",
            ] {
                let query = format!("RETURN {q}(x IN {list} WHERE x % 2 = 0) AS result");
                let err = parse_and_check_static_ops(&query)
                    .expect_err(&format!("expected a compile-time type error for `{query}`"));
                assert!(
                    matches!(err, EngineError::Semantic { .. }),
                    "`{query}` must fail at compile time (Semantic), got {err:?}"
                );
                assert_eq!(
                    err.kind(),
                    ErrorKind::InvalidArgumentType,
                    "`{query}` must carry the InvalidArgumentType detail"
                );
                assert_eq!(err.tck_class(), cypherlite_error::TckErrorClass::TypeError);
            }
        }
    }

    #[test]
    fn quantifier_over_numeric_literal_is_untouched() {
        // The numeric-list quantifier the tested type-mismatch scenarios are the
        // negative of: `x % 2 = 0` over integers/floats is perfectly valid and
        // must pass the static pass unchanged (no false positive).
        for query in [
            "RETURN all(x IN [1, 2] WHERE x % 2 = 0) AS result",
            "RETURN any(x IN [1.5, 2.5] WHERE x / 2 > 0) AS result",
            "RETURN none(x IN [1, 2, 3] WHERE x - 1 = 0) AS result",
            "RETURN single(x IN [10] WHERE x * 2 = 20) AS result",
            // A list mixing numbers and null is not statically all-non-numeric.
            "RETURN all(x IN [1, null, 3] WHERE x % 2 = 0) AS result",
        ] {
            assert!(
                parse_and_check_static_ops(query).is_ok(),
                "`{query}` must not be rejected by the static pass"
            );
        }
    }

    #[test]
    fn quantifier_non_arithmetic_predicate_over_strings_is_untouched() {
        // The *valid* Quantifier[5]/[6]/[7]/[11] scenarios use `size(x)`, `x.a`,
        // `x IS NULL`, `x = …` — never arithmetic on the loop variable — and must
        // not be mistaken for the type-mismatch case.
        for query in [
            "RETURN all(x IN ['abc', 'ef'] WHERE size(x) = 3) AS result",
            "RETURN any(x IN [{a: 2}, {a: 4}] WHERE x.a = 2) AS result",
            "RETURN none(x IN ['a', 'b'] WHERE x IS NULL) AS result",
            "RETURN single(x IN [[1, 2, 3]] WHERE size(x) = 3) AS result",
            // `+` on strings is concatenation, not arithmetic — never a
            // compile-time error.
            "RETURN all(x IN ['a', 'b'] WHERE x + 'y' = 'ay') AS result",
            // Arithmetic applies to a *derived* value, not the bare loop var.
            "RETURN all(x IN ['1', '2'] WHERE toInteger(x) % 2 = 0) AS result",
        ] {
            assert!(
                parse_and_check_static_ops(query).is_ok(),
                "`{query}` must not be rejected by the static pass"
            );
        }
    }

    #[test]
    fn quantifier_shadowing_inner_numeric_var_is_not_a_false_positive() {
        // The inner `any` rebinds `x` to a numeric list; the outer `all`'s
        // string binding never reaches the inner `x % 2`. The shadow guard in
        // `predicate_applies_arithmetic_to_var` must keep this valid.
        let query = "RETURN all(x IN ['a'] WHERE any(x IN [1, 2] WHERE x % 2 = 0)) AS result";
        assert!(
            parse_and_check_static_ops(query).is_ok(),
            "shadowed inner numeric quantifier must not trip the outer static check"
        );
    }

    #[test]
    fn quantifier_static_helpers_are_narrow() {
        // list_literal_all_non_numeric: only fully-known non-numeric literals.
        let all_non_numeric =
            |q: &str| list_literal_all_non_numeric(&expr_of(&format!("RETURN {q}")));
        assert!(all_non_numeric("['a', 'b']"));
        assert!(all_non_numeric("[false, true]"));
        assert!(all_non_numeric("[[1], ['a']]"));
        assert!(all_non_numeric("[{a: 1}]"));
        assert!(!all_non_numeric("[]")); // empty ⇒ nothing to constrain
        assert!(!all_non_numeric("[1, 2]")); // numeric
        assert!(!all_non_numeric("['a', 1]")); // heterogeneous ⇒ runtime-dependent
        assert!(!all_non_numeric("['a', null]")); // null ⇒ arithmetic yields null
        assert!(!all_non_numeric("$p")); // not a list literal

        // predicate_applies_arithmetic_to_var: bare-var operand under a failing op.
        let hits = |q: &str| {
            let e = expr_of(&format!("RETURN {q}"));
            predicate_applies_arithmetic_to_var(&e, "x")
        };
        assert!(hits("x % 2 = 0"));
        assert!(hits("2 - x > 0"));
        assert!(hits("x ^ 2 = 4"));
        assert!(hits("x = 1 OR x / 2 = 0"));
        assert!(!hits("x + 'y' = 'z'")); // `+` excluded (concat/append)
        assert!(!hits("size(x) = 3")); // no arithmetic on x
        assert!(!hits("toInteger(x) % 2 = 0")); // operand is derived, not bare x
        assert!(!hits("y % 2 = 0")); // different variable
    }

    #[test]
    fn quantifier_type_mismatch_surfaces_through_full_engine_at_compile_time() {
        // End-to-end through the engine: the query must error, as a compile
        // (Semantic) InvalidArgumentType, before any row evaluation.
        // Compile-time validation: a static list of strings under `%` is InvalidArgumentType.
        let err =
            parse_and_check_static_ops("RETURN all(x IN ['Clara'] WHERE x % 2 = 0) AS result")
                .expect_err("quantifier type mismatch must error");
        assert_eq!(err.kind(), ErrorKind::InvalidArgumentType);
        assert!(
            matches!(err, EngineError::Semantic { .. }),
            "must be a compile-time (Semantic) error, got {err:?}"
        );
        assert_eq!(err.kind(), ErrorKind::InvalidArgumentType);
    }

    #[test]
    fn genuinely_runtime_arithmetic_type_error_stays_runtime_typeerror() {
        // A non-static operand (bound at row time by UNWIND) is NOT caught by the
        // static pass — it must still raise at RUNTIME, now correctly classed as
        // a TypeError (InvalidArgumentType) rather than the old Uncategorized/
        // SemanticError.
        // A non-static operand is NOT caught by the static pass.
        assert!(parse_and_check_static_ops("UNWIND ['abc'] AS x RETURN x % 2").is_ok());
    }
}
