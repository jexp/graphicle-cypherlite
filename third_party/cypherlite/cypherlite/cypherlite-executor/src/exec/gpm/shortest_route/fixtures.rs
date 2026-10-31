//! Fixtures the route tests share: `plain` index-keyed graphs over the
//! shared builder, query runners over the counting combinator, and
//! order-insensitive rows.

use cypherlite_core::Value;

use crate::exec::test_support::{build, exec_cypher};
use crate::graph::Graph;
use crate::shortest::test_support::counting_expands;

use super::{take_route_runs, with_forced_gpm};

/// Relationships shortest-path searches followed, and (pattern, input row)
/// evaluations the shortest-path route accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub expands: u64,
    pub route_runs: u64,
}

/// `f`'s result and the [`Counts`] it caused.
pub(super) fn counting<T>(f: impl FnOnce() -> T) -> (T, Counts) {
    let _ = take_route_runs();
    let (out, expands) = counting_expands(f);
    let counts = Counts {
        expands,
        route_runs: take_route_runs(),
    };
    (out, counts)
}

/// [`build`] with `n` unlabelled nodes and `(from, to)` relationships of
/// type `R`.
pub(super) fn plain(n: usize, rels: &[(usize, usize)]) -> Graph {
    let rels: Vec<_> = rels.iter().map(|&(a, b)| (a, b, "R")).collect();
    build(&vec![&[][..]; n], &rels).graph
}

/// 0→1→2→3, the shortcut 0→4→3, and 3→1 closing a cycle.
pub(super) fn shortcut() -> Graph {
    plain(5, &[(0, 1), (1, 2), (2, 3), (0, 4), (4, 3), (3, 1)])
}

/// Rows of `q` and the [`Counts`] it caused.
pub(super) fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, Counts) {
    counting(|| exec_cypher(g, q).rows)
}

/// Rows of `q`, sorted; asserts the route ran and expanded.
pub(super) fn on_route(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let (rows, counts) = run(g, q);
    assert!(counts.route_runs > 0, "expected the BFS route for {q}");
    assert!(counts.expands > 0, "the route expanded nothing for {q}");
    sorted(rows)
}

/// Rows of `q` with every routed pattern forced onto GPM, sorted.
pub(super) fn gpm(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    sorted(with_forced_gpm(|| exec_cypher(g, q).rows))
}

/// `rows` in [`Value::order_by_cmp`] order, for order-insensitive
/// comparison.
pub(super) fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| {
        let mut cells = a.iter().zip(b).map(|(x, y)| x.order_by_cmp(y));
        cells.find(|o| o.is_ne()).unwrap_or(a.len().cmp(&b.len()))
    });
    rows
}
