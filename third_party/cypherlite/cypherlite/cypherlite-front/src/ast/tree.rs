//! The whole-tree walker family shared by the semantic validation passes:
//! the ADR-0257 nesting-depth guard (`super::validate_nesting_depth`), the
//! nested-statement validation battery (`super::validate_statement_tree`),
//! and the CIP-150 dialect gates ([`super::dialect`]) all ride
//! [`walk_statement_tree`]; the find-and-check validators that treat
//! subqueries as opaque leaves ride [`walk_expr`], a thin wrapper over the
//! same expression-walker core ([`walk_expr_at`]).

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr, LocalDefinitionBody, PatternElement, Statement};

use super::stmt_ref::StmtRef;

use super::search_spec::walk_search_spec;
use super::MAX_NESTING_DEPTH;

pub(crate) type StmtVisit<'v> = dyn FnMut(StmtRef<'_>) -> Result<(), EngineError> + 'v;
pub(crate) type ClauseVisit<'v> = dyn FnMut(&Clause) -> Result<(), EngineError> + 'v;
pub(crate) type ExprVisit<'v> = dyn FnMut(&Expr) -> Result<(), EngineError> + 'v;

/// Whether the walk recurses from an expression node (first argument) into
/// one of its expression children (second argument). The whole-tree walk
/// and plain [`walk_expr`] pass an always-true filter; a find-and-check
/// validator whose semantics deliberately stop at a node boundary the
/// walker would otherwise cross (a scope-introducing comprehension, a
/// dynamic property *key*, a function's non-expression argument, …) passes
/// a narrower one via [`walk_expr_with`] so the walk visits exactly the
/// nodes its hand-rolled predecessor reached. The filter gates only
/// expression→expression edges inside [`walk_expr_at`]; structural descents
/// (into patterns, label expressions, nested statements) are unaffected.
pub(crate) type ExprChildFilter<'v> = dyn Fn(&Expr, &Expr) -> bool + 'v;

/// The two ADR-0257 nesting counters the walker family threads through its
/// recursion, bundled into one `Copy` argument so no walker signature
/// carries a loose `depth`/`expr_depth` pair:
///
/// - `statement` — statement nesting: UNION arms, NEXT stages, `CALL { }` /
///   `WHEN`/`ELSE`/FOREACH bodies, and expression-embedded subquery bodies
///   each count one level. Capped in [`walk_statement_tree_at`].
/// - `expr` — the expression-nesting depth the expressions currently being
///   walked sit at: 0 for a clause-level expression of a top-level
///   statement, carried *across* expression-embedded subquery boundaries
///   (see [`walk_statement_tree_at`]). Capped in [`walk_expr_at`].
#[derive(Clone, Copy)]
pub(crate) struct Depths {
    pub(crate) statement: usize,
    pub(crate) expr: usize,
}

impl Depths {
    /// The top-level entry counts: no enclosing statement or expression.
    const ZERO: Self = Depths {
        statement: 0,
        expr: 0,
    };

    /// One statement level deeper (a UNION arm, NEXT stage, or nested
    /// body), inheriting the entry expression depth: lowering lowers
    /// arms/stages/clause-level bodies from the same activation, with no
    /// enclosing expression frames of their own.
    fn deeper_statement(self) -> Self {
        Depths {
            statement: self.statement + 1,
            ..self
        }
    }

    /// One expression level deeper (an expression child, or the contained
    /// expressions of a pattern/label expression embedded at this node).
    fn deeper_expr(self) -> Self {
        Depths {
            expr: self.expr + 1,
            ..self
        }
    }

    /// An expression-embedded subquery body: one statement level deeper
    /// *and* one past the enclosing expression's depth, because
    /// `lower_expr` lowers `EXISTS`/`COUNT`/`COLLECT { ... }` bodies from
    /// inside its own recursion, so the body's expressions nest on top of
    /// the enclosing expression's frames on the same native stack.
    fn deeper_subquery(self) -> Self {
        Depths {
            statement: self.statement + 1,
            expr: self.expr + 1,
        }
    }
}

/// The three visitors a whole-tree walk carries, bundled so the walker
/// family passes one argument instead of a repeated parameter triple, plus
/// the walk's [`SubqueryDescent`] mode and expression-child edge filter.
pub(crate) struct TreeWalk<'a, 'v> {
    pub(crate) stmt: &'a mut StmtVisit<'v>,
    pub(crate) clause: &'a mut ClauseVisit<'v>,
    pub(crate) expr: &'a mut ExprVisit<'v>,
    pub(crate) descend: SubqueryDescent,
    pub(crate) enter_expr_child: &'a ExprChildFilter<'v>,
}

/// Whether the walk enters subquery statements and pattern-embedded
/// expressions, or treats them as opaque leaves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubqueryDescent {
    /// [`walk_expr`] semantics: `EXISTS`/`COUNT`/`COLLECT { ... }` bodies,
    /// clause-level nested bodies (`CALL { }` / `WHEN` / `ELSE` / FOREACH),
    /// the patterns of `shortestPath()`/pattern predicates/pattern
    /// comprehensions, and dynamic-label expressions are leaves.
    Opaque,
    /// Whole-tree semantics: nested statements are entered one statement
    /// level deeper (expression-embedded subqueries with the expression
    /// count carried across the boundary), and expression-embedded patterns
    /// and dynamic labels are walked.
    Enter,
}

/// Unlike [`walk_expr`] (which treats subquery expressions as opaque
/// leaves), this walk reaches *inside* every nested statement: UNION arms,
/// CIP-213 NEXT stages, `CALL { }` and `WHEN`/`ELSE` bodies, FOREACH clause
/// lists (presented to the statement visitor as a borrowed [`StmtRef::Body`],
/// since a FOREACH body is a clause list rather than a `Statement` in the
/// AST), and `EXISTS`/`COUNT`/`COLLECT` expression subqueries. The ADR-0257
/// statement-nesting cap fires structurally inside [`walk_statement_tree_at`]
/// — before the statement visitor, so the walk early-exits at the limit + 1
/// and no [`StmtVisit`] closure can opt out of it — and the walk carries the
/// expression-nesting depth the statement is *entered* at (see
/// [`walk_statement_tree_at`]). The walk is pre-order and stops at the
/// first `Err`.
pub(crate) fn walk_statement_tree(
    stmt: &Statement,
    visit_stmt: &mut StmtVisit,
    visit_clause: &mut ClauseVisit,
    visit_expr: &mut ExprVisit,
) -> Result<(), EngineError> {
    let enter_all = |_: &Expr, _: &Expr| true;
    let mut walk = TreeWalk {
        stmt: visit_stmt,
        clause: visit_clause,
        expr: visit_expr,
        descend: SubqueryDescent::Enter,
        enter_expr_child: &enter_all,
    };
    walk_statement_tree_at(StmtRef::Real(stmt), Depths::ZERO, &mut walk)
}

/// The ADR-0257 depth-cap error, shared by the statement check
/// ([`walk_statement_tree_at`]) and the expression check ([`walk_expr_at`])
/// so both produce the identical message.
fn nesting_depth_error() -> EngineError {
    EngineError::semantic(
        ErrorKind::SyntaxError,
        format!("maximum nesting depth of {MAX_NESTING_DEPTH} exceeded"),
    )
}

/// The statement-nesting half of the ADR-0257 cap fires at the head of
/// this function (see the check inside), so every whole-tree walk enforces
/// it regardless of what the `StmtVisit` closure does.
///
/// `depths.expr` is the expression-nesting depth the statement's own clause
/// expressions sit at — the `lower_expr` frames live on the native stack
/// when lowering reaches this statement. It is 0 for the top-level
/// statement and is inherited unchanged by clause-level nested statements
/// (`CALL { }` / `WHEN` / `ELSE` / FOREACH bodies, UNION arms, NEXT
/// stages): lowering lowers a statement's clauses sequentially, so a nested
/// clause body is entered with exactly the frames the enclosing statement
/// was entered with, whatever sibling expressions peaked at. Only
/// expression-embedded subquery bodies (`EXISTS`/`COUNT`/`COLLECT { ... }`)
/// are entered deeper — one past the enclosing expression's depth — because
/// `lower_expr` lowers those bodies from *inside* its own recursion
/// (`crate::lower::lower_expr`'s subquery arms call `lower_statement_impl`
/// directly), so their expressions nest on top of the enclosing
/// expression's frames on the same native stack. Resetting the count at
/// that boundary let statement depth × per-statement expression depth
/// multiply past the native-stack budget (3 nested `collect { }` levels ×
/// 450 subscript hops composed to ~1350 uncapped `lower_expr` frames and
/// SIGABRT'd on an 8 MiB release stack); carrying it makes the walk count
/// exactly the frames lowering will consume, so the combined composed depth
/// stays bounded by [`MAX_NESTING_DEPTH`].
fn walk_statement_tree_at(
    stmt: StmtRef<'_>,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    // The ADR-0257 statement-nesting cap, structural in the walker core:
    // it fires before the statement visitor (so the recursion early-exits
    // at the limit + 1) and there is no way to walk a statement tree
    // without it.
    if depths.statement > MAX_NESTING_DEPTH {
        return Err(nesting_depth_error());
    }
    (walk.stmt)(stmt)?;
    walk_clauses_tree(stmt.clauses(), depths, walk)?;
    // CIP-231: a `DEFINE` block heads this statement; its definition bodies
    // are nested statements (and expressions) of the same tree, so every
    // whole-tree validator covers them -- a rule violation cannot hide
    // inside a local callable body. The ADR-0257 depth cap above recurses
    // through here as well, so definition nesting is capped on the walk
    // side too.
    for definition in stmt.definitions() {
        match &definition.body {
            LocalDefinitionBody::Expression(expr) => {
                walk_expr_at(expr, depths, walk)?;
            }
            LocalDefinitionBody::Query(body) => {
                walk_statement_tree_at(StmtRef::Real(body), depths.deeper_statement(), walk)?;
            }
        }
    }
    // UNION arms and NEXT stages are chained statements; each link nests one
    // level deeper, since the walkers that consume them (this one included)
    // recurse per link. They inherit this statement's entry expression
    // depth: lowering lowers arms/stages from the same activation, with no
    // enclosing expression frames of their own.
    if let Some(union) = stmt.union() {
        walk_statement_tree_at(
            StmtRef::UnionArm(&union.statement),
            depths.deeper_statement(),
            walk,
        )?;
    }
    if let Some(next) = stmt.next() {
        walk_statement_tree_at(StmtRef::NextStage(next), depths.deeper_statement(), walk)?;
    }
    Ok(())
}

fn walk_clauses_tree(
    clauses: &[Clause],
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    for clause in clauses {
        (walk.clause)(clause)?;
        walk_clause_exprs_tree(clause, depths, walk)?;
    }
    Ok(())
}

fn walk_clause_exprs_tree(
    clause: &Clause,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    match clause {
        Clause::Match(mc) => {
            for path in &mc.pattern {
                walk_path_exprs_tree(path, depths, walk)?;
            }
            if let Some(search) = &mc.search {
                // Every expression of the (possibly nested, CIP-278) source
                // specification, through the one shared enumeration --
                // straight into the depth walk, an over-deep expression
                // stopping it.
                walk_search_spec(&search.spec, &mut |_| Ok(()), &mut |_, expr| {
                    walk_expr_at(expr, depths, walk)
                })?;
            }
            if let Some(w) = &mc.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
        Clause::Create(cc) => {
            for path in &cc.pattern {
                walk_path_exprs_tree(path, depths, walk)?;
            }
        }
        Clause::Merge(mc) => {
            walk_path_exprs_tree(&mc.pattern, depths, walk)?;
            for item in mc.on_create.iter().chain(mc.on_match.iter()) {
                walk_set_item_exprs_tree(item, depths, walk)?;
            }
        }
        Clause::Set(sc) => {
            for item in &sc.items {
                walk_set_item_exprs_tree(item, depths, walk)?;
            }
        }
        Clause::Remove(rc) => {
            for item in &rc.items {
                if let Some(p) = &item.property {
                    walk_expr_at(&p.expr, depths, walk)?;
                    if let Some(k) = &p.key_expr {
                        walk_expr_at(k, depths, walk)?;
                    }
                }
                for label in &item.labels {
                    walk_label_expr_tree(label, depths, walk)?;
                }
            }
        }
        Clause::Delete(dc) => {
            for e in &dc.expressions {
                walk_expr_at(e, depths, walk)?;
            }
        }
        Clause::Return(rc) => {
            walk_projection_exprs_tree(
                &rc.items,
                &rc.order_by,
                rc.skip.as_ref(),
                rc.limit.as_ref(),
                depths,
                walk,
            )?;
        }
        Clause::With(wc) => {
            walk_projection_exprs_tree(
                &wc.items,
                &wc.order_by,
                wc.skip.as_ref(),
                wc.limit.as_ref(),
                depths,
                walk,
            )?;
            if let Some(w) = &wc.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
        Clause::Unwind(uw) => {
            walk_expr_at(&uw.expr, depths, walk)?;
        }
        Clause::CallProcedure(cp) => {
            for arg in &cp.args {
                walk_expr_at(arg, depths, walk)?;
            }
            if let Some(w) = &cp.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
        Clause::CallSubquery(csq) => {
            if let Some(cit) = &csq.in_transactions {
                if let cypherlite_parser::Concurrency::Value(e) = &cit.concurrency {
                    walk_expr_at(e, depths, walk)?;
                }
                if let Some(b) = &cit.batch_size {
                    walk_expr_at(b, depths, walk)?;
                }
                if let cypherlite_parser::OnErrorMode::Retry {
                    duration: Some(d), ..
                } = &cit.on_error
                {
                    walk_expr_at(d, depths, walk)?;
                }
            }
            if walk.descend == SubqueryDescent::Enter {
                walk_statement_tree_at(
                    StmtRef::Real(&csq.statement),
                    depths.deeper_statement(),
                    walk,
                )?;
            }
        }
        Clause::StandaloneOrderBy(page) => {
            walk_page_stages_tree(&page.stages, depths, walk)?;
        }
        Clause::Foreach(f) => {
            walk_expr_at(&f.list_expr, depths, walk)?;
            // A FOREACH body is a clause list, not a `Statement`; present it
            // to the statement visitor as a borrowed body one nesting level
            // deeper so per-statement validators (and the depth cap) cover
            // it. It inherits this statement's entry expression depth: the
            // list expression has returned by the time lowering enters the
            // body.
            if walk.descend == SubqueryDescent::Enter {
                walk_statement_tree_at(StmtRef::Body(&f.clauses), depths.deeper_statement(), walk)?;
            }
        }
        Clause::Conditional(cc) => {
            for branch in &cc.branches {
                walk_expr_at(&branch.predicate, depths, walk)?;
                if walk.descend == SubqueryDescent::Enter {
                    walk_statement_tree_at(
                        StmtRef::Real(&branch.body),
                        depths.deeper_statement(),
                        walk,
                    )?;
                }
            }
            if walk.descend == SubqueryDescent::Enter {
                if let Some(else_body) = &cc.else_body {
                    walk_statement_tree_at(
                        StmtRef::Real(else_body),
                        depths.deeper_statement(),
                        walk,
                    )?;
                }
            }
        }
        Clause::Use(uc) => {
            walk_expr_at(&uc.graph_expr, depths, walk)?;
        }
        Clause::LoadCSV(lc) => {
            walk_expr_at(&lc.source, depths, walk)?;
        }
        Clause::CreateIndex(ix) => {
            for (_, v) in &ix.options {
                walk_expr_at(v, depths, walk)?;
            }
        }
        Clause::CreateConstraint(cc) => {
            for (_, v) in &cc.options {
                walk_expr_at(v, depths, walk)?;
            }
        }
        // The three catalog commands carrying an extra filter expression
        // the descriptor does not model; the shared WHERE/stages walk below
        // covers every other catalog command.
        Clause::ShowSettings(c) => {
            if let Some(names) = &c.names {
                walk_expr_at(names, depths, walk)?;
            }
            walk_show_where_tree(&c.where_expr, depths, walk)?;
        }
        Clause::ShowTransactions(c) => {
            for id in &c.ids {
                walk_expr_at(id, depths, walk)?;
            }
            walk_show_where_tree(&c.where_expr, depths, walk)?;
        }
        Clause::TerminateTransactions(c) => {
            for id in &c.ids {
                walk_expr_at(id, depths, walk)?;
            }
        }
        // Every other catalog command, through the one descriptor: its
        // WHERE and YIELD paging stages are the only expressions it carries,
        // so a new SHOW command's WHERE cannot escape the depth walk.
        clause if crate::show_columns::catalog_info(clause).is_some() => {
            let info = crate::show_columns::catalog_info(clause)
                .expect("the guard above established a catalog clause");
            if let Some(w) = info.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
            walk_page_stages_tree(info.stages, depths, walk)?;
        }
        // The catalog variants listed here are already handled by the
        // guarded arm above; a guard does not satisfy the exhaustiveness
        // checker, so they are restated (unreachable at run time) to keep
        // the match total.
        Clause::ShowFunctions(_)
        | Clause::ShowConstraints(_)
        | Clause::ShowIndexes(_)
        | Clause::ShowDatabases(_)
        | Clause::ShowRoles(_)
        | Clause::ShowServers(_)
        | Clause::ShowUsers(_)
        | Clause::ShowProcedures(_)
        | Clause::ShowCurrentGraphType(_)
        | Clause::ShowGraphType(_)
        | Clause::CreateGraphType(_)
        | Clause::AlterCurrentGraphType(_)
        | Clause::AlterDatabase(_)
        | Clause::DropIndex(_)
        | Clause::DropConstraint(_)
        | Clause::Finish => {}
    }
    Ok(())
}

fn walk_show_where_tree(
    where_expr: &Option<Expr>,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    if let Some(w) = where_expr {
        walk_expr_at(w, depths, walk)?;
    }
    Ok(())
}

fn walk_projection_exprs_tree(
    items: &[cypherlite_parser::ReturnItem],
    order_by: &[cypherlite_parser::OrderByItem],
    skip: Option<&Expr>,
    limit: Option<&Expr>,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    for item in items {
        if !item.all {
            walk_expr_at(&item.expr, depths, walk)?;
        }
    }
    for ob in order_by {
        walk_expr_at(&ob.expr, depths, walk)?;
    }
    if let Some(s) = skip {
        walk_expr_at(s, depths, walk)?;
    }
    if let Some(l) = limit {
        walk_expr_at(l, depths, walk)?;
    }
    Ok(())
}

fn walk_page_stages_tree(
    stages: &[cypherlite_parser::PageStage],
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    for stage in stages {
        match stage {
            cypherlite_parser::PageStage::OrderBy(items) => {
                for ob in items {
                    walk_expr_at(&ob.expr, depths, walk)?;
                }
            }
            cypherlite_parser::PageStage::Skip(e) | cypherlite_parser::PageStage::Limit(e) => {
                walk_expr_at(e, depths, walk)?;
            }
        }
    }
    Ok(())
}

fn walk_set_item_exprs_tree(
    item: &cypherlite_parser::SetItem,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    if let Some(p) = &item.property {
        walk_expr_at(&p.expr, depths, walk)?;
        if let Some(k) = &p.key_expr {
            walk_expr_at(k, depths, walk)?;
        }
    }
    if let Some((target, _)) = &item.labels_function {
        walk_expr_at(target, depths, walk)?;
    }
    for label in &item.labels {
        walk_label_expr_tree(label, depths, walk)?;
    }
    if let Some(v) = &item.value {
        walk_expr_at(v, depths, walk)?;
    }
    Ok(())
}

/// `depths.expr` is the expression-nesting depth the path's contained
/// expressions sit at: 0 for a clause-level pattern, one past the enclosing
/// expression's depth for a pattern embedded in an expression (a pattern
/// comprehension, pattern predicate, or shortestPath()), since lowering
/// inlines those expressions into the enclosing `lower_expr` recursion.
fn walk_path_exprs_tree(
    path: &cypherlite_parser::PatternPath,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    for elem in &path.elements {
        walk_element_exprs_tree(elem, depths, walk)?;
    }
    if let Some(w) = &path.where_expr {
        walk_expr_at(w, depths, walk)?;
    }
    Ok(())
}

fn walk_element_exprs_tree(
    elem: &PatternElement,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    match elem {
        PatternElement::Node(np) => {
            if let Some(le) = &np.label_expr {
                walk_label_expr_tree(le, depths, walk)?;
            }
            for (_, v) in &np.properties {
                walk_expr_at(v, depths, walk)?;
            }
            if let Some(p) = &np.properties_param {
                walk_expr_at(p, depths, walk)?;
            }
            if let Some(w) = &np.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
        PatternElement::Relationship(rp) => {
            if let Some(te) = &rp.type_expr {
                walk_label_expr_tree(te, depths, walk)?;
            }
            for (_, v) in &rp.properties {
                walk_expr_at(v, depths, walk)?;
            }
            if let Some(p) = &rp.properties_param {
                walk_expr_at(p, depths, walk)?;
            }
            if let Some(w) = &rp.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
        PatternElement::Quantified(qpp) => {
            for inner in &qpp.elements {
                walk_element_exprs_tree(inner, depths, walk)?;
            }
            if let Some(w) = &qpp.where_expr {
                walk_expr_at(w, depths, walk)?;
            }
        }
    }
    Ok(())
}

/// `depths.expr` follows the same convention as [`walk_path_exprs_tree`]:
/// the depth a contained dynamic-label expression (`$(expr)`) sits at.
fn walk_label_expr_tree(
    label: &cypherlite_parser::LabelExpr,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    match label {
        cypherlite_parser::LabelExpr::Dynamic(d) => {
            walk_expr_at(&d.expr, depths, walk)?;
        }
        cypherlite_parser::LabelExpr::Not(inner) => {
            walk_label_expr_tree(inner, depths, walk)?;
        }
        cypherlite_parser::LabelExpr::And(items) | cypherlite_parser::LabelExpr::Or(items) => {
            for item in items {
                walk_label_expr_tree(item, depths, walk)?;
            }
        }
        cypherlite_parser::LabelExpr::Label(_) | cypherlite_parser::LabelExpr::Wildcard => {}
    }
    Ok(())
}

/// The one expression-walker core: a pre-order walk where `walk.expr` runs
/// on every node before its children and an `Err` stops the walk.
/// `walk.descend` selects between the two historical walkers that each
/// re-encoded this same child-descent boilerplate:
///
/// - [`SubqueryDescent::Enter`] (under [`walk_statement_tree`]): the
///   whole-tree walker. Subquery expressions (`EXISTS`/`COUNT`/`COLLECT
///   { ... }`) recurse into their nested statement so statement-level
///   visitors see those bodies too, and the walk descends into the
///   patterns of pattern comprehensions, pattern predicates and
///   `shortestPath()` (via [`walk_path_exprs_tree`]), so subquery
///   statements and rule violations cannot hide inside a pattern embedded
///   in an expression.
///
/// - [`SubqueryDescent::Opaque`] (via [`walk_expr`]): the dialect-family
///   walker the find-and-check validators build on. It descends into every
///   *expression* child — including CASE, comprehension, reduce, quantifier
///   and map-projection bodies — but never into a subquery expression's
///   nested `Statement` or a pattern-bearing leaf: those validators treat
///   `EXISTS/COUNT/COLLECT { ... }` as opaque leaves (a validator that must
///   look inside a subquery, like the dialect gates in [`super::dialect`],
///   runs on every nested statement through the whole-tree walk instead).
///
/// Either way, every expression node along the walk counts one level of
/// expression nesting against [`MAX_NESTING_DEPTH`]: the parser's postfix
/// loop builds `.key` property chains, subscripts and map projections
/// *iteratively*, so its own depth counter never sees them — without this
/// accounting `lower_expr` recursed one native frame per hop and an
/// over-cap chain overflowed the stack (a 16000-hop property chain
/// SIGABRT'd on an 8 MiB release stack). The count starts at 0 for each
/// clause-level expression of a top-level statement, continues into
/// expression-embedded patterns and dynamic-label expressions (which
/// lowering inlines), and — in `Enter` mode — is *carried* into
/// expression-embedded subquery bodies: `lower_expr` lowers an
/// `EXISTS`/`COUNT`/`COLLECT { ... }` body from inside its own recursion,
/// so the body's expressions consume native frames on top of the enclosing
/// expression's — resetting the count there let statement depth and
/// per-statement expression depth multiply past the stack budget (see
/// [`walk_statement_tree_at`]). `Opaque` walks start the count at 0 too,
/// which keeps every consumer's own recursion bounded by the same cap.
fn walk_expr_at(
    expr: &Expr,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    if depths.expr > MAX_NESTING_DEPTH {
        return Err(nesting_depth_error());
    }
    (walk.expr)(expr)?;
    match expr {
        Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::CountStar
        | Expr::Star => {}
        // CIP-261: interpolation embeds are full expressions; CIP-272: a
        // map comprehension's source/key/value are walked like any other
        // comprehension's children.
        Expr::StringInterp(parts) => {
            for part in parts {
                if let cypherlite_parser::InterpPart::Interp(e) = part {
                    walk_expr_child(expr, e, depths.deeper_expr(), walk)?;
                }
            }
        }
        Expr::MapComprehension(mc) => {
            walk_expr_child(expr, &mc.source_expr, depths.deeper_expr(), walk)?;
            if let Some(w) = &mc.where_expr {
                walk_expr_child(expr, w, depths.deeper_expr(), walk)?;
            }
            walk_expr_child(expr, &mc.key_expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &mc.value_expr, depths.deeper_expr(), walk)?;
        }
        Expr::ShortestPath(s) => {
            if walk.descend == SubqueryDescent::Enter {
                walk_path_exprs_tree(&s.pattern, depths.deeper_expr(), walk)?;
            }
        }
        Expr::PatternPredicate(p) => {
            if walk.descend == SubqueryDescent::Enter {
                walk_path_exprs_tree(&p.pattern, depths.deeper_expr(), walk)?;
            }
        }
        Expr::ExistsSubquery(s) => {
            if walk.descend == SubqueryDescent::Enter {
                // Expression-embedded subquery: lowering lowers the body from
                // inside `lower_expr`'s recursion, so the body's expressions
                // sit one past this node's depth on the same native stack —
                // carry the count across the statement boundary instead of
                // resetting.
                walk_statement_tree_at(
                    StmtRef::Real(&s.statement),
                    depths.deeper_subquery(),
                    walk,
                )?;
            }
        }
        Expr::CountSubquery(s) => {
            if walk.descend == SubqueryDescent::Enter {
                walk_statement_tree_at(
                    StmtRef::Real(&s.statement),
                    depths.deeper_subquery(),
                    walk,
                )?;
            }
        }
        Expr::CollectSubquery(s) => {
            if walk.descend == SubqueryDescent::Enter {
                walk_statement_tree_at(
                    StmtRef::Real(&s.statement),
                    depths.deeper_subquery(),
                    walk,
                )?;
            }
        }
        Expr::List(items) => {
            for e in items {
                walk_expr_child(expr, e, depths.deeper_expr(), walk)?;
            }
        }
        Expr::Map(pairs) => {
            for p in pairs {
                walk_expr_child(expr, &p.value, depths.deeper_expr(), walk)?;
            }
        }
        Expr::Property(p) => {
            walk_expr_child(expr, &p.expr, depths.deeper_expr(), walk)?;
            if let Some(k) = &p.key_expr {
                walk_expr_child(expr, k, depths.deeper_expr(), walk)?;
            }
        }
        Expr::FunctionCall(fc) => {
            for a in &fc.args {
                walk_expr_child(expr, a, depths.deeper_expr(), walk)?;
            }
        }
        Expr::Binary(b) => {
            walk_expr_child(expr, &b.left, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &b.right, depths.deeper_expr(), walk)?;
        }
        Expr::Unary(u) => walk_expr_child(expr, &u.expr, depths.deeper_expr(), walk)?,
        Expr::Comparison(c) => {
            walk_expr_child(expr, &c.left, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &c.right, depths.deeper_expr(), walk)?;
        }
        Expr::In(ie) => {
            walk_expr_child(expr, &ie.expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &ie.list, depths.deeper_expr(), walk)?;
        }
        Expr::IsNull(x) => walk_expr_child(expr, &x.expr, depths.deeper_expr(), walk)?,
        Expr::IsType(x) => walk_expr_child(expr, &x.expr, depths.deeper_expr(), walk)?,
        Expr::IsNormalized(x) => walk_expr_child(expr, &x.expr, depths.deeper_expr(), walk)?,
        Expr::StringMatch(sm) => {
            walk_expr_child(expr, &sm.left, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &sm.right, depths.deeper_expr(), walk)?;
        }
        Expr::Case(c) => {
            if let Some(t) = &c.test {
                walk_expr_child(expr, t, depths.deeper_expr(), walk)?;
            }
            for w in &c.whens {
                walk_expr_child(expr, &w.condition, depths.deeper_expr(), walk)?;
                walk_expr_child(expr, &w.result, depths.deeper_expr(), walk)?;
            }
            if let Some(e) = &c.else_expr {
                walk_expr_child(expr, e, depths.deeper_expr(), walk)?;
            }
        }
        Expr::ListComprehension(lc) => {
            walk_expr_child(expr, &lc.list_expr, depths.deeper_expr(), walk)?;
            if let Some(w) = &lc.where_expr {
                walk_expr_child(expr, w, depths.deeper_expr(), walk)?;
            }
            if let Some(m) = &lc.map_expr {
                walk_expr_child(expr, m, depths.deeper_expr(), walk)?;
            }
        }
        Expr::PatternComprehension(pc) => {
            if walk.descend == SubqueryDescent::Enter {
                walk_path_exprs_tree(&pc.pattern, depths.deeper_expr(), walk)?;
            }
            if let Some(w) = &pc.where_expr {
                walk_expr_child(expr, w, depths.deeper_expr(), walk)?;
            }
            walk_expr_child(expr, &pc.map_expr, depths.deeper_expr(), walk)?;
        }
        Expr::Reduce(r) => {
            walk_expr_child(expr, &r.init_expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &r.list_expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &r.expression, depths.deeper_expr(), walk)?;
        }
        Expr::AllReduce(r) => {
            walk_expr_child(expr, &r.init_expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &r.list_expr, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &r.expression, depths.deeper_expr(), walk)?;
            walk_expr_child(expr, &r.predicate, depths.deeper_expr(), walk)?;
        }
        Expr::Subscript(s) => {
            walk_expr_child(expr, &s.expr, depths.deeper_expr(), walk)?;
            if let Some(i) = &s.index {
                walk_expr_child(expr, i, depths.deeper_expr(), walk)?;
            }
            if let Some(f) = &s.from {
                walk_expr_child(expr, f, depths.deeper_expr(), walk)?;
            }
            if let Some(t) = &s.to {
                walk_expr_child(expr, t, depths.deeper_expr(), walk)?;
            }
        }
        Expr::Quantifier(q) => {
            walk_expr_child(expr, &q.list, depths.deeper_expr(), walk)?;
            if let Some(p) = &q.predicate {
                walk_expr_child(expr, p, depths.deeper_expr(), walk)?;
            }
        }
        Expr::HasLabel(hl) => {
            walk_expr_child(expr, &hl.expr, depths.deeper_expr(), walk)?;
            if walk.descend == SubqueryDescent::Enter {
                walk_label_expr_tree(&hl.label_expr, depths.deeper_expr(), walk)?;
            }
        }
        Expr::Paren(inner) => walk_expr_child(expr, inner, depths.deeper_expr(), walk)?,
        Expr::MapProjection(mp) => {
            walk_expr_child(expr, &mp.expr, depths.deeper_expr(), walk)?;
            for item in &mp.items {
                if let cypherlite_parser::MapProjectionItem::KeyValue(_, v) = item {
                    walk_expr_child(expr, v, depths.deeper_expr(), walk)?;
                }
            }
        }
    }
    Ok(())
}

/// Recurse from `parent` into its expression child `child` when the walk's
/// [`ExprChildFilter`] admits the edge; otherwise the child's whole subtree
/// is skipped (but the walk continues with the parent's remaining
/// children). Only expression→expression edges are filtered; the structural
/// descents (patterns, label expressions, nested statements) are not.
fn walk_expr_child(
    parent: &Expr,
    child: &Expr,
    depths: Depths,
    walk: &mut TreeWalk<'_, '_>,
) -> Result<(), EngineError> {
    if (walk.enter_expr_child)(parent, child) {
        walk_expr_at(child, depths, walk)?;
    }
    Ok(())
}

/// Generic expression walker: one pre-order traversal of an expression tree,
/// replacing the family of near-identical hand-rolled recursions that each
/// re-encoded the same child-descent boilerplate. `visit` runs on every node
/// before its children; an `Err` stops the walk and propagates (early exit).
///
/// It descends into every *expression* child — including CASE, comprehension,
/// reduce, quantifier and map-projection bodies — but never into a subquery
/// expression's nested `Statement`: the find-and-check validators built on it
/// treat `EXISTS/COUNT/COLLECT { ... }` as opaque leaves. A validator that
/// must look inside a subquery (like the dialect gates in [`super::dialect`])
/// runs on every nested statement through [`walk_statement_tree`] instead, or
/// keeps a bespoke traversal; a validator that must deliberately *not*
/// descend into a scope-introducing construct rides [`walk_expr_with`] with
/// an edge filter matching its historical reach, or keeps its own traversal.
/// Expression nesting is capped at [`MAX_NESTING_DEPTH`] from the root `expr`
/// (see [`walk_expr_at`]), so the walk's own recursion stays bounded.
pub(crate) fn walk_expr<F>(expr: &Expr, visit: &mut F) -> Result<(), EngineError>
where
    F: FnMut(&Expr) -> Result<(), EngineError>,
{
    walk_expr_with(expr, visit, |_, _| true)
}

/// [`walk_expr`] with an [`ExprChildFilter`]: the walk recurses from an
/// expression node into one of its expression children only when
/// `enter_child(parent, child)` returns true, so a find-and-check validator
/// whose historical hand-rolled recursion stopped at specific node
/// boundaries (a dynamic property key, a scope-introducing comprehension, a
/// function's non-expression argument, …) can ride the shared self-capping
/// walker while visiting exactly the node set its predecessor did. The root
/// `expr` is always visited; everything else is unchanged from
/// [`walk_expr`] (pre-order, early exit on `Err`, opaque subqueries,
/// [`MAX_NESTING_DEPTH`] cap).
pub(crate) fn walk_expr_with<F, G>(
    expr: &Expr,
    visit: &mut F,
    enter_child: G,
) -> Result<(), EngineError>
where
    F: FnMut(&Expr) -> Result<(), EngineError>,
    G: Fn(&Expr, &Expr) -> bool,
{
    let mut skip_stmt = |_: StmtRef<'_>| Ok(());
    let mut skip_clause = |_: &Clause| Ok(());
    let mut walk = TreeWalk {
        stmt: &mut skip_stmt,
        clause: &mut skip_clause,
        expr: visit,
        descend: SubqueryDescent::Opaque,
        enter_expr_child: &enter_child,
    };
    walk_expr_at(expr, Depths::ZERO, &mut walk)
}

#[cfg(test)]
pub(crate) mod tests;
