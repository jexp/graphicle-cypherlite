//! `shortestPath` / `allShortestPaths` in expression form: one value per
//! input row. A single-relationship pattern the BFS can follow with
//! `min_hops` ≤ 1 runs on the shared searches of `crate::shortest`
//! (ADR-0189); every other pattern enumerates and keeps the shortest
//! (ADR-0025).

use std::collections::BTreeMap;

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::{NodePattern, PathPattern};

use crate::overlay::ExecGraph;
use crate::shortest::{followable, pair, path_from_trail, Search};

use super::pattern::{bound_node, collect_pattern_rows, pattern_node_candidates, pattern_node_ok};
use super::Row;

#[derive(Clone, Copy)]
pub(crate) enum ShortestWant {
    Path,
    Length,
}

pub(crate) fn shortest_value(
    graph: &ExecGraph<'_>,
    start: NodeId,
    trail: &[RelId],
    want: ShortestWant,
) -> Value {
    match want {
        ShortestWant::Length => Value::Int(trail.len() as i64),
        ShortestWant::Path => Value::from_path(path_from_trail(graph, start, trail)),
    }
}

pub(crate) fn eval_shortest(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    all: bool,
    path: &PathPattern,
    want: ShortestWant,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let ([rp], [start_np, end_np]) = (path.rels.as_slice(), path.nodes.as_slice()) else {
        return enumerate(graph, row, params, all, path, want, cypher_version);
    };
    let (min, max) = crate::ctx::hop_bounds(rp);
    if min > 1 || !followable(start_np, rp, end_np) {
        return enumerate(graph, row, params, all, path, want, cypher_version);
    }
    let expr = ShortestExpr {
        search: Search {
            graph,
            types: &rp.types,
            dir: rp.direction,
            min,
            max,
        },
        end: end_np,
        end_bound: bound_node(end_np, row),
        row,
        params,
        cypher_version,
        all,
    };
    // Start candidates combine as before ADR-0189. shortestPath returns the
    // trail of the first candidate, in candidate order, that has one, not
    // the shortest over all candidates. For allShortestPaths the first
    // candidate with a trail sets the length; later candidates add only
    // their shortest trails of exactly that length, so a shorter trail is
    // dropped, and so is a trail of that length to an end they reach
    // sooner (ADR-0189 Consequences). A candidate matching the end with
    // min_hops 0 adds the zero-length path and sets the length to 0.
    // Enumeration (min_hops > 1) instead takes the shortest over all
    // candidates.
    let mut found: Vec<Value> = Vec::new();
    let mut best: Option<usize> = None;
    // LABS-448: the seek phase — the shortest-path's start resolution,
    // through the one wrapper every walker resolves its candidates
    // through, so the kind↔phase pairing holds on MATCH SHORTEST too.
    let candidates = crate::exec::phase_seek(|| {
        pattern_node_candidates(graph, start_np, row, params, cypher_version)
    })?;
    for sid in candidates {
        graph.cancel_tick()?;
        if !pattern_node_ok(graph, sid, start_np, row, params, cypher_version) {
            continue;
        }
        let trails = if min == 0 && expr.end_ok(sid) {
            vec![Vec::new()]
        } else {
            expr.trails_from(sid, best)?
        };
        if let Some(t) = trails.first() {
            best = Some(t.len());
        }
        found.extend(trails.iter().map(|t| shortest_value(graph, sid, t, want)));
        if !all && !found.is_empty() {
            return Ok(found.swap_remove(0));
        }
    }
    Ok(if found.is_empty() {
        Value::Null
    } else {
        Value::List(found.into())
    })
}

fn enumerate(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    all: bool,
    path: &PathPattern,
    want: ShortestWant,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let v = eval_shortest_enumerate(graph, row, params, all, path, cypher_version)?;
    Ok(shortest_from_enum(v, want))
}

/// One expression's searches from each start candidate.
struct ShortestExpr<'a, 'g> {
    search: Search<'a, 'g>,
    end: &'a NodePattern,
    end_bound: Option<NodeId>,
    row: &'a Row,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    all: bool,
}

impl ShortestExpr<'_, '_> {
    /// Whether `id` is the bound end node, if any, and matches the end
    /// pattern.
    fn end_ok(&self, id: NodeId) -> bool {
        self.end_bound.is_none_or(|e| e == id)
            && pattern_node_ok(
                self.search.graph,
                id,
                self.end,
                self.row,
                self.params,
                self.cypher_version,
            )
    }

    /// The shortest trails of at least one relationship from `sid`, all of
    /// one length: one trail for shortestPath, every one for
    /// allShortestPaths. With `cap`, a previous candidate's length, only
    /// trails of exactly that length.
    fn trails_from(&self, sid: NodeId, cap: Option<usize>) -> Result<Vec<Vec<RelId>>> {
        let s = Search {
            max: cap.map_or(self.search.max, |c| c as u32),
            ..self.search
        };
        let mut trails = match self.end_bound {
            Some(e) if !self.end_ok(e) => Vec::new(),
            Some(e) if e == sid => s.closed(sid, self.all)?,
            Some(e) if self.all => s.dag(sid, true, None, |n, _| n == e)?.trails(s.graph, e)?,
            Some(e) => pair(&s, sid, e)?.into_iter().collect(),
            None => self.unbound(&s, sid, cap)?,
        };
        if let Some(c) = cap {
            trails.retain(|t| t.len() == c);
        }
        Ok(trails)
    }

    /// Unbound end: the trails to the matching nodes of the first layer
    /// that holds one (with `cap`, of exactly that layer). When `sid`
    /// matches the end pattern too, its closed trails up to that length
    /// join them: the shorter length wins and all-path ties merge.
    fn unbound(
        &self,
        s: &Search<'_, '_>,
        sid: NodeId,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<RelId>>> {
        let mut ends = Vec::new();
        let dag = s.dag(sid, self.all, None, |n, depth| {
            let end = depth > 0 && cap.is_none_or(|c| depth as usize == c) && self.end_ok(n);
            if end {
                ends.push(n);
            }
            end && cap.is_none()
        })?;
        let mut trails = Vec::new();
        for t in ends {
            trails.extend(dag.trails(s.graph, t)?);
            if !self.all {
                break;
            }
        }
        if self.end_ok(sid) {
            let max = match trails.first() {
                Some(t) if self.all => t.len() as u32,
                Some(t) => t.len() as u32 - 1,
                None => s.max,
            };
            let closed = Search { max, ..*s }.closed(sid, self.all)?;
            if closed
                .first()
                .is_some_and(|c| trails.first().is_none_or(|t| c.len() < t.len()))
            {
                trails.clear();
            }
            trails.extend(closed);
        }
        Ok(trails)
    }
}

pub(crate) fn shortest_from_enum(v: Value, want: ShortestWant) -> Value {
    match want {
        ShortestWant::Path => v,
        ShortestWant::Length => match v {
            Value::Path(p) => Value::Int(p.rels.len() as i64),
            Value::List(xs) => Value::Int(xs.len() as i64),
            other => other,
        },
    }
}

pub(crate) fn eval_shortest_enumerate(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    all: bool,
    path: &PathPattern,
    cypher_version: Option<u32>,
) -> Result<Value> {
    // The walk binds only a named path; name an anonymous one (the usual
    // expression form) like the walk's other internal bindings.
    let named;
    let (name, path) = match &path.variable {
        Some(n) => (n.as_str(), path),
        None => {
            let anon = "__shortest_path";
            named = PathPattern {
                variable: Some(anon.into()),
                ..path.clone()
            };
            (anon, &named)
        }
    };
    let rows = collect_pattern_rows(graph, row, path, params, cypher_version)?;
    let mut scored: Vec<(usize, Value)> = Vec::new();
    for mut r in rows {
        if let Some(Value::Path(p)) = r.swap_remove(name) {
            scored.push((p.rels.len(), Value::Path(p)));
        }
    }
    if scored.is_empty() {
        return Ok(Value::Null);
    }
    let min = scored.iter().map(|(l, _)| *l).min().unwrap();
    scored.retain(|(l, _)| *l == min);
    if all {
        Ok(Value::List(
            scored
                .into_iter()
                .map(|(_, p)| p)
                .collect::<Vec<_>>()
                .into(),
        ))
    } else {
        Ok(scored.remove(0).1)
    }
}
