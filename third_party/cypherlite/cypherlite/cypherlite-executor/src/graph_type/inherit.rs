//! CIP-266 (ADR-0388): implied labels that identify node element types
//! -- inheritance. The one module for the inheritance graph over a
//! `GraphTypeBody`: which implied labels identify other in-body node
//! element types (the inheritance edges), the cycle check, the effective
//! property-definition resolution (a child's own declaration or the
//! intersection of its parents' effective definitions -- the CIP-266
//! diamond rule), and the effective default-value resolution (CIP-262's
//! interaction with inheritance: own default, else the one inherited
//! default -- two is a conflict the child must resolve itself).
//!
//! Everything here is a pure function of the *whole* body (the merged
//! body, for `ALTER CURRENT GRAPH TYPE`: `apply_add`/`apply_alter` merge
//! before validating), so an `ADD` that turns an existing implied label
//! into an identifying one (CIP-266 PAGTE6) and a child declared in one
//! statement with its parent in another are the same graph.

use std::collections::{BTreeMap, BTreeSet};

use super::canon_intersection;
use cypherlite_error::EngineError;
use cypherlite_parser::type_normalize::{type_expr_to_canon, CanonType};
use cypherlite_parser::{GraphTypeBody, GraphTypeNodeTypeSpec};

use super::syn;

/// The deepest inheritance chain a body may declare (ADR-0388 review,
/// LABS-386): a flat comma-list chain of element types never trips the
/// parser's `MAX_NESTING_DEPTH` (nesting, not list length), so the fold
/// bounds itself here, aligned with that guard family's value
/// neighborhood -- far above any real hierarchy, typed error at the
/// boundary, never an 8 MiB stack overflow on a ~25k-link body.
const MAX_INHERITANCE_DEPTH: usize = 500;

/// The inheritance graph of one whole body: for every node element type,
/// the implied labels that identify another in-body node element type --
/// its parents (CIP-266). Child order is the body's declaration order;
/// the parent list keeps the implied-label order.
#[derive(Debug, Default)]
pub struct Inheritance {
    parents: BTreeMap<String, Vec<String>>,
}

impl Inheritance {
    /// Builds the inheritance graph of a whole body.
    pub fn of(body: &GraphTypeBody) -> Self {
        let identifying: BTreeSet<&str> = body
            .node_types
            .iter()
            .map(|n| n.identifying_label.as_str())
            .collect();
        let parents = body
            .node_types
            .iter()
            .map(|n| {
                (
                    n.identifying_label.clone(),
                    n.implied_labels
                        .iter()
                        .filter(|l| identifying.contains(l.as_str()))
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        Self { parents }
    }

    /// Whether the body has any inheritance edge at all (the CIP-266
    /// gate's executor-side predicate for the stored-state half).
    pub fn is_empty(&self) -> bool {
        self.parents.values().all(|p| p.is_empty())
    }

    /// The first inheritance cycle, as the `(x, y)` pair that closes it
    /// (CIP-266 NAGTE3: "`X` is not allowed as an implied label on the
    /// node element type identified by the label `Y`"). An iterative
    /// three-color DFS over the parent edges -- no recursion, so a deep
    /// chain cannot grow the stack.
    pub fn find_cycle(&self) -> Option<(String, String)> {
        let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 0 white, 1 gray, 2 black
        for start in self.parents.keys() {
            if state.get(start.as_str()).is_some_and(|&s| s != 0) {
                continue;
            }
            // Stack of (label, next-parent-index): the DFS frames.
            let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
            state.insert(start.as_str(), 1);
            while let Some(&mut (label, ref mut index)) = stack.last_mut() {
                let parents = match self.parents.get(label) {
                    Some(parents) => parents,
                    None => {
                        stack.pop();
                        state.insert(label, 2);
                        continue;
                    }
                };
                if *index >= parents.len() {
                    stack.pop();
                    state.insert(label, 2);
                    continue;
                }
                let parent = parents[*index].as_str();
                *index += 1;
                match state.get(parent) {
                    None | Some(0) => {
                        state.insert(parent, 1);
                        stack.push((parent, 0));
                    }
                    Some(1) => {
                        // A gray parent closes a cycle: the edge on the
                        // stack from the parent back to this child.
                        return Some((label.to_string(), parent.to_string()));
                    }
                    Some(_) => {}
                }
            }
        }
        None
    }

    /// The parents of `label` (its implied labels that identify in-body
    /// node element types).
    pub fn parents_of(&self, label: &str) -> &[String] {
        self.parents.get(label).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `ancestor` is a proper ancestor of `descendant` in the
    /// inheritance graph (a parent, or a parent's parent, ...). Iterative
    /// BFS over the parent edges -- cycles were rejected, so the walk
    /// terminates; a visited set guards a degenerate body anyway.
    pub fn is_ancestor_of(&self, ancestor: &str, descendant: &str) -> bool {
        let mut queue: Vec<&str> = self
            .parents_of(descendant)
            .iter()
            .map(String::as_str)
            .collect();
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        while let Some(label) = queue.pop() {
            if !visited.insert(label) {
                continue;
            }
            if label == ancestor {
                return true;
            }
            queue.extend(self.parents_of(label).iter().map(String::as_str));
        }
        false
    }
}

/// One node element type's effective property definition for one
/// property: its own declared type, or the intersection of its parents'
/// effective definitions (CIP-266 diamond rule: parents not defining the
/// property contribute `ANY`).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveProperty {
    /// The property's effective (normalized) type.
    pub canon: CanonType,
    /// Whether the child re-specifies the property itself (as opposed to
    /// inheriting every part of it).
    pub declared: bool,
}

/// The effective definitions of every node element type's every
/// inherited-or-declared property, resolved in one memoized pass. A
/// label that names no node element type in the body resolves to an
/// empty entry (see [`EffectiveProperties::property`]).
pub struct EffectiveProperties {
    resolved: BTreeMap<String, BTreeMap<String, EffectiveProperty>>,
}

impl EffectiveProperties {
    /// Resolves the effective property definitions of every node element
    /// type in the body. Cycles must have been rejected first
    /// ([`Inheritance::find_cycle`]); a cycle here is an internal
    /// invariant violation, raised as an error rather than a silent
    /// fallback.
    pub fn resolve(body: &GraphTypeBody, inheritance: &Inheritance) -> Result<Self, EngineError> {
        if let Some((x, y)) = inheritance.find_cycle() {
            return Err(inheritance_cycle_error(&x, &y));
        }
        let by_label: BTreeMap<&str, &GraphTypeNodeTypeSpec> = body
            .node_types
            .iter()
            .map(|n| (n.identifying_label.as_str(), n))
            .collect();
        let mut resolved: BTreeMap<String, BTreeMap<String, EffectiveProperty>> = BTreeMap::new();
        for label in by_label.keys() {
            effective_properties_of(&by_label, inheritance, label, &mut resolved)?;
        }
        Ok(Self { resolved })
    }

    /// One label's effective property definitions (empty for a label
    /// naming no node element type of the body).
    pub fn of(&self, label: &str) -> &BTreeMap<String, EffectiveProperty> {
        self.resolved_entry(label)
    }

    /// One label's effective definition of one property.
    pub fn property(&self, label: &str, property: &str) -> Option<&EffectiveProperty> {
        self.resolved_entry(label).get(property)
    }

    fn resolved_entry(&self, label: &str) -> &BTreeMap<String, EffectiveProperty> {
        // Every node element type of the body was resolved in
        // `resolve`; a label outside them has an empty map (not an
        // error: callers ask for arbitrary labels' inheritance).
        static EMPTY: std::sync::OnceLock<BTreeMap<String, EffectiveProperty>> =
            std::sync::OnceLock::new();
        self.resolved
            .get(label)
            .unwrap_or_else(|| EMPTY.get_or_init(BTreeMap::new))
    }
}

/// The one memoized resolution walk both effective resolutions share
/// (properties, defaults): per label, the label's own contributions
/// (`own`) folded with every parent's resolved entries through `merge`
/// (an own entry wins over an inherited one, two inherited entries
/// combine per the caller's policy). Cycles were rejected first; the
/// pre-insert empty-map memoization guards a degenerate body anyway.
/// One walk, two policies -- every fix to the traversal lands once.
#[allow(clippy::type_complexity)] // one memo table per resolution kind
fn fold_over_parents<V: Clone>(
    by_label: &BTreeMap<&str, &GraphTypeNodeTypeSpec>,
    inheritance: &Inheritance,
    label: &str,
    resolved: &mut BTreeMap<String, BTreeMap<String, V>>,
    depth: usize,
    own: &dyn Fn(&GraphTypeNodeTypeSpec) -> Result<BTreeMap<String, V>, EngineError>,
    merge: &dyn Fn(&str, &str, &V, &V) -> Result<V, EngineError>,
) -> Result<(), EngineError> {
    if resolved.contains_key(label) {
        return Ok(());
    }
    // Bounded before any deep recursion happens (ADR-0388 review,
    // LABS-386): the cap is checked at the entry of each recursion
    // level, so a chain longer than it fails typed here -- the authored
    // depth-limit error of the guard family, the same shape the
    // parser's MAX_NESTING_DEPTH raises -- instead of growing the stack
    // a frame per link.
    if depth >= MAX_INHERITANCE_DEPTH {
        return Err(inheritance_depth_exceeded());
    }
    let node = by_label
        .get(label)
        .copied()
        .ok_or_else(|| syn(format!("node element type `{label}` does not exist")))?;
    let own_map = own(node)?;
    // No empty seed (round-15 F-15-2): the memo holds only *completed*
    // maps (inserted at the fold's end), so `resolved.contains_key` means
    // completed, not present-and-in-progress -- the guard's contract is
    // provable locally, not by the separate acyclicity pre-check alone.
    let mut effective = own_map.clone();
    for parent in inheritance.parents_of(label) {
        fold_over_parents(
            by_label,
            inheritance,
            parent,
            resolved,
            depth + 1,
            own,
            merge,
        )?;
        let parent_map = &resolved[parent];
        for (property, parent_value) in parent_map {
            // The child's own declaration wins over every inherited
            // contribution; only two inherited contributions merge.
            if own_map.contains_key(property) {
                continue;
            }
            let merged = match effective.get(property) {
                Some(existing) => merge(label, property, existing, parent_value)?,
                None => parent_value.clone(),
            };
            effective.insert(property.clone(), merged);
        }
    }
    resolved.insert(label.to_string(), effective);
    Ok(())
}

/// The recursive half of [`EffectiveProperties::resolve`], memoized
/// through the shared fold: own declared properties, or the pointwise
/// intersection of the parents' effective definitions (CIP-266 diamond
/// rule: parents not defining the property contribute `ANY`).
fn effective_properties_of(
    by_label: &BTreeMap<&str, &GraphTypeNodeTypeSpec>,
    inheritance: &Inheritance,
    label: &str,
    resolved: &mut BTreeMap<String, BTreeMap<String, EffectiveProperty>>,
) -> Result<(), EngineError> {
    fold_over_parents(
        by_label,
        inheritance,
        label,
        resolved,
        0,
        &|node| {
            node.properties
                .iter()
                .map(|p| {
                    Ok((
                        p.name.clone(),
                        EffectiveProperty {
                            canon: type_expr_to_canon(&p.value_type),
                            declared: true,
                        },
                    ))
                })
                .collect()
        },
        &|_, _, existing, parent| {
            // Two inherited contributions intersect pointwise (the
            // child's own declaration never reaches the merge: the
            // fold's own-wins rule handles it).
            Ok(EffectiveProperty {
                canon: canon_intersection(&existing.canon, &parent.canon),
                declared: false,
            })
        },
    )
}

/// The one cycle rejection both resolution drivers share (round 8's
/// F-8-2): the check, message, and error kind live in a single home.
fn inheritance_cycle_error(x: &str, y: &str) -> EngineError {
    syn(format!(
        "the label `{x}` closes an inheritance cycle with `{y}`: node element \
         types cannot have circular inheritance"
    ))
}

/// One resolved default value with the node element types that declare
/// it: one declaring type is the healthy case (an inherited default
/// carries its declaring ancestor); more than one, or two disagreeing
/// values, is the multiple-default conflict (CIP-262 FE8).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveDefault {
    /// The resolved default value.
    pub value: super::DefaultValue,
    /// The labels of the node element types that declare it.
    pub declared_by: BTreeSet<String>,
}

/// The effective default values of every node element type's properties
/// (CIP-262's interaction with CIP-266): a property's default is the
/// type's own declared one, or -- for a property the type declares no
/// default for -- the one inherited from its parents. Two *declaring*
/// node element types in the child's ancestry (identical values
/// included, CIP-262 FE8) or two ancestors' disagreeing values are the
/// multiple-default conflict: the child must declare its own default.
pub fn effective_default_map(
    body: &GraphTypeBody,
    inheritance: &Inheritance,
) -> Result<BTreeMap<String, BTreeMap<String, EffectiveDefault>>, EngineError> {
    // The same cyclic-spec rejection as EffectiveProperties::resolve (the
    // rust review's Low): a poisoned persisted cyclic body fails typed
    // like the rest of the family, instead of silently truncating the
    // inherited defaults at the statement boundary.
    if let Some((x, y)) = inheritance.find_cycle() {
        return Err(inheritance_cycle_error(&x, &y));
    }
    let by_label: BTreeMap<&str, &GraphTypeNodeTypeSpec> = body
        .node_types
        .iter()
        .map(|n| (n.identifying_label.as_str(), n))
        .collect();
    let mut resolved: BTreeMap<String, BTreeMap<String, EffectiveDefault>> = BTreeMap::new();
    for label in by_label.keys() {
        effective_defaults_of(&by_label, inheritance, label, &mut resolved)?;
    }
    Ok(resolved)
}

/// The recursive half of [`effective_default_map`], memoized through
/// the shared fold: a property's default is the type's own declared one,
/// or -- for a property the type declares no default for -- the one
/// inherited from its parents. Two *declaring* node element types in the
/// child's ancestry (identical values included, CIP-262 FE8) or two
/// ancestors' disagreeing values are the multiple-default conflict: the
/// child must declare its own default.
fn effective_defaults_of(
    by_label: &BTreeMap<&str, &GraphTypeNodeTypeSpec>,
    inheritance: &Inheritance,
    label: &str,
    resolved: &mut BTreeMap<String, BTreeMap<String, EffectiveDefault>>,
) -> Result<(), EngineError> {
    fold_over_parents(
        by_label,
        inheritance,
        label,
        resolved,
        0,
        &|node| {
            node.properties
                .iter()
                .filter_map(|p| {
                    p.default_value.as_ref().map(|d| {
                        let value = super::DefaultValue::from_expr(d)?;
                        let mut declared_by = BTreeSet::new();
                        // The fold calls `own` for exactly the node it is
                        // resolving, so the declaring element type is the
                        // node's own identifying label -- the provenance
                        // expressed once here, never re-derived from the
                        // outer label the fold already guarantees equals it.
                        declared_by.insert(node.identifying_label.clone());
                        Ok((p.name.clone(), EffectiveDefault { value, declared_by }))
                    })
                })
                .collect()
        },
        &|label, property, existing, parent| {
            // Two inherited contributions must be the same declaring
            // element type's same value -- the same ancestor reached
            // through two parents -- or they are two declaring element
            // types, the CIP-262 FE8 conflict (identical values
            // included); the child's own declaration never reaches the
            // merge (the fold's own-wins rule handles it).
            if existing.value != parent.value || existing.declared_by != parent.declared_by {
                return Err(multiple_defaults(label, property));
            }
            Ok(existing.clone())
        },
    )
}

/// The inheritance-chain depth-limit error (ADR-0388 review, LABS-386):
/// the authored depth-limit error of the guard family -- the same shape
/// the parser's `MAX_NESTING_DEPTH` raises for nesting.
fn inheritance_depth_exceeded() -> EngineError {
    syn(format!(
        "maximum inheritance depth of {MAX_INHERITANCE_DEPTH} exceeded"
    ))
}

/// The CIP-262 FE8 conflict error: the property inherits default values
/// from more than one implied node element type; the child element type
/// must specify the default value itself.
fn multiple_defaults(label: &str, property: &str) -> EngineError {
    syn(format!(
        "The property `{property}` has multiple default values coming from multiple \
         implied node element types: the node element type `{label}` must specify the \
         default value itself"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_parser::parse;

    fn body_of(query: &str) -> GraphTypeBody {
        let stmt = parse(query).expect("should parse");
        match stmt.clauses.into_iter().next().unwrap() {
            cypherlite_parser::Clause::AlterCurrentGraphType(c) => match c.target {
                cypherlite_parser::AlterCurrentGraphTypeTarget::Body(body) => body,
                cypherlite_parser::AlterCurrentGraphTypeTarget::Named(name) => {
                    panic!("expected an inline body, got named type {name}")
                }
            },
            other => panic!("expected a graph-type clause, got {other:?}"),
        }
    }

    #[test]
    fn inheritance_edges_follow_identifying_implied_labels() {
        // Only `:A` identifies a node element type, so only `:D`'s implied
        // `:A` is an inheritance edge; `:B`/`:C` identify nothing.
        let body = body_of("ALTER CURRENT GRAPH TYPE SET { (:A => :B&:C), (:D => :A) }");
        let inheritance = Inheritance::of(&body);
        assert_eq!(inheritance.parents_of("D"), &["A".to_string()][..]);
        assert!(inheritance.parents_of("A").is_empty());
    }

    #[test]
    fn cycles_are_detected_including_self_reference() {
        let cycle = |query: &str| Inheritance::of(&body_of(query)).find_cycle().is_some();
        assert!(cycle(
            "ALTER CURRENT GRAPH TYPE SET { (:A => :B), (:B => :A) }"
        ));
        assert!(cycle("ALTER CURRENT GRAPH TYPE SET { (:A => :A) }"));
        assert!(cycle(
            "ALTER CURRENT GRAPH TYPE SET { (:A => :B), (:B => :C), (:C => :D),              (:D => :E), (:E => :A) }"
        ));
        assert!(!cycle(
            "ALTER CURRENT GRAPH TYPE SET { (:A => {}), (:B => :A), (:C => :B) }"
        ));
    }

    #[test]
    fn deep_inheritance_chains_fail_typed_not_stack_overflow() {
        // ADR-0388 review (LABS-386): a flat comma-list chain of element
        // types never trips the parser's MAX_NESTING_DEPTH (nesting, not
        // list length), so a ~5000-link chain must fail typed at the
        // fold's own cap -- never overflow the stack a frame per link.
        let mut query = String::from("ALTER CURRENT GRAPH TYPE SET { (:N0 => :N1)");
        for i in 1..5000 {
            query.push_str(&format!(", (:N{i} => :N{})", i + 1));
        }
        query.push_str(" }");
        let body = body_of(&query);
        let inheritance = Inheritance::of(&body);
        let error = match EffectiveProperties::resolve(&body, &inheritance) {
            Ok(_) => panic!("a too-deep chain fails typed, but resolution succeeded"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("maximum inheritance depth of 500 exceeded"),
            "expected the typed depth error, got: {error}"
        );
        let error =
            effective_default_map(&body, &inheritance).expect_err("a too-deep chain fails typed");
        assert!(
            error
                .to_string()
                .contains("maximum inheritance depth of 500 exceeded"),
            "expected the typed depth error, got: {error}"
        );
    }

    #[test]
    fn diamond_properties_intersect() {
        let body = body_of(
            "ALTER CURRENT GRAPH TYPE SET { \
             (:A => {p :: STRING}), (:B => {p :: INTEGER}), (:C => :A&B) }",
        );
        let effective =
            EffectiveProperties::resolve(&body, &Inheritance::of(&body)).expect("resolves");
        let c_p = effective.property("C", "p").expect("inherited");
        assert!(
            matches!(
                c_p.canon.kind,
                cypherlite_parser::type_normalize::CanonKind::Predefined(
                    cypherlite_parser::PredefinedKind::Null
                )
            ),
            "the C diamond intersection was {:?} (rendered {})",
            c_p.canon.kind,
            cypherlite_parser::type_normalize::render_canon(&c_p.canon)
        );
        assert!(!c_p.declared);
    }
}
