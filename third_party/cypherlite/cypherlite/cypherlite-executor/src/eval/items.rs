//! Element-by-element consumption of an evaluated list (ADR-0170).

use std::sync::Arc;

use cypherlite_core::Value;

/// Consume an evaluated list element by element. A uniquely owned list
/// (freshly built, e.g. `nodes(p)` or a literal) moves its elements out; a
/// shared one (still bound in a row, a parameter, …) is left intact and each
/// element is cloned only when `next()` reaches it, so a consumer that stops
/// early clones just the elements it visited.
pub(crate) fn into_items(xs: Arc<Vec<Value>>) -> impl ExactSizeIterator<Item = Value> {
    match Arc::try_unwrap(xs) {
        Ok(owned) => Items::Owned(owned.into_iter()),
        Err(shared) => Items::Shared { xs: shared, at: 0 },
    }
}

enum Items {
    Owned(std::vec::IntoIter<Value>),
    Shared { xs: Arc<Vec<Value>>, at: usize },
}

impl Iterator for Items {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        match self {
            Items::Owned(items) => items.next(),
            Items::Shared { xs, at } => {
                let item = xs.get(*at)?.clone();
                *at += 1;
                Some(item)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match self {
            Items::Owned(items) => items.len(),
            Items::Shared { xs, at } => xs.len() - *at,
        };
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Items {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Elements whose payload is itself an `Arc`, so a move and a clone are
    /// told apart by the payload's strong count.
    fn nested(n: i64) -> Vec<Value> {
        (0..n)
            .map(|i| Value::List(vec![Value::Int(i)].into()))
            .collect()
    }

    fn payload(v: &Value) -> &Arc<Vec<Value>> {
        let Value::List(inner) = v else {
            panic!("expected a list element, got {v:?}");
        };
        inner
    }

    #[test]
    fn unique_list_moves_its_elements_out() {
        let elements = nested(4);
        let originals: Vec<Arc<Vec<Value>>> =
            elements.iter().map(|v| Arc::clone(payload(v))).collect();
        let mut items = into_items(Arc::new(elements));
        assert_eq!(items.len(), 4);
        for (i, original) in originals.iter().enumerate() {
            let yielded = items.next().expect("element");
            let yielded = payload(&yielded);
            assert!(Arc::ptr_eq(yielded, original));
            // Only `originals` and the yielded value hold the payload: the
            // element left the list instead of being copied out of it.
            assert_eq!(Arc::strong_count(yielded), 2);
            assert_eq!(items.len(), 3 - i);
        }
        assert!(items.next().is_none());
    }

    #[test]
    fn shared_list_clones_only_the_visited_elements() {
        let xs = Arc::new(nested(5));
        let held = Arc::clone(&xs);
        let mut items = into_items(xs);
        assert_eq!(items.len(), 5);
        let taken: Vec<Value> = items.by_ref().take(2).collect();
        assert_eq!(taken, held[..2]);
        assert_eq!(items.len(), 3);
        for (i, element) in held.iter().enumerate() {
            let expected = if i < 2 { 2 } else { 1 };
            assert_eq!(Arc::strong_count(payload(element)), expected, "element {i}");
        }
        drop(items);
        drop(taken);
        assert_eq!(Arc::strong_count(&held), 1);
        assert_eq!(*held, nested(5));
    }

    #[test]
    fn shared_list_yields_every_element_in_order() {
        let xs: Arc<Vec<Value>> = Arc::new((0..3).map(Value::Int).collect());
        let held = Arc::clone(&xs);
        let all: Vec<Value> = into_items(xs).collect();
        assert_eq!(all, *held);
    }
}
