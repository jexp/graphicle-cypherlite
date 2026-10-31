//! Clause-composition structure rules: USE placement (CIP-202), NEXT-chain
//! admin-command restrictions (CIP-213), UNION column shapes (CIP-140/146),
//! duplicate WITH/RETURN column names, and relationship-variable uniqueness
//! in MATCH patterns. The CIP-224 SEARCH-clause rules live in
//! [`super::search`].

use std::collections::HashSet;

use cypherlite_error::{
    EngineError, ErrorKind, COUNT_UNION_MIX_MESSAGE, COUNT_UNION_OMITTED_MESSAGE,
};
use cypherlite_parser::{Clause, Expr, PatternElement, Statement};

use crate::ast::collect::union_arms;
use crate::ast::stmt_ref::StmtRef;

/// CIP-202 scenario [7]: `USE` has its own placement restriction (unrelated
/// to and not lifted by CIP-202's removal of the update-boundary rule) --
/// within any one linear statement body (the top-level query, or a `CALL {}`
/// subquery body, checked recursively so a nested `CALL` inside a `CALL`
/// body is covered too), a `USE` clause is only valid as that body's very
/// first clause, or as its second clause immediately after an importing
/// `WITH`. Any other position raises `InvalidPlacementOfUseClause`, e.g. a
/// `USE` sandwiched between an update and a read
/// (`CREATE (:A) USE g MATCH (v:A) RETURN v`) -- CIP-202 only removed the
/// syntactic *update-boundary* restriction; USE's own placement rule is
/// orthogonal and still applies, verified against live Neo4j.
///
/// Takes the tree walk's borrowed [`StmtRef`]: a FOREACH body has no UNION
/// chain of its own, exactly like the synthetic statement the walk used to
/// fabricate for it.
///
/// The UNION-suffix scan runs only at the chain's *head* (a statement not
/// itself reached via a `union` edge). The tree walk invokes this validator
/// on every arm too, and the walk is pre-order — the head's visitor runs
/// before any arm is visited — so an arm's own suffix scan could only
/// re-scan bodies the head already checked (finding nothing new: if the
/// head's in-order scan errored, the walk stopped there) or re-report the
/// head's verdict. Skipping it keeps a length-L chain O(L) instead of
/// O(L²) with a byte-identical first error. An arm still checks its own
/// body (and, via `check_use_placement_in_body`, its `CALL { }` bodies).
pub fn validate_use_placement(stmt: StmtRef<'_>) -> Result<(), EngineError> {
    check_use_placement_in_body(stmt.clauses())?;
    if stmt.is_union_arm() {
        return Ok(());
    }
    for (arm, _) in union_arms(stmt.union()) {
        check_use_placement_in_body(&arm.clauses)?;
    }
    Ok(())
}

fn check_use_placement_in_body(clauses: &[Clause]) -> Result<(), EngineError> {
    for (idx, clause) in clauses.iter().enumerate() {
        if matches!(clause, Clause::Use(_)) {
            // Valid at the very front (any run of leading USE clauses is
            // itself valid -- CIP-146's nested-USE-brace collapse can stack
            // more than one same-graph USE at the front of a body), or as
            // the clause immediately following a leading importing WITH.
            let all_use_before = clauses[..idx].iter().all(|c| matches!(c, Clause::Use(_)));
            let valid_position =
                all_use_before || (idx == 1 && matches!(clauses[0], Clause::With(_)));
            if !valid_position {
                return Err(EngineError::semantic(ErrorKind::InvalidPlacementOfUseClause,
                    "USE clause must be either the first clause in a query, or preceded by an importing WITH clause"
                        .to_string(),
                ));
            }
        }
        if let Clause::CallSubquery(csq) = clause {
            validate_use_placement(StmtRef::Real(&csq.statement))?;
        }
    }
    Ok(())
}
/// CIP-213 NE1.4: an administrative command (`SHOW DATABASES`/`SHOW
/// ROLES`/`SHOW SERVERS`/`SHOW USERS`/`TERMINATE TRANSACTIONS`) cannot
/// participate in a NEXT chain. In live Neo4j this isn't actually a
/// NEXT-grammar rule -- these commands require the `system` database
/// (`51N28` if run elsewhere), and `NEXT` is separately disallowed on the
/// system database itself (`42N17`) -- so the combination is rejected in
/// both directions for different underlying reasons (CIP-213 NOTES.md
/// Deviations #4). Modeled here as a single compile-time check on any
/// statement that is part of a NEXT chain (has a `.next`, or is reached by
/// following one): non-admin SHOW commands (FUNCTIONS/CONSTRAINTS/INDEXES/
/// PROCEDURES/SETTINGS/TRANSACTIONS) are schema/catalog introspection, not
/// DBMS administration, and are unaffected.
///
/// The chain scan runs only at the chain's head (a statement carrying
/// `.next` that was not itself reached via a `next` edge). The tree walk
/// visits every stage after the head, and a non-head stage's own suffix
/// scan could only re-check bodies the head's in-order scan already
/// checked — re-reporting the head's verdict or nothing — so skipping it
/// keeps a length-L chain O(L) instead of O(L²) with a byte-identical
/// first error (a violation at the tail is still reported at the head's
/// invocation, exactly as before).
pub fn validate_next_no_admin_commands(stmt: StmtRef<'_>) -> Result<(), EngineError> {
    if stmt.next().is_none() || stmt.is_next_stage() {
        return Ok(());
    }
    check_no_admin_commands_in_body(stmt.clauses())?;
    let mut cur = stmt.next();
    while let Some(s) = cur {
        check_no_admin_commands_in_body(&s.clauses)?;
        cur = s.next.as_deref();
    }
    Ok(())
}

fn check_no_admin_commands_in_body(clauses: &[Clause]) -> Result<(), EngineError> {
    if clauses.iter().any(|c| {
        matches!(
            c,
            Clause::ShowDatabases(_)
                | Clause::ShowRoles(_)
                | Clause::ShowServers(_)
                | Clause::ShowUsers(_)
                | Clause::TerminateTransactions(_)
        )
    }) {
        return Err(EngineError::semantic(
            ErrorKind::SystemDatabaseRequired,
            "an administrative SHOW/TERMINATE command cannot participate in a NEXT chain"
                .to_string(),
        ));
    }
    Ok(())
}

/// A FOREACH body cannot contain a RETURN: the body is side effects only —
/// a RETURN would end the inner part and try to export through a boundary
/// that consumes nothing but the writes, so it is the one clause that
/// categorically cannot appear there. Like the writes-in-`EXISTS { }` rule,
/// the composition is statically decidable from the statement's own text,
/// so it is rejected here, before lowering reaches execution. The
/// executor's FOREACH boundary (`exec_foreach`) keeps the same check as
/// defense-in-depth for callers that bypass this validation; the two
/// spellings raise the same message.
///
/// Only ever invoked on a FOREACH body: the tree walk presents each body as
/// a borrowed [`StmtRef::Body`] (the only clause-list body there is), and
/// `validate_statement_node` routes it here — a RETURN anywhere else is a
/// projection, not a violation.
pub fn validate_foreach_body(clauses: &[Clause]) -> Result<(), EngineError> {
    if clauses.iter().any(|c| matches!(c, Clause::Return(_))) {
        return Err(EngineError::semantic(
            ErrorKind::InvalidClauseComposition,
            "a FOREACH body cannot contain a RETURN".to_string(),
        ));
    }
    Ok(())
}
/// A UNION branch's result shape, as far as it's staticaly checkable: either
/// a concrete list of RETURN column names, `RETURN *` (can't determine the
/// columns until runtime, so skipped for name comparison), or no RETURN
/// clause at all (an "omitted" result -- CIP-140 FINISH, or an updating
/// clause with no trailing RETURN).
enum UnionBranchShape {
    Cols(Vec<String>),
    Wildcard,
    Omitted,
}

fn union_branch_shape(clauses: &[Clause]) -> UnionBranchShape {
    // Note: a union arm carrying its *own* private CIP-213 NEXT chain
    // (e.g. a top-level-brace block used as one arm, whose own content
    // chains internally) never reaches here with `.next` set -- the
    // parser's `maybe_wrap_composite_union_arm` absorbs that chain into a
    // self-contained `CALL (*) { ... } RETURN *`/`FINISH` node before it
    // is ever attached as an arm, specifically so this function (and every
    // other "what does this arm project" analysis) never has to
    // distinguish "this arm's own continuation" from "the whole
    // composite's shared trailing NEXT stage" (this `stmt` itself, when
    // it's the outermost composite node, may still carry `.next` for the
    // *latter* meaning -- deliberately not consulted here, since it
    // applies uniformly after every arm, not per-arm).
    //
    // CIP-135: a `{ WHEN ... }`/`{ WHEN ... ELSE ... }` UNION arm has no
    // direct `Clause::Return` of its own -- its shape is whatever its
    // branches project. Rule 9/10 (validated separately) guarantees every
    // branch/ELSE body has the same shape, so any one of them stands in
    // for the whole clause.
    if let [Clause::Conditional(cc)] = clauses {
        let inner = cc
            .branches
            .first()
            .map(|b| &b.body)
            .or(cc.else_body.as_ref());
        return match inner {
            Some(s) => union_branch_shape(&s.clauses),
            None => UnionBranchShape::Omitted,
        };
    }
    if !clauses.iter().any(|c| matches!(c, Clause::Return(_))) {
        return UnionBranchShape::Omitted;
    }
    match get_return_columns(clauses) {
        Some(cols) => UnionBranchShape::Cols(cols),
        None => UnionBranchShape::Wildcard,
    }
}

/// Validate UNION column consistency: all branches must have the same column
/// names, and either every branch returns a tabular result or none do (a
/// "some RETURN, some omitted/FINISH" mix is also a
/// `DifferentColumnsInUnion` error -- CIP-140 `03_union.feature` scenarios
/// 4-6, CIP-146 `union-braces.feature` scenario 9).
///
/// The whole-chain scan runs only at the chain's head (a statement not
/// itself reached via a `union` edge). The tree walk invokes this
/// validator on every arm too; an arm's scan covers a *suffix* of the
/// shapes the head's in-order scan already covered, so it can only
/// re-report the head's verdict (the omitted/tabular mix test and the
/// first-column-set comparison are both monotone under shrinking the shape
/// set) — and it allocated a fresh `shapes` Vec per link, making a
/// length-L chain O(L²). Skipping non-head scans is byte-identical and
/// O(L) per chain.
pub fn validate_union_columns(stmt: StmtRef<'_>) -> Result<(), EngineError> {
    if stmt.union().is_none() || stmt.is_union_arm() {
        return Ok(());
    }

    let mut shapes = vec![union_branch_shape(stmt.clauses())];
    for (arm, _) in union_arms(stmt.union()) {
        shapes.push(union_branch_shape(&arm.clauses));
    }

    let any_omitted = shapes
        .iter()
        .any(|s| matches!(s, UnionBranchShape::Omitted));
    let any_tabular = shapes
        .iter()
        .any(|s| !matches!(s, UnionBranchShape::Omitted));
    if any_omitted && any_tabular {
        return Err(EngineError::semantic(
            ErrorKind::DifferentColumnsInUnion,
            "cannot combine a UNION branch that returns results with one that does not".to_string(),
        ));
    }

    let mut first_cols: Option<&Vec<String>> = None;
    for shape in &shapes {
        if let UnionBranchShape::Cols(cols) = shape {
            match first_cols {
                None => first_cols = Some(cols),
                Some(fc) if fc != cols => {
                    return Err(EngineError::semantic(
                        ErrorKind::DifferentColumnsInUnion,
                        "all UNION branches must have the same column names".to_string(),
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}
/// Get the column names from a RETURN clause in a statement body.
fn get_return_columns(clauses: &[Clause]) -> Option<Vec<String>> {
    for clause in clauses {
        if let Clause::Return(rc) = clause {
            let mut cols = Vec::new();
            for item in &rc.items {
                if item.all {
                    return None; // RETURN * - can't easily determine columns
                }
                if let Some(alias) = &item.alias {
                    cols.push(alias.clone());
                } else if let Expr::Variable(v) = &item.expr {
                    cols.push(v.clone());
                } else if let Some(src) = &item.source_text {
                    cols.push(src.clone());
                }
            }
            return Some(cols);
        }
    }
    None
}

/// The two statement-shape UNION rules a `COUNT { … }` body's own UNION
/// chain must satisfy (CIP-66): its branches must make the same
/// explicit-vs-omitted RETURN choice, and a body whose every branch omits
/// RETURN may join its branches with UNION ALL only. Both compositions
/// are statically decidable from the body's own text, so the validate
/// battery owns them (the FOREACH pattern: like `validate_foreach_body`,
/// the rule lives here and the executor's `eval_count_query` keeps the
/// same scan as defense-in-depth, raising the identical messages). The
/// battery's expression visitor routes every `Expr::CountSubquery` body
/// here: the statement visitor's `StmtRef` cannot tell a COUNT body from
/// an EXISTS/COLLECT one, and only COUNT carries these rules (CIP-63:
/// EXISTS has no RETURN-omission restriction; a top-level or `CALL { }`
/// omitted-RETURN union is CIP-140 FINISH semantics, not an error).
///
/// The predicate is the one the executor's per-row scan spelled: a branch
/// is "explicit" when its own clause list ends in a RETURN (a UNION arm
/// never carries a NEXT chain of its own — the parser's
/// `maybe_wrap_composite_union_arm` absorbs it — and the body head's NEXT
/// chain lowers out of `parts`, so each branch's clause list is exactly
/// what the executor classifies). For parsed text the parser's own
/// COUNT-site check (`check_subquery_union_consistency`) rejects both
/// shapes even earlier, with its parse-time wording — that wording is
/// the one user queries see; this validator is the front's canonical
/// home of the rule, reached by any statement tree the parser did not
/// itself screen (a hand-built AST, a future grammar gap).
pub fn validate_count_subquery_union_shapes(body: &Statement) -> Result<(), EngineError> {
    // The same classification the executor's per-row scan spells, in one
    // spelling over every branch (head plus arms): a lone branch is never
    // mixed and has no joiners, which the classification itself answers.
    let arms = union_arms(body.union.as_deref()).collect::<Vec<_>>();
    let any_distinct = arms.iter().any(|(_, all)| !*all);
    let branches = std::iter::once(body)
        .chain(arms.into_iter().map(|(arm, _)| arm))
        .collect::<Vec<_>>();
    let explicit = branches
        .iter()
        .filter(|branch| matches!(branch.clauses.last(), Some(Clause::Return(_))))
        .count();
    if explicit > 0 && explicit < branches.len() {
        return Err(EngineError::semantic(
            ErrorKind::DifferentColumnsInUnion,
            COUNT_UNION_MIX_MESSAGE,
        ));
    }
    if explicit == 0 && any_distinct {
        return Err(EngineError::semantic(
            ErrorKind::DifferentColumnsInUnion,
            COUNT_UNION_OMITTED_MESSAGE,
        ));
    }
    Ok(())
}

/// Validate that WITH/RETURN don't have duplicate column names.
pub fn validate_column_name_conflicts(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        match clause {
            Clause::With(wc) => {
                let mut names: HashSet<String> = HashSet::new();
                for item in &wc.items {
                    if item.all {
                        continue;
                    }
                    // In WITH, non-variable expressions MUST have an alias
                    if item.alias.is_none() && !matches!(&item.expr, Expr::Variable(_)) {
                        return Err(EngineError::semantic(
                            ErrorKind::NoExpressionAlias,
                            "expressions in WITH must be aliased with AS".to_string(),
                        ));
                    }
                    let name = if let Some(alias) = &item.alias {
                        alias.clone()
                    } else if let Expr::Variable(v) = &item.expr {
                        v.clone()
                    } else {
                        continue;
                    };
                    if !names.insert(name.clone()) {
                        return Err(EngineError::semantic(
                            ErrorKind::ColumnNameConflict,
                            format!("multiple result columns with the same name '{name}'"),
                        ));
                    }
                }
            }
            Clause::Return(rc) => {
                let mut names: HashSet<String> = HashSet::new();
                for item in &rc.items {
                    if item.all {
                        continue;
                    }
                    let name = if let Some(alias) = &item.alias {
                        alias.clone()
                    } else if let Expr::Variable(v) = &item.expr {
                        v.clone()
                    } else if let Some(src) = &item.source_text {
                        src.clone()
                    } else {
                        continue;
                    };
                    if !names.insert(name.clone()) {
                        return Err(EngineError::semantic(
                            ErrorKind::ColumnNameConflict,
                            format!("multiple result columns with the same name '{name}'"),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
/// Validate that relationship variables are not reused within a single top-
/// level path pattern, when that reuse also closes a cycle back onto a
/// repeated *node* variable.
///
/// This is scoped narrowly to match the openCypher TCK's own pinned
/// expectation (`clauses/match/Match3.feature` #29, `MATCH
/// (a)-[r]->()-[r]->(a)`): a compile-time `RelationshipUniquenessViolation`.
/// CIP-19's own D4 deviation, and CIP-51/CIP-60's own worked examples, say
/// Neo4j (CYPHER 25 GPM) no longer raises this error for a repeated
/// relationship variable in general (e.g. `MATCH (n)-[r]->(m)-[r]->(o)`,
/// with no repeated node variable) -- it compiles and returns no rows at
/// runtime instead, via the engine's own relationship-uniqueness pool in
/// `match_rel_and_node`. The TCK's one pinned scenario happens to also
/// repeat a node variable (`a` at both ends), so the check below only fires
/// when *both* a relationship variable and a node variable repeat in the
/// same path -- narrow enough to keep that exact TCK case erroring (the TCK
/// wins per this project's conflict-resolution rule) while letting the
/// CIP-19/51/60 no-repeated-node cases fall through to the runtime's
/// already-correct no-solutions behavior. Reuse *across* comma-separated
/// top-level path patterns of one MATCH clause (CIP-60 5.3.3.3/8.1.1) is
/// intentionally not rejected here either: it compiles, and the engine's
/// relationship-uniqueness pool (which now spans the whole graph pattern)
/// makes it unsatisfiable at runtime, per the CIP-60 doc's own "no longer a
/// syntax error" product change.
pub fn validate_relationship_uniqueness(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        if let Clause::Match(mc) = clause {
            // CIP-51 Section 2.1.3/2.4: under the `REPEATABLE ELEMENTS` match
            // mode, a relationship variable declared twice is a legitimate
            // equijoin (the same relationship bound at both positions), not
            // a compile-time error -- this mode doesn't exist in the pinned
            // TCK scenario above, so relaxing the check here doesn't
            // conflict with it.
            if mc.match_mode == cypherlite_parser::MatchMode::RepeatableElements {
                continue;
            }
            let mut shortest_relationships = HashSet::new();
            for path in &mc.pattern {
                if path.is_shortest_path_function {
                    for elem in &path.elements {
                        if let PatternElement::Relationship(rp) = elem {
                            if let Some(variable) = &rp.variable {
                                if !shortest_relationships.insert(variable.clone()) {
                                    return Err(EngineError::semantic(
                                        ErrorKind::RelationshipUniquenessViolation,
                                        format!(
                                            "relationship '{variable}' is reused by two shortest-path patterns"
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
                let mut seen_rels: HashSet<String> = HashSet::new();
                let mut seen_nodes: HashSet<String> = HashSet::new();
                let mut repeated_rel: Option<String> = None;
                let mut repeated_node = false;
                for elem in &path.elements {
                    match elem {
                        PatternElement::Relationship(rp) => {
                            if let Some(v) = &rp.variable {
                                if !seen_rels.insert(v.clone()) && repeated_rel.is_none() {
                                    repeated_rel = Some(v.clone());
                                }
                            }
                        }
                        PatternElement::Node(np) => {
                            if let Some(v) = &np.variable {
                                if !seen_nodes.insert(v.clone()) {
                                    repeated_node = true;
                                }
                            }
                        }
                        PatternElement::Quantified(_) => {}
                    }
                }
                if repeated_node {
                    if let Some(v) = repeated_rel {
                        return Err(EngineError::semantic(
                            ErrorKind::RelationshipUniquenessViolation,
                            format!(
                                "relationship '{v}' used more than once in the same MATCH pattern"
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_variable_cannot_span_two_shortest_path_patterns() {
        let stmt = cypherlite_parser::parse(
            "MATCH p = shortestPath((a)-[r*]->(b)), \
             q = shortestPath((c)-[r*]->(d)) RETURN p, q",
        )
        .unwrap();
        let error = validate_relationship_uniqueness(&stmt.clauses).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RelationshipUniquenessViolation);
    }

    /// A RETURN inside a FOREACH body is rejected by the validate battery —
    /// before lowering, never at execution — with the authored composition
    /// error text (the executor's FOREACH boundary keeps the identical
    /// message as defense-in-depth).
    #[test]
    fn foreach_body_with_return_is_rejected_before_execution() {
        let error = crate::validate::validate_statement_tree(
            &cypherlite_parser::parse("FOREACH (i IN [1] | RETURN 1)").unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidClauseComposition);
        assert_eq!(
            error.to_string(),
            "InvalidClauseComposition: a FOREACH body cannot contain a RETURN"
        );
    }

    /// The rule fires wherever the body sits: the tree walk reaches a
    /// FOREACH inside a `CALL { }` body (and every other nested body) as
    /// its own `StmtRef::Body`.
    #[test]
    fn foreach_body_with_return_is_rejected_inside_nesting() {
        let error = crate::validate::validate_statement_tree(
            &cypherlite_parser::parse(
                "CALL { FOREACH (i IN [1] | CREATE (:N {v: i}) RETURN i) } RETURN 1",
            )
            .unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "InvalidClauseComposition: a FOREACH body cannot contain a RETURN"
        );
    }

    /// A write-only FOREACH body (the only legal shape) passes, and a
    /// top-level RETURN is a projection, not a violation.
    #[test]
    fn foreach_body_without_return_and_top_level_return_pass() {
        crate::validate::validate_statement_tree(
            &cypherlite_parser::parse("FOREACH (i IN [1] | CREATE (:N {v: i}))").unwrap(),
        )
        .expect("a write-only FOREACH body is valid");
        crate::validate::validate_statement_tree(&cypherlite_parser::parse("RETURN 1").unwrap())
            .expect("a top-level RETURN is a projection");
    }

    /// Splice a parsed statement in as the body of a parsed `count { … }`
    /// wrapper: the bodies the two COUNT-subquery UNION rules reject are
    /// legal top-level queries, so the only way to hand the battery one
    /// is to build it from a wrapper the parser admits and a body it
    /// never screens in that position (its own COUNT-site check,
    /// `check_subquery_union_consistency`, is what rejects the spelled
    /// form at parse time — with its parse-time wording, the one user
    /// queries see).
    fn count_subquery_over(body: &str) -> cypherlite_parser::Statement {
        let mut wrapper =
            cypherlite_parser::parse("RETURN count { MATCH (n) UNION ALL MATCH (m) }")
                .expect("the all-omitted UNION ALL count body parses");
        let body = cypherlite_parser::parse(body).expect("the body parses at the top level");
        let Clause::Return(rc) = &mut wrapper.clauses[0] else {
            panic!("expected the wrapper's RETURN clause");
        };
        let Expr::CountSubquery(cs) = &mut rc.items[0].expr else {
            panic!("expected the wrapper's count subquery item");
        };
        cs.statement = body;
        wrapper
    }

    /// A COUNT subquery body whose UNION branches mix explicit and omitted
    /// RETURN is rejected by the validate battery — before lowering, never
    /// at execution — with the executor scan's byte-identical message (the
    /// executor's `eval_count_query` keeps the same scan as
    /// defense-in-depth for callers that bypass validation).
    #[test]
    fn count_subquery_union_mix_is_rejected_before_execution() {
        let error = crate::validate::validate_statement_tree(&count_subquery_over(
            "RETURN 1 UNION ALL MATCH (n)",
        ))
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DifferentColumnsInUnion);
        assert_eq!(
            error.to_string(),
            "DifferentColumnsInUnion: cannot mix explicit and omitted RETURN"
        );
    }

    /// A COUNT subquery body whose every branch omits RETURN may join its
    /// branches with UNION ALL only — the second of the two CIP-66 shape
    /// rules, pinned with the executor scan's byte-identical message.
    #[test]
    fn count_subquery_union_omitted_return_requires_union_all() {
        let error = crate::validate::validate_statement_tree(&count_subquery_over(
            "MATCH (n) UNION MATCH (m)",
        ))
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DifferentColumnsInUnion);
        assert_eq!(
            error.to_string(),
            "DifferentColumnsInUnion: omitted RETURN requires UNION ALL"
        );
    }

    /// The two legal shapes pass: every branch omitted with UNION ALL
    /// joiners, and every branch explicit (a plain UNION is fine there —
    /// its dedup has named columns to compare).
    #[test]
    fn count_subquery_union_legal_shapes_pass() {
        for body in [
            "MATCH (n) UNION ALL MATCH (m)",
            "MATCH (n) RETURN n UNION MATCH (n) RETURN n",
        ] {
            crate::validate::validate_statement_tree(&count_subquery_over(body))
                .unwrap_or_else(|e| panic!("a legal COUNT union body was rejected: {e}"));
        }
    }

    /// The rules are COUNT-scoped, not general: an EXISTS body with the
    /// same all-omitted plain-UNION chain stays valid (CIP-63 — existence
    /// does not care about column identity), and so does the same chain
    /// at the top level (CIP-140 omitted-RETURN unions are FINISH
    /// semantics, not errors).
    #[test]
    fn exists_and_top_level_omitted_unions_stay_valid() {
        for query in [
            "RETURN exists { MATCH (n) UNION MATCH (m) }",
            "CREATE (:A) UNION CREATE (:B)",
        ] {
            crate::validate::validate_statement_tree(&cypherlite_parser::parse(query).unwrap())
                .unwrap_or_else(|e| {
                    panic!("an omitted-RETURN union outside COUNT was rejected: {e}")
                });
        }
    }
}
