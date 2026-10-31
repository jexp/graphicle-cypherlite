//! The shared three-valued quantifier fold and the membership predicate —
//! the policies the list-quantifier expressions (`quantifier.rs`), the
//! `coll.*` functions (`collection.rs`), and the `IN` operator (`expr.rs`)
//! all reduce onto. Sibling feature modules import these downward from the
//! eval layer, never sideways from each other.

use cypherlite_core::{Result, Value};
use cypherlite_ir::QuantifierKind;

/// The one three-valued quantifier fold — the `(kind, truthiness)` state
/// machine the list quantifiers and the `coll.has*Of` functions (CIP-286)
/// share: an early exit per kind (All on the first false, Any on the first
/// true, None on the first true, Single on the second true), a `None`
/// truthiness records the unknown, and the final resolution is
/// three-valued. `pred` receives each item — borrowed or owned, whichever
/// the caller's iteration yields — and yields its truthiness.
pub(crate) fn fold_quantifier<V>(
    kind: QuantifierKind,
    items: impl IntoIterator<Item = V>,
    mut pred: impl FnMut(V) -> Result<Option<bool>>,
) -> Result<Value> {
    let mut saw_null = false;
    let mut true_count = 0usize;
    for item in items.into_iter() {
        let truth = pred(item)?;
        match (kind, truth) {
            (QuantifierKind::All, Some(false)) => return Ok(Value::Bool(false)),
            (QuantifierKind::Any, Some(true)) => return Ok(Value::Bool(true)),
            (QuantifierKind::None, Some(true)) => return Ok(Value::Bool(false)),
            (QuantifierKind::Single, Some(true)) => {
                true_count += 1;
                if true_count >= 2 {
                    return Ok(Value::Bool(false));
                }
            }
            (_, None) => saw_null = true,
            _ => {}
        }
    }
    // A scan that ends undecided is null if any truthiness was unknown,
    // otherwise the kind's default; Single's `>= 2` early exit guarantees
    // true_count <= 1 here.
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Bool(match kind {
            QuantifierKind::All | QuantifierKind::None => true,
            QuantifierKind::Any => false,
            QuantifierKind::Single => true_count == 1,
        })
    })
}

/// Membership of `x` in `xs` under Cypher's equals semantics, three-valued:
/// `true` on the first equal element, `null` when no element is equal but one
/// is null (an unknown value), `false` otherwise — and `false` for a null `x`
/// in an empty list, `null` in a non-empty one: a null `x` decides through
/// the same equals relation (every comparison is unknown), never a special
/// case. The one membership policy of the `IN` operator and the
/// `coll.has*Of` functions (CIP-286), which are quantifiers over exactly
/// this predicate.
pub(crate) fn membership(x: &Value, xs: &[Value]) -> Value {
    // The Any column of the one shared fold: the first true decides, an
    // unknown comparison stays unknown, no member is false. The predicate
    // is infallible, so the fold's error channel cannot fire.
    fold_quantifier(QuantifierKind::Any, xs.iter(), |item| {
        Ok(x.cypher_eq(item).truthy())
    })
    .expect("the membership predicate is infallible")
}
