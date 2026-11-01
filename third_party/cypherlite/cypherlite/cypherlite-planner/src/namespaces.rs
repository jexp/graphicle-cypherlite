//! ADR-0116 namespace classifiers: the §2 database-administration
//! classifiers (the admin-command classifier and the `SHOW DATABASES` +
//! `NEXT` rule), the §4 `system` gate and the statement read/write
//! classification they build on. This module is the one home for the
//! namespace rules — the sibling §3 `USE` rules live in
//! [`crate::use_target`]; the facade (`cypherlite::connection`) calls here
//! and keeps only `CALL`-shape validation. The `system_ontologies` gate is
//! ADR-0229's — every non-administration statement routed there fails with
//! one fixed text, so it needs no classifier here (the facade raises it in
//! `Connection::namespace_gate_error`).
//!
//! The procedure-aware check ([`plan_calls_write_procedure`]) takes the
//! write-procedure predicate as an argument: the canonical procedure
//! signature registry lives in the executor, which depends on this crate
//! (never the reverse).

use cypherlite_ir::Clause;

use crate::clause_kind::ir_clause_keyword;
use crate::LogicalPlan;

/// The write classifier is a projection of the one shared clause fold
/// ([`crate::access::plan_access`]): each part's TOP-LEVEL clauses (plus
/// `NEXT` stages and `WHEN`/`ELSE` branch bodies) only — it deliberately
/// does NOT descend into `CALL { … }` bodies or expression subqueries,
/// because the fold already classifies a top-level `CALL { … }` as a
/// write outright and a read-only `CALL { … }` nested in an
/// `EXISTS`/`COUNT`/`COLLECT` expression must not reclassify the statement
/// as a write. Schema (index/constraint DDL) counts as a write here; see
/// [`crate::access::Access::is_write`].
pub fn is_write(plan: &LogicalPlan) -> bool {
    crate::access::plan_access(plan, &mut |_| false).is_write()
}

/// Whether any `CALL` of a write procedure appears anywhere in the plan.
/// [`crate::walk_plan_clauses`] descends into `CALL { … }` subqueries,
/// `WHEN`/`ELSE` branches, `FOREACH` bodies, expression subqueries and
/// `NEXT` stages, so a write procedure nested in any of those positions
/// still counts — the query-type classification reads it
/// ([`crate::query_type`]).
pub fn plan_calls_write_procedure(
    plan: &LogicalPlan,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> bool {
    let mut calls_write = false;
    crate::walk_plan_clauses(plan, &mut |clause| {
        if let Clause::Call { name, .. } = clause {
            calls_write |= is_write_procedure(name);
        }
    });
    calls_write
}

/// ADR-0116 §2: the database-administration `Catalog` vocabulary, matched
/// once. [`is_database_admin_command`] asks whether a clause is one of the
/// two commands; the `SHOW DATABASES` + `NEXT` rule
/// ([`is_show_databases_with_next`]) asks for `ShowDatabases` specifically.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DatabaseAdminCommand {
    ShowDatabases,
    AlterDatabase,
}

/// ADR-0116 §2: classify a parser `Catalog` command as one of the
/// database-administration commands, or `None` for every other `Catalog`
/// clause. The one vocabulary match both §2 classifiers read.
fn database_admin_command(command: &cypherlite_parser::Clause) -> Option<DatabaseAdminCommand> {
    use cypherlite_parser::Clause as C;
    match command {
        C::ShowDatabases(_) => Some(DatabaseAdminCommand::ShowDatabases),
        C::AlterDatabase(_) => Some(DatabaseAdminCommand::AlterDatabase),
        _ => None,
    }
}

/// ADR-0116 §2: database-administration commands run from any namespace and
/// execute against the store's catalog (the `neo4j` graph). A plan qualifies
/// when every clause is a database-administration `Catalog` command.
pub fn is_database_admin_command(plan: &LogicalPlan) -> bool {
    !plan.parts.is_empty()
        && plan.next.is_none()
        && plan.parts.iter().flatten().all(|clause| {
            matches!(
                clause,
                Clause::Catalog(command) if database_admin_command(command.as_ref()).is_some()
            )
        })
}

/// ADR-0116 §2 / CIP-213 NE1.4: whether the plan combines `SHOW DATABASES`
/// with a `NEXT` stage. The front-end's NEXT-chain validation rejects the
/// combination before planning; this classifier is the execution-phase
/// backstop the facade checks after its EXPLAIN early return (EXPLAIN plans
/// without executing, so the rule does not apply there). Part-granular over
/// every stage's TOP-LEVEL clauses, matching the front-end's per-body scan:
/// a `SHOW DATABASES` nested in an expression subquery does not participate
/// in a `NEXT` chain.
pub fn is_show_databases_with_next(plan: &LogicalPlan) -> bool {
    plan.next.is_some()
        && plan.parts().any(|part| {
            part.iter().any(|clause| {
                matches!(
                    clause,
                    Clause::Catalog(command)
                        if database_admin_command(command.as_ref())
                            == Some(DatabaseAdminCommand::ShowDatabases)
                )
            })
        })
}

/// ADR-0116 §4: on a `system` session only SHOW commands, supported
/// administration commands and standalone procedure calls
/// (`CALL … [YIELD …] [RETURN …]`) run. Returns the offending clause kinds
/// (deduplicated, in order of appearance) for CE's 42N17 text, or `None`
/// when the statement is allowed.
pub fn unsupported_system_clause_names(plan: &LogicalPlan) -> Option<String> {
    let mut offending: Vec<&'static str> = Vec::new();
    // Part-granular coverage is deliberate: only each part's TOP-LEVEL
    // clauses (plus `NEXT` stages, via `walk_plan_parts`) are judged — a
    // read MATCH nested in an expression subquery attached to a standalone
    // CALL is allowed on `system`. `walk_plan_clauses` would descend into
    // those nested positions and is the wrong walk here.
    //
    // Deliberate contrast with the facade's `validate_calls`, which walks
    // the FULL clause tree (`walk_plan_clauses`) so a `CALL` nested in a
    // `CALL { … }` subquery is still validated against the procedure
    // registry: 42N17 judges the statement's top-level shape, while CALL
    // validation must cover every call site. Do not "unify" the two walks.
    crate::walk_plan_parts(plan, &mut |clauses| {
        collect_system_offenders(clauses, &mut offending);
    });
    if offending.is_empty() {
        None
    } else {
        Some(offending.join(", "))
    }
}

/// ADR-0116 §4: the tail-clause keywords CE suppresses from the 42N17
/// offender list when a non-tail offender is present. Encoded ONCE — both
/// the offender collection (`is_tail` below) and the suppression
/// (`partition` below) read this list, so the two can never drift apart.
const SYSTEM_TAIL_KEYWORDS: &[&str] = &["RETURN", "WITH", "FINISH"];

fn collect_system_offenders(clauses: &[Clause], out: &mut Vec<&'static str>) {
    // A RETURN/WITH/FINISH is only allowed as the tail of a SHOW or a
    // standalone procedure call in the same query part. The anchor set is
    // encoded ONCE: both `has_show_or_call` and `allowed` read
    // `is_system_head`, so the two can never drift apart. `Use` is
    // deliberately absent — it is allowed on `system` but does not anchor
    // a tail.
    let is_system_head = |clause: &Clause| match clause {
        Clause::Catalog(command) => catalog_allowed_on_system(command.as_ref()),
        Clause::Call { .. } | Clause::ShowIndexes => true,
        _ => false,
    };
    let has_show_or_call = clauses.iter().any(&is_system_head);
    let is_tail = |clause: &Clause| SYSTEM_TAIL_KEYWORDS.contains(&ir_clause_keyword(clause));
    let allowed = |clause: &Clause| {
        is_system_head(clause)
            || matches!(clause, Clause::Use { .. })
            || (is_tail(clause) && has_show_or_call)
    };
    // CE lists only the clause kinds outside its supported
    // CALL/YIELD/RETURN structure: `MATCH (n) RETURN count(n)` reports
    // `MATCH`, not `MATCH, RETURN` (testdata/ce-errors/neo4j-ce.json
    // `system_data_clause`). A lone tail offender (`RETURN 1`) still
    // reports itself.
    let (non_tail, tail): (Vec<&'static str>, Vec<&'static str>) = clauses
        .iter()
        .filter(|clause| !allowed(clause))
        .map(ir_clause_keyword)
        .partition(|name| !SYSTEM_TAIL_KEYWORDS.contains(name));
    let part_offenders = if non_tail.is_empty() { tail } else { non_tail };
    for name in part_offenders {
        if !out.contains(&name) {
            out.push(name);
        }
    }
}

/// SHOW commands and database-administration commands (ADR-0116 §2) are the
/// only `Catalog` clauses allowed on `system`.
fn catalog_allowed_on_system(command: &cypherlite_parser::Clause) -> bool {
    use cypherlite_parser::Clause as C;
    matches!(
        command,
        C::ShowFunctions(_)
            | C::ShowConstraints(_)
            | C::ShowIndexes(_)
            | C::ShowDatabases(_)
            | C::ShowRoles(_)
            | C::ShowServers(_)
            | C::ShowUsers(_)
            | C::ShowProcedures(_)
            | C::ShowSettings(_)
            | C::ShowTransactions(_)
            | C::ShowGraphType(_)
            | C::ShowCurrentGraphType(_)
            | C::AlterDatabase(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_of;

    #[test]
    fn writes_classify_as_writes() {
        for cypher in [
            "CREATE (n)",
            "MERGE (n:N {id: 1})",
            "MATCH (n) SET n.x = 1",
            "MATCH (n) DELETE n",
            "MATCH (n) REMOVE n.x",
            "CREATE INDEX i FOR (n:N) ON (n.p)",
            "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.id IS UNIQUE",
            "FOREACH (x IN [1] | CREATE (n))",
        ] {
            assert!(is_write(&plan_of(cypher)), "{cypher}");
        }
    }

    #[test]
    fn reads_classify_as_reads() {
        for cypher in [
            "RETURN 1 AS n",
            "MATCH (n) RETURN n",
            "SHOW INDEXES",
            "CALL db.labels()",
        ] {
            assert!(!is_write(&plan_of(cypher)), "{cypher}");
        }
    }

    #[test]
    fn read_only_call_in_expression_subqueries_classifies_as_read() {
        // Regression: `is_write` must not descend into expression
        // subqueries — a read-only `CALL { … }` there is legal (plan-time
        // validation only rejects actual writes), and classifying the
        // statement as a write would reject a pure read on the read-only
        // `system_ontologies` namespace and misroute it on `neo4j`.
        for cypher in [
            "RETURN EXISTS { CALL { RETURN 1 AS x } } AS x",
            "RETURN COUNT { CALL { RETURN 1 AS x } } AS x",
            "RETURN COLLECT { CALL { RETURN 1 AS x } RETURN x } AS x",
        ] {
            assert!(!is_write(&plan_of(cypher)), "{cypher}");
        }
    }

    #[test]
    fn nested_queries_recurse_through_one_predicate() {
        // WHEN branches, ELSE bodies and NEXT tails are all recursed into
        // by `is_write`'s shared fold (`access::plan_access`), so the one
        // clause classification classifies them.
        assert!(is_write(&plan_of("WHEN true THEN { CREATE (n) }")));
        assert!(!is_write(&plan_of(
            "WHEN true THEN { RETURN 1 AS x } ELSE { RETURN 2 AS x }"
        )));
        assert!(is_write(&plan_of(
            "WHEN false THEN { MATCH (n) RETURN count(n) AS x } \
             ELSE { CREATE (m) RETURN 0 AS x }"
        )));
        assert!(is_write(&plan_of("RETURN 1 AS x NEXT CREATE (n)")));
        assert!(!is_write(&plan_of("RETURN 1 AS x UNION RETURN 2 AS x")));
    }

    #[test]
    fn write_procedure_calls_are_found_anywhere_in_the_clause_tree() {
        // The facade's predicate classifies `db.createLabel` as a write
        // (it is flagged `writes` in the canonical registry); the tests
        // here stand in with a name predicate.
        let is_write_proc = &mut |name: &str| name == "db.createLabel";
        for cypher in [
            "CALL db.createLabel('X')",
            "WHEN true THEN { CALL db.createLabel('X') }",
            "WHEN false THEN { RETURN 1 AS x } ELSE { CALL db.createLabel('X') RETURN 1 AS x }",
            "CALL { CALL db.createLabel('X') }",
            "FOREACH (x IN [1] | CALL db.createLabel('X'))",
            "RETURN 1 AS x NEXT CALL db.createLabel('X')",
        ] {
            assert!(
                plan_calls_write_procedure(&plan_of(cypher), is_write_proc),
                "{cypher}"
            );
        }
        // Read procedures and write-free statements still classify as reads.
        for cypher in [
            "CALL db.labels()",
            "WHEN true THEN { CALL db.labels() YIELD label RETURN label }",
            "RETURN 1 AS x",
        ] {
            assert!(
                !plan_calls_write_procedure(&plan_of(cypher), is_write_proc),
                "{cypher}"
            );
        }
    }

    #[test]
    fn database_admin_commands_classify() {
        assert!(is_database_admin_command(&plan_of("SHOW DATABASES")));
        assert!(is_database_admin_command(&plan_of(
            "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 25"
        )));
        assert!(!is_database_admin_command(&plan_of("MATCH (n) RETURN n")));
        assert!(!is_database_admin_command(&plan_of("SHOW INDEXES")));
        // A non-administrative clause disqualifies the statement.
        assert!(!is_database_admin_command(&plan_of(
            "SHOW DATABASES YIELD name RETURN name"
        )));
    }

    /// A lowered `SHOW DATABASES` catalog clause. The front-end's
    /// NEXT-chain validation rejects the command in a chain before
    /// planning, so the classifier's chain inputs are assembled directly
    /// here — this is exactly the shape the execution-phase backstop sees.
    fn show_databases_clause() -> Clause {
        Clause::Catalog(Box::new(cypherlite_parser::Clause::ShowDatabases(
            cypherlite_parser::ShowDatabasesClause {
                filter: cypherlite_parser::DatabaseFilter::All,
                yields: Default::default(),
                where_expr: None,
            },
        )))
    }

    #[test]
    fn show_databases_with_next_classifies_the_chain() {
        // The command in the head stage.
        let mut head = plan_of("RETURN 1 AS x NEXT RETURN x");
        head.parts[0] = vec![show_databases_clause()];
        assert!(is_show_databases_with_next(&head));

        // The command in a NEXT stage: `parts()` spans every stage.
        let mut tail = plan_of("RETURN 1 AS x NEXT RETURN x");
        tail.next.as_mut().unwrap().parts[0] = vec![show_databases_clause()];
        assert!(is_show_databases_with_next(&tail));

        // SHOW DATABASES without a NEXT stage is the plain §2 command.
        let mut plain = plan_of("RETURN 1 AS x");
        plain.parts[0] = vec![show_databases_clause()];
        assert!(!is_show_databases_with_next(&plain));

        // ALTER DATABASE is §2 but not a SHOW DATABASES chain participant.
        let mut alter = plan_of("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 25");
        alter.next = Some(Box::new(plan_of("RETURN 1 AS x")));
        assert!(!is_show_databases_with_next(&alter));

        // An ordinary NEXT chain does not classify.
        assert!(!is_show_databases_with_next(&plan_of(
            "RETURN 1 AS x NEXT RETURN x + 1 AS y"
        )));
    }

    #[test]
    fn system_offender_names_follow_ce() {
        // Data clauses offend; CE suppresses the tail when a non-tail
        // offender is present.
        assert_eq!(
            unsupported_system_clause_names(&plan_of("MATCH (n) RETURN count(n) AS c")),
            Some("MATCH".to_string())
        );
        // A lone tail offender still reports itself.
        assert_eq!(
            unsupported_system_clause_names(&plan_of("RETURN 1 AS x")),
            Some("RETURN".to_string())
        );
        // Standalone procedure calls, SHOW commands and administration
        // commands are allowed.
        for cypher in [
            "CALL db.labels()",
            "CALL db.labels() YIELD label RETURN label",
            "SHOW INDEXES",
            "SHOW DATABASES",
        ] {
            assert_eq!(
                unsupported_system_clause_names(&plan_of(cypher)),
                None,
                "{cypher}"
            );
        }
    }
}
