//! One routed pattern against one input row: the row's endpoint anchoring,
//! resolved once, picks the searches; each (start, end) partition binds
//! its selected trails the way the GPM walker does.

use std::borrow::Cow;

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::{NodePattern, PathPattern};

use crate::ctx::ExecCtx;
use crate::eval::{bound_node, Row};
use crate::exec::candidates::{node_candidates, node_ok};
use crate::exec::gpm::gpm_bind_node;
use crate::exec::pushdown::Residual;
use crate::shortest::layered::Dag;
use crate::shortest::{pair, path_from_trail, Search};

use super::guard::Shape;
use super::Matched;

/// Which endpoints one input row fixes, resolved once per row.
enum Anchor {
    /// Both endpoints bound: the one (s, e) partition.
    Pair(NodeId, NodeId),
    /// Only the start bound: one forward search reaches every end.
    Start(NodeId),
    /// Only the end bound: one backward search reaches every start.
    End(NodeId),
    /// Neither bound: one forward search per start candidate.
    Free,
    /// One unbound variable at both ends (`(a)-[*]-(a)`): the (s, s)
    /// partition of every start candidate.
    Loop,
}

/// The endpoint a fan search starts from.
#[derive(Clone, Copy)]
enum Way {
    /// From the start, along the pattern's direction.
    Forward,
    /// From the end, against the pattern's direction.
    Backward,
}

impl Way {
    /// The (start, end) partition of `far`, a node reached from `origin`.
    fn ends(self, origin: NodeId, far: NodeId) -> (NodeId, NodeId) {
        match self {
            Way::Forward => (origin, far),
            Way::Backward => (far, origin),
        }
    }

    /// Trails read from the origin, turned to run start → end.
    fn orient(self, mut trails: Vec<Vec<RelId>>) -> Vec<Vec<RelId>> {
        match self {
            Way::Forward => {}
            Way::Backward => trails.iter_mut().for_each(|t| t.reverse()),
        }
        trails
    }
}

/// One routed pattern evaluated against one input row.
pub(super) struct Route<'a> {
    ctx: ExecCtx<'a>,
    path: &'a PathPattern,
    row: &'a Row,
    shape: Shape<'a>,
    search: Search<'a, 'a>,
}

impl<'a> Route<'a> {
    pub(super) fn new(
        ctx: ExecCtx<'a>,
        path: &'a PathPattern,
        row: &'a Row,
        shape: Shape<'a>,
    ) -> Self {
        let rel = shape.rel;
        let search = Search {
            graph: ctx.graph,
            types: &rel.types,
            dir: rel.direction,
            min: rel.min_hops,
            max: rel.max_hops.unwrap_or(u32::MAX),
        };
        Self {
            ctx,
            path,
            row,
            shape,
            search,
        }
    }

    /// Every selected row: the row's anchoring, resolved once, picks the
    /// searches. A pattern whose minimum exceeds its maximum matches nothing.
    pub(super) fn run(&self) -> Result<Matched> {
        let mut out = Vec::new();
        if self.search.min > self.search.max {
            return Ok(out);
        }
        match self.anchor() {
            Anchor::Pair(s, e) => self.pair(s, e, &mut out),
            Anchor::Start(s) => self.fan(s, Way::Forward, &mut out),
            Anchor::End(e) => self.fan(e, Way::Backward, &mut out),
            Anchor::Free => self.each_start(|s| self.fan(s, Way::Forward, &mut out)),
            Anchor::Loop => self.each_start(|s| self.pair(s, s, &mut out)),
        }?;
        Ok(out)
    }

    fn anchor(&self) -> Anchor {
        let (start, end) = (self.shape.start, self.shape.end);
        match (self.bound(start), self.bound(end)) {
            (Some(s), Some(e)) => Anchor::Pair(s, e),
            (Some(s), None) => Anchor::Start(s),
            (None, Some(e)) => Anchor::End(e),
            (None, None) if start.variable.is_some() && start.variable == end.variable => {
                Anchor::Loop
            }
            (None, None) => Anchor::Free,
        }
    }

    /// The node `np`'s variable is bound to in the input row. The guard
    /// admits only node bindings and unbound variables.
    fn bound(&self, np: &NodePattern) -> Option<NodeId> {
        bound_node(np, self.row)
    }

    fn node_ok(&self, id: NodeId, np: &NodePattern, row: &Row) -> Result<bool> {
        let ExecCtx {
            graph,
            params,
            cypher_version,
            ..
        } = self.ctx;
        node_ok(graph, id, np, row, params, cypher_version)
    }

    /// Call `f` with every candidate of the unbound start pattern.
    fn each_start(&self, mut f: impl FnMut(NodeId) -> Result<()>) -> Result<()> {
        let ExecCtx {
            graph,
            params,
            cypher_version,
            ..
        } = self.ctx;
        let residual = Residual::empty();
        // LABS-448: the seek phase — the route's start resolution, through
        // the one wrapper every walker shares.
        let starts = crate::exec::phase_seek(|| {
            node_candidates(
                graph,
                self.shape.start,
                self.row,
                params,
                cypher_version,
                &residual,
            )
        })?;
        for s in starts {
            graph.cancel_tick()?;
            f(s)?;
        }
        Ok(())
    }

    /// The input row with the start node bound, or `None` when `id` fails
    /// the start pattern.
    fn bind_start(&self, id: NodeId) -> Result<Option<Row>> {
        if !self.node_ok(id, self.shape.start, self.row)? {
            return Ok(None);
        }
        let mut row = self.row.clone();
        Ok(gpm_bind_node(self.ctx.graph, self.shape.start, id, &mut row).then_some(row))
    }

    /// A layered search from `origin`; an all-path selector keeps every
    /// shortest predecessor.
    fn dag(
        &self,
        search: Search<'_, '_>,
        origin: NodeId,
        stop: impl FnMut(NodeId, u32) -> bool,
    ) -> Result<Dag> {
        search.dag(origin, self.shape.all, None, stop)
    }

    /// Both endpoints fixed: the pair search for single-path selectors, the
    /// layered search stopped at `e` for all-path ones.
    fn pair(&self, s: NodeId, e: NodeId, out: &mut Matched) -> Result<()> {
        let Some(srow) = self.bind_start(s)? else {
            return Ok(());
        };
        self.partition(s, e, &srow, out, || {
            if self.shape.all {
                self.dag(self.search, s, |n, _| n == e)?
                    .trails(self.ctx.graph, e)
            } else {
                Ok(pair(&self.search, s, e)?.into_iter().collect())
            }
        })
    }

    /// One endpoint fixed: one layered search from it, `way` saying which,
    /// reaches every node at the other end. Each reached node, and the
    /// origin itself, is one partition.
    fn fan(&self, origin: NodeId, way: Way, out: &mut Matched) -> Result<()> {
        // Forward, the origin is the start, bound once; backward, each
        // reached node is a start, bound in its own partition.
        let (search, origin_row) = match way {
            Way::Forward => {
                let Some(row) = self.bind_start(origin)? else {
                    return Ok(());
                };
                (self.search, Some(row))
            }
            Way::Backward => (self.search.reversed(), None),
        };
        let dag = self.dag(search, origin, |_, _| false)?;
        for &far in std::iter::once(&origin).chain(dag.order()) {
            self.ctx.graph.cancel_tick()?;
            let (s, e) = way.ends(origin, far);
            let srow = match &origin_row {
                Some(row) => Cow::Borrowed(row),
                None => match self.bind_start(s)? {
                    Some(row) => Cow::Owned(row),
                    None => continue,
                },
            };
            self.partition(s, e, &srow, out, || {
                Ok(way.orient(dag.trails(self.ctx.graph, far)?))
            })?;
        }
        Ok(())
    }

    /// Emit the (s, e) partition of a row with the start bound: nothing
    /// when `e` fails the end pattern, else every trail `trails` returns.
    /// The only home of the s = e partition: the zero-length path when `min`
    /// is 0, otherwise the shortest closed trail(s) through `s`.
    fn partition(
        &self,
        s: NodeId,
        e: NodeId,
        srow: &Row,
        out: &mut Matched,
        trails: impl FnOnce() -> Result<Vec<Vec<RelId>>>,
    ) -> Result<()> {
        if !self.node_ok(e, self.shape.end, srow)? {
            return Ok(());
        }
        let trails = if s != e {
            trails()?
        } else if self.search.min == 0 {
            vec![Vec::new()]
        } else {
            self.search.closed(s, self.shape.all)?
        };
        for trail in &trails {
            self.emit(s, e, trail, srow, out)?;
        }
        Ok(())
    }

    /// Bind one selected trail the way `match_gpm_path` does: start node,
    /// relationship list, end node, then the path.
    fn emit(
        &self,
        s: NodeId,
        e: NodeId,
        trail: &[RelId],
        srow: &Row,
        out: &mut Matched,
    ) -> Result<()> {
        let graph = self.ctx.graph;
        graph.cancel_tick()?;
        let path = path_from_trail(graph, s, trail);
        let mut row = srow.clone();
        if let Some(name) = &self.shape.rel.variable {
            let rels: Vec<Value> = path
                .rels
                .iter()
                .cloned()
                .map(Value::from_relationship)
                .collect();
            row.insert(name.clone(), Value::List(rels.into()));
        }
        if !gpm_bind_node(graph, self.shape.end, e, &mut row) {
            return Ok(());
        }
        if let Some(name) = &self.path.variable {
            row.insert(name.clone(), Value::from_path(path));
        }
        out.push((row, trail.iter().copied().collect()));
        Ok(())
    }
}
