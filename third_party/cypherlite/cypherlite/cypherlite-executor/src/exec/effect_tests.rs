//! The ADR-0402 write-effect record: the engine's two facts (`attempted` from
//! the statement entry's one plan walk, `applied` from the write scratch's
//! growth) through the real execution entry points, including the cases
//! neither the plan's query type nor the mutation counters can answer
//! truthfully.

use std::collections::BTreeMap;

use cypherlite_front::parse_and_lower;
use cypherlite_planner::plan;

use super::test_support::{exec_cypher, exec_write, write_scratch};
use super::{ExecRequest, QueryStats};
use crate::graph::Graph;
use crate::overlay::ExecGraph;
use crate::StatementEffect;

/// A read carries no attempt and no application: the note policy can never
/// mistake a read for a zero-effect write, whatever its counters are.
#[test]
fn a_read_reports_no_write_effect() {
    let g = Graph::default();
    let result = exec_cypher(&g, "MATCH (n:Missing) RETURN n");
    assert_eq!(result.effect, StatementEffect::None);
    assert!(!result.effect.changed_nothing());
}

/// The short-circuit that is the note's subject: the mutation operators ran,
/// the row pipeline fed them nothing, so nothing was applied.
#[test]
fn an_empty_write_pipeline_is_an_attempt_without_an_application() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(
        &g,
        &scratch,
        "MATCH (m:Missing) DELETE m CREATE (n:Label {id: 1})",
    )
    .expect("the block succeeds");
    assert_eq!(result.effect, StatementEffect::Attempted);
    assert!(result.effect.changed_nothing());
    assert_eq!(
        result.stats,
        QueryStats::default(),
        "every mutation counter stays zero, which is why the counters alone cannot drive the note"
    );
}

/// An effective write applies a change.
#[test]
fn an_effective_write_reports_an_application() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(&g, &scratch, "CREATE (n:Kept)").expect("the create succeeds");
    assert_eq!(result.effect, StatementEffect::Applied);
    assert!(!result.effect.changed_nothing());
}

/// A catalog change (`ALTER DATABASE`) leaves every ADR-0309 mutation counter
/// at zero while the store changes: the effect's applied fact comes from the
/// redo, not the counters, so it is not reported as changed nothing.
#[test]
fn a_catalog_change_applies_without_moving_a_counter() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(
        &g,
        &scratch,
        "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
    )
    .expect("the admin command succeeds");
    assert_eq!(
        result.stats,
        QueryStats::default(),
        "a catalog write moves no graph mutation counter"
    );
    assert_eq!(result.effect, StatementEffect::Applied);
    assert!(!result.effect.changed_nothing());
}

/// A read-only statement the planner conservatively classifies as a write — a
/// top-level `CALL { … }` with a read-only body — ran no mutation operator, so
/// it carries the read effect, not a zero-effect write.
#[test]
fn a_read_only_call_subquery_attempts_no_write() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(&g, &scratch, "CALL { MATCH (n) RETURN n } RETURN n")
        .expect("the subquery succeeds");
    assert_eq!(result.effect, StatementEffect::None);
    assert!(!result.effect.changed_nothing());
}

/// The attempt fact reaches into a `CALL { … }` body: a write that never
/// dispatches — the outer `MATCH` binds no rows, so the body never runs — is
/// still a write attempt, and it applied nothing. Without the walk the engine
/// would report the statement as a read.
#[test]
fn a_write_inside_a_subquery_that_never_dispatches_still_attempts() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(
        &g,
        &scratch,
        "MATCH (m:Missing) CALL { WITH m CREATE (n:Boxed) }",
    )
    .expect("the statement succeeds");
    assert_eq!(result.effect, StatementEffect::Attempted);
    assert!(result.effect.changed_nothing());
}

/// The same walk reaches a `WHEN/THEN/ELSE` body: an untaken branch's write
/// is an attempt, and nothing was applied.
#[test]
fn a_write_inside_an_untaken_conditional_branch_still_attempts() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(&g, &scratch, "WHEN false THEN { CREATE (n:Hidden) }")
        .expect("the statement succeeds");
    assert_eq!(result.effect, StatementEffect::Attempted);
    assert!(result.effect.changed_nothing());
}

/// The attempt mark is per statement: a view reused across statements does not
/// carry the previous statement's attempt into a later read.
#[test]
fn a_reused_view_starts_each_statement_unmarked() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    {
        let view = ExecGraph::write(&g, &scratch);
        let noop = plan(
            parse_and_lower("MATCH (m:Missing) DELETE m").unwrap(),
            &mut |_| false,
        )
        .unwrap();
        let first = super::execute(&view, &ExecRequest::new(&noop, &BTreeMap::new(), "noop"))
            .expect("the no-op succeeds");
        assert!(
            first.effect.changed_nothing(),
            "the first statement attempted"
        );

        let read = plan(
            parse_and_lower("MATCH (n) RETURN count(n)").unwrap(),
            &mut |_| false,
        )
        .unwrap();
        let second = super::execute(&view, &ExecRequest::new(&read, &BTreeMap::new(), "read"))
            .expect("the read succeeds");
        assert_eq!(
            second.effect,
            StatementEffect::None,
            "a read after a write is still a read"
        );
    }
    // The read did not push redo either, so nothing is pending.
    assert_eq!(scratch.borrow().applied_ops(), 0);
}

/// The applied fact is the statement's own count, not the redo log's length: a
/// statement appended changes through the one funnel, and the count survives a
/// later drain.
#[test]
fn the_applied_count_is_monotonic_across_a_drain() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (n:A)").expect("the create succeeds");
    let after_first = scratch.borrow().applied_ops();
    assert!(after_first > 0, "the create applied a change");

    let ops = scratch.borrow_mut().take_redo();
    assert!(!ops.is_empty());
    assert_eq!(
        scratch.borrow().applied_ops(),
        after_first,
        "draining the redo log does not reset the applied count"
    );
}

/// The applied count restarts at the same statement entry that resets the
/// counters: a view reused across statements reports the later statement's
/// applied fact, never an earlier statement's change.
#[test]
fn the_applied_count_resets_at_the_next_statement_entry() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (n:A)").expect("the create succeeds");
    assert_eq!(scratch.borrow().applied_ops(), 1, "the create applied");

    let result =
        exec_write(&g, &scratch, "MATCH (m:Missing) DELETE m").expect("the no-op write succeeds");
    assert_eq!(
        scratch.borrow().applied_ops(),
        0,
        "the second statement's entry reset zeroed the count and it applied nothing"
    );
    assert_eq!(result.effect, StatementEffect::Attempted);
    assert!(result.effect.changed_nothing());
}

/// A rolled-back batch restores the statement span's applied count with its
/// checkpoint: the batch's changes never committed, so the statement's applied
/// fact must not count them and `IN TRANSACTIONS ON ERROR CONTINUE` leaves the
/// zero-effect write visible.
#[test]
fn a_rolled_back_batch_restores_its_applied_count() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    // The `RETURN` divides by zero, so this batch fails after its create and
    // the `ON ERROR CONTINUE` path restores the pre-batch checkpoint.
    let result = exec_write(
        &g,
        &scratch,
        "UNWIND [1] AS i \
         CALL { WITH i CREATE (:Gone {name: 'no'}) RETURN i / 0 AS bad } \
         IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE \
         RETURN bad",
    )
    .expect("ON ERROR CONTINUE keeps the statement successful");
    assert_eq!(
        scratch.borrow().applied_ops(),
        0,
        "the rolled-back batch's applied change restores with the checkpoint"
    );
    assert_eq!(
        result.stats.nodes_created, 0,
        "the rolled-back batch's counters restore with its checkpoint"
    );
    assert_eq!(result.effect, StatementEffect::Attempted);
    assert!(
        result.effect.changed_nothing(),
        "a statement whose every batch rolled back applied nothing"
    );
}

/// A store-registered procedure is a static row table, not a mutation: calling
/// it is no write attempt, so the note policy never announces a read as a
/// zero-effect write. The namespace gate still treats registration as "could
/// write" — a different question, asked by `call_is_write`.
#[test]
fn a_registered_read_only_procedure_is_not_a_write_attempt() {
    let g = Graph::default();
    g.register_procedure(crate::graph::Procedure {
        name: "custom.ping".to_string(),
        arg_names: Vec::new(),
        arg_types: Vec::new(),
        yield_names: vec!["ok".to_string()],
        rows: vec![BTreeMap::from([(
            "ok".to_string(),
            cypherlite_core::Value::Bool(true),
        )])],
    });
    let scratch = write_scratch(&g);
    let result = exec_write(&g, &scratch, "CALL custom.ping()").expect("the call succeeds");
    assert_eq!(
        result.effect,
        StatementEffect::None,
        "a registered procedure cannot write the store"
    );
    assert!(!result.effect.changed_nothing());
}
