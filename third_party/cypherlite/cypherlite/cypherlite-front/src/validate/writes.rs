//! CREATE/MERGE write-pattern legality: exactly one relationship type,
//! directed relationships, no variable-length relationships or quantified
//! path patterns in writes, CIP-156 parameter property-map shorthand
//! (CREATE-only) and dynamic label/type rules.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, LabelExpr, PatternElement, PatternPath};

use super::duplicate_variable;
use crate::ast::collect::collect_expr_variables;

/// True if this relationship type spec is anything other than exactly one
/// plain type name -- used by the CREATE/MERGE "exactly one relationship
/// type" validation, which now must also reject GQL label-expression
/// operators (`|`, `&`, `!`, `%`), not just old-style multi-type pipe lists.
fn is_single_type(type_expr: &Option<LabelExpr>) -> bool {
    // A dynamic type atom's resolved count isn't known until runtime
    // (CIP-156) -- the exactly-one-type rule for CREATE/MERGE relationships
    // is enforced against the resolved name list instead (see
    // the executor's `eval::labels::resolve_write_labels`).
    if type_expr.as_ref().is_some_and(LabelExpr::contains_dynamic) {
        return true;
    }
    matches!(
        type_expr
            .as_ref()
            .and_then(|e| e.as_label_list())
            .as_deref(),
        Some([_])
    )
}
/// Validate CREATE patterns for VariableAlreadyBound errors.
/// In CREATE, a node variable that is already bound must not introduce new labels
/// or properties — it can only be used as a reference. If it has labels or props,
/// that's an error. If it has neither and is bound, it's also VariableAlreadyBound
/// (you can't CREATE a node that's already bound).
pub(crate) fn validate_create_bindings(
    patterns: &[PatternPath],
    bound_vars: &HashSet<String>,
) -> Result<(), EngineError> {
    use cypherlite_parser::Direction;
    // Track variables defined within THIS CREATE clause (across multiple paths)
    let mut create_node_vars: HashSet<String> = HashSet::new();

    for path in patterns {
        for elem in &path.elements {
            match elem {
                PatternElement::Node(np) => {
                    if let Some(var) = &np.variable {
                        let already_in_scope =
                            bound_vars.contains(var) || create_node_vars.contains(var);
                        if already_in_scope {
                            // Variable is already bound — check if it's being "re-created"
                            if np.label_expr.is_some()
                                || !np.properties.is_empty()
                                || np.has_properties
                            {
                                return Err(duplicate_variable(var));
                            }
                            // Even with no labels/props, creating an already-bound node is invalid
                            // UNLESS it appears alongside relationships (i.e., it's used as a reference
                            // endpoint for relationship creation)
                            let has_rels = path
                                .elements
                                .iter()
                                .any(|e| matches!(e, PatternElement::Relationship(_)));
                            if !has_rels {
                                return Err(duplicate_variable(var));
                            }
                        }
                        create_node_vars.insert(var.clone());
                    }
                }
                PatternElement::Relationship(rp) => {
                    // A relationship variable that is already bound (e.g. from a
                    // preceding MATCH — Create2 [23] `MATCH ()-[r]->() CREATE
                    // ()-[r]->()`) is `VariableAlreadyBound`. This check runs
                    // before the single-type/direction checks because the
                    // already-bound diagnosis is the more specific (and
                    // openCypher-expected) one — a bound `r` reused with no type
                    // must report VariableAlreadyBound, not NoSingleRelationshipType.
                    if let Some(var) = &rp.variable {
                        if bound_vars.contains(var) {
                            return Err(duplicate_variable(var));
                        }
                    }
                    // CREATE relationships must have exactly one type
                    if !is_single_type(&rp.type_expr) {
                        return Err(EngineError::semantic(
                            ErrorKind::NoSingleRelationshipType,
                            "CREATE relationships must have exactly one type".to_string(),
                        ));
                    }
                    // CREATE relationships must have a direction (not undirected)
                    if rp.direction == Direction::Both {
                        return Err(EngineError::semantic(
                            ErrorKind::RequiresDirectedRelationship,
                            "CREATE relationships must have a direction".to_string(),
                        ));
                    }
                    // CREATE relationships cannot be variable-length
                    if rp.min_hops.is_some() || rp.max_hops.is_some() {
                        return Err(EngineError::semantic(
                            ErrorKind::CreatingVarLength,
                            "variable length relationships cannot be used in CREATE".to_string(),
                        ));
                    }
                    // (The already-bound relationship-variable check now runs at
                    // the top of this arm, before the single-type check.)
                }
                PatternElement::Quantified(_) => {
                    return Err(EngineError::semantic(
                        ErrorKind::QuantifiedPathPatternNotSupported,
                        "quantified path patterns cannot be used in CREATE".to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// CIP-156's `$paramName` whole-property-map pattern shorthand (e.g.
/// `CREATE (n:Label $props)`) is CREATE-only -- the parser accepts the
/// syntax generically for any pattern position, but Neo4j rejects it in
/// MATCH/MERGE patterns with `InvalidParameterUse` (TCK
/// `clauses/match/Match1.feature` scenario 6, `Match2.feature` scenario 8,
/// `clauses/merge/Merge1.feature` scenario 16).
///
/// Checks one linear body. Nested bodies (`CALL { }`, `WHEN`/`ELSE`, UNION
/// arms, NEXT stages) are covered by the whole-tree validation battery,
/// which applies this pass to every nested statement it walks.
pub fn validate_param_property_shorthand(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        match clause {
            Clause::Match(mc) => {
                for path in &mc.pattern {
                    check_no_param_property_shorthand_in_elements(&path.elements)?;
                }
            }
            Clause::Merge(mc) => {
                check_no_param_property_shorthand_in_elements(&mc.pattern.elements)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_no_param_property_shorthand_in_elements(
    elements: &[PatternElement],
) -> Result<(), EngineError> {
    for elem in elements {
        match elem {
            PatternElement::Node(np) => {
                if np.properties_param.is_some() {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidParameterUse,
                        "a parameter may not be used as a node predicate outside of CREATE"
                            .to_string(),
                    ));
                }
            }
            PatternElement::Relationship(rp) => {
                if rp.properties_param.is_some() {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidParameterUse,
                        "a parameter may not be used as a relationship predicate outside of CREATE"
                            .to_string(),
                    ));
                }
            }
            PatternElement::Quantified(qpp) => {
                check_no_param_property_shorthand_in_elements(&qpp.elements)?;
            }
        }
    }
    Ok(())
}
/// CIP-156: `$any(...)` is a MATCH-only disjunction quantifier for dynamic
/// label/type expressions -- Neo4j rejects it in CREATE/MERGE (GQLSTATUS
/// `42I55`) for both node labels and relationship types, since a
/// conjunction is required to unambiguously determine what to create.
/// `$(...)`/`$all(...)` remain allowed (checked at runtime by
/// the executor's `eval::labels::resolve_write_labels`).
pub(crate) fn validate_dynamic_label_no_any(patterns: &[PatternPath]) -> Result<(), EngineError> {
    for path in patterns {
        for elem in &path.elements {
            let label_expr = match elem {
                PatternElement::Node(np) => &np.label_expr,
                PatternElement::Relationship(rp) => &rp.type_expr,
                PatternElement::Quantified(_) => continue,
            };
            if let Some(le) = label_expr {
                check_no_any_quantifier(le)?;
            }
        }
    }
    Ok(())
}

fn check_no_any_quantifier(expr: &LabelExpr) -> Result<(), EngineError> {
    match expr {
        LabelExpr::Dynamic(d) if d.quantifier == cypherlite_parser::DynamicLabelQuantifier::Any => {
            Err(EngineError::semantic(
                ErrorKind::InvalidLabelExpression,
                "Dynamic labels/types using $any() are not allowed in CREATE/MERGE".to_string(),
            ))
        }
        LabelExpr::Not(inner) => check_no_any_quantifier(inner),
        LabelExpr::And(items) | LabelExpr::Or(items) => {
            for item in items {
                check_no_any_quantifier(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// CIP-156: a dynamic label/type expression (`$(...)`/`$all(...)`/`$any(...)`)
/// within a CREATE/MERGE clause cannot reference any variable that this
/// same clause itself creates -- unlike ordinary property values, which may
/// freely reference an earlier-created node/relationship of the same
/// clause. MATCH is exempt (its dynamic expressions are resolved against
/// already-bound rows with no such restriction -- see
/// `EvalContext::label_expr_matches`).
pub(crate) fn validate_dynamic_label_scope(
    patterns: &[PatternPath],
    bound_vars: &HashSet<String>,
) -> Result<(), EngineError> {
    let mut created_here: HashSet<String> = HashSet::new();
    for path in patterns {
        for elem in &path.elements {
            let var = match elem {
                PatternElement::Node(np) => &np.variable,
                PatternElement::Relationship(rp) => &rp.variable,
                PatternElement::Quantified(_) => continue,
            };
            if let Some(v) = var {
                if !bound_vars.contains(v) {
                    created_here.insert(v.clone());
                }
            }
        }
    }
    if created_here.is_empty() {
        return Ok(());
    }
    for path in patterns {
        for elem in &path.elements {
            let label_expr = match elem {
                PatternElement::Node(np) => &np.label_expr,
                PatternElement::Relationship(rp) => &rp.type_expr,
                PatternElement::Quantified(_) => continue,
            };
            if let Some(le) = label_expr {
                check_dynamic_label_scope(le, &created_here)?;
            }
        }
    }
    Ok(())
}

fn check_dynamic_label_scope(
    expr: &LabelExpr,
    created_here: &HashSet<String>,
) -> Result<(), EngineError> {
    match expr {
        LabelExpr::Dynamic(d) => {
            let mut vars = HashSet::new();
            collect_expr_variables(&d.expr, &mut vars);
            if let Some(var) = vars.iter().find(|v| created_here.contains(*v)) {
                return Err(EngineError::semantic(
                    ErrorKind::SyntaxError,
                    format!("Entity, '{var}', cannot be created and referenced in the same clause"),
                ));
            }
            Ok(())
        }
        LabelExpr::Not(inner) => check_dynamic_label_scope(inner, created_here),
        LabelExpr::And(items) | LabelExpr::Or(items) => {
            for item in items {
                check_dynamic_label_scope(item, created_here)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate MERGE pattern for VariableAlreadyBound errors.
pub(crate) fn validate_merge_bindings(
    pattern: &PatternPath,
    bound_vars: &HashSet<String>,
) -> Result<(), EngineError> {
    // Check if all node variables in the pattern are already bound
    // A MERGE with a fully-bound pattern is fine (it's matching)
    // But a MERGE with a node variable that is bound AND has new labels = error
    // Also check that relationship variables aren't already bound
    let has_rels = pattern
        .elements
        .iter()
        .any(|e| matches!(e, PatternElement::Relationship(_)));

    for elem in &pattern.elements {
        match elem {
            PatternElement::Node(np) => {
                if let Some(var) = &np.variable {
                    if bound_vars.contains(var) {
                        // For MERGE, a bound node used as endpoint for relationships is ok
                        // But a bare MERGE (n) or MERGE (n:Label) where n is already bound is an error
                        if !has_rels {
                            return Err(duplicate_variable(var));
                        }
                        // Even with rels, if it introduces new labels, that's an error
                        if np.label_expr.is_some() || !np.properties.is_empty() {
                            return Err(duplicate_variable(var));
                        }
                    }
                }
            }
            PatternElement::Relationship(rp) => {
                if let Some(var) = &rp.variable {
                    if bound_vars.contains(var) {
                        return Err(duplicate_variable(var));
                    }
                }
                // Check for variable-length relationships in MERGE
                if rp.min_hops.is_some() || rp.max_hops.is_some() {
                    return Err(EngineError::semantic(
                        ErrorKind::CreatingVarLength,
                        "variable length relationships cannot be used in MERGE".to_string(),
                    ));
                }
                // Check for missing/non-single relationship type in MERGE
                if !is_single_type(&rp.type_expr) {
                    return Err(EngineError::semantic(
                        ErrorKind::NoSingleRelationshipType,
                        "MERGE relationships must have exactly one type".to_string(),
                    ));
                }
            }
            PatternElement::Quantified(_) => {
                return Err(EngineError::semantic(
                    ErrorKind::QuantifiedPathPatternNotSupported,
                    "quantified path patterns cannot be used in MERGE".to_string(),
                ));
            }
        }
    }
    Ok(())
}
