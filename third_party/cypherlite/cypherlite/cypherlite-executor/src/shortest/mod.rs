//! Shortest-path graph search shared by the expression form
//! (`eval/shortest.rs`) and the `MATCH`-clause route (ADR-0186, ADR-0189).
//!
//! - [`pair`]: one shortest trail between two distinct nodes, by the
//!   ADR-0037 shallow intersection or the ADR-0034 bidirectional BFS.
//! - [`Search::dag`]: a layered single-source BFS that keeps shortest-path
//!   predecessors, with [`layered::Dag::trails`] enumerating them, and
//!   [`Search::closed`] for the shortest closed trails through one node.
//!
//! Searches take the graph and the pattern's direction, types and hop
//! bounds, and return relationship-id trails; callers build paths with
//! [`path_from_trail`]. Every followed relationship is one cancellation
//! tick (ADR-0111) and one counted expansion.

use std::collections::HashSet;

use cypherlite_core::{Error, NodeId, Path, RelId, Result};
use cypherlite_ir::{Direction, NodePattern, RelPattern};

use crate::overlay::ExecGraph;

pub(crate) mod layered;
mod pair;

pub(crate) use pair::pair;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static EXPANDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one relationship a shortest-path search followed. The counter
/// exists only in tests, for the bounded-expansion regression tests.
#[inline(always)]
fn note_expand() {
    #[cfg(test)]
    EXPANDS.with(|c| c.set(c.get() + 1));
}

/// Take and reset this thread's count of followed relationships.
#[cfg(test)]
pub(crate) fn take_shortest_expands() -> u64 {
    EXPANDS.with(|c| c.replace(0))
}

/// What one pattern's searches follow. Every search returns trails of at
/// least one relationship and assumes `min` ≤ 1 (ADR-0189 sends larger
/// minimums to enumeration): callers answer `min` = 0 with the zero-length
/// path themselves, and `min` only picks the pair search's strategy.
#[derive(Clone, Copy)]
pub(crate) struct Search<'a, 'g> {
    pub graph: &'a ExecGraph<'g>,
    /// Relationship types to follow; empty follows every type.
    pub types: &'a [String],
    pub dir: Direction,
    pub min: u32,
    /// Longest trail in relationships; `u32::MAX` for no cap.
    pub max: u32,
}

impl Search<'_, '_> {
    /// The same search along the reversed direction.
    pub(crate) fn reversed(self) -> Self {
        Self {
            dir: self.dir.flip(),
            ..self
        }
    }

    /// Call `f` with each relationship followed out of `node` and the node
    /// it reaches.
    fn each_hop(&self, node: NodeId, mut f: impl FnMut(NodeId, RelId) -> Result<()>) -> Result<()> {
        let graph = self.graph;
        graph.try_for_each_neighbor::<Error>(node, self.dir, self.types, |rid| {
            graph.cancel_tick()?;
            note_expand();
            match graph.hop(node, rid, self.dir) {
                Some(next) => f(next, rid),
                None => Ok(()),
            }
        })
    }

    /// Every relationship followed out of `node`, with the node it reaches.
    fn hops(&self, node: NodeId) -> Result<Vec<(NodeId, RelId)>> {
        let mut out = Vec::new();
        self.each_hop(node, |next, rid| {
            out.push((next, rid));
            Ok(())
        })?;
        Ok(out)
    }
}

/// Whether the searches follow `start-rel-end` exactly: relationship types
/// absent or a static disjunction that `rel.types` spells out, no
/// relationship property map (the searches check types only), and endpoint
/// labels absent or a static conjunction.
pub(crate) fn followable(start: &NodePattern, rel: &RelPattern, end: &NodePattern) -> bool {
    let static_labels = |np: &NodePattern| {
        np.label_expr
            .as_ref()
            .is_none_or(|e| e.is_static_conjunction())
    };
    rel.type_expr
        .as_ref()
        .is_none_or(|e| e.is_static_disjunction() && !rel.types.is_empty())
        && rel.properties.is_none()
        && static_labels(start)
        && static_labels(end)
}

/// Whether no relationship occurs twice in `trail`.
fn unique_rels(trail: &[RelId]) -> bool {
    let mut seen = HashSet::new();
    trail.iter().all(|r| seen.insert(*r))
}

/// The path that starts at `start` and follows `trail`.
///
/// Precondition: `trail` comes from a search over this same graph view, so
/// every relationship it names and every node it touches is present and
/// the trail is contiguous. A miss breaks that same-view invariant and
/// panics rather than silently dropping a trail element.
pub(crate) fn path_from_trail(graph: &ExecGraph<'_>, start: NodeId, trail: &[RelId]) -> Path {
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    nodes.push(
        graph
            .node(start)
            .expect("path_from_trail: start node of the search's own view"),
    );
    let mut cur = start;
    for rid in trail {
        let rel = graph
            .rel(*rid)
            .expect("path_from_trail: trail relationship of the search's own view");
        debug_assert!(
            rel.start == cur || rel.end == cur,
            "path_from_trail: trail is contiguous"
        );
        let next = if rel.start == cur { rel.end } else { rel.start };
        rels.push(rel);
        nodes.push(
            graph
                .node(next)
                .expect("path_from_trail: trail node of the search's own view"),
        );
        cur = next;
    }
    Path { nodes, rels }
}
