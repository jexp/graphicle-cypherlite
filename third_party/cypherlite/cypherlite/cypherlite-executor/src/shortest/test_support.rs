//! Shortest-path test support: the expansion-counting combinator over
//! [`take_shortest_expands`], the ternary-tree builder helpers the
//! counterexample graphs share, and the ADR-0189 `min_hops`
//! counterexample graph.

use cypherlite_core::NodeId;

use crate::exec::test_support::Builder;
use crate::graph::Graph;

use super::take_shortest_expands;

/// `f`'s result and the relationships shortest-path searches followed
/// while it ran.
pub(crate) fn counting_expands<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let _ = take_shortest_expands();
    let out = f();
    (out, take_shortest_expands())
}

/// Ternary trees hung under a root: three unlabelled children per node,
/// each joined to its parent by one `E` relationship.
pub(crate) trait TernaryTree {
    /// Hangs a ternary tree `levels` deep under `root`, every relationship
    /// pointing toward `root`.
    fn fan_in_tree(&self, root: NodeId, levels: usize);

    /// [`Self::fan_in_tree`] with every relationship pointing away from
    /// `root`.
    fn fan_out_tree(&self, root: NodeId, levels: usize);
}

impl TernaryTree for Builder {
    fn fan_in_tree(&self, root: NodeId, levels: usize) {
        // Toward `root`: the child points at its parent.
        ternary_tree(self, root, levels, |b, parent, child| b.edge(child, parent));
    }

    fn fan_out_tree(&self, root: NodeId, levels: usize) {
        // Away from `root`: the parent points at its child.
        ternary_tree(self, root, levels, |b, parent, child| b.edge(parent, child));
    }
}

/// The tree of [`TernaryTree::fan_in_tree`] / [`TernaryTree::fan_out_tree`],
/// built level by level, each parent-child pair joined by one `E`
/// relationship the passed edge builder directs.
fn ternary_tree(b: &Builder, root: NodeId, levels: usize, edge: impl Fn(&Builder, NodeId, NodeId)) {
    let mut level = vec![root];
    for _ in 0..levels {
        let mut next = Vec::new();
        for &parent in &level {
            for _ in 0..3 {
                let child = b.anon(&[]);
                edge(b, parent, child);
                next.push(child);
            }
        }
        level = next;
    }
}

/// The ADR-0189 `min_hops` > 1 counterexample: 50 dead ends off `s`,
/// `s→p→x→t` (3 hops, below the minimum) plus `x→y→t`, so `s→p→x→y→t` is
/// the only 4-hop trail; the 4-node `q` chain from `s` to `t` (5 hops);
/// and a fan-in tree of depth 4 under `t`.
pub(crate) fn min_hops_counterexample() -> Graph {
    let b = Builder::new();
    let (s, t) = (b.node("s", &["N"]), b.node("t", &["N"]));
    for _ in 0..50 {
        b.edge(s, b.node("", &[]));
    }
    let (p, x, y) = (b.node("p", &[]), b.node("x", &[]), b.node("y", &[]));
    b.chain(&[s, p, x, t]);
    b.chain(&[x, y, t]);
    let q: Vec<_> = (0..4).map(|_| b.node("q", &[])).collect();
    b.chain(&[&[s], &q[..], &[t]].concat());
    b.fan_in_tree(t, 4);
    b.done()
}
