//! Variable binding: the scope environment threaded through validation and
//! the rules for introducing pattern variables into it.

use std::collections::HashMap;

use cypherlite_core::{Error, Result};
use cypherlite_ir::{PathPattern, PatternElement};

/// What kind of value a name in scope is bound to. The binder tracks only
/// what later checks need: entity-ness and literal-ness for type-conflict
/// and argument-type errors.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    Node,
    Rel,
    Path,
    Value,
    /// Non-null literal (int/bool/string/float). MATCH as a node/rel is a type conflict.
    Scalar,
    /// Literal list. Property access is InvalidArgumentType.
    List,
    /// Literal map. MATCH as a node/rel is a type conflict.
    Map,
}

pub(crate) type Env = HashMap<String, Binding>;

pub(crate) fn bind(env: &mut Env, name: &str, kind: Binding, create: bool) -> Result<()> {
    if create {
        if env.contains_key(name) {
            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
        }
        env.insert(name.to_string(), kind);
        return Ok(());
    }
    if kind == Binding::Path && env.contains_key(name) {
        return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
    }
    if let Some(prev) = env.get(name).copied() {
        if prev == Binding::Path && kind != Binding::Path {
            return Err(Error::syntax(format!(
                "VariableTypeConflict: `{name}` already bound to a different type"
            )));
        }
        if prev == Binding::Scalar && kind != Binding::Scalar {
            return Err(Error::syntax(format!(
                "VariableTypeConflict: `{name}` already bound to a different type"
            )));
        }
        if prev == Binding::Map && kind != Binding::Map {
            return Err(Error::syntax(format!(
                "VariableTypeConflict: `{name}` already bound to a different type"
            )));
        }
        if prev == Binding::List && kind != Binding::List {
            // A list of relationships may be reused as a var-length pattern.
            if kind == Binding::Rel {
                return Ok(());
            }
            return Err(Error::syntax(format!(
                "VariableTypeConflict: `{name}` already bound to a different type"
            )));
        }
        if prev == Binding::Value {
            env.insert(name.to_string(), kind);
            return Ok(());
        }
        if prev != kind {
            return Err(Error::syntax(format!(
                "VariableTypeConflict: `{name}` already bound to a different type"
            )));
        }
        return Ok(());
    }
    env.insert(name.to_string(), kind);
    Ok(())
}

pub(crate) fn bind_path(env: &mut Env, p: &PathPattern, create: bool) -> Result<()> {
    if !create
        && p.nodes
            .first()
            .and_then(|node| node.variable.as_ref())
            .zip(p.nodes.last().and_then(|node| node.variable.as_ref()))
            .is_some_and(|(first, last)| first == last)
    {
        let mut rels = std::collections::HashSet::new();
        if p.rels
            .iter()
            .filter_map(|rel| rel.variable.as_ref())
            .any(|name| !rels.insert(name))
        {
            return Err(Error::syntax(
                "RelationshipUniquenessViolation: a relationship variable cannot be reused in a closed legacy pattern",
            ));
        }
    }
    if let Some(v) = &p.variable {
        bind(env, v, Binding::Path, create)?;
    }
    for n in &p.nodes {
        if let Some(v) = &n.variable {
            bind(env, v, Binding::Node, create)?;
        }
    }
    for r in &p.rels {
        if let Some(v) = &r.variable {
            if env.get(v) == Some(&Binding::List) && !r.var_length {
                return Err(Error::syntax(format!(
                    "VariableTypeConflict: `{v}` already bound to a different type"
                )));
            }
            bind(
                env,
                v,
                if r.var_length {
                    Binding::List
                } else {
                    Binding::Rel
                },
                create,
            )?;
        }
    }
    if p.nodes.is_empty() && p.rels.is_empty() {
        bind_gpm_elements(env, &p.elements, create, false)?;
    }
    Ok(())
}

fn bind_gpm_elements(
    env: &mut Env,
    elements: &[PatternElement],
    create: bool,
    grouped: bool,
) -> Result<()> {
    for element in elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(name) = &node.variable {
                    bind(
                        env,
                        name,
                        if grouped {
                            Binding::List
                        } else {
                            Binding::Node
                        },
                        create,
                    )?;
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(name) = &rel.variable {
                    bind(
                        env,
                        name,
                        if grouped || rel.var_length {
                            Binding::List
                        } else {
                            Binding::Rel
                        },
                        create,
                    )?;
                }
            }
            PatternElement::Quantified(qpp) => {
                bind_gpm_elements(env, &qpp.elements, create, true)?;
                if let Some(name) = &qpp.subpath_variable {
                    bind(env, name, Binding::List, create)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn bind_create_path(env: &mut Env, p: &PathPattern) -> Result<()> {
    if let Some(v) = &p.variable {
        bind(env, v, Binding::Path, true)?;
    }
    for n in &p.nodes {
        if let Some(v) = &n.variable {
            if env.contains_key(v) {
                if p.rels.is_empty() || !n.labels.is_empty() || n.properties.is_some() {
                    return Err(Error::syntax(format!("VariableAlreadyBound: `{v}`")));
                }
            } else {
                env.insert(v.clone(), Binding::Node);
            }
        }
    }
    for r in &p.rels {
        if let Some(v) = &r.variable {
            if env.contains_key(v) {
                if !r.types.is_empty() || r.properties.is_some() {
                    return Err(Error::syntax(format!("VariableAlreadyBound: `{v}`")));
                }
            } else {
                env.insert(v.clone(), Binding::Rel);
            }
        }
    }
    Ok(())
}
