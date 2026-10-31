//! The Cypher keyword for a parser [`Clause`] or a planner IR
//! [`cypherlite_ir::Clause`]. Lives in the planner so the namespace rules
//! ([`crate::namespaces`]) and the facade's `EXPLAIN` renderer share one
//! table and report identical clause names.

use cypherlite_parser::Clause;

/// The Cypher clause keyword(s) for any parser [`Clause`] — e.g. `"MATCH"`,
/// `"SHOW INDEXES"`. Exhaustive over the parser enum.
pub fn clause_keyword(clause: &Clause) -> &'static str {
    match clause {
        Clause::Match(_) => "MATCH",
        Clause::Create(_) => "CREATE",
        Clause::Merge(_) => "MERGE",
        Clause::Delete(_) => "DELETE",
        Clause::Set(_) => "SET",
        Clause::Remove(_) => "REMOVE",
        Clause::Return(_) => "RETURN",
        Clause::With(_) => "WITH",
        Clause::Unwind(_) => "UNWIND",
        Clause::CreateIndex(_) => "CREATE INDEX",
        Clause::DropIndex(_) => "DROP INDEX",
        Clause::CreateConstraint(_) => "CREATE CONSTRAINT",
        Clause::DropConstraint(_) => "DROP CONSTRAINT",
        Clause::Foreach(_) => "FOREACH",
        Clause::LoadCSV(_) => "LOAD CSV",
        Clause::CallSubquery(_) | Clause::CallProcedure(_) => "CALL",
        Clause::ShowFunctions(_) => "SHOW FUNCTIONS",
        Clause::ShowConstraints(_) => "SHOW CONSTRAINTS",
        Clause::ShowIndexes(_) => "SHOW INDEXES",
        Clause::ShowDatabases(_) => "SHOW DATABASES",
        Clause::ShowRoles(_) => "SHOW ROLES",
        Clause::ShowServers(_) => "SHOW SERVERS",
        Clause::ShowUsers(_) => "SHOW USERS",
        Clause::ShowProcedures(_) => "SHOW PROCEDURES",
        Clause::ShowSettings(_) => "SHOW SETTINGS",
        Clause::ShowTransactions(_) => "SHOW TRANSACTIONS",
        Clause::TerminateTransactions(_) => "TERMINATE TRANSACTIONS",
        Clause::Use(_) => "USE",
        Clause::Finish => "FINISH",
        Clause::StandaloneOrderBy(_) => "ORDER BY",
        Clause::Conditional(_) => "WHEN",
        Clause::AlterCurrentGraphType(_) => "ALTER CURRENT GRAPH TYPE",
        Clause::CreateGraphType(_) => "CREATE GRAPH TYPE",
        Clause::ShowGraphType(_) => "SHOW GRAPH TYPE",
        Clause::ShowCurrentGraphType(_) => "SHOW CURRENT GRAPH TYPE",
        Clause::AlterDatabase(_) => "ALTER DATABASE",
    }
}

/// The Cypher clause keyword(s) for a planner IR [`cypherlite_ir::Clause`]
/// — e.g. `"MATCH"`, `"LOAD CSV"`. Exhaustive over the IR enum; `Catalog`
/// delegates to [`clause_keyword`] so catalog names come from the parser
/// table.
pub(crate) fn ir_clause_keyword(clause: &cypherlite_ir::Clause) -> &'static str {
    use cypherlite_ir::Clause as Ir;
    match clause {
        Ir::Catalog(command) => clause_keyword(command),
        Ir::Match { .. } => "MATCH",
        Ir::Create { .. } => "CREATE",
        Ir::Merge { .. } => "MERGE",
        Ir::Call { .. } => "CALL",
        Ir::Delete { .. } => "DELETE",
        Ir::Set { .. } => "SET",
        Ir::Remove { .. } => "REMOVE",
        Ir::With { .. } => "WITH",
        Ir::Unwind { .. } => "UNWIND",
        Ir::Return { .. } => "RETURN",
        Ir::CreateIndex { .. } => "CREATE INDEX",
        Ir::DropIndex { .. } => "DROP INDEX",
        Ir::ShowIndexes => "SHOW INDEXES",
        Ir::Finish => "FINISH",
        Ir::LoadCsv { .. } => "LOAD CSV",
        Ir::CallSubquery { .. } => "CALL",
        Ir::Conditional { .. } => "WHEN",
        Ir::Foreach { .. } => "FOREACH",
        Ir::Use { .. } => "USE",
        Ir::CreateConstraint { .. } => "CREATE CONSTRAINT",
        Ir::DropConstraint { .. } => "DROP CONSTRAINT",
    }
}

/// The read/write/schema class of a planner IR clause — the ONE home of
/// the schema-vs-write split the namespace gates ([`crate::namespaces`])
/// and the query-type classifier ([`crate::query_type`]) share through the
/// access fold ([`crate::access`]). Index/constraint DDL is
/// [`ClauseClass::Schema`] in either its IR or `Catalog` form; the
/// remaining catalog DDL (database / graph-type commands) and the plain
/// data-write variants are [`ClauseClass::Write`]. CALL, CALL { … } and
/// WHEN/ELSE are [`ClauseClass::Read`] here: the consumers classify those
/// differently by design — `validate.rs` rejects writes in subquery
/// expressions (inspecting CALL bodies), the access fold classifies a
/// top-level CALL { … } outright.
///
/// [`clause_mutates_store`] is the published per-clause reading of this
/// table; the table itself (this enum and [`clause_class`]) stays
/// crate-private, so the classification has one exported answer instead of
/// two spellings of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClauseClass {
    Read,
    Write,
    Schema,
}

/// Classify one parser catalog command — the `Ir::Catalog` arm of
/// [`clause_class`], and so the one home for that arm's table: the published
/// [`clause_mutates_store`] reads this classification rather than a second
/// list of catalog commands.
fn catalog_command_class(command: &cypherlite_parser::Clause) -> ClauseClass {
    use cypherlite_parser::Clause as C;
    match command {
        C::CreateIndex(_) | C::DropIndex(_) | C::CreateConstraint(_) | C::DropConstraint(_) => {
            ClauseClass::Schema
        }
        // Catalog writes the namespace gate already treats as writes.
        C::AlterDatabase(_) | C::CreateGraphType(_) | C::AlterCurrentGraphType(_) => {
            ClauseClass::Write
        }
        // The SHOW family and the other read-only commands. The plain
        // data-write variants (Create/Merge/Delete/Set/Remove/Foreach/
        // LoadCSV) are Read in `Catalog` form: the front-end lowers them
        // to the native IR variants classified below, so this arm only
        // ever sees catalog/admin commands in practice — the
        // classification is preserved, not widened.
        C::Match(_)
        | C::Create(_)
        | C::Merge(_)
        | C::Delete(_)
        | C::Set(_)
        | C::Remove(_)
        | C::Return(_)
        | C::With(_)
        | C::Unwind(_)
        | C::Foreach(_)
        | C::LoadCSV(_)
        | C::CallSubquery(_)
        | C::CallProcedure(_)
        | C::ShowFunctions(_)
        | C::ShowConstraints(_)
        | C::ShowIndexes(_)
        | C::ShowDatabases(_)
        | C::ShowRoles(_)
        | C::ShowServers(_)
        | C::ShowUsers(_)
        | C::ShowProcedures(_)
        | C::ShowSettings(_)
        | C::ShowTransactions(_)
        | C::TerminateTransactions(_)
        | C::Use(_)
        | C::Finish
        | C::StandaloneOrderBy(_)
        | C::Conditional(_)
        | C::ShowGraphType(_)
        | C::ShowCurrentGraphType(_) => ClauseClass::Read,
    }
}

/// Classify one IR clause; see [`ClauseClass`] for the rules.
///
/// Both matches are exhaustive on purpose — no `_ =>` fallback — so a
/// new IR variant or a new parser command routed through `Ir::Catalog`
/// is a compile error here instead of silently classifying as
/// [`ClauseClass::Read`] (which would let a write reach a read-only
/// namespace undetected).
pub(crate) fn clause_class(clause: &cypherlite_ir::Clause) -> ClauseClass {
    use cypherlite_ir::Clause as Ir;
    match clause {
        Ir::Catalog(command) => catalog_command_class(command.as_ref()),
        Ir::CreateIndex { .. }
        | Ir::DropIndex { .. }
        | Ir::CreateConstraint { .. }
        | Ir::DropConstraint { .. } => ClauseClass::Schema,
        Ir::Create { .. }
        | Ir::Merge { .. }
        | Ir::Delete { .. }
        | Ir::Set { .. }
        | Ir::Remove { .. }
        | Ir::LoadCsv { .. }
        | Ir::Foreach { .. } => ClauseClass::Write,
        // CALL, CALL { … } and WHEN/ELSE are Read AT THIS LAYER: the
        // consumers classify them by design — the access fold
        // ([`crate::access`]) classifies a top-level CALL via the
        // write-procedure predicate and recurses into CALL { … } / WHEN
        // bodies, and `validate.rs` rejects writes in subquery
        // expressions.
        Ir::Match { .. }
        | Ir::Call { .. }
        | Ir::With { .. }
        | Ir::Unwind { .. }
        | Ir::Return { .. }
        | Ir::ShowIndexes
        | Ir::Finish
        | Ir::CallSubquery { .. }
        | Ir::Conditional { .. }
        | Ir::Use { .. } => ClauseClass::Read,
    }
}

/// Whether `clause` is a mutation operator against the store — the ONE
/// published per-clause answer to "does this clause write?", read by the
/// namespace gates, `validate.rs`'s composition rules, the executor's
/// ADR-0402 statement write-attempt walk and its read-path admission policy
/// alike, so no consumer keeps a second list of write clauses.
///
/// The answer is the clause table: every `Write` and `Schema` class,
/// **catalog commands included** (`ALTER DATABASE` and the graph-type
/// commands are catalog writes that persist while moving no ADR-0309
/// counter, and index/constraint DDL is a schema write). Two clause shapes
/// are deliberately not this table's to answer, because only the caller's
/// layer knows: `CALL` is `Read` here — whether the procedure writes is the
/// registry's fact ([`crate::is_write`]'s predicate) — and `LOAD CSV` is
/// `Write` here for the conservative access fold (ADR-0332) although it only
/// reads a file. A clause that carries a nested body (`CALL { … }`,
/// `WHEN`/`ELSE`, `FOREACH`) is answered for the clause itself; a caller that
/// needs the bodies' answer walks them
/// ([`crate::walk_plan_clauses`] / [`cypherlite_ir::walk_query`]), never a
/// hand-rolled recursion.
///
/// `FOREACH` is a mutation operator outright: its body is an updating clause
/// list, so it is never a read-only clause and a statement cannot admit it to
/// a read path. The body's own clauses are visited individually by the walk,
/// so a body that applies nothing still leaves the `FOREACH` clause itself
/// saying "this statement carries a write".
pub fn clause_mutates_store(clause: &cypherlite_ir::Clause) -> bool {
    matches!(
        clause_class(clause),
        ClauseClass::Write | ClauseClass::Schema
    )
}

#[cfg(test)]
mod tests {
    use cypherlite_front::parse_and_lower;

    use super::*;

    /// The one published per-clause answer counts catalog writes: they persist
    /// a change while moving no ADR-0309 counter, so every reader of the table
    /// — the namespace gates, the composition rules and the executor's
    /// ADR-0402 write-attempt walk — must see them as writes. The catalog
    /// *reads* (the SHOW family) stay reads, and the plain data writes the
    /// front-end lowers to native variants keep their answer.
    #[test]
    fn catalog_writes_are_mutation_operators() {
        for (cypher, expected) in [
            ("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5", true),
            (
                "CREATE GRAPH TYPE gt AS { (n:Person => {name :: STRING}) }",
                true,
            ),
            ("ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph", true),
            ("CREATE INDEX i FOR (n:N) ON (n.p)", true),
            ("SHOW DATABASES", false),
            ("SHOW INDEXES", false),
            ("MATCH (n) RETURN n", false),
        ] {
            let query = parse_and_lower(cypher).unwrap_or_else(|error| panic!("{cypher}: {error}"));
            let [part] = query.parts.as_slice() else {
                panic!("{cypher}: expected one part");
            };
            assert!(
                !part.clauses.is_empty(),
                "{cypher}: expected at least one top-level clause"
            );
            for clause in &part.clauses {
                assert_eq!(
                    clause_mutates_store(clause),
                    expected,
                    "{cypher} clause {clause:?}"
                );
            }
        }
    }
}
