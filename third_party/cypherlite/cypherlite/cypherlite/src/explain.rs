//! `EXPLAIN` / `PROFILE` plan introspection (GitHub issue #14).
//!
//! The plan is returned as the structured tree (ADR-0359: text is
//! rendered at the boundary, never in the engine):
//!
//! - `EXPLAIN <statement>` plans the statement and stamps the structured
//!   tree on `QueryResult.plan`, with no result columns or rows of its
//!   own. Execution never touches the wrapped statement: no writes,
//!   no procedures, no `LOAD CSV`.
//! - `PROFILE <statement>` executes the statement through the normal
//!   dispatch and returns the query's own rows and columns unchanged,
//!   with the structured plan tree — per-operator metrics (rows produced,
//!   elapsed time) on every clause operator the dispatch loop executed —
//!   stamped beside them on `QueryResult.plan` (ADR-0344/0345 phase 2;
//!   the deprecated `_profile_*` columns are removed).
//!
//! Both builders also stamp the query notifications (ADR-0363) derived
//! from the same tree and plan. Every statement class the front-end lowers
//! yields at least one plan node; a statement that cannot be planned fails
//! with the planner's typed error instead of a silently empty table.

use std::time::Duration;

use cypherlite_executor::ExecGraph;
use cypherlite_model::wire::Notification;
use cypherlite_planner::{LogicalPlan, QueryType};

use crate::plan_node;
use crate::statement_notifications;
use crate::{QueryResult, ResultHeader};
use cypherlite_core::Namespace;

/// The wrapped statement as the plan header echoes it (ADR-0359): the
/// parse classified the preamble (`parsed.explain`/`parsed.profile`), so
/// the engine strips the one classified keyword token here, once, at the
/// producer — the boundary reads the stamped text and never re-scans for
/// the keyword. `keyword` is the classified token (`EXPLAIN`/`PROFILE`).
pub(crate) fn statement_echo(cypher: &str, keyword: &str) -> String {
    let mut stripped = false;
    cypher
        .split_whitespace()
        .filter(|token| {
            if !stripped && token.eq_ignore_ascii_case(keyword) {
                stripped = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `EXPLAIN <statement>`: the plan tree; the statement is not executed.
/// `query_type` is the wrapped statement's plan-derived type (ADR-0162),
/// stamped onto the result like the namespace.
pub(crate) fn explain_result(
    plan: &LogicalPlan,
    namespace: Namespace,
    query_type: QueryType,
    elapsed: Duration,
    stats: &crate::stats::GraphPatternStats,
    statement_echo: String,
    view: &ExecGraph<'_>,
) -> QueryResult {
    // The structured tree (ADR-0359): the plan rides `QueryResult.plan`
    // alone — the rendered plan column is gone, so every surface renders
    // its own text through `cypherlite-formatter` on demand.
    let tree = plan_node::plan_tree(plan, stats).with_statement_echo(statement_echo);
    // The notifications (ADR-0363) derive from the same tree and the plan —
    // the one place a tree is built — never on the plain path. The tree walk
    // reads the staged facts the construction walk recorded beside it, so
    // the handed-out tree stays derivable.
    let notifications = notifications(&tree, plan, view);
    // A plan-producing run executed nothing (ADR-0402 Decision 2): the one
    // construction for a non-executed result names that effect itself, so no
    // construction site spells the placeholder.
    QueryResult::not_executed(ResultHeader::new(
        Vec::new(),
        &[],
        query_type,
        namespace,
        elapsed,
    ))
    .with_plan(tree.root, notifications)
}

pub(crate) fn profile_result(
    plan: &LogicalPlan,
    metrics: &crate::profile::ProfileMetrics,
    result: QueryResult,
    stats: &crate::stats::GraphPatternStats,
    statement_echo: String,
    view: &ExecGraph<'_>,
) -> QueryResult {
    // ADR-0344/0345 phase 2: the structured tree with per-operator
    // metrics rides the result beside the statement's own rows — the
    // deprecated `_profile_*` columns are gone (ADR-0345's sanctioned
    // removal), their information strictly exceeded by the tree.
    let tree = plan_node::profiled_tree(plan, Some(&metrics.collected()), stats)
        .with_statement_echo(statement_echo);
    // ADR-0363: the list derives from the same profiled tree, so the
    // PROFILE-only `EstimationMiss` (stamped beside the metrics) rides it,
    // and from the same plan, so the statement-scoped kinds do too.
    let notifications = notifications(&tree, plan, view);
    // The columns and rows are the statement's own — the ADR-0224 stamp
    // stays valid; the tree carries the profile facts (per-operator
    // time, so no wall-clock cell is appended).
    result.with_plan(tree.root, notifications)
}

/// The one notification derivation (ADR-0363 decisions 3 and 6): the
/// tree's operator-scoped entries merged with the plan's statement-scoped
/// entries. Both EXPLAIN and PROFILE call it, so the two builders cannot
/// order or deduplicate the list differently.
fn notifications(
    tree: &plan_node::PlanTree,
    plan: &LogicalPlan,
    view: &ExecGraph<'_>,
) -> Vec<Notification> {
    statement_notifications::merge(
        plan_node::tree_notifications(tree),
        statement_notifications::statement_notifications(plan, view),
    )
}
