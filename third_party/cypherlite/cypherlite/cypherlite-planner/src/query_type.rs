//! The statement query-type classifier: Neo4j Query API v2 `queryType`
//! values (`r` / `w` / `rw` / `s`) computed from a [`LogicalPlan`].
//!
//! The classifier is a projection of the one shared clause fold
//! ([`crate::access::plan_access`]) — the same traversal and clause
//! classification the ADR-0116 §4 namespace write gate
//! ([`crate::namespaces::is_write`]) uses — plus the full-tree
//! write-procedure sweep ([`crate::namespaces::plan_calls_write_procedure`])
//! and the statement's returned columns ([`crate::plan_output_names`],
//! ADR-0319), so it cannot drift from the namespace gate or from the
//! columns every surface announces. The schema-vs-write clause split
//! lives in [`crate::clause_kind::clause_class`].

use std::fmt;

use crate::LogicalPlan;

/// Neo4j Query API v2 `queryType` of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryType {
    /// Read-only (`r`): no write and no schema clause — MATCH / RETURN /
    /// WITH / UNWIND / SHOW, CALL of a read procedure, USE, FINISH, …
    Read,
    /// A write with no returned columns (`w`): CREATE / MERGE / DELETE /
    /// SET / REMOVE / FOREACH / LOAD CSV without a RETURN, `MATCH … SET`,
    /// `MATCH … DETACH DELETE`, CALL of a writes-flagged procedure, …
    Write,
    /// A write with returned columns (`rw`): `CREATE (n) RETURN n`,
    /// `MATCH … SET … RETURN …`, a UNION or NEXT chain that writes and
    /// ends in a RETURN, …
    ReadWrite,
    /// Schema commands (`s`): CREATE/DROP INDEX, CREATE/DROP CONSTRAINT.
    Schema,
}

impl QueryType {
    /// The Neo4j Query API v2 `queryType` string: `"r"`, `"w"`, `"rw"` or
    /// `"s"`.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryType::Read => "r",
            QueryType::Write => "w",
            QueryType::ReadWrite => "rw",
            QueryType::Schema => "s",
        }
    }
}

impl fmt::Display for QueryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a planned statement into its Neo4j Query API v2 `queryType`.
///
/// `is_write_procedure` classifies `CALL` targets (user-registered
/// procedures and the built-ins flagged `writes` in the executor's
/// canonical signature registry, per ADR-0116 §4); the facade supplies it
/// because the procedure registry lives in the executor, which depends on
/// this crate (never the reverse) — the same threading
/// [`crate::namespaces::plan_calls_write_procedure`] uses.
///
/// Classification rules (ADR-0332, superseding ADR-0162 item 2's
/// clause-presence split of `w` vs `rw`):
///
/// - No write and no schema clause → [`QueryType::Read`] (`r`).
/// - Schema clauses with no data clause alongside →
///   [`QueryType::Schema`] (`s`).
/// - A write with no returned columns → [`QueryType::Write`] (`w`):
///   `CREATE (n)`, `MATCH … SET`, `MATCH … DETACH DELETE`, without RETURN.
/// - A write with returned columns → [`QueryType::ReadWrite`] (`rw`):
///   `CREATE (n) RETURN n`, `MATCH … SET … RETURN …`.
///
/// "Returned columns" is the planned statement's output names
/// ([`crate::plan_output_names`], ADR-0319) — the same column fact every
/// surface announces — so a MATCH that only feeds a SET contributes no
/// columns and the statement classifies `w`, exactly as latest Neo4j
/// (2026.09.0) does. A planned statement passed the UNION
/// column-agreement check at validation, so its output names always
/// derive (`plan_output_names`' `Err` arm is that check's error).
///
/// The write/schema flags still come from the shared fold
/// ([`crate::access::plan_access`]): it accumulates read/write/schema
/// flags over each part's TOP-LEVEL clauses (UNION arms are parts; NEXT
/// stages recurse), with `WHEN`/`ELSE` branch bodies recursed into — the
/// same coverage as the ADR-0116 §4 namespace write gate. Schema clauses
/// (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT, in either their IR or
/// `Catalog` form) accumulate the fold's schema flag via
/// [`crate::clause_kind::clause_class`], never its write flag. A `CALL {
/// … }` subquery counts as a write outright and `FOREACH` bodies via the
/// shared clause classification, exactly as the namespace gate classifies
/// them (conservative; see [`crate::access`]). A `CALL` of a
/// writes-flagged procedure is a write: the fold's `CALL` arm sets both
/// bits from one predicate call (read iff NOT writes-flagged), and
/// [`crate::namespaces::plan_calls_write_procedure`]'s full-tree sweep,
/// OR'd into the write flag here, covers the nested positions the fold
/// skips (`CALL { … }` bodies, expression subqueries) — a bare one has no
/// plan-time output names, so it stays `w`.
///
/// Mixed schema+data: a statement whose schema clauses have NO data clause
/// alongside them is [`QueryType::Schema`]. Plan-time validation forbids
/// schema clauses sharing a part or a UNION with data clauses, so a mix is
/// reachable only across NEXT stages; there the schema clause counts as a
/// WRITE (the statement still mutates the catalog, so it must not report
/// `r`), and the returned-columns split applies to it like any other
/// write.
pub fn query_type(
    plan: &LogicalPlan,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> QueryType {
    let mut access = crate::access::plan_access(plan, is_write_procedure);
    access.write |= crate::namespaces::plan_calls_write_procedure(plan, is_write_procedure);
    if access.schema && !access.read && !access.write {
        return QueryType::Schema;
    }
    // A schema clause mixed with data clauses (NEXT stages only — see
    // above) counts as a write: the statement mutates the catalog, so it
    // must not classify as a read.
    let returns_columns = !crate::plan_output_names_expect(plan).is_empty();
    match (returns_columns, access.is_write()) {
        (_, false) => QueryType::Read,
        (false, true) => QueryType::Write,
        (true, true) => QueryType::ReadWrite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_of;

    fn type_of(cypher: &str) -> QueryType {
        query_type(&plan_of(cypher), &mut |_| false)
    }

    #[test]
    fn query_type_strings_match_query_api_v2() {
        assert_eq!(QueryType::Read.as_str(), "r");
        assert_eq!(QueryType::Write.as_str(), "w");
        assert_eq!(QueryType::ReadWrite.as_str(), "rw");
        assert_eq!(QueryType::Schema.as_str(), "s");
        assert_eq!(QueryType::ReadWrite.to_string(), "rw");
    }

    #[test]
    fn reads_classify_as_r() {
        for cypher in [
            "RETURN 1 AS n",
            "MATCH (n) RETURN n",
            "MATCH (n) WITH n UNWIND [1, 2] AS x RETURN n, x",
            "SHOW INDEXES",
            "SHOW DATABASES",
            "CALL db.labels()",
            "USE neo4j MATCH (n) RETURN n",
            "RETURN 1 AS x UNION RETURN 2 AS x",
            "WHEN true THEN { RETURN 1 AS x } ELSE { RETURN 2 AS x }",
        ] {
            assert_eq!(type_of(cypher), QueryType::Read, "{cypher}");
        }
    }

    #[test]
    fn writes_without_returned_columns_classify_as_w() {
        // ADR-0332: the write's returned columns — not the read clauses it
        // consumes — split `w` from `rw`. A MATCH that only feeds a SET,
        // DELETE or REMOVE returns no columns, exactly like latest Neo4j
        // (2026.09.0, the LABS-323 driver findings D-53/D-54).
        for cypher in [
            "CREATE (n)",
            "MERGE (n:N {id: 1})",
            "MATCH (n) SET n.x = 1",
            "MATCH (n) DETACH DELETE n",
            "MATCH (n) DELETE n",
            "MATCH (n) REMOVE n.x",
            "FOREACH (x IN [1] | CREATE (n))",
            // The shared write list counts LOAD CSV as a write outright.
            "LOAD CSV FROM 'file:///f.csv' AS row CREATE (n)",
            // Conservative, matching the namespace write gate: a top-level
            // CALL { … } counts as a write outright.
            "CALL { CREATE (n) }",
            "WHEN true THEN { CREATE (n) }",
            // A NEXT chain whose final stage writes and returns nothing.
            "RETURN 1 AS x NEXT CREATE (n)",
        ] {
            assert_eq!(type_of(cypher), QueryType::Write, "{cypher}");
        }
    }

    #[test]
    fn writes_with_returned_columns_classify_as_rw() {
        for cypher in [
            "CREATE (n) RETURN n",
            "MATCH (n) SET n.x = 1 RETURN n",
            "MATCH (n) DETACH DELETE n RETURN n",
            "MATCH (n) REMOVE n.x RETURN n",
            // UNION of a read and a write part, both returning columns.
            "RETURN 1 AS x UNION CREATE (n) RETURN 1 AS x",
            // LOAD CSV is on the shared write list; the RETURN exports the
            // loaded row.
            "LOAD CSV FROM 'file:///f.csv' AS row RETURN row",
            // A top-level CALL { … } is a write outright even when its body
            // only reads; the RETURN tail returns columns.
            "CALL { RETURN 1 AS x } RETURN x",
            // A MERGE pattern's variables are returned columns for
            // `RETURN *`, exactly like CREATE's.
            "MERGE (n:N {id: 1}) RETURN *",
        ] {
            assert_eq!(type_of(cypher), QueryType::ReadWrite, "{cypher}");
        }
    }

    #[test]
    fn schema_commands_classify_as_s() {
        for cypher in [
            "CREATE INDEX i FOR (n:N) ON (n.p)",
            "DROP INDEX i",
            "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.id IS UNIQUE",
            "DROP CONSTRAINT c",
        ] {
            assert_eq!(type_of(cypher), QueryType::Schema, "{cypher}");
        }
    }

    #[test]
    fn schema_mixed_with_data_counts_the_schema_clause_as_a_write() {
        // Reachable only across NEXT stages: plan-time validation forbids
        // schema clauses sharing a part or a UNION with data clauses. The
        // returned-columns split then applies to it like any other write.
        assert_eq!(
            type_of("CREATE INDEX i FOR (n:N) ON (n.p) NEXT MATCH (n) RETURN n"),
            QueryType::ReadWrite
        );
        assert_eq!(
            type_of("CREATE INDEX i FOR (n:N) ON (n.p) NEXT CREATE (n)"),
            QueryType::Write
        );
    }

    #[test]
    fn writes_flagged_procedures_classify_as_writes() {
        let is_write_proc = &mut |name: &str| name == "db.createLabel";
        // A writes-flagged procedure is a write wherever it nests; the
        // built-ins are void, and a bare CALL contributes no plan-time
        // output names (default yields are runtime knowledge), so a bare
        // one classifies w (ADR-0332).
        for cypher in [
            "CALL db.createLabel('X')",
            "MATCH (n) RETURN n AS x NEXT CALL db.createLabel('X')",
        ] {
            assert_eq!(
                query_type(&plan_of(cypher), is_write_proc),
                QueryType::Write,
                "{cypher}"
            );
        }
        // With returned columns around it, the statement is rw, wherever
        // in the statement the call nests.
        for cypher in [
            "WHEN true THEN { CALL db.createLabel('X') RETURN 1 AS x }",
            "CALL { CALL db.createLabel('X') RETURN 1 AS x } RETURN x",
        ] {
            assert_eq!(
                query_type(&plan_of(cypher), is_write_proc),
                QueryType::ReadWrite,
                "{cypher}"
            );
        }
        // Read procedures stay reads under the same predicate.
        assert_eq!(
            query_type(&plan_of("CALL db.labels()"), is_write_proc),
            QueryType::Read
        );
    }

    /// ADR-0332 acceptance: the one statement of each kind the rule is
    /// about, at the classifier itself — including an EXPLAIN-wrapped
    /// write (the front-end lowers the wrapped statement, so the plan the
    /// classifier sees is the write's, with no returned columns).
    #[test]
    fn the_adr0332_rule_table() {
        assert_eq!(type_of("MATCH (n) RETURN n"), QueryType::Read);
        assert_eq!(type_of("CREATE (n)"), QueryType::Write);
        assert_eq!(type_of("CREATE (n) RETURN n"), QueryType::ReadWrite);
        assert_eq!(type_of("MATCH (n) SET n.x = 1"), QueryType::Write);
        assert_eq!(
            type_of("MATCH (n) SET n.x = 1 RETURN n"),
            QueryType::ReadWrite
        );
        assert_eq!(type_of("MATCH (n) DETACH DELETE n"), QueryType::Write);
        assert_eq!(
            type_of("MATCH (n) DETACH DELETE n RETURN n"),
            QueryType::ReadWrite
        );
        assert_eq!(
            type_of("CREATE INDEX i FOR (n:N) ON (n.p)"),
            QueryType::Schema
        );
        assert_eq!(type_of("EXPLAIN CREATE (n)"), QueryType::Write);
    }
}
