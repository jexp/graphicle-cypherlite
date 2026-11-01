//! The CIP-224 `<search clause>` rule family: the SEARCH-clause
//! binding-variable, SCORE-conflict, and scope rules of
//! [`validate_search_clauses`], plus the single-path pattern-shape
//! restriction behind it.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, PatternElement, PatternPath, SearchClause, SearchKind};

use crate::ast::collect::{
    bind_pattern_vars, clause_introduced_names, collect_expr_variables, projected_names,
    search_score_var,
};
use crate::ast::search_spec::{walk_search_spec, SearchSlot};

/// CIP-224 `<search clause>` rules that don't fit any existing generic
/// scope tracker:
/// - Rule 2's self-reference sub-case (the search query can't reference the
///   binding variable it's about to produce, Examples SQ1/SQ2).
/// - Rule 4 (the binding variable reference must be declared by this same
///   MATCH statement's own pattern -- Rule 4's NODE/RELATIONSHIP value-type
///   half is checked at runtime, see `match_clause::execute_search_for_row`).
/// - Rule 5 (the SCORE AS variable must not conflict with anything already
///   in scope, including the binding variable reference itself).
/// - Rule 7 (the match statement's pattern shape restriction).
///
/// Rule 5's scope check builds `scope` by walking the body's clauses from
/// empty, deriving each clause's introduced names from
/// [`clause_introduced_names`]: a SEARCH clause sees the names earlier
/// clauses of the same linear body declared. (A CIP-213 NEXT stage starts a
/// fresh body -- its names are bound per stage on the lowered IR, not by
/// this pass.)
pub fn validate_search_clauses(clauses: &[Clause]) -> Result<(), EngineError> {
    let mut scope: HashSet<String> = HashSet::new();

    for clause in clauses {
        match clause {
            // A searching MATCH is the one arm this fold spells itself: the
            // CIP-224 rules need the clause's own pattern variables *before*
            // they enter the scope (Rule 4's binding reference and Rule 5's
            // SCORE conflict both test against them), while
            // [`clause_introduced_names`] folds the pattern variables and the
            // SCORE variable together.
            Clause::Match(mc) => match mc.search.as_ref() {
                Some(search) => {
                    let mut this_match_vars: HashSet<String> = HashSet::new();
                    bind_pattern_vars(&mc.pattern, &mut this_match_vars);
                    validate_search_clause_rules(&mc.pattern, search, &this_match_vars)?;
                    scope.extend(this_match_vars);
                    // Rule 5, checked where the scope lifecycle lives: after
                    // this MATCH's own variables have entered the scope, one
                    // `insert` decides the conflict (and binds on win).
                    if let Some(sv) = search_score_var(mc) {
                        if !scope.insert(sv.to_string()) {
                            return Err(EngineError::semantic(
                                ErrorKind::VariableAlreadyDefined,
                                format!("variable '{sv}' is already defined"),
                            ));
                        }
                    }
                }
                None => scope.extend(clause_introduced_names(clause)),
            },
            // A WITH replaces the scope (a projection), it does not extend
            // it -- the canonical project-items-to-names skeleton answers
            // exactly that (`*` passes the incoming scope through).
            Clause::With(wc) => {
                scope = projected_names(&wc.items, &scope);
            }
            // A `CALL { }` body's exports depend on the enclosing scope.
            Clause::CallSubquery(csq) => {
                let exports = crate::call_scope::call_subquery_exports(csq, &scope);
                scope.extend(exports);
            }
            clause => scope.extend(clause_introduced_names(clause)),
        }
    }
    Ok(())
}

/// The CIP-224 SEARCH rules proper, extracted from the MATCH arm's scope
/// bookkeeping: the pattern shape (Rule 7, via
/// [`validate_search_pattern_shape`]), the binding-variable reference
/// (Rule 4), and the source-specification rules -- the per-index
/// CIP-225/240 kind×option compatibility (on the index) and the
/// CIP-224/278 self-reference rules (Rule 2's `FOR` query and Rule 6's
/// WEIGHT/CONSTANT controls, by slot) -- as one slot-tagged pass over the
/// (possibly nested, CIP-278) source tree, the first violation in walk
/// order stopping it. The Rule 5 SCORE-conflict check stays with the
/// caller, which owns the scope lifecycle.
fn validate_search_clause_rules(
    pattern: &[PatternPath],
    search: &SearchClause,
    this_match_vars: &HashSet<String>,
) -> Result<(), EngineError> {
    validate_search_pattern_shape(pattern)?;

    if !this_match_vars.contains(&search.binding_var) {
        return Err(EngineError::semantic(
            ErrorKind::InvalidSearchVariableReference,
            format!(
                "'{}' must be a variable bound by this SEARCH clause's own MATCH statement",
                search.binding_var
            ),
        ));
    }

    walk_search_spec(
        &search.spec,
        // The kind×option compatibility policy, one spelling per kind and
        // one statement of its polarity: the inner WHERE knob is
        // vector-search-only (CIP-240), while `WITH ANALYZER` and
        // SKIP/OFFSET are fulltext-only (CIP-225 grammar -- accepting them
        // on a VECTOR INDEX silently deviates from the surface, round-4
        // review); CIP-225 on live Neo4j 2026.09.0 additionally rejects
        // WHERE together with FULLTEXT INDEX (42N14). All three rejections
        // raise the typed `InvalidUseOfCommand` detail name.
        &mut |index| {
            match index.kind {
                SearchKind::Fulltext => {
                    if index.inner_where.is_some() {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidUseOfCommand,
                            "WHERE cannot be used together with 'FULLTEXT INDEX'".to_string(),
                        ));
                    }
                }
                SearchKind::Vector => {
                    if index.analyzer.is_some() {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidUseOfCommand,
                            "WITH ANALYZER cannot be used together with VECTOR INDEX".to_string(),
                        ));
                    }
                    if index.offset_expr.is_some() {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidUseOfCommand,
                            "SKIP cannot be used together with VECTOR INDEX".to_string(),
                        ));
                    }
                }
            }
            Ok(())
        },
        // The self-reference rules, dispatched by slot: Rule 2's search
        // query (`FOR`) and CIP-278 Rule 6's WEIGHT/CONSTANT controls may
        // reference any incoming variable but not the search clause's own
        // binding variable. The inner WHERE is deliberately excluded --
        // CIP-240's predicate shape is `binding variable . property`, so
        // it names the binding variable by design.
        &mut |slot, expr| {
            let message = match slot {
                SearchSlot::For => "the search query cannot reference the binding variable",
                SearchSlot::Weight | SearchSlot::Constant => {
                    "the WEIGHT or CONSTANT expression may not depend on the search clause \
                     binding variable"
                }
                // Analyzer/InnerWhere/Offset/Limit are exempt by policy
                // (the inner WHERE names the binding variable by design);
                // the match is exhaustive so a new slot must state its
                // policy here before it compiles.
                SearchSlot::Analyzer
                | SearchSlot::InnerWhere
                | SearchSlot::Offset
                | SearchSlot::Limit => return Ok(()),
            };
            let mut vars = HashSet::new();
            collect_expr_variables(expr, &mut vars);
            if vars.contains(&search.binding_var) {
                return Err(EngineError::semantic(
                    ErrorKind::SelfReferencingSearch,
                    format!("{message} '{}'", search.binding_var),
                ));
            }
            Ok(())
        },
    )
}

fn validate_search_pattern_shape(pattern: &[PatternPath]) -> Result<(), EngineError> {
    if pattern.len() != 1 {
        return Err(EngineError::semantic(ErrorKind::SyntaxError,
            "SearchClauseTooComplexPattern: a SEARCH clause's MATCH statement must be a single path pattern".to_string(),
        ));
    }
    let path = &pattern[0];
    if path.selector.is_some() || path.is_shortest_path_function {
        return Err(EngineError::semantic(ErrorKind::SyntaxError,
            "SearchClauseTooComplexPattern: a SEARCH clause's MATCH statement cannot use a path selector".to_string(),
        ));
    }
    match path.elements.as_slice() {
        [PatternElement::Node(_)] => Ok(()),
        [PatternElement::Node(n0), PatternElement::Relationship(rp), PatternElement::Node(n1)] => {
            if rp.min_hops.is_some() || rp.max_hops.is_some() || rp.quantified {
                return Err(EngineError::semantic(ErrorKind::SyntaxError,
                    "SearchClauseTooComplexPattern: a SEARCH clause's MATCH statement cannot use a variable-length relationship".to_string(),
                ));
            }
            let bound_count = [n0.variable.is_some(), rp.variable.is_some(), n1.variable.is_some()]
                .iter()
                .filter(|b| **b)
                .count();
            if bound_count > 1 {
                return Err(EngineError::semantic(ErrorKind::SyntaxError,
                    "SearchClauseMultipleBoundVariables: a SEARCH clause's MATCH statement may bind only one variable".to_string(),
                ));
            }
            // The bound variable is the only element that may carry
            // predicates (Rule 7) -- one spelling of the rejection, checked
            // over every element position.
            let unbound_with_predicates = [
                (n0.variable.is_none(), has_node_pattern_predicates(n0)),
                (n1.variable.is_none(), has_node_pattern_predicates(n1)),
                (rp.variable.is_none(), has_rel_pattern_predicates(rp)),
            ]
            .into_iter()
            .any(|(unbound, has_predicates)| unbound && has_predicates);
            if unbound_with_predicates {
                return Err(EngineError::semantic(ErrorKind::SyntaxError,
                    "SearchClauseInvalidPredicates: a SEARCH clause's MATCH statement cannot have predicates on elements other than the bound variable".to_string(),
                ));
            }
            Ok(())
        }
        _ => Err(EngineError::semantic(ErrorKind::SyntaxError,
            "SearchClauseTooComplexPattern: a SEARCH clause's MATCH statement must be a single node or single-hop relationship pattern".to_string(),
        )),
    }
}

fn has_node_pattern_predicates(np: &cypherlite_parser::NodePattern) -> bool {
    np.label_expr.is_some()
        || np.has_properties
        || np.properties_param.is_some()
        || np.where_expr.is_some()
}

fn has_rel_pattern_predicates(rp: &cypherlite_parser::RelPattern) -> bool {
    rp.type_expr.is_some()
        || !rp.properties.is_empty()
        || rp.properties_param.is_some()
        || rp.where_expr.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CIP-224 rules that raise typed [`ErrorKind`]s render with their
    /// own detail name (the payload unchanged after it) -- pinned here
    /// because the rendered text is the error contract, raised by the
    /// validate battery before lowering. The remaining SEARCH rejections
    /// (the pattern-shape family) keep the `SyntaxError:` outer kind with
    /// their detail in the message.
    #[test]
    fn search_rules_raise_their_typed_kinds() {
        for (query, kind, text) in [
            (
                "MATCH (n) SEARCH m IN (FULLTEXT INDEX ft FOR 'x' LIMIT 1) RETURN n",
                ErrorKind::InvalidSearchVariableReference,
                "InvalidSearchVariableReference: 'm' must be a variable bound by this \
                 SEARCH clause's own MATCH statement",
            ),
            (
                "MATCH (n) SEARCH n IN (FULLTEXT INDEX ft FOR n.title LIMIT 1) RETURN n",
                ErrorKind::SelfReferencingSearch,
                "SelfReferencingSearch: the search query cannot reference the binding \
                 variable 'n'",
            ),
            (
                "MATCH (n) SEARCH n IN (FULLTEXT INDEX ft FOR 'x' LIMIT 1) SCORE AS n \
                 RETURN n",
                ErrorKind::VariableAlreadyDefined,
                "VariableAlreadyDefined: variable 'n' is already defined",
            ),
            (
                "MATCH (n) SEARCH n IN (FULLTEXT INDEX ft FOR 'x' WHERE n.p > 1 LIMIT 1) \
                 RETURN n",
                ErrorKind::InvalidUseOfCommand,
                "InvalidUseOfCommand: WHERE cannot be used together with 'FULLTEXT INDEX'",
            ),
            (
                "MATCH (n) SEARCH n IN (VECTOR INDEX v FOR [1.0] WITH ANALYZER 'a' LIMIT 1) \
                 RETURN n",
                ErrorKind::InvalidUseOfCommand,
                "InvalidUseOfCommand: WITH ANALYZER cannot be used together with VECTOR INDEX",
            ),
            (
                "MATCH (n) SEARCH n IN (VECTOR INDEX v FOR [1.0] SKIP 1 LIMIT 1) RETURN n",
                ErrorKind::InvalidUseOfCommand,
                "InvalidUseOfCommand: SKIP cannot be used together with VECTOR INDEX",
            ),
        ] {
            let error =
                crate::validate::validate_statement_tree(&cypherlite_parser::parse(query).unwrap())
                    .unwrap_err();
            assert_eq!(error.kind(), kind, "wrong kind for: {query}");
            assert_eq!(error.to_string(), text, "wrong text for: {query}");
        }
    }
}
