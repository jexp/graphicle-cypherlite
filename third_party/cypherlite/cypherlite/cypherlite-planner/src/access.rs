//! The one read/write/schema fold over a statement's clauses, shared by
//! the ADR-0116 §4 namespace write gate ([`crate::namespaces::is_write`])
//! and the Neo4j Query API v2 `queryType` classifier
//! ([`crate::query_type::query_type`]): both are projections of
//! [`plan_access`], so their traversal coverage and clause classification
//! cannot drift apart.
//!
//! Coverage: each part's TOP-LEVEL clauses (UNION arms are parts; NEXT
//! stages recurse via [`crate::walk_plan_parts`] on plans and
//! [`crate::walk_query_parts`] on nested branch queries), with
//! `WHEN`/`ELSE` branch bodies recursed into via [`query_access`] —
//! deliberately NOT
//! `CALL { … }` bodies or expression subqueries, because a top-level
//! `CALL { … }` already counts as a write outright and a read-only
//! `CALL { … }` nested in an `EXISTS`/`COUNT`/`COLLECT` expression must
//! not reclassify the statement as a write.
//!
//! Distinct from the plan-time `clause_is_write` in `validate.rs`, which
//! rejects writes inside subquery EXPRESSIONS and so inspects `CALL { … }`
//! bodies instead of classifying them outright.
//!
//! The schema-vs-write clause split lives in
//! [`crate::clause_kind::clause_class`]; this module only folds its
//! classes into [`Access`] flags.

use cypherlite_ir::{Clause, Query};

use crate::clause_kind::{clause_class, ClauseClass};
use crate::LogicalPlan;

/// The read/write/schema flags the statement fold accumulates.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Access {
    /// Any read clause (MATCH, RETURN, WITH, UNWIND, SHOW, a `CALL` of a
    /// procedure not flagged writes, USE, FINISH, …).
    pub read: bool,
    /// Any write clause (CREATE / MERGE / DELETE / SET / REMOVE / FOREACH /
    /// LOAD CSV, database / graph-type DDL, a top-level `CALL { … }`, a
    /// `CALL` of a writes-flagged procedure, …).
    pub write: bool,
    /// Any schema clause (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT, in
    /// either IR or `Catalog` form).
    pub schema: bool,
}

impl Access {
    const READ: Access = Access {
        read: true,
        write: false,
        schema: false,
    };
    const WRITE: Access = Access {
        read: false,
        write: true,
        schema: false,
    };
    const SCHEMA: Access = Access {
        read: false,
        write: false,
        schema: true,
    };

    fn absorb(&mut self, other: Access) {
        self.read |= other.read;
        self.write |= other.write;
        self.schema |= other.schema;
    }

    /// The write projection the ADR-0116 §4 namespace gates use: schema
    /// (index/constraint DDL) mutates the catalog, so it counts as a
    /// write there.
    pub(crate) fn is_write(self) -> bool {
        self.write || self.schema
    }
}

/// Fold a whole plan: each part's top-level clauses, recursing into NEXT
/// stages.
pub(crate) fn plan_access(
    plan: &LogicalPlan,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> Access {
    let mut acc = Access::default();
    crate::walk_plan_parts(plan, &mut |part| {
        for clause in part {
            acc.absorb(clause_access(clause, is_write_procedure));
        }
    });
    acc
}

/// Fold a nested `WHEN`/`ELSE` branch query: its parts and its own NEXT
/// tail, via the shared [`crate::walk_query_parts`].
fn query_access(query: &Query, is_write_procedure: &mut impl FnMut(&str) -> bool) -> Access {
    let mut acc = Access::default();
    crate::walk_query_parts(query, &mut |part| {
        for clause in part {
            acc.absorb(clause_access(clause, is_write_procedure));
        }
    });
    acc
}

/// Classify one top-level clause into the fold. A `CALL` yields both bits
/// from one predicate call; [`crate::query_type::query_type`] still ORs in
/// [`crate::namespaces::plan_calls_write_procedure`]'s full-tree sweep for
/// the nested positions this fold skips (`CALL { … }` bodies, expression
/// subqueries).
fn clause_access(clause: &Clause, is_write_procedure: &mut impl FnMut(&str) -> bool) -> Access {
    match clause {
        Clause::Call { name, .. } => {
            let write = is_write_procedure(name);
            Access {
                read: !write,
                write,
                ..Access::default()
            }
        }
        // Outright: the body is not descended into from a top-level walk.
        // Conservative but not over-rejecting vs Neo4j CE: a live probe of
        // `neo4j:2026.09-community` (captured 2026-09-30, pinned in
        // `testdata/ce-errors/neo4j-ce.json` as `read_only_call_subquery_*`)
        // shows CE rejects read-only top-level `CALL { … }` bodies on a
        // read-only database too — with 42N71 at semantic-check time (a
        // query may not conclude with `CALL`), before read-only
        // enforcement, while the writing control `CALL { CREATE (n) }`
        // reaches execution and fails 42N18. CypherLite reports 42N18 for
        // both shapes: the outcome matches CE, only the error code and
        // layer differ for the concluding case.
        Clause::CallSubquery { .. } => Access::WRITE,
        Clause::Conditional {
            branches,
            else_query,
        } => {
            let mut acc = Access::default();
            for (_, query) in branches {
                acc.absorb(query_access(query, is_write_procedure));
            }
            if let Some(query) = else_query {
                acc.absorb(query_access(query, is_write_procedure));
            }
            acc
        }
        other => match clause_class(other) {
            ClauseClass::Read => Access::READ,
            ClauseClass::Write => Access::WRITE,
            ClauseClass::Schema => Access::SCHEMA,
        },
    }
}
