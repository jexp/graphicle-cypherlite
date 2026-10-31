//! CIP-231 local callables: the front-end resolver that rewrites `DEFINE`
//! blocks and their call sites away before lowering (ADR-0382).
//!
//! The parser attaches every `DEFINE PROCEDURE`/`DEFINE FUNCTION` block to
//! the [`Statement`] whose block heads it ([`Statement::definitions`]).
//! This module walks the whole statement tree once, in scope order, and
//! eliminates local callables: each definition is validated, its body
//! rewritten (so nested definitions and nested invocations resolve
//! first), and each visible call site rewritten into plain pre-existing
//! Cypher (see [`rewrite`]). The planner and executor never see a local
//! callable.
//!
//! Scoping (the CIP's visibility rules, made structural):
//!
//! - A definition is visible only inside the statement block that contains
//!   it and the blocks nested within it (a fresh frame per block: UNION
//!   arms, NEXT stages, `CALL`/expression-subquery bodies, and WHEN/ELSE
//!   arms are each their own block, so a definition never escapes its
//!   block -- "Local callable not visible because: defined in another
//!   (sub)query").
//! - Within its block, a definition is visible only from its own position
//!   onward (registration order -- "no calls before definition").
//! - A call site inside a callable's own body, or inside the body of a
//!   callable defined within it, cannot name it (the `bodies` stack --
//!   "recursive calls are not supported", "call of parent definition").
//! - Compilation-unit-wide name uniqueness per kind (procedures and
//!   functions are independent namespaces, CIP-230): a repeated name is
//!   the 42I77 error.
//!
//! Gating: the engine's statement admission calls
//! [`crate::parse_and_lower_statement_with_preview`], which fails closed
//! on any `DEFINE` with the ADR-0371 authored error unless
//! `CYPHERLITE_PREVIEW` enables `local-callables` (the same admission path
//! as the CIP-278 `hybrid-search` gate).
/// The AST walker's depth-guard error, flattened to this module's error
/// text: the one conversion, consulted by every walk boundary (the
/// statement/clause/expression walks in rewrite and validate) instead of
/// an ad-hoc closure per site.
pub(super) fn walker_error(error: cypherlite_error::EngineError) -> cypherlite_core::Error {
    cypherlite_core::Error::syntax(error.to_string())
}

mod rewrite;
mod validate;

use std::collections::HashSet;

use cypherlite_core::{Error, Result};
use cypherlite_error::EngineError;

use crate::ast::tree::walk_statement_tree;
use cypherlite_parser::{
    Clause, Expr, FunctionCallExpr, GraphTypeBody, LocalCallableKind, LocalDefinition,
    LocalDefinitionBody, LocalParam, PageStage, ReturnItem, Statement,
};

/// Rewrites every local callable definition and call site in `stmt` away.
/// Called by the front-end parse entries before lowering; after it, no
/// `DEFINE` block and no local invocation remains anywhere in the tree.
/// All the CIP-231 authored errors (name conflicts, recursion, USE and
/// `IN TRANSACTIONS` combinations, non-scalar function bodies, procedure
/// output-signature mismatches) are raised here, before lowering.
pub(crate) fn resolve(stmt: &mut Statement) -> Result<()> {
    let mut resolver = Resolver::default();
    resolver.resolve_statement(stmt)?;
    if resolver.has_definitions && resolver.has_in_transactions {
        return Err(Error::syntax(
            "'CALL { ... } IN TRANSACTIONS' is not supported in combination with 'DEFINE'.",
        ));
    }
    Ok(())
}

/// Whether the statement's AST carries a CIP-231 `DEFINE` block anywhere
/// (a local callable definition at any subquery depth) -- the predicate
/// the ADR-0371 `local-callables` preview gate checks before the resolver
/// rewrites anything. A walk error propagates: the gate never answers
/// "no" over a tree it could not walk.
pub fn statement_uses_local_callables(stmt: &Statement) -> std::result::Result<bool, EngineError> {
    let mut uses = false;
    walk_statement_tree(
        stmt,
        &mut |stmt| {
            if !stmt.definitions().is_empty() {
                uses = true;
            }
            Ok(())
        },
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )?;
    Ok(uses)
}

/// One registered local callable: everything a call-site rewrite needs,
/// cloned out of its frame at lookup (a callable may be called many times,
/// and each rewrite consumes its own copy of the body).
#[derive(Clone)]
struct Callable {
    kind: LocalCallableKind,
    name: String,
    params: Vec<LocalParam>,
    body: LocalDefinitionBody,
}

/// The resolver state: the scope frames (one per statement block, the
/// innermost last), the body stack (recursion guard), the
/// compilation-unit-wide name registries, and the whole-unit flags.
#[derive(Default)]
pub(super) struct Resolver {
    frames: Vec<Vec<Callable>>,
    bodies: Vec<(LocalCallableKind, String)>,
    /// The compilation-unit-wide name registry, keyed on the (kind,
    /// name) pair -- procedures and functions are independent namespaces
    /// (CIP-230), so the pair is one vocabulary, not two parallel sets.
    names: HashSet<(LocalCallableKind, String)>,
    has_definitions: bool,
    has_in_transactions: bool,
    /// The fresh-internal-name counter of this resolve pass (see
    /// `rewrite.rs`'s `next_internal_name`).
    internal_counter: usize,
    /// The call-site expansion budget (security): each rewritten call
    /// site deep-clones its definition's body, so chained definitions
    /// compound exponentially. One budget for the whole compilation
    /// unit bounds the total expansion before the clone storm can
    /// hang or OOM the process (a preview-gated store still serves
    /// untrusted queries).
    expansions: u32,
}

impl Resolver {
    /// Resolves one statement block: registers its definitions (rewriting
    /// each body first, in order), rewrites its clauses, then recurses
    /// into its UNION arms and NEXT stages (each a fresh frame).
    pub(super) fn resolve_statement(&mut self, stmt: &mut Statement) -> Result<()> {
        self.frames.push(Vec::new());
        let definitions = std::mem::take(&mut stmt.definitions);
        for definition in definitions {
            self.register_definition(definition)?;
        }
        self.visit_clauses(&mut stmt.clauses)?;
        // A brace-wrapped statement's definitions do not escape its
        // braces: the UNION arms and NEXT stages chained *after* a
        // `{ DEFINE ... }` body are sibling blocks, not nested ones, so
        // the frame carrying them is popped before the chain is walked
        // (an unbraced block's definitions, per the grammar, head its
        // whole `<statement block>` -- so they DO reach its stages).
        let escaped = stmt.brace_wrapped;
        if escaped {
            self.frames.pop();
        }
        if let Some(union) = &mut stmt.union {
            self.resolve_statement(&mut union.statement)?;
        }
        if let Some(next) = &mut stmt.next {
            self.resolve_statement(next)?;
        }
        if !escaped {
            self.frames.pop();
        }
        Ok(())
    }

    /// Validates one definition, rewrites its body (with the recursion
    /// guard raised), and registers it in the current frame.
    fn register_definition(&mut self, definition: LocalDefinition) -> Result<()> {
        self.has_definitions = true;
        validate::validate_definition(&definition)?;
        if !self
            .names
            .insert((definition.kind, definition.name.clone()))
        {
            return Err(Error::syntax(format!(
                "Local callable {}() is already defined.",
                definition.name
            )));
        }
        let mut body = definition.body;
        self.bodies.push((definition.kind, definition.name.clone()));
        let result = match &mut body {
            LocalDefinitionBody::Expression(expr) => self.visit_expr(expr),
            LocalDefinitionBody::Query(query) => self.resolve_statement(query),
        };
        self.bodies.pop();
        result?;
        self.frames
            .last_mut()
            .expect("resolve_statement pushed a frame")
            .push(Callable {
                kind: definition.kind,
                name: definition.name,
                params: definition.params,
                body,
            });
        Ok(())
    }

    /// The visible callable of `kind` named `name`, innermost frame first,
    /// or `None` when the name is not a visible local callable (the call
    /// site then falls through to the engine's own named-callable
    /// handling). A name on the body stack is the recursion error.
    fn lookup(&mut self, kind: LocalCallableKind, name: &str) -> Result<Option<Callable>> {
        if self
            .bodies
            .iter()
            .any(|(body_kind, body_name)| *body_kind == kind && body_name == name)
        {
            return Err(Error::syntax(format!(
                "Local callable {name}() is not visible from this call site: recursive \
                 calls and calls of parent definitions are not supported"
            )));
        }
        for frame in self.frames.iter().rev() {
            if let Some(callable) = frame
                .iter()
                .find(|callable| callable.kind == kind && callable.name == name)
            {
                // The budget is charged at the clone's birth (the only
                // place every lookup consumer -- the rewrites, the
                // implicit-args fall-through, any future call-site kind
                // -- pays by construction, not by remembered
                // convention).
                let callable = callable.clone();
                self.charge_expansion(&callable.body)?;
                return Ok(Some(callable));
            }
        }
        Ok(None)
    }

    /// The function-invocation rewrite arm of [`Self::visit_expr`] (the
    /// visitor itself lives in `rewrite.rs`): looks the name up and
    /// replaces the call with the scalar-subquery rewrite.
    pub(super) fn rewrite_function_call(&mut self, call: &mut FunctionCallExpr) -> Result<()> {
        for arg in &mut call.args {
            self.visit_expr(arg)?;
        }
        if let Some(callable) = self.lookup(LocalCallableKind::Function, &call.name)? {
            let args = std::mem::take(&mut call.args);
            *call = self.rewrite_function_invocation(
                &callable.name,
                &callable.params,
                &callable.body,
                args,
                call.distinct,
            )?;
        }
        Ok(())
    }

    /// The mutable clause visitor: rewrites local procedure calls in
    /// place, recurses into nested statement bodies (fresh frames), and
    /// visits every expression slot of every other clause. Exhaustive over
    /// [`Clause`] on purpose -- a later clause variant that can carry an
    /// invocation fails compilation here instead of silently skipping it.
    pub(super) fn visit_clauses(&mut self, clauses: &mut [Clause]) -> Result<()> {
        for clause in clauses {
            self.visit_clause(clause)?;
        }
        Ok(())
    }

    fn visit_clause(&mut self, clause: &mut Clause) -> Result<()> {
        match clause {
            Clause::Match(match_clause) => {
                for path in &mut match_clause.pattern {
                    self.visit_pattern(path)?;
                }
                if let Some(where_expr) = &mut match_clause.where_expr {
                    self.visit_expr(where_expr)?;
                }
                if let Some(search) = &mut match_clause.search {
                    self.visit_search_spec(&mut search.spec)?;
                }
                Ok(())
            }
            Clause::Create(create) => {
                for path in &mut create.pattern {
                    self.visit_pattern(path)?;
                }
                Ok(())
            }
            Clause::Merge(merge) => {
                self.visit_pattern(&mut merge.pattern)?;
                self.visit_set_items(&mut merge.on_create)?;
                self.visit_set_items(&mut merge.on_match)
            }
            Clause::Delete(delete) => {
                for expr in &mut delete.expressions {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Clause::Set(set) => self.visit_set_items(&mut set.items),
            Clause::Remove(remove) => {
                for item in &mut remove.items {
                    if let Some(property) = &mut item.property {
                        self.visit_expr(&mut property.expr)?;
                        if let Some(key) = &mut property.key_expr {
                            self.visit_expr(key)?;
                        }
                    }
                    // Dynamic-label atoms (CIP-162): a local-call
                    // invocation in a REMOVE label expression is a call
                    // site like any other slot.
                    for label in &mut item.labels {
                        self.visit_label_expr(label)?;
                    }
                }
                Ok(())
            }
            Clause::Return(ret) => self.visit_projection(
                &mut ret.items,
                &mut ret.order_by,
                &mut ret.skip,
                &mut ret.limit,
                ret.group_by.as_mut(),
            ),
            Clause::With(with) => {
                self.visit_projection(
                    &mut with.items,
                    &mut with.order_by,
                    &mut with.skip,
                    &mut with.limit,
                    with.group_by.as_mut(),
                )?;
                // The WITH's own WHERE slot, visited at its own arm (the
                // RETURN arm has none -- no mode parameter to fake).
                if let Some(where_expr) = &mut with.where_expr {
                    self.visit_expr(where_expr)?;
                }
                Ok(())
            }
            Clause::Unwind(unwind) => self.visit_expr(&mut unwind.expr),
            Clause::CreateIndex(create_index) => {
                for (_, value) in &mut create_index.options {
                    self.visit_expr(value)?;
                }
                Ok(())
            }
            Clause::DropIndex(_) | Clause::DropConstraint(_) | Clause::AlterDatabase(_) => Ok(()),
            Clause::CreateConstraint(constraint) => {
                for (_, value) in &mut constraint.options {
                    self.visit_expr(value)?;
                }
                Ok(())
            }
            Clause::Foreach(foreach) => {
                self.visit_expr(&mut foreach.list_expr)?;
                // A FOREACH body shares the enclosing scope: it is a bare
                // clause list (it cannot carry a definition block), so the
                // same frame -- just the clauses.
                self.visit_clauses(&mut foreach.clauses)
            }
            Clause::LoadCSV(load) => self.visit_expr(&mut load.source),
            Clause::CallSubquery(call) => {
                if call.in_transactions.is_some() {
                    self.has_in_transactions = true;
                }
                self.resolve_statement(&mut call.statement)
            }
            Clause::CallProcedure(call) => {
                for arg in &mut call.args {
                    self.visit_expr(arg)?;
                }
                // CALL ... WHERE (CIP-225): the post-call filter is an
                // expression slot the rewrite must reach too.
                if let Some(where_expr) = &mut call.where_expr {
                    self.visit_expr(where_expr)?;
                }
                if let Some(callable) =
                    self.lookup(LocalCallableKind::Procedure, &call.procedure)?
                {
                    if let Some(rewritten) = self.rewrite_procedure_call(
                        &callable.name,
                        &callable.params,
                        &callable.body,
                        call,
                    )? {
                        *clause = Clause::CallSubquery(rewritten);
                    }
                }
                Ok(())
            }
            Clause::ShowFunctions(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowConstraints(show) => {
                self.visit_opt_expr(&mut show.where_expr)?;
                self.visit_stages(&mut show.stages)
            }
            Clause::ShowIndexes(show) => {
                self.visit_opt_expr(&mut show.where_expr)?;
                self.visit_stages(&mut show.stages)
            }
            Clause::ShowDatabases(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowRoles(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowServers(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowUsers(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowProcedures(show) => self.visit_opt_expr(&mut show.where_expr),
            Clause::ShowSettings(show) => {
                self.visit_opt_expr(&mut show.where_expr)?;
                if let Some(names) = &mut show.names {
                    self.visit_expr(names)?;
                }
                Ok(())
            }
            Clause::ShowTransactions(show) => {
                self.visit_opt_expr(&mut show.where_expr)?;
                for id in &mut show.ids {
                    self.visit_expr(id)?;
                }
                Ok(())
            }
            Clause::TerminateTransactions(terminate) => {
                for id in &mut terminate.ids {
                    self.visit_expr(id)?;
                }
                Ok(())
            }
            Clause::Use(use_clause) => self.visit_expr(&mut use_clause.graph_expr),
            Clause::Finish => Ok(()),
            Clause::StandaloneOrderBy(order_by) => self.visit_stages(&mut order_by.stages),
            Clause::Conditional(conditional) => {
                for branch in &mut conditional.branches {
                    self.visit_expr(&mut branch.predicate)?;
                    self.resolve_statement(&mut branch.body)?;
                }
                if let Some(else_body) = &mut conditional.else_body {
                    self.resolve_statement(else_body)?;
                }
                Ok(())
            }
            Clause::ShowGraphType(_) => Ok(()),
            Clause::ShowCurrentGraphType(show) => {
                self.visit_opt_expr(&mut show.where_expr)?;
                self.visit_stages(&mut show.stages)
            }
            Clause::AlterCurrentGraphType(alter) => {
                if let cypherlite_parser::AlterCurrentGraphTypeTarget::Body(body) =
                    &mut alter.target
                {
                    self.visit_graph_type_body(body)?;
                }
                Ok(())
            }
            Clause::CreateGraphType(create) => self.visit_graph_type_body(&mut create.body),
        }
    }

    /// An optional expression slot (`WHERE` on a SHOW clause): the visit
    /// result propagates -- a rewrite error in a SHOW's WHERE is an
    /// authored local-callable error like anywhere else.
    fn visit_opt_expr(&mut self, expr: &mut Option<Expr>) -> Result<()> {
        if let Some(expr) = expr {
            self.visit_expr(expr)?;
        }
        Ok(())
    }

    fn visit_stages(&mut self, stages: &mut [PageStage]) -> Result<()> {
        for stage in stages {
            match stage {
                PageStage::OrderBy(items) => {
                    for item in items {
                        self.visit_expr(&mut item.expr)?;
                    }
                }
                PageStage::Skip(expr) | PageStage::Limit(expr) => {
                    self.visit_expr(expr)?;
                }
            }
        }
        Ok(())
    }

    fn visit_graph_type_body(&mut self, body: &mut GraphTypeBody) -> Result<()> {
        // Only constraint `OPTIONS {key: value}` entries carry
        // expressions in a graph-type body; property types are types.
        for constraint in &mut body.constraints {
            for (_, value) in &mut constraint.options {
                self.visit_expr(value)?;
            }
        }
        Ok(())
    }

    fn visit_set_items(&mut self, items: &mut [cypherlite_parser::SetItem]) -> Result<()> {
        for item in items {
            if let Some(property) = &mut item.property {
                self.visit_expr(&mut property.expr)?;
                if let Some(key) = &mut property.key_expr {
                    self.visit_expr(key)?;
                }
            }
            if let Some(value) = &mut item.value {
                self.visit_expr(value)?;
            }
            if let Some((expr, _)) = &mut item.labels_function {
                self.visit_expr(expr)?;
            }
            for label in &mut item.labels {
                self.visit_label_expr(label)?;
            }
        }
        Ok(())
    }

    /// WITH/RETURN projection slots: items, ORDER BY, SKIP/LIMIT, and the
    /// WITH's WHERE.
    fn visit_projection(
        &mut self,
        items: &mut [ReturnItem],
        order_by: &mut [cypherlite_parser::OrderByItem],
        skip: &mut Option<Expr>,
        limit: &mut Option<Expr>,
        group_by: Option<&mut cypherlite_parser::GroupBySpec>,
    ) -> Result<()> {
        for item in items {
            self.visit_expr(&mut item.expr)?;
        }
        for item in order_by {
            self.visit_expr(&mut item.expr)?;
        }
        // GROUP BY keys are expression slots (`All` and `Empty` carry
        // none).
        if let Some(cypherlite_parser::GroupBySpec::Keys(keys)) = group_by {
            for key in keys.iter_mut() {
                self.visit_expr(key)?;
            }
        }
        for slot in [skip, limit] {
            if let Some(expr) = slot.as_mut() {
                self.visit_expr(expr)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_and_lower;

    fn err(query: &str) -> String {
        parse_and_lower(query).unwrap_err().to_string()
    }

    #[test]
    fn simple_function_invocation_rewrites_and_lowers() {
        parse_and_lower(
            "DEFINE FUNCTION isEven(num) = CASE WHEN num % 2 = 0 THEN true ELSE false END \
             UNWIND range(1,4) AS x RETURN x, isEven(x) AS isEven",
        )
        .unwrap();
    }

    #[test]
    fn namespaced_procedure_call_rewrites_and_lowers() {
        parse_and_lower(
            "DEFINE PROCEDURE foo.bar() { RETURN 123 AS abc } \
             CALL foo.bar() YIELD abc RETURN abc",
        )
        .unwrap();
    }

    #[test]
    fn duplicate_names_across_the_unit_are_rejected() {
        let error = err("DEFINE PROCEDURE foo() { RETURN 1 AS a } \
             DEFINE PROCEDURE foo() { RETURN 2 AS a } \
             CALL foo() YIELD a RETURN a");
        assert!(
            error.contains("Local callable foo() is already defined."),
            "{error}"
        );
    }

    #[test]
    fn recursion_and_parent_calls_are_rejected() {
        let error = err("DEFINE FUNCTION foo() { RETURN foo() AS x LIMIT 1 } RETURN foo()");
        assert!(
            error.contains("recursive calls and calls of parent definitions"),
            "{error}"
        );
        let error = err(
            "DEFINE PROCEDURE foo() { DEFINE PROCEDURE bar() { CALL foo() YIELD a \
             RETURN a AS b } RETURN 1 AS a } CALL foo() YIELD a RETURN a",
        );
        assert!(
            error.contains("recursive calls and calls of parent definitions"),
            "{error}"
        );
    }

    #[test]
    fn definitions_do_not_escape_their_block() {
        // A definition inside a braced body is invisible after the braces:
        // the NEXT stage's call falls through to the engine's own
        // named-procedure path (the unknown-procedure error fires at
        // execution, pinned by the integration tests) instead of the
        // local rewrite -- the first stage, inside the braces, rewrites.
        parse_and_lower(
            "{ DEFINE PROCEDURE foo() { RETURN 1 AS a } CALL foo() YIELD a RETURN a } \
             NEXT CALL foo() YIELD a RETURN a",
        )
        .unwrap();
    }

    #[test]
    fn use_inside_bodies_is_rejected() {
        let error =
            err("DEFINE PROCEDURE foo() { USE neo4j RETURN 1 AS a } CALL foo() YIELD a RETURN a");
        assert!(
            error.contains("USE clause is not supported in local procedure definitions."),
            "{error}"
        );
        let error =
            err("DEFINE FUNCTION f() { USE neo4j MATCH (n) RETURN n.name LIMIT 1 } RETURN f()");
        assert!(
            error.contains("USE clause is not supported in local function definitions."),
            "{error}"
        );
    }

    #[test]
    fn in_transactions_combination_is_rejected() {
        let error =
            err("DEFINE FUNCTION f() = 1 CALL { RETURN 1 AS a } IN TRANSACTIONS RETURN f()");
        assert!(
            error.contains(
                "'CALL { ... } IN TRANSACTIONS' is not supported in combination with 'DEFINE'."
            ),
            "{error}"
        );
    }

    #[test]
    fn non_scalar_function_bodies_are_rejected() {
        let error = err("DEFINE FUNCTION f() { RETURN 1 AS a, 2 AS b } RETURN f()");
        assert!(
            error.contains("Non-scalar query result not supported"),
            "{error}"
        );
        let error = err("DEFINE FUNCTION f() { MATCH (n) RETURN n.name } RETURN f()");
        assert!(
            error.contains("Non-scalar query result not supported"),
            "{error}"
        );
        // LIMIT 1 and aggregation bodies are the two scalar shapes.
        parse_and_lower(
            "DEFINE FUNCTION f() { MATCH (n:Person) RETURN n.name LIMIT 1 } RETURN f()",
        )
        .unwrap();
        parse_and_lower("DEFINE FUNCTION f() { MATCH (n:Person) RETURN count(n) } RETURN f()")
            .unwrap();
    }

    #[test]
    fn procedure_output_signature_mismatches_are_rejected() {
        let error = err(
            "DEFINE PROCEDURE foo(x) :: (foo :: STRING, bar) { RETURN \"foo\" AS foo } \
             CALL foo(1) YIELD foo RETURN foo",
        );
        assert!(
            error.contains("Return column bar is missing to match output signature"),
            "{error}"
        );
        let error = err(
            "DEFINE PROCEDURE foo(x) :: (bar :: INT) { RETURN 123 AS baz } \
             CALL foo(1) YIELD baz RETURN baz",
        );
        assert!(
            error.contains("output signature of local procedure foo"),
            "{error}"
        );
    }

    #[test]
    fn unaliased_procedure_returns_are_rejected() {
        let error = err("DEFINE PROCEDURE foo() { RETURN 123 } CALL foo() YIELD `123` RETURN 1");
        assert!(error.contains("must be aliased (use AS)"), "{error}");
    }

    #[test]
    fn missing_yield_is_rejected_for_non_empty_bodies() {
        let error = err("DEFINE PROCEDURE foo() { RETURN 1 AS a } CALL foo() RETURN 1 AS one");
        assert!(
            error.contains(
                "Procedure call inside a query does not support naming results implicitly."
            ),
            "{error}"
        );
        // A FINISH (void) body needs no YIELD.
        parse_and_lower(
            "DEFINE PROCEDURE foo() { CREATE (n:Void {id: 1}) FINISH } \
             CALL foo() RETURN 1 AS one",
        )
        .unwrap();
    }

    #[test]
    fn yielding_unknown_columns_is_rejected() {
        let error =
            err("DEFINE PROCEDURE foo() { RETURN 1 AS a } CALL foo() YIELD c AS y RETURN y");
        assert!(
            error.contains("The procedure return column c is not defined for this procedure."),
            "{error}"
        );
    }

    #[test]
    fn distinct_on_local_functions_is_rejected() {
        let error = err("DEFINE FUNCTION f(x) = x RETURN f(DISTINCT x)");
        assert!(
            error.contains("DISTINCT cannot be used with the f() function"),
            "{error}"
        );
    }

    #[test]
    fn variable_referencing_defaults_are_rejected() {
        let error = err("DEFINE FUNCTION f(x = y) = x RETURN f(1)");
        assert!(
            error.contains(
                "default values of local callable parameters may not reference variables"
            ),
            "{error}"
        );
    }

    #[test]
    fn arity_mismatches_are_rejected() {
        let error = err("DEFINE FUNCTION f(a, b, c = 1) = a RETURN f(1, 2, 3, 4)");
        assert!(
            error.contains("expects at most 3 argument(s), got 4"),
            "{error}"
        );
        let error = err("DEFINE PROCEDURE p(a, b) { RETURN a AS r } CALL p(1) YIELD r RETURN r");
        assert!(
            error.contains("expects at least 2 argument(s), got 1"),
            "{error}"
        );
    }

    #[test]
    fn function_invoking_function_resolves() {
        parse_and_lower(
            "DEFINE FUNCTION numberOfDigits(num) = CASE WHEN num = 0 THEN 1 \
             ELSE toInteger(ceil(log10(abs(num)+1))) END \
             DEFINE FUNCTION isEven(num) = CASE WHEN num % 2 = 0 THEN true ELSE false END \
             DEFINE FUNCTION hasEvenNumberOfDigits(num) = isEven(numberOfDigits(num)) \
             UNWIND [-11, -1, 0, 17, 100] AS x RETURN x, hasEvenNumberOfDigits(x)",
        )
        .unwrap();
    }

    #[test]
    fn a_local_function_shadows_the_built_in_name() {
        // SHADOW-BUILT-IN-CALLABLE-PER-COMPILATION-UNIT: the local one
        // wins inside the unit.
        parse_and_lower("DEFINE FUNCTION abs(x) = x * -1 UNWIND [1, 2] AS n RETURN abs(n) AS a")
            .unwrap();
    }
}
