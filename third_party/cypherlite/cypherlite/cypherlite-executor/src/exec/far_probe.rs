//! Far-node index prefilters. Built only when the candidate set is no larger
//! than the relationship frontier already walked.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};

use cypherlite_core::{NodeId, Value};
use cypherlite_ir::{NodePattern, PathPattern};

use crate::eval::Row;
use crate::overlay::ExecGraph;

use super::candidates::{label_index_seekable, leaf_seek, Seek};
use super::create::map_from_expr;
use super::pushdown::Residual;

/// One far node's probe, from the cardinality decision through the built set.
enum Probe {
    /// An index-served seek matched nothing. The path cannot match.
    Empty,
    /// A text or spatial seek, or a dirty write overlay. Build on first use.
    Unpriced,
    /// Counted cost. `seen` is neighbors reached before the set exists.
    Pending { cost: usize, seen: usize },
    /// `None` means the seeks were considered and none is index-served.
    Ready(Option<HashSet<NodeId>>),
}

struct FarNodeSeeks {
    labels: Vec<String>,
    seeks: Vec<Seek>,
    state: RefCell<Probe>,
}

/// Far-node index prefilters, built only when they are no larger than the
/// relationship frontier about to be walked.
pub(crate) struct FarProbe {
    nodes: HashMap<String, FarNodeSeeks>,
    /// A prefilter set exists. Neighbor rejection stays off the probe state
    /// until then: borrowing it on every neighbor regressed Q22 by about 18%
    /// when no set was installed. Written only from [`Self::materialize`].
    filtering: Cell<bool>,
}

#[cfg(test)]
thread_local! {
    static FAR_RANGE_MATERIALIZATIONS: Cell<u64> = const { Cell::new(0) };
    /// The far-node index's rejections: one per candidate the prefilter
    /// refused, bumped by the admission path ([`super::expand`]).
    static FAR_NODE_INDEX_REJECTS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
pub(super) fn count_far_node_reject() {
    FAR_NODE_INDEX_REJECTS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(super) fn take_far_range_materializations() -> u64 {
    FAR_RANGE_MATERIALIZATIONS.with(|count| count.replace(0))
}

#[cfg(test)]
pub(super) fn take_far_node_index_rejects() -> u64 {
    FAR_NODE_INDEX_REJECTS.with(|count| count.replace(0))
}

impl FarProbe {
    pub(crate) fn prepare(
        graph: &ExecGraph<'_>,
        path: &PathPattern,
        row: &Row,
        params: &BTreeMap<String, Value>,
        cypher_version: Option<u32>,
        residual: &Residual<'_>,
    ) -> Self {
        let mut nodes = HashMap::new();
        let overlay_dirty = graph.overlay_changes_node_membership();
        for node in path.nodes.iter().skip(1) {
            let Some(var) = node.variable.as_ref() else {
                continue;
            };
            if row.contains_key(var) {
                continue;
            }
            let seeks = far_node_seeks(graph, node, var, row, params, cypher_version, residual);
            if seeks.is_empty() {
                continue;
            }
            let state = initial_probe(graph, &node.labels, &seeks, overlay_dirty);
            nodes.insert(
                var.clone(),
                FarNodeSeeks {
                    labels: node.labels.clone(),
                    seeks,
                    state: RefCell::new(state),
                },
            );
        }
        Self {
            nodes,
            filtering: Cell::new(false),
        }
    }

    pub(crate) fn covers(&self, var: &str) -> bool {
        self.nodes.contains_key(var)
    }

    /// A mandatory far node is index-proven empty, so the path matches nothing.
    pub(crate) fn blocks_path(&self) -> bool {
        self.nodes
            .values()
            .any(|node| matches!(*node.state.borrow(), Probe::Empty))
    }

    /// `true` when this expansion's decision depends on the relationship frontier.
    pub(crate) fn needs_frontier(&self, var: &str) -> bool {
        let Some(node) = self.nodes.get(var) else {
            return false;
        };
        matches!(*node.state.borrow(), Probe::Pending { .. })
    }

    /// Install this variable's prefilter for the expansion about to start.
    /// `frontier` is [`usize::MAX`] for a variable-length walk, which does not
    /// have a one-hop bound. `false` means the destination cannot match.
    pub(crate) fn admit(&self, graph: &ExecGraph<'_>, var: &str, frontier: usize) -> bool {
        let Some(node) = self.nodes.get(var) else {
            return true;
        };
        // Copy the phase out so materialize can borrow the state mutably.
        let step = match *node.state.borrow() {
            Probe::Ready(_) => return true,
            Probe::Empty => return false,
            Probe::Unpriced => None,
            Probe::Pending { cost, seen } => Some((cost, seen)),
        };
        let Some((cost, seen)) = step else {
            self.materialize(graph, var);
            return true;
        };
        // One hub can be smaller than the estimate while the expansions
        // already walked are not. The set is built once that total catches
        // the estimate, then reused.
        if frontier >= cost || seen.saturating_add(frontier) >= cost {
            self.materialize(graph, var);
        } else {
            *node.state.borrow_mut() = Probe::Pending {
                cost,
                seen: seen.saturating_add(frontier),
            };
        }
        true
    }

    pub(crate) fn rejects(&self, var: &str, id: NodeId) -> bool {
        if !self.filtering.get() {
            return false;
        }
        let Some(node) = self.nodes.get(var) else {
            return false;
        };
        match &*node.state.borrow() {
            Probe::Ready(Some(ids)) => !ids.contains(&id),
            Probe::Ready(None) | Probe::Empty | Probe::Unpriced | Probe::Pending { .. } => false,
        }
    }

    fn materialize(&self, graph: &ExecGraph<'_>, var: &str) {
        let Some(node) = self.nodes.get(var) else {
            return;
        };
        let mut served: Option<HashSet<NodeId>> = None;
        for seek in &node.seeks {
            let Some(ids) = seek.serve(graph, &node.labels) else {
                continue;
            };
            #[cfg(test)]
            if matches!(seek, Seek::Range(_, crate::graph::RangeSeek::Cmp { .. })) {
                FAR_RANGE_MATERIALIZATIONS.with(|count| count.set(count.get() + 1));
            }
            intersect_served(&mut served, ids);
        }
        if served.is_some() {
            self.filtering.set(true);
        }
        *node.state.borrow_mut() = Probe::Ready(served);
    }
}

fn initial_probe(
    graph: &ExecGraph<'_>,
    labels: &[String],
    seeks: &[Seek],
    overlay_dirty: bool,
) -> Probe {
    if overlay_dirty {
        return Probe::Unpriced;
    }
    let mut largest = 0usize;
    for seek in seeks {
        match seek.cardinality(graph, labels) {
            Some(0) => return Probe::Empty,
            Some(n) => largest = largest.max(n),
            None => {}
        }
    }
    // A counted seek is at least one id: zero returned `Empty` above.
    if largest > 0 {
        Probe::Pending {
            cost: largest,
            seen: 0,
        }
    } else {
        Probe::Unpriced
    }
}

fn far_node_seeks(
    graph: &ExecGraph<'_>,
    node: &NodePattern,
    var: &str,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Vec<Seek> {
    let label_seekable = label_index_seekable(node);
    let mut seeks = Vec::new();
    if label_seekable {
        if let Ok(properties) = match &node.properties {
            Some(expr) => map_from_expr(graph, row, expr, params, cypher_version),
            None => Ok(BTreeMap::new()),
        } {
            for (key, value) in properties {
                seeks.push(Seek::Range(key, crate::graph::RangeSeek::Eq(value)));
            }
        }
    }
    for conjunct in residual.conjuncts {
        let Some(seek) = leaf_seek(var, conjunct, graph, row, params, cypher_version) else {
            continue;
        };
        if !seek.servable(label_seekable) {
            continue;
        }
        seeks.push(seek);
    }
    seeks
}

fn intersect_served(served: &mut Option<HashSet<NodeId>>, ids: impl IntoIterator<Item = NodeId>) {
    let ids: HashSet<NodeId> = ids.into_iter().collect();
    *served = Some(match served.take() {
        None => ids,
        Some(previous) => previous.intersection(&ids).copied().collect(),
    });
}
