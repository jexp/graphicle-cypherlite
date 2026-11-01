//! Pattern semantic checks: inline property maps and predicates, group
//! variable scoping (QPPs), and the CREATE/MERGE pattern restrictions.

use std::collections::HashMap;

use cypherlite_core::{Error, Result};
use cypherlite_ir::{expr_vars, PathPattern, PatternElement};

use crate::binding::{Binding, Env};
use crate::expr_check::{check_expr, check_pred};

pub(crate) fn check_path_exprs(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    p: &PathPattern,
) -> Result<()> {
    for n in &p.nodes {
        if let Some(e) = &n.properties {
            check_expr(env, is_write_procedure, e)?;
        }
    }
    for r in &p.rels {
        if let Some(e) = &r.properties {
            check_expr(env, is_write_procedure, e)?;
        }
    }
    check_gpm_element_exprs(env, is_write_procedure, &p.elements)?;
    if let Some(predicate) = &p.where_clause {
        check_pred(env, is_write_procedure, predicate)?;
    }
    Ok(())
}

fn check_gpm_element_exprs(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    elements: &[PatternElement],
) -> Result<()> {
    for element in elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(expr) = &node.properties {
                    check_expr(env, is_write_procedure, expr)?;
                }
                if let Some(predicate) = &node.where_clause {
                    check_pred(env, is_write_procedure, predicate)?;
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(expr) = &rel.properties {
                    check_expr(env, is_write_procedure, expr)?;
                }
                if let Some(predicate) = &rel.where_clause {
                    if rel.quantified {
                        let mut local = env.clone();
                        if let Some(name) = &rel.variable {
                            local.insert(name.clone(), Binding::Rel);
                        }
                        check_pred(&local, is_write_procedure, predicate)?;
                    } else {
                        check_pred(env, is_write_procedure, predicate)?;
                    }
                }
            }
            PatternElement::Quantified(qpp) => {
                let local_names: std::collections::HashSet<String> = qpp
                    .elements
                    .iter()
                    .filter_map(|inner| match inner {
                        PatternElement::Node(node) => node.variable.clone(),
                        PatternElement::Relationship(rel) => rel.variable.clone(),
                        PatternElement::Quantified(nested) => nested.subpath_variable.clone(),
                    })
                    .chain(qpp.subpath_variable.iter().cloned())
                    .collect();
                let mut predicate_vars = std::collections::HashSet::new();
                if let Some(predicate) = &qpp.where_clause {
                    expr_vars(predicate, &mut predicate_vars);
                }
                for inner in &qpp.elements {
                    match inner {
                        PatternElement::Node(node) => {
                            if let Some(predicate) = &node.where_clause {
                                expr_vars(predicate, &mut predicate_vars);
                            }
                        }
                        PatternElement::Relationship(rel) => {
                            if let Some(predicate) = &rel.where_clause {
                                expr_vars(predicate, &mut predicate_vars);
                            }
                        }
                        PatternElement::Quantified(_) => {}
                    }
                }
                if predicate_vars.iter().any(|name| {
                    !local_names.contains(name) && env.get(name) == Some(&Binding::List)
                }) {
                    return Err(Error::syntax(
                        "NonLocalGroupVariableReference: quantified predicates cannot reference another group variable",
                    ));
                }
                let mut local = env.clone();
                for inner in &qpp.elements {
                    match inner {
                        PatternElement::Node(node) => {
                            if let Some(name) = &node.variable {
                                local.insert(name.clone(), Binding::Node);
                            }
                        }
                        PatternElement::Relationship(rel) => {
                            if let Some(name) = &rel.variable {
                                local.insert(name.clone(), Binding::Rel);
                            }
                        }
                        PatternElement::Quantified(_) => {}
                    }
                }
                check_gpm_element_exprs(&local, is_write_procedure, &qpp.elements)?;
                if let Some(predicate) = &qpp.where_clause {
                    check_pred(&local, is_write_procedure, predicate)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_write_label_exprs(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    p: &PathPattern,
) -> Result<()> {
    fn check_label(
        env: &Env,
        is_write_procedure: &mut impl FnMut(&str) -> bool,
        expr: &cypherlite_ir::LabelExpr,
    ) -> Result<()> {
        match expr {
            cypherlite_ir::LabelExpr::Dynamic { expr, .. } => {
                check_expr(env, is_write_procedure, expr)
            }
            cypherlite_ir::LabelExpr::Not(inner) => check_label(env, is_write_procedure, inner),
            cypherlite_ir::LabelExpr::And(items) | cypherlite_ir::LabelExpr::Or(items) => {
                for item in items {
                    check_label(env, is_write_procedure, item)?;
                }
                Ok(())
            }
            cypherlite_ir::LabelExpr::Label(_) | cypherlite_ir::LabelExpr::Wildcard => Ok(()),
        }
    }
    for node in &p.nodes {
        if let Some(expr) = &node.label_expr {
            check_label(env, is_write_procedure, expr)?;
        }
    }
    for rel in &p.rels {
        if let Some(expr) = &rel.type_expr {
            check_label(env, is_write_procedure, expr)?;
        }
    }
    Ok(())
}

/// Named variables in a pattern predicate / shortestPath must already be bound.
pub(crate) fn check_pattern_pred_vars(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    p: &PathPattern,
) -> Result<()> {
    if let Some(v) = &p.variable {
        if !env.contains_key(v) {
            return Err(Error::syntax(format!("UndefinedVariable: `{v}`")));
        }
    }
    for n in &p.nodes {
        if let Some(v) = &n.variable {
            if !env.contains_key(v) {
                return Err(Error::syntax(format!("UndefinedVariable: `{v}`")));
            }
        }
        if let Some(e) = &n.properties {
            check_expr(env, is_write_procedure, e)?;
        }
    }
    for r in &p.rels {
        if let Some(v) = &r.variable {
            if !env.contains_key(v) {
                return Err(Error::syntax(format!("UndefinedVariable: `{v}`")));
            }
        }
        if let Some(e) = &r.properties {
            check_expr(env, is_write_procedure, e)?;
        }
    }
    Ok(())
}

pub(crate) fn check_write_rels(p: &PathPattern, create: bool) -> Result<()> {
    if p.where_clause.is_some()
        || p.elements.iter().any(|element| match element {
            PatternElement::Node(node) => node.where_clause.is_some(),
            PatternElement::Relationship(rel) => rel.where_clause.is_some(),
            PatternElement::Quantified(_) => true,
        })
    {
        return Err(Error::syntax(
            "InvalidNodePatternPredicate: inline pattern predicates are not allowed in CREATE/MERGE",
        ));
    }
    for r in &p.rels {
        if r.var_length {
            return Err(Error::syntax(
                "CreatingVarLength: variable-length relationships cannot be created",
            ));
        }
        if create && r.direction == cypherlite_ir::Direction::Both {
            return Err(Error::syntax(
                "RequiresDirectedRelationship: CREATE/MERGE requires a directed relationship",
            ));
        }
        if r.types.len() != 1
            && !r
                .type_expr
                .as_ref()
                .is_some_and(label_expr_contains_dynamic)
        {
            return Err(Error::syntax(
                "NoSingleRelationshipType: CREATE/MERGE requires exactly one relationship type",
            ));
        }
    }
    Ok(())
}

fn label_expr_contains_dynamic(expr: &cypherlite_ir::LabelExpr) -> bool {
    match expr {
        cypherlite_ir::LabelExpr::Dynamic { .. } => true,
        cypherlite_ir::LabelExpr::Not(inner) => label_expr_contains_dynamic(inner),
        cypherlite_ir::LabelExpr::And(items) | cypherlite_ir::LabelExpr::Or(items) => {
            items.iter().any(label_expr_contains_dynamic)
        }
        cypherlite_ir::LabelExpr::Label(_) | cypherlite_ir::LabelExpr::Wildcard => false,
    }
}

pub(crate) fn element_has_qpp(element: &PatternElement) -> bool {
    match element {
        PatternElement::Quantified(_) => true,
        PatternElement::Relationship(rel) => rel.quantified,
        PatternElement::Node(_) => false,
    }
}

pub(crate) fn collect_qpp_group_names(
    elements: &[PatternElement],
    seen: &mut HashMap<String, ()>,
    outer: &Env,
) -> Result<()> {
    for element in elements {
        if let PatternElement::Relationship(rel) = element {
            if rel.quantified {
                if let Some(name) = &rel.variable {
                    if outer.contains_key(name) || seen.insert(name.clone(), ()).is_some() {
                        return Err(Error::syntax(format!(
                            "GroupVariableReuse: `{name}` is already bound"
                        )));
                    }
                }
            }
        }
        if let PatternElement::Quantified(qpp) = element {
            let mut local = std::collections::HashSet::new();
            for inner in &qpp.elements {
                let name = match inner {
                    PatternElement::Node(node) => node.variable.as_ref(),
                    PatternElement::Relationship(rel) => rel.variable.as_ref(),
                    PatternElement::Quantified(nested) => nested.subpath_variable.as_ref(),
                };
                if let Some(name) = name {
                    local.insert(name.clone());
                }
            }
            if let Some(name) = &qpp.subpath_variable {
                local.insert(name.clone());
            }
            for name in local {
                if outer.contains_key(&name) || seen.insert(name.clone(), ()).is_some() {
                    return Err(Error::syntax(format!(
                        "GroupVariableReuse: `{name}` is already bound"
                    )));
                }
            }
            collect_qpp_group_names(&qpp.elements, seen, outer)?;
        }
    }
    Ok(())
}
