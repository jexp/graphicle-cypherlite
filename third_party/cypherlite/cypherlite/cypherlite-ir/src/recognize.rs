//! The two-sided comparison-orientation policy shared by the planner
//! and the executor, plus the filtered property-access recognizers
//! derived from the base shape: `key_of` and `var_key_side`, the
//! side-selection core, and the op-mirror policy stacked on it. The
//! pure `<var>.<key>` shape recognizers (`var_prop`, `var_prop_owned`)
//! live in `ast` beside the `Expr` type they recognize; the dependency
//! points one way, from this module into `ast`.

use crate::ast::{var_prop, BinaryOp};
use crate::Expr;

/// The side-selection core with the from-left mode flag `orient` needs;
/// the public `orient_side` maps it down.
fn side_core<'a, K, R>(
    recognizer: R,
    left: &'a Expr,
    right: &'a Expr,
) -> Option<(K, &'a Expr, bool)>
where
    R: Fn(&'a Expr) -> Option<K>,
{
    if let Some(k) = recognizer(left) {
        return Some((k, right, true));
    }
    recognizer(right).map(|k| (k, left, false))
}

/// The two-sided side-selection core: what the `recognizer` matched, and
/// the other side. Op-free two-sided sites call this directly; the
/// op-carrying policy on top of it is `orient` below.
pub fn orient_side<'a, K, R>(
    recognizer: R,
    left: &'a Expr,
    right: &'a Expr,
) -> Option<(K, &'a Expr)>
where
    R: Fn(&'a Expr) -> Option<K>,
{
    side_core(recognizer, left, right).map(|(k, other, _)| (k, other))
}

/// The var's own property key, two-sided: whichever operand carries
/// `var` contributes its key, and the other operand comes back. The
/// `orient_side` composition over
/// `key_of` — a named fn because a closure cannot tie the borrowed
/// key's lifetime to the core's generic input.
pub fn var_key_side<'a>(var: &str, left: &'a Expr, right: &'a Expr) -> Option<(&'a str, &'a Expr)> {
    orient_side(|e| key_of(e, var), left, right)
}

/// The var's own property key — `var_prop` filtered to `var`.
pub fn key_of<'e>(expr: &'e Expr, var: &str) -> Option<&'e str> {
    var_prop(expr)
        .filter(|(name, _)| *name == var)
        .map(|(_, key)| key)
}

/// `Some(op)` means "the comparison mirrored for swapped operands". `In`
/// is not operand-symmetric (`a IN b` != `b IN a`), so it mirrors to
/// nothing — a recognized operand on the right of an `IN` declines the
/// recognition, like every other unservable shape.
pub(crate) fn flip_cmp(op: BinaryOp) -> Option<BinaryOp> {
    Some(match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Lte => BinaryOp::Gte,
        BinaryOp::Gte => BinaryOp::Lte,
        BinaryOp::Eq | BinaryOp::Neq => op,
        _ => return None,
    })
}

/// The two-sided orientation policy, stated once: the operand `recognizer`
/// matches on the left keeps the op; on the right the op mirrors via
/// [`flip_cmp`] — which also declines the operand-asymmetric ops (`In`
/// among them) — and an operand neither side recognizes declines the whole
/// recognition. The `Expr` in the result is the other side, in oriented
/// (left-hand) position.
pub fn orient<'a, K, R>(
    recognizer: R,
    left: &'a Expr,
    right: &'a Expr,
    op: BinaryOp,
) -> Option<(K, &'a Expr, BinaryOp)>
where
    R: Fn(&'a Expr) -> Option<K>,
{
    let (k, other, from_left) = side_core(recognizer, left, right)?;
    let op = if from_left { op } else { flip_cmp(op)? };
    Some((k, other, op))
}
