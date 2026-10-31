//! The CIP-231 call-site rewrites and the mutable expression/pattern
//! visitor that finds them.
//!
//! Local callables never reach the planner: every invocation site is
//! rewritten, before lowering, into plain pre-existing constructs (the
//! CIP's own implementation plan -- "Local Procedures are planned to be
//! implemented through a rewrite of `<named procedure call>`s to `<inline
//! procedure call>`s", and a function's body *is* the value expression
//! `head( COLLECT { <procedure specification> } )`).
//!
//! Rewrites (uniform for both kinds: bind parameters once, then run the
//! body isolated behind a scope clause so nothing but the parameters is
//! visible -- structurally enforcing the CIP's
//! REFERENCE-OUTER-DEFINITION-SCOPE-NO decision):
//!
//! ```text
//! f(a1, ..., an)                       (simple or query body F)
//!   => head( COLLECT {
//!        WITH a1 AS p1, ..., an AS pn
//!        CALL (p1, ..., pn) { <F> }
//!      } )
//!
//! CALL p(a1, ..., an) YIELD x AS y [WHERE w]
//!   => CALL (*) {
//!        WITH a1 AS p1, ..., an AS pn
//!        CALL (p1, ..., pn) { <body> }
//!        WITH <body items> [WHERE w]    -- restates the emitting RETURN
//!        RETURN y, ...                  -- the yield mapping
//!      }
//! ```
//!
//! Call-by-value falls out of the leading `WITH`: each argument is
//! evaluated exactly once per outer row (`WITH rand() AS r` binds one
//! value; the CIP's `foo(rand()) = "… = …"` self-equality example pins
//! it). A function's query body returning zero rows yields
//! `head(<empty list>) = null`, the CIP's scalar-subquery semantics.
//!
//! The `YIELD` `WHERE` referencing outer-row variables is supported for
//! the common shapes: the mapping `WITH` restates the body's RETURN items
//! and folds the `WHERE` in, and the engine's CALL-subquery column-add
//! semantics keep every outer variable flowing around the rewritten
//! clause. A `WHERE` whose outer-variable reference sits *inside* a
//! correlated subquery of its own is deferred (NOTES.md): the mapping
//! `WITH`'s projection cannot forward a name it cannot see.

use cypherlite_core::{Error, Result};
use cypherlite_parser::{
    CallProcedureClause, CallScope, CallSubqueryClause, Clause, Expr, InterpPart, LabelExpr,
    LocalDefinitionBody, LocalParam, MapProjectionItem, PatternElement, PatternPath, ReturnClause,
    ReturnItem, SearchSpec, Statement, WithClause, WithOrigin, YieldMode,
};

use super::{walker_error, Resolver};

/// The weight of a cloned local-callable body: one per visited
/// statement, clause, and expression node (the whole-tree walker in
/// [`crate::validate::tree`]; an expression body rides the expression
/// walker core). The clone cost is linear in nodes, so the security
/// budget is node-weighted -- a per-call-site count misses the
/// exponential compounding of chained definitions.
pub(super) fn body_weight(body: &LocalDefinitionBody) -> Result<u32> {
    // A `Cell`: the three visitor closures each need `weight` mutably.
    let weight = std::cell::Cell::new(0u32);
    let bump = || weight.set(weight.get() + 1);
    let mut on_stmt =
        |_: crate::ast::stmt_ref::StmtRef| -> core::result::Result<(), cypherlite_error::EngineError> {
            bump();
            Ok(())
        };
    let mut on_clause = |_: &Clause| -> core::result::Result<(), cypherlite_error::EngineError> {
        bump();
        Ok(())
    };
    let mut on_expr = |_: &Expr| -> core::result::Result<(), cypherlite_error::EngineError> {
        bump();
        Ok(())
    };
    // The counting visitors never fail, so the walks cannot either.
    match body {
        LocalDefinitionBody::Query(stmt) => {
            // The walkers enforce the ADR-0257 nesting cap independently
            // of the counting visitors, so a too-deep body propagates the
            // walker's own typed depth error -- never a panic.
            crate::ast::tree::walk_statement_tree(stmt, &mut on_stmt, &mut on_clause, &mut on_expr)
                .map_err(walker_error)?;
            Ok(weight.get())
        }
        LocalDefinitionBody::Expression(expr) => {
            crate::ast::tree::walk_expr(expr, &mut on_expr).map_err(walker_error)?;
            Ok(weight.get() + 1)
        }
    }
}

/// The per-compilation-unit cloned-node budget (security): every
/// rewritten call site adds its cloned body's node weight to the total;
/// a chained-definition bomb (each level cloning the previous level's
/// already-expanded body) hits this with a typed error instead of
/// hanging or OOM-ing the process.
pub(super) const MAX_CLONED_NODES: u32 = 100_000;

impl Resolver {
    /// The one mutable expression visitor: rewrites local function
    /// invocations in place and recurses into nested subquery statements
    /// (each a fresh scope frame, so definitions inside an expression
    /// subquery stay local to it). Exhaustive over [`Expr`] on purpose --
    /// no catch-all arm, so a later `Expr` variant that can carry an
    /// invocation fails compilation here instead of silently skipping it.
    pub(super) fn visit_expr(&mut self, expr: &mut Expr) -> Result<()> {
        match expr {
            Expr::Literal(_)
            | Expr::Variable(_)
            | Expr::Parameter(_)
            | Expr::CountStar
            | Expr::Star => Ok(()),
            Expr::List(items) => {
                for item in items {
                    self.visit_expr(item)?;
                }
                Ok(())
            }
            Expr::Map(pairs) => {
                for pair in pairs {
                    self.visit_expr(&mut pair.value)?;
                }
                Ok(())
            }
            Expr::StringInterp(parts) => {
                for part in parts {
                    if let InterpPart::Interp(embed) = part {
                        self.visit_expr(embed)?;
                    }
                }
                Ok(())
            }
            Expr::Property(access) => {
                self.visit_expr(&mut access.expr)?;
                if let Some(key) = &mut access.key_expr {
                    self.visit_expr(key)?;
                }
                Ok(())
            }
            Expr::FunctionCall(call) => self.rewrite_function_call(call),
            Expr::Binary(binary) => {
                self.visit_expr(&mut binary.left)?;
                self.visit_expr(&mut binary.right)
            }
            Expr::Unary(unary) => self.visit_expr(&mut unary.expr),
            Expr::Comparison(cmp) => {
                self.visit_expr(&mut cmp.left)?;
                self.visit_expr(&mut cmp.right)
            }
            Expr::In(in_expr) => {
                self.visit_expr(&mut in_expr.expr)?;
                self.visit_expr(&mut in_expr.list)
            }
            Expr::IsNull(is_null) => self.visit_expr(&mut is_null.expr),
            Expr::IsType(is_type) => self.visit_expr(&mut is_type.expr),
            Expr::IsNormalized(is_norm) => self.visit_expr(&mut is_norm.expr),
            Expr::StringMatch(match_expr) => {
                self.visit_expr(&mut match_expr.left)?;
                self.visit_expr(&mut match_expr.right)
            }
            Expr::Case(case) => {
                if let Some(test) = &mut case.test {
                    self.visit_expr(test)?;
                }
                for when in &mut case.whens {
                    self.visit_expr(&mut when.condition)?;
                    self.visit_expr(&mut when.result)?;
                }
                if let Some(else_expr) = &mut case.else_expr {
                    self.visit_expr(else_expr)?;
                }
                Ok(())
            }
            Expr::ExistsSubquery(sub) => self.resolve_statement(&mut sub.statement),
            Expr::CountSubquery(sub) => self.resolve_statement(&mut sub.statement),
            Expr::CollectSubquery(sub) => self.resolve_statement(&mut sub.statement),
            Expr::ListComprehension(list_comp) => {
                self.visit_expr(&mut list_comp.list_expr)?;
                if let Some(where_expr) = &mut list_comp.where_expr {
                    self.visit_expr(where_expr)?;
                }
                if let Some(map_expr) = &mut list_comp.map_expr {
                    self.visit_expr(map_expr)?;
                }
                Ok(())
            }
            Expr::MapComprehension(map_comp) => {
                self.visit_expr(&mut map_comp.source_expr)?;
                if let Some(where_expr) = &mut map_comp.where_expr {
                    self.visit_expr(where_expr)?;
                }
                self.visit_expr(&mut map_comp.key_expr)?;
                self.visit_expr(&mut map_comp.value_expr)
            }
            Expr::PatternComprehension(pattern_comp) => {
                self.visit_pattern(&mut pattern_comp.pattern)?;
                if let Some(where_expr) = &mut pattern_comp.where_expr {
                    self.visit_expr(where_expr)?;
                }
                self.visit_expr(&mut pattern_comp.map_expr)
            }
            Expr::Reduce(reduce) => {
                self.visit_expr(&mut reduce.init_expr)?;
                self.visit_expr(&mut reduce.list_expr)?;
                self.visit_expr(&mut reduce.expression)
            }
            Expr::AllReduce(reduce) => {
                self.visit_expr(&mut reduce.init_expr)?;
                self.visit_expr(&mut reduce.list_expr)?;
                self.visit_expr(&mut reduce.expression)?;
                self.visit_expr(&mut reduce.predicate)
            }
            Expr::ShortestPath(shortest) => self.visit_pattern(&mut shortest.pattern),
            Expr::Subscript(subscript) => {
                self.visit_expr(&mut subscript.expr)?;
                for slot_expr in [&mut subscript.index, &mut subscript.from, &mut subscript.to]
                    .into_iter()
                    .flatten()
                {
                    self.visit_expr(slot_expr)?;
                }
                Ok(())
            }
            Expr::Quantifier(quantifier) => {
                self.visit_expr(&mut quantifier.list)?;
                if let Some(predicate) = &mut quantifier.predicate {
                    self.visit_expr(predicate)?;
                }
                Ok(())
            }
            Expr::HasLabel(has_label) => {
                self.visit_expr(&mut has_label.expr)?;
                self.visit_label_expr(&mut has_label.label_expr)
            }
            Expr::Paren(inner) => self.visit_expr(inner),
            Expr::PatternPredicate(predicate) => self.visit_pattern(&mut predicate.pattern),
            Expr::MapProjection(projection) => {
                self.visit_expr(&mut projection.expr)?;
                for item in &mut projection.items {
                    if let MapProjectionItem::KeyValue(_, value) = item {
                        self.visit_expr(value)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// The mutable label-expression visitor: only the dynamic
    /// `$(…)`/`$all(…)`/`$any(…)` atoms carry expressions.
    pub(super) fn visit_label_expr(&mut self, label_expr: &mut LabelExpr) -> Result<()> {
        match label_expr {
            LabelExpr::Label(_) | LabelExpr::Wildcard => Ok(()),
            LabelExpr::Dynamic(dynamic) => self.visit_expr(&mut dynamic.expr),
            LabelExpr::Not(inner) => self.visit_label_expr(inner),
            LabelExpr::And(items) | LabelExpr::Or(items) => {
                for item in items {
                    self.visit_label_expr(item)?;
                }
                Ok(())
            }
        }
    }

    /// The mutable path-pattern visitor: property values, dynamic property
    /// keys, and per-element `WHERE` clauses can all carry invocations.
    pub(super) fn visit_pattern(&mut self, path: &mut PatternPath) -> Result<()> {
        if let Some(where_expr) = &mut path.where_expr {
            self.visit_expr(where_expr)?;
        }
        for element in &mut path.elements {
            self.visit_pattern_element(element)?;
        }
        Ok(())
    }

    /// The element core shared by [`Self::visit_pattern`] and its
    /// quantified-sub-path recursion (a quantified sub-path's elements are
    /// always a node/relationship pair, never another quantified pattern).
    fn visit_pattern_element(&mut self, element: &mut PatternElement) -> Result<()> {
        match element {
            PatternElement::Node(node) => {
                for (_, value) in &mut node.properties {
                    self.visit_expr(value)?;
                }
                if let Some(param) = &mut node.properties_param {
                    self.visit_expr(param)?;
                }
                if let Some(label_expr) = &mut node.label_expr {
                    self.visit_label_expr(label_expr)?;
                }
                if let Some(where_expr) = &mut node.where_expr {
                    self.visit_expr(where_expr)?;
                }
            }
            PatternElement::Relationship(rel) => {
                for (_, value) in &mut rel.properties {
                    self.visit_expr(value)?;
                }
                if let Some(param) = &mut rel.properties_param {
                    self.visit_expr(param)?;
                }
                if let Some(type_expr) = &mut rel.type_expr {
                    self.visit_label_expr(type_expr)?;
                }
                if let Some(where_expr) = &mut rel.where_expr {
                    self.visit_expr(where_expr)?;
                }
            }
            PatternElement::Quantified(quantified) => {
                if let Some(where_expr) = &mut quantified.where_expr {
                    self.visit_expr(where_expr)?;
                }
                for inner in &mut quantified.elements {
                    self.visit_pattern_element(inner)?;
                }
            }
        }
        Ok(())
    }

    /// The mutable search-source visitor (CIP-278 sources on a MATCH
    /// SEARCH clause): every expression slot of an index specification or
    /// a nested COMBINE, mirroring `validate::walk_search_spec`'s slots.
    pub(super) fn visit_search_spec(&mut self, spec: &mut SearchSpec) -> Result<()> {
        match spec {
            SearchSpec::Index(index) => {
                self.visit_expr(&mut index.for_expr)?;
                if let Some(analyzer) = &mut index.analyzer {
                    self.visit_expr(analyzer)?;
                }
                if let Some(inner_where) = &mut index.inner_where {
                    self.visit_expr(inner_where)?;
                }
                if let Some(offset) = &mut index.offset_expr {
                    self.visit_expr(offset)?;
                }
                self.visit_expr(&mut index.limit_expr)
            }
            SearchSpec::Combine(combine) => {
                for source in &mut combine.sources {
                    self.visit_search_spec(&mut source.spec)?;
                    if let Some(weight) = &mut source.weight_expr {
                        self.visit_expr(weight)?;
                    }
                }
                if let Some(constant) = &mut combine.constant_expr {
                    self.visit_expr(constant)?;
                }
                if let Some(limit) = &mut combine.limit_expr {
                    self.visit_expr(limit)?;
                }
                Ok(())
            }
        }
    }
}

/// A `Statement` shell for rewrite-built statements: no union/next chain,
/// no preamble flags, no definitions (the rewrite output is plain
/// pre-existing Cypher).
fn shell_statement(clauses: Vec<Clause>) -> Statement {
    Statement {
        clauses,
        ..Statement::default()
    }
}

impl Resolver {
    /// The next fresh internal name of this resolve pass (`_lc1`,
    /// `_lc2`, ...): deterministic per query, and chosen so no user
    /// variable can collide without the engine's own duplicate-name
    /// validation firing loudly first.
    pub(super) fn next_internal_name(&mut self) -> String {
        self.internal_counter += 1;
        format!("_lc{}", self.internal_counter)
    }

    /// Rewrites one local function invocation into the scalar-subquery
    /// shape. A query body returning zero rows yields `head([]) = null`
    /// (the CIP's scalar-subquery semantics); a simple body always yields
    /// exactly one row. Returned as the replacement `FunctionCallExpr`
    /// (`head(...)`).
    ///
    /// The shape (the CIP's own "argument materialization with upfront
    /// projection" sketch): the arguments are evaluated once and bound to
    /// *fresh* internal names (`WITH a1 AS _lc1, ...` -- call-by-value,
    /// and a fresh name can never collide with an outer variable, which
    /// the CIP's own same-name-parameter examples would otherwise), then
    /// the body runs behind a scope clause importing exactly those names,
    /// with the parameters rebound from them -- so the body sees its
    /// parameters and nothing else (REFERENCE-OUTER-DEFINITION-SCOPE-NO,
    /// made structural) -- and the single result column is re-exported
    /// under one more fresh name by the COLLECT's terminal RETURN.
    /// The security budget's charge (one policy, both clone sites): a
    /// clone's node weight is counted against the compilation-unit
    /// total, and an over-budget expansion raises the authored error
    /// instead of hanging or OOM-ing the process.
    pub(super) fn charge_expansion(&mut self, body: &LocalDefinitionBody) -> Result<()> {
        self.expansions += body_weight(body)?;
        if self.expansions > MAX_CLONED_NODES {
            return Err(Error::syntax(format!(
                "InvalidSyntax: local callable expansion exceeds {MAX_CLONED_NODES} cloned nodes; \
                 chained definitions compound per call site (security budget)."
            )));
        }
        Ok(())
    }

    pub(super) fn rewrite_function_invocation(
        &mut self,
        name: &str,
        params: &[LocalParam],
        body: &LocalDefinitionBody,
        args: Vec<Expr>,
        distinct: bool,
    ) -> Result<cypherlite_parser::FunctionCallExpr> {
        if distinct {
            return Err(Error::syntax(format!(
                "invalid use of DISTINCT with non-aggregate function. DISTINCT cannot be used \
                 with the {name}() function"
            )));
        }
        let temps: Vec<String> = params.iter().map(|_| self.next_internal_name()).collect();
        let value = self.next_internal_name();
        let bind = bind_params_clause(name, params, args, &temps)?;

        // Inside the isolating wrapper: the parameters rebound from the
        // temp names, then the body -- for a query body, its final RETURN
        // item is aliased to the fresh value name (the COLLECT's single
        // column must be nameable), for a UNION body on every arm.
        let mut inner = match body {
            LocalDefinitionBody::Expression(expr) => {
                shell_statement(vec![Clause::Return(ReturnClause {
                    group_by: None,
                    distinct: false,
                    items: vec![ReturnItem {
                        expr: expr.clone(),
                        alias: Some(value.clone()),
                        all: false,
                        source_text: None,
                    }],
                    order_by: Vec::new(),
                    skip: None,
                    limit: None,
                })])
            }
            LocalDefinitionBody::Query(query) => {
                // The final RETURN item is aliased to the fresh value name
                // in the same arm that decides the kind (the rebind
                // prepends touch clause heads, this touches the final
                // RETURN items -- order-independent), not re-decided
                // after the match.
                let mut inner = query.clone();
                map_final_return_items(&mut inner, &FinalItemMap::AliasTo(value.clone()))?;
                inner
            }
        };
        let rebind = rebind_params_clause(params, &temps);
        inner.clauses.insert(0, Clause::With(rebind.clone()));
        prepend_clause_to_every_arm(&mut inner, Clause::With(rebind));

        let statement = shell_statement(vec![
            Clause::With(bind),
            Clause::CallSubquery(CallSubqueryClause {
                statement: inner,
                scope: CallScope::Vars(temps),
                optional: false,
                in_transactions: None,
            }),
            Clause::Return(ReturnClause {
                group_by: None,
                distinct: false,
                items: vec![ReturnItem {
                    expr: Expr::Variable(value),
                    alias: None,
                    all: false,
                    source_text: None,
                }],
                order_by: Vec::new(),
                skip: None,
                limit: None,
            }),
        ]);
        Ok(cypherlite_parser::FunctionCallExpr {
            name: "head".into(),
            args: vec![Expr::CollectSubquery(Box::new(
                cypherlite_parser::CollectSubqueryExpr { statement },
            ))],
            distinct: false,
        })
    }

    /// Rewrites one local procedure call clause into the inline-subquery
    /// shape. `Ok(None)` when the call site should fall through to the
    /// engine's own named-procedure handling.
    ///
    /// Same argument-materialization shape as the function rewrite: the
    /// arguments evaluated once and bound to fresh internal names, the
    /// body running behind a scope clause that imports exactly them (the
    /// parameters rebound from them inside), the yield mapping applied to
    /// the body's own emitting RETURN (renaming its items in place, so the
    /// mapped items keep referencing the body's own variables), and the
    /// mapped columns re-exported by a terminal RETURN (a nested terminal
    /// CALL clause does not export inside a subquery).
    pub(super) fn rewrite_procedure_call(
        &mut self,
        name: &str,
        params: &[LocalParam],
        body: &LocalDefinitionBody,
        call: &mut CallProcedureClause,
    ) -> Result<Option<CallSubqueryClause>> {
        let args = if call.explicit_args {
            std::mem::take(&mut call.args)
        } else if params.is_empty() {
            Vec::new()
        } else {
            // A named procedure call requires an explicit argument list
            // (CIP-231 call-site grammar): an implicit-argument call site
            // is not a local callable surface and falls through to the
            // engine's own named-procedure handling.
            return Ok(None);
        };
        let temps: Vec<String> = params.iter().map(|_| self.next_internal_name()).collect();
        let bind = bind_params_clause(name, params, args, &temps)?;
        // The parser guarantees a procedure definition has a braced query
        // body (an `= <expr>` procedure body is a parse-time error), so
        // the expression arm is unreachable here.
        let LocalDefinitionBody::Query(query) = body else {
            unreachable!("the parser guarantees a procedure body is a query body")
        };
        let body_stmt = query.clone();
        let exports = super::validate::body_export_names(&body_stmt);

        let mapping = yield_mapping(call, &body_stmt, exports.as_deref())?;

        if call.where_expr.is_some() {
            // CIP-159's YIELD WHERE on a *local* procedure call is
            // deferred (NOTES.md): the faithful rewrite would have to fold
            // a WHERE referencing both the mapped columns and the outer
            // row into a scope the engine's subquery rebind rules cannot
            // express.
            return Err(Error::syntax(
                "YIELD ... WHERE on a local procedure call is not supported yet",
            ));
        }

        // The isolating wrapper: the parameters rebound from the temp
        // names (on every UNION arm -- an arm without the rebind could not
        // see the parameters), the yield mapping applied to each arm's
        // emitting RETURN in place.
        let mut inner = body_stmt;
        let rebind = rebind_params_clause(params, &temps);
        inner.clauses.insert(0, Clause::With(rebind.clone()));
        prepend_clause_to_every_arm(&mut inner, Clause::With(rebind));
        // The yield mapping is decided once, as a value: both the in-place
        // arm mapping (the item map) and the terminal re-exporting RETURN
        // (the mapped names) read their half of this one `map`, so the
        // mapping decision itself is never restated.
        let map = mapping.as_ref().map(|(pairs, mapped_names)| {
            (
                FinalItemMap::ReplaceWith(pairs),
                mapped_names
                    .iter()
                    .map(|name| ReturnItem {
                        expr: Expr::Variable(name.clone()),
                        alias: None,
                        all: false,
                        source_text: None,
                    })
                    .collect::<Vec<_>>(),
            )
        });
        if let Some((ref item_map, _)) = map {
            map_final_return_items(&mut inner, item_map)?;
        }

        let mut clauses = vec![
            Clause::With(bind),
            Clause::CallSubquery(CallSubqueryClause {
                statement: inner,
                scope: CallScope::Vars(temps),
                optional: false,
                in_transactions: None,
            }),
        ];
        if let Some((_, ref items)) = map {
            clauses.push(Clause::Return(ReturnClause {
                group_by: None,
                distinct: false,
                items: items.clone(),
                order_by: Vec::new(),
                skip: None,
                limit: None,
            }));
        }

        Ok(Some(CallSubqueryClause {
            statement: shell_statement(clauses),
            // `CALL (*)` imports every outer variable the arguments
            // reference, so call-by-value needs no static free-variable
            // analysis at the rewrite.
            scope: CallScope::Wildcard,
            optional: call.optional,
            // IN TRANSACTIONS is not a local-callable surface (CIP-231
            // IN-TRANSACTION-NO); the combination is rejected on the
            // whole compilation unit before any rewrite runs.
            in_transactions: None,
        }))
    }
}

/// Fills omitted optional arguments with their defaults and builds the
/// argument-materialization prelude: `WITH a1 AS _t1, …, an AS _tn`, one
/// fresh internal name per parameter. Call-by-value: every argument
/// (given or defaulted) is evaluated exactly once per outer row, before
/// the body runs. Arity mismatches are authored errors.
fn bind_params_clause(
    callable_name: &str,
    params: &[LocalParam],
    mut args: Vec<Expr>,
    temps: &[String],
) -> Result<WithClause> {
    let mandatory = params.iter().filter(|p| p.default.is_none()).count();
    if args.len() < mandatory {
        return Err(Error::syntax(format!(
            "Local callable {callable_name}() expects at least {mandatory} argument(s), got {}",
            args.len()
        )));
    }
    if args.len() > params.len() {
        return Err(Error::syntax(format!(
            "Local callable {callable_name}() expects at most {} argument(s), got {}",
            params.len(),
            args.len()
        )));
    }
    while args.len() < params.len() {
        // The fill-in default: evaluated once per invocation, per the CIP
        // ("default arguments are evaluated once per argument and
        // definition"); the `WITH` item binds exactly that per outer row.
        let param = &params[args.len()];
        args.push(
            param
                .default
                .clone()
                .expect("the arity check leaves only optional parameters"),
        );
    }
    let items = params
        .iter()
        .zip(args)
        .zip(temps)
        .map(|((_param, arg), temp)| ReturnItem {
            expr: arg,
            alias: Some(temp.clone()),
            all: false,
            source_text: None,
        })
        .collect();
    Ok(WithClause {
        group_by: None,
        distinct: false,
        items,
        order_by: Vec::new(),
        skip: None,
        limit: None,
        where_expr: None,
        origin: WithOrigin::Explicit,
    })
}

/// The parameter-rebind prelude inside the isolating wrapper:
/// `WITH _t1 AS p1, …, _tn AS pn` -- the body's own parameter names,
/// rebound from the fresh argument-materialization names, so the body's
/// text is used verbatim and sees nothing but its parameters.
fn rebind_params_clause(params: &[LocalParam], temps: &[String]) -> WithClause {
    let items = params
        .iter()
        .zip(temps)
        .map(|(param, temp)| ReturnItem {
            expr: Expr::Variable(temp.clone()),
            alias: Some(param.name.clone()),
            all: false,
            source_text: None,
        })
        .collect();
    WithClause {
        group_by: None,
        distinct: false,
        items,
        order_by: Vec::new(),
        skip: None,
        limit: None,
        where_expr: None,
        origin: WithOrigin::Explicit,
    }
}

/// Prepends `clause` to every UNION arm of `statement` (a rebind WITH for
/// a UNION-shaped callable body: each arm needs the parameters bound, and
/// a WITH heading only the first arm would leave the others blind). The
/// head statement's own clause list is NOT touched -- the caller builds
/// the head's prefix itself.
fn prepend_clause_to_every_arm(statement: &mut Statement, clause: Clause) {
    if let Some(union) = &mut statement.union {
        union.statement.clauses.insert(0, clause.clone());
        prepend_clause_to_every_arm(&mut union.statement, clause);
    }
}

/// The final-RETURN item mapping of a callable body's UNION arms: alias
/// each arm's single item to the function rewrite's fresh value name, or
/// replace each arm's items with the yield-mapped ones. Expressions,
/// ORDER BY, SKIP, LIMIT and DISTINCT carry over verbatim either way.
enum FinalItemMap<'a> {
    /// The function rewrite: the arm's single item, aliased to the fresh
    /// value name.
    AliasTo(String),
    /// The procedure rewrite: the yield selection, as (export name,
    /// applied alias) pairs -- each arm matches its own items by export
    /// name, so per-arm expressions survive the mapping.
    ReplaceWith(&'a [(String, Option<String>)]),
}

/// Applies `map` to the final RETURN items of every UNION arm of `body`,
/// in place.
fn map_final_return_items(body: &mut Statement, map: &FinalItemMap<'_>) -> Result<()> {
    if let Some(ret) = final_return_clause_mut(body) {
        ret.items = match map {
            FinalItemMap::AliasTo(value) => {
                // The scalar-shape check pinned exactly one return item
                // per arm, so the first item is the arm's only item.
                vec![ReturnItem {
                    expr: ret
                        .items
                        .first()
                        .expect("check_scalar_shape pinned exactly one item (the define-time scalar check)")
                        .expr
                        .clone(),
                    alias: Some(value.clone()),
                    all: false,
                    source_text: None,
                }]
            }
            FinalItemMap::ReplaceWith(pairs) => {
                // Each arm's OWN item (matched by export name), its
                // expression kept, the yield alias applied in place;
                // without an alias the item keeps its own naming. The
                // define-time UNION column check guarantees the find;
                // the fallible form is the boundary backstop -- no
                // residual panic in this mechanism.
                let mut mapped = Vec::with_capacity(pairs.len());
                for (name, alias) in pairs.iter() {
                    let item = ret
                        .items
                        .iter()
                        .find(|item| {
                            crate::ast::collect::effective_name(item).as_deref()
                                == Some(name.as_str())
                        })
                        .ok_or_else(|| {
                            Error::syntax(
                                "All sub queries in a UNION must have the same return column names",
                            )
                        })?;
                    mapped.push(ReturnItem {
                        alias: alias.clone().or_else(|| item.alias.clone()),
                        ..item.clone()
                    });
                }
                mapped
            }
        };
    }
    if let Some(union) = &mut body.union {
        map_final_return_items(&mut union.statement, map)?;
    }
    Ok(())
}

/// The procedure/function body's emitting RETURN (its last clause), when
/// the caller needs it; the authored error when the body does not end in
/// one.
fn final_return_clause(body: &Statement) -> Result<ReturnClause> {
    // The shared terminal-RETURN extraction (validate.rs's
    // `terminal_return`), cloned here -- the one probe, not a second
    // hand-spelling of the clauses.last() RETURN match.
    super::validate::terminal_return(body)
        .cloned()
        .ok_or_else(|| {
            Error::syntax("YIELD requires the local callable body to end in a RETURN clause")
        })
}

/// Whether the body's last clause is a RETURN (the mutable probe of
/// [`final_return_clause`]).
fn final_return_clause_mut(body: &mut Statement) -> Option<&mut ReturnClause> {
    match body.clauses.last_mut() {
        Some(Clause::Return(ret)) => Some(ret),
        _ => None,
    }
}

/// The 42N25-shaped authored error: a non-empty result type consumed
/// without YIELD.
fn yield_required_error() -> Error {
    Error::syntax(
        "Procedure call inside a query does not support naming results implicitly. \
         Use YIELD instead.",
    )
}

/// The yield mapping of a local procedure call: `YIELD *` keeps the
/// body's emitting RETURN as written; an explicit list renames the
/// RETURN's items in place (each yielded export name -> its yield alias,
/// in list order, dropping unyielded items); no YIELD at all requires the
/// empty result type (a FINISH-ending body). `Some` carries the mapped
/// items plus the export names they leave the body under.
/// The yield mapping: the per-arm selection pairs -- (export name to
/// select, applied alias) -- and the final (post-alias) names the
/// terminal re-exporting RETURN references.
type YieldMapping = (Vec<(String, Option<String>)>, Vec<String>);

fn yield_mapping(
    call: &CallProcedureClause,
    body_stmt: &Statement,
    exports: Option<&[String]>,
) -> Result<Option<YieldMapping>> {
    match (call.yields.mode(), exports) {
        (YieldMode::Star, _) => {
            // The terminal export names through the one derivation
            // (terminal_names): a body whose terminal projection has an
            // unnameable item is the same failure the UNION-arm check
            // reports, never a panic beside a handled form.
            let names = super::validate::terminal_names(body_stmt).ok_or_else(|| {
                Error::syntax("a procedure body's terminal RETURN must name every item".to_string())
            })?;
            // YIELD *: every body export, in projection order, unrenamed
            // (the same selection-pair shape the named form builds).
            let pairs: Vec<(String, Option<String>)> =
                names.iter().map(|name| (name.clone(), None)).collect();
            Ok(Some((pairs, names)))
        }
        (YieldMode::Named, Some(exports)) => {
            // The final-RETURN validation gates the named path the same way
            // (its error path); the items come from each arm at the mapping
            // site, not from this head RETURN.
            final_return_clause(body_stmt)?;
            let mut mapped_pairs = Vec::with_capacity(call.yields.items.len());
            let mut mapped_names = Vec::with_capacity(call.yields.items.len());
            for (yield_name, alias, final_name) in call.yields.pairs() {
                if !exports.iter().any(|export| export == yield_name) {
                    return Err(Error::syntax(format!(
                        "The procedure return column {yield_name} is not defined for this \
                         procedure. Verify that the spelling is correct."
                    )));
                }
                // The selection pair, not a head-stamped item: every UNION
                // arm matches its OWN item by export name (a stamped item
                // would stamp the head arm's expressions onto the other
                // arms -- rust round 7: `RETURN 1 AS a UNION RETURN 2 AS a`
                // yielded `1, 1`); the alias and the final (post-alias) name
                // both come from the one YieldList model.
                mapped_pairs.push((yield_name.to_owned(), alias.map(str::to_owned)));
                mapped_names.push(final_name.to_owned());
            }
            Ok(Some((mapped_pairs, mapped_names)))
        }
        (YieldMode::Named, None) => Err(yield_required_error()),
        (YieldMode::Default, Some(_)) => Err(yield_required_error()),
        (YieldMode::Default, None) => Ok(None),
    }
}
