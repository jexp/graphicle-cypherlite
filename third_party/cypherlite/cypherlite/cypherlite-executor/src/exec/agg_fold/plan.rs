//! Compile half of the fixed-length path fold: the plan types and the
//! helpers that lower a pattern list into a [`PathPlan`]. The runtime fold
//! lives in [`super::path`].

use std::collections::HashSet;

use cypherlite_ir::{
    classify_aggregate, AggregateKind, BinaryOp, Expr, PathPattern, Projection, UnaryOp,
};

use crate::eval::{expr_vars, is_agg_expr, orient};

use super::super::pushdown::flatten_and_owned;
use super::fold_hop_vars;

/// A node in the folded patterns. Shared variables use one slot.
/// Compile-time only: the slot assignments flow into the plan's steps.
struct Occ {
    pattern: usize,
    node: usize,
    slot: usize,
}

/// One hop the fold walks: which relationship of which pattern, and the
/// slots its endpoints bind. The shape's steps are data — the runtime driver
/// threads a slot→id binding map so each step anchors on the slots earlier
/// steps bound.
pub(super) struct HopStep {
    pub(super) pattern: usize,
    pub(super) rel: usize,
    pub(super) start_slot: usize,
    pub(super) end_slot: usize,
}

pub(super) enum KeyPlan {
    None,
    One(usize),
    Mixed,
}

pub(super) enum PathPred {
    Cmp {
        key: String,
        op: BinaryOp,
        other: Expr,
    },
    Null {
        key: String,
        is_null: bool,
    },
}

/// One count encoding per form: `count(*)` and the argument-free
/// `count()` are both `CountStar` (acc.rs's `offer_value` counts a row
/// for both); only a property operand keeps the `Count` variant.
pub(super) enum PathItem {
    Key { slot: usize, prop: String },
    CountStar,
    Count { slot: usize, prop: String },
    CountDistinct { slot: usize, prop: String },
}

/// A slot's pushed-down conjuncts: the compiled predicates beside the
/// original expressions, so an unbound start can seek.
#[derive(Default)]
pub(super) struct SlotConjuncts {
    pub(super) preds: Vec<PathPred>,
    pub(super) exprs: Vec<Expr>,
}

pub(super) struct PathPlan {
    pub(super) nslots: usize,
    pub(super) conjuncts: Vec<SlotConjuncts>,
    pub(super) items: Vec<PathItem>,
    pub(super) key_plan: KeyPlan,
    pub(super) steps: Vec<HopStep>,
}

fn hop_names(patterns: &[PathPattern]) -> HashSet<String> {
    patterns.iter().flat_map(fold_hop_vars).collect()
}

fn mentions_any(expr: &Expr, names: &HashSet<String>) -> bool {
    let mut used = HashSet::new();
    expr_vars(expr, &mut used);
    used.iter().any(|name| names.contains(name))
}

/// The same relationship variable in two positions is an equijoin. The fold
/// walks each hop independently, so that pattern stays on match-then-aggregate.
fn repeats_rel_variable(patterns: &[PathPattern]) -> bool {
    let mut seen = HashSet::new();
    for path in patterns {
        for rel in &path.rels {
            if let Some(name) = &rel.variable {
                if !seen.insert(name.as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

fn pattern_streams(path: &PathPattern, names: &HashSet<String>) -> bool {
    !path.needs_gpm()
        && path.variable.is_none()
        && !path.is_shortest_path_function
        && path.nodes.len() == path.rels.len() + 1
        && path.rels.iter().all(|rel| {
            !rel.var_length
                && rel
                    .type_expr
                    .as_ref()
                    .is_none_or(|expr| expr.is_static_disjunction())
                && rel
                    .properties
                    .as_ref()
                    .is_none_or(|expr| !mentions_any(expr, names))
        })
        && path.nodes.iter().all(|node| {
            node.label_expr
                .as_ref()
                .is_none_or(|expr| expr.is_static_conjunction())
                && node
                    .properties
                    .as_ref()
                    .is_none_or(|expr| !mentions_any(expr, names))
        })
}

fn occurrences(patterns: &[PathPattern]) -> (Vec<Occ>, usize) {
    let mut occ = Vec::new();
    let mut nslots = 0usize;
    let mut first: Vec<(String, usize)> = Vec::new();
    for (pattern_idx, pattern) in patterns.iter().enumerate() {
        for (node_idx, node) in pattern.nodes.iter().enumerate() {
            let slot = if let Some(name) = &node.variable {
                if let Some((_, slot)) = first.iter().find(|(seen, _)| seen == name) {
                    *slot
                } else {
                    let slot = nslots;
                    nslots += 1;
                    first.push((name.clone(), slot));
                    slot
                }
            } else {
                let slot = nslots;
                nslots += 1;
                slot
            };
            occ.push(Occ {
                pattern: pattern_idx,
                node: node_idx,
                slot,
            });
        }
    }
    (occ, nslots)
}

/// The slot a pattern node variable occupies — the one canonical
/// variable-name → slot lookup over the occurrence table, shared by every
/// site that needs it.
fn slot_of(occ: &[Occ], patterns: &[PathPattern], name: &str) -> Option<usize> {
    occ.iter().find_map(|item| {
        patterns[item.pattern].nodes[item.node]
            .variable
            .as_deref()
            .filter(|var| *var == name)
            .map(|_| item.slot)
    })
}

/// The path-fold slot of a `var.prop` access — `var_prop` mapped through
/// the occurrence table.
fn node_prop_slot(expr: &Expr, occ: &[Occ], patterns: &[PathPattern]) -> Option<(usize, String)> {
    let (name, key) = crate::eval::var_prop(expr)?;
    let slot = slot_of(occ, patterns, name)?;
    Some((slot, key.to_string()))
}

fn is_pred_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::Neq
            | BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Lte
            | BinaryOp::Gte
            | BinaryOp::In
            | BinaryOp::StartsWith
            | BinaryOp::EndsWith
            | BinaryOp::Contains
            | BinaryOp::Regex
    )
}

fn path_conjunct(
    expr: &Expr,
    names: &HashSet<String>,
    occ: &[Occ],
    patterns: &[PathPattern],
) -> Option<(usize, PathPred)> {
    match expr {
        Expr::Unary { op, expr } => {
            let is_null = match op {
                UnaryOp::IsNull => true,
                UnaryOp::IsNotNull => false,
                _ => return None,
            };
            let (slot, key) = node_prop_slot(expr, occ, patterns)?;
            Some((slot, PathPred::Null { key, is_null }))
        }
        Expr::Binary { op, left, right } if is_pred_op(*op) => {
            // orient owns the mirror policy: the property on the left
            // keeps the op, a property on the right of anything
            // operand-asymmetric (In, the string predicates) declines the
            // fold here.
            let ((slot, key), other, op) =
                orient(|e| node_prop_slot(e, occ, patterns), left, right, *op)?;
            if mentions_any(other, names) {
                return None;
            }
            Some((
                slot,
                PathPred::Cmp {
                    key,
                    op,
                    other: other.clone(),
                },
            ))
        }
        _ => None,
    }
}

fn path_item(expr: &Expr, occ: &[Occ], patterns: &[PathPattern]) -> Option<PathItem> {
    match expr {
        Expr::CountStar => Some(PathItem::CountStar),
        // The typed aggregate contract (ADR-0217): classify the name once
        // and match the kind — other aggregates fall through to the
        // aggregate-decline arm, non-aggregates to the key arm.
        Expr::Function {
            name,
            distinct,
            args,
        } if classify_aggregate(name) == Some(AggregateKind::Count) => {
            if args.is_empty() {
                if *distinct {
                    None
                } else {
                    Some(PathItem::CountStar)
                }
            } else if args.len() == 1 {
                let (slot, prop) = node_prop_slot(&args[0], occ, patterns)?;
                if *distinct {
                    Some(PathItem::CountDistinct { slot, prop })
                } else {
                    Some(PathItem::Count { slot, prop })
                }
            } else {
                None
            }
        }
        other if !is_agg_expr(other) => {
            let (slot, prop) = node_prop_slot(other, occ, patterns)?;
            Some(PathItem::Key { slot, prop })
        }
        _ => None,
    }
}

/// The slot of one pattern node's occurrence — every node gets one, so
/// every step endpoint resolves directly.
fn occ_slot(occ: &[Occ], pattern: usize, node: usize) -> usize {
    occ.iter()
        .find(|item| item.pattern == pattern && item.node == node)
        .expect("occurrence built for every node")
        .slot
}

/// One step of one pattern's hop: rel `rel` connects nodes `rel` and
/// `rel + 1` (pattern_streams guarantees nodes == rels + 1), so the
/// endpoint slots come straight from the occurrence table.
fn hop_step(occ: &[Occ], pattern: usize, rel: usize) -> HopStep {
    HopStep {
        pattern,
        rel,
        start_slot: occ_slot(occ, pattern, rel),
        end_slot: occ_slot(occ, pattern, rel + 1),
    }
}

pub(super) fn compile_path(
    patterns: &[PathPattern],
    where_clause: Option<&Expr>,
    items: &[Projection],
) -> Option<PathPlan> {
    if repeats_rel_variable(patterns) {
        return None;
    }
    let names = hop_names(patterns);
    let (occ, nslots) = occurrences(patterns);
    // Shape gates: every pattern streams; a single pattern takes one or
    // two hops; the comma shape takes two one-hop patterns with exactly
    // one shared node variable (the second pattern anchors on the slot it
    // bound). Wider or unshared shapes decline the fold.
    let shape_ok = patterns.iter().all(|path| pattern_streams(path, &names))
        && match patterns {
            [path] => matches!(path.rels.len(), 1 | 2),
            [first, second] => {
                first.rels.len() == 1
                    && second.rels.len() == 1
                    && shared_node(first, second).is_some()
            }
            _ => false,
        };
    if !shape_ok {
        return None;
    }
    // Every accepted shape compiles the same way: one step per
    // relationship of each pattern, in pattern order.
    let occ = &occ;
    let steps = patterns
        .iter()
        .enumerate()
        .flat_map(|(p, path)| (0..path.rels.len()).map(move |r| hop_step(occ, p, r)))
        .collect();
    let mut conjuncts: Vec<SlotConjuncts> = (0..nslots).map(|_| SlotConjuncts::default()).collect();
    if let Some(where_clause) = where_clause {
        for conjunct in flatten_and_owned(where_clause) {
            let (slot, pred) = path_conjunct(&conjunct, &names, occ, patterns)?;
            let entry = &mut conjuncts[slot];
            entry.preds.push(pred);
            entry.exprs.push(conjunct);
        }
    }
    let mut compiled = Vec::with_capacity(items.len());
    for item in items {
        compiled.push(path_item(&item.expr, occ, patterns)?);
    }
    let key_plan = key_plan_of(&compiled);
    Some(PathPlan {
        nslots,
        conjuncts,
        items: compiled,
        key_plan,
        steps,
    })
}

/// The key-slot classification of the compiled items: no key items, one
/// key item (or several all reading the same slot — one key column), or
/// items reading different slots.
fn key_plan_of(items: &[PathItem]) -> KeyPlan {
    let slots: Vec<usize> = items
        .iter()
        .filter_map(|item| match item {
            PathItem::Key { slot, .. } => Some(*slot),
            _ => None,
        })
        .collect();
    match slots.as_slice() {
        [] => KeyPlan::None,

        slots if slots.iter().all(|slot| *slot == slots[0]) => KeyPlan::One(slots[0]),
        _ => KeyPlan::Mixed,
    }
}

fn shared_node<'a>(first: &'a PathPattern, second: &'a PathPattern) -> Option<&'a str> {
    let mut shared = None;
    for node in &first.nodes {
        let Some(name) = node.variable.as_deref() else {
            continue;
        };
        if second
            .nodes
            .iter()
            .any(|other| other.variable.as_deref() == Some(name))
        {
            if shared.is_some() {
                return None;
            }
            shared = Some(name);
        }
    }
    shared
}
