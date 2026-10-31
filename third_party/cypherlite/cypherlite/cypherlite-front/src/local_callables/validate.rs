//! The CIP-231 define-time validators: the per-definition rules
//! decidable before any call site exists (the CIP's authored 42I77/42NAF/
//! 42NAG/42NAM/42NAI/42NAH/42N21-shaped errors and the
//! REFERENCE-OUTER-DEFINITION-SCOPE-NO default-expression rule). Each
//! rides the shared whole-tree walkers in [`crate::validate`], so a rule
//! violation cannot hide inside a nested body.

use cypherlite_core::{Error, Result};
use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{
    Clause, Expr, LocalCallableKind, LocalDefinition, LocalDefinitionBody, LocalOutput,
    LocalOutputField, ReturnClause, Statement,
};

use super::walker_error;
use crate::ast::tree::{walk_expr, walk_statement_tree};

/// The body of a definition, validated: the CIP-231 authored define-time
/// rules that are decidable before any call site exists. A procedure
/// definition always has a braced query body (the parser rejects an `=`
/// procedure body at parse time), so the expression arm never reaches
/// here.
pub(super) fn validate_definition(definition: &LocalDefinition) -> Result<()> {
    let name = &definition.name;
    for param in &definition.params {
        if let Some(default) = &param.default {
            check_default_is_constant(default)?;
        }
    }
    match definition.kind {
        LocalCallableKind::Procedure => {
            let LocalDefinitionBody::Query(body) = &definition.body else {
                unreachable!("the parser guarantees a procedure body is a query body")
            };
            check_no_use(body, "procedure")?;
            check_no_next(body)?;
            check_emitting_returns_aliased(body)?;
            check_union_arms_export_head_names(body)?;
            if let Some(LocalOutput::Fields(fields)) = &definition.output {
                check_output_signature(body, name, fields)?;
            }
        }
        LocalCallableKind::Function => {
            if let LocalDefinitionBody::Query(body) = &definition.body {
                check_no_use(body, "function")?;
                check_no_next(body)?;
                check_no_updates(body, name)?;
                check_scalar_shape(body, name)?;
            }
        }
    }
    Ok(())
}

/// A default expression sees the unit record only: no variable references
/// anywhere in it (CIP-231 `<callable input signature>` SR 3). The error
/// is raised at the detection point -- no defer-through-flag detour.
fn check_default_is_constant(default: &Expr) -> Result<()> {
    let mut visit = |expr: &Expr| -> std::result::Result<(), EngineError> {
        match expr {
            Expr::Variable(name) => Err(rejection(format!(
                "default values of local callable parameters may not reference variables \
                 (found {name})"
            ))),
            _ => Ok(()),
        }
    };
    walk_expr(default, &mut visit).map_err(walker_error)
}

/// The one whole-tree rejection helper: walks `body` with the shared
/// statement-tree walker and raises `message` from the detection point
/// when `clause_offended` (a clause) or `statement_offended` (a statement
/// chain link) fires. The CIP-231 whole-body rules below are its
/// predicates.
fn reject_if(
    body: &Statement,
    message: &str,
    clause_offended: impl Fn(&Clause) -> bool,
    statement_offended: impl Fn(&crate::ast::stmt_ref::StmtRef<'_>) -> bool,
) -> Result<()> {
    let mut clause_visitor = |clause: &Clause| -> std::result::Result<(), EngineError> {
        if clause_offended(clause) {
            Err(rejection(message.to_string()))
        } else {
            Ok(())
        }
    };
    let mut stmt_visitor =
        |stmt: crate::ast::stmt_ref::StmtRef<'_>| -> std::result::Result<(), EngineError> {
            if statement_offended(&stmt) {
                Err(rejection(message.to_string()))
            } else {
                Ok(())
            }
        };
    walk_statement_tree(
        body,
        &mut stmt_visitor,
        &mut clause_visitor,
        &mut |_| Ok(()),
    )
    .map_err(walker_error)
}

/// The syntax-class rejection the whole-tree helpers raise, rendered
/// through the front's `Error::syntax` channel by their `map_err`.
fn rejection(message: String) -> EngineError {
    EngineError::semantic(ErrorKind::SyntaxError, message)
}

/// The 42NAF/42NAG rule: no `USE` clause anywhere inside a local callable
/// body (including nested subqueries).
fn check_no_use(body: &Statement, kind: &str) -> Result<()> {
    reject_if(
        body,
        &format!("USE clause is not supported in local {kind} definitions."),
        |clause| matches!(clause, Clause::Use(_)),
        |_| false,
    )
}

/// NEXT stages inside a local callable body are not supported: the
/// parameters bind once at the body's head, so a later stage could not
/// see them (NOTES.md records the deferral).
fn check_no_next(body: &Statement) -> Result<()> {
    reject_if(
        body,
        "NEXT stages are not supported in local callable bodies.",
        |_| false,
        |stmt| stmt.next().is_some(),
    )
}

/// A function's query body shall not contain updates (CIP-231).
fn check_no_updates(body: &Statement, name: &str) -> Result<()> {
    reject_if(
        body,
        &format!("Updates are not supported in the query body of local function {name}()."),
        |clause| {
            matches!(
                clause,
                Clause::Create(_)
                    | Clause::Merge(_)
                    | Clause::Delete(_)
                    | Clause::Set(_)
                    | Clause::Remove(_)
                    | Clause::LoadCSV(_)
                    | Clause::Foreach(_)
            )
        },
        |_| false,
    )
}

/// Each UNION-chain link of `body`'s statement chain (the head first,
/// then each arm): the one enumeration of "every final projection site"
/// the chain-walking checks below share.
pub(super) fn chain_links(body: &Statement) -> Vec<&Statement> {
    let mut links = Vec::new();
    let mut stmt = body;
    loop {
        links.push(stmt);
        match &stmt.union {
            Some(union) => stmt = &union.statement,
            None => return links,
        }
    }
}

/// Every UNION arm of a procedure body exports the head arm's return
/// column names (the define-time twin of the engine-level
/// `validate_union_columns`, which runs in `lower_statement` -- after
/// the CIP-231 rewrite's per-arm yield selection): an arm that diverges
/// would leave the per-arm selection with nothing to find.
fn check_union_arms_export_head_names(body: &Statement) -> Result<()> {
    let links = chain_links(body);
    // Fallible name derivation: a link without a terminal RETURN (the
    // FINISH shape) or with an unnameable item is skipped, not panicked
    // on -- the aliasing check owns the unnameable rejection.
    // chain_links always pushes the head first, so the head's names are
    // the body's own -- no identity Option layer.
    let Some(head_names) = terminal_names(body) else {
        // The emitting-RETURN shape is check_emitting_returns_aliased's
        // and the call site's own gate (a FINISH body ends without one).
        return Ok(());
    };
    for arm in &links[1..] {
        let Some(arm_names) = terminal_names(arm) else {
            continue;
        };
        if arm_names != head_names {
            // The engine-level twin's contract: the same semantic error
            // class and message `validate_union_columns` raises, so the
            // violation surfaces identically whether or not it sits in a
            // procedure body.
            return Err(Error::semantic(
                "all UNION branches must have the same column names",
            ));
        }
    }
    Ok(())
}

/// The terminal RETURN clause of a chain link, when it ends in one -- the
/// one extraction every terminal-projection check in this module (and the
/// rewrite's probes, via thin variants) consults.
pub(super) fn terminal_return(stmt: &Statement) -> Option<&ReturnClause> {
    match stmt.clauses.last() {
        Some(Clause::Return(ret)) => Some(ret),
        _ => None,
    }
}

/// The terminal RETURN's items' effective names, in order -- `None` when
/// the link has no terminal RETURN or any item is unnameable: the one
/// derivation both the UNION-arm check and `body_export_names` consult.
pub(super) fn terminal_names(stmt: &Statement) -> Option<Vec<String>> {
    terminal_return(stmt).and_then(|ret| {
        ret.items
            .iter()
            .map(crate::ast::collect::effective_name)
            .collect::<Option<Vec<String>>>()
    })
}

/// Every emitting RETURN of a procedure body shall be aliased (42N21):
/// each UNION arm's terminal projection, and a conditional body's arm
/// projections, recursively.
fn check_emitting_returns_aliased(body: &Statement) -> Result<()> {
    for link in chain_links(body) {
        if let Some(ret) = terminal_return(link) {
            check_items_aliased(ret)?;
        } else if let Some(Clause::Conditional(conditional)) = link.clauses.last() {
            for branch in &conditional.branches {
                check_emitting_returns_aliased(&branch.body)?;
            }
            if let Some(else_body) = &conditional.else_body {
                check_emitting_returns_aliased(else_body)?;
            }
        }
    }
    Ok(())
}

fn check_items_aliased(ret: &ReturnClause) -> Result<()> {
    for item in &ret.items {
        let aliased = item.alias.is_some() || matches!(item.expr, Expr::Variable(_));
        if item.all || !aliased {
            return Err(Error::syntax(
                "Expression in DEFINE PROCEDURE ... { RETURN ... } must be aliased (use AS).",
            ));
        }
    }
    Ok(())
}

/// The 42NAI/42NAH rules: the body's emitted columns and the declared
/// output signature must name the same fields.
fn check_output_signature(body: &Statement, name: &str, fields: &[LocalOutputField]) -> Result<()> {
    let Some(exports) = body_export_names(body) else {
        // The body's columns cannot be determined (conditional returns),
        // so no column can be named as the offender -- and `fields` may
        // be empty (`:: ()`), so none may be fabricated either.
        return Err(Error::syntax(format!(
            "The body of local procedure {name} returns data in a way that \
             cannot be matched against its declared output signature."
        )));
    };
    for field in fields {
        if !exports.contains(&field.name) {
            return Err(Error::syntax(format!(
                "Return column {} is missing to match output signature of local \
                 procedure {name}.",
                field.name
            )));
        }
    }
    for export in &exports {
        if !fields.iter().any(|field| &field.name == export) {
            return Err(Error::syntax(format!(
                "Return column {export} does not match output signature of local \
                 procedure {name}."
            )));
        }
    }
    Ok(())
}

/// The 42NAM scalar-shape rule: every emitting RETURN of a function's
/// query body has exactly one non-`*` item and either `LIMIT 1` or a
/// total aggregation in that item.
fn check_scalar_shape(body: &Statement, name: &str) -> Result<()> {
    if matches!(body.clauses.as_slice(), [Clause::Conditional(_)]) {
        return Err(Error::syntax(format!(
            "Conditional statements are not supported in the query body of local \
             function {name}()."
        )));
    }
    for link in chain_links(body) {
        let Some(ret) = terminal_return(link) else {
            return Err(Error::syntax(non_scalar_message(
                name,
                "No RETURN clause in",
            )));
        };
        let single = ret.items.len() == 1 && !ret.items[0].all;
        if !single {
            return Err(Error::syntax(non_scalar_message(
                name,
                "More than one projection item in",
            )));
        }
        let limit_one = matches!(
            &ret.limit,
            Some(Expr::Literal(cypherlite_parser::LiteralValue::Integer(1)))
        );
        let aggregating = expression_aggregates(&ret.items[0].expr);
        if !limit_one && !aggregating {
            return Err(Error::syntax(non_scalar_message(
                name,
                "No LIMIT 1 or total aggregation in",
            )));
        }
    }
    Ok(())
}

/// Whether the return item's own expression contains an aggregate
/// function call (the shared expression walker; an aggregate inside a
/// nested subquery of the item counts too, the over-count NOTES.md
/// records). A predicate, not a rejection, so it collects a flag; the
/// visitor never errors, so the expect is the invariant.
fn expression_aggregates(expr: &Expr) -> bool {
    let mut found = false;
    let mut visit = |expr: &Expr| -> std::result::Result<(), EngineError> {
        if let Expr::FunctionCall(call) = expr {
            if crate::agg::is_aggregate_name(&call.name) {
                found = true;
            }
        }
        Ok(())
    };
    walk_expr(expr, &mut visit).expect("the aggregate visitor never errors");
    found
}

fn non_scalar_message(name: &str, because: &str) -> String {
    format!(
        "Non-scalar query result not supported in local function definitions. Query in \
         local function definition {name} requires a RETURN clause with a single column \
         and either a total aggregation or LIMIT 1. Because: {because} the query function \
         definition body"
    )
}

/// The columns a procedure body exports, in projection order: `None`
/// when the body does not export (a FINISH/void body or an unnamed item).
pub(super) fn body_export_names(body: &Statement) -> Option<Vec<String>> {
    let set = crate::validate::compute_subquery_output_vars(body)?;
    for link in chain_links(body) {
        if let Some(names) = terminal_names(link) {
            if names.iter().all(|name| set.contains(name)) {
                return Some(names);
            }
            return None;
        }
    }
    None
}
