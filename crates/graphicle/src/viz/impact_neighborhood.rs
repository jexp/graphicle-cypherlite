//! The impact neighborhood (the plan's §5 D4, `impact-neighborhood`): the
//! bounded calls and imports neighborhood of a focus symbol or file, walked
//! over the code graph's live edges up to the parameters' `depth` hops.
//!
//! The walk is a depth-capped loop, one statement per direction per hop: the
//! impact machinery `get_pr_impact` runs (`graphicle-graph`'s `prs::ImpactIndex`)
//! maps changed files to communities on the engine's in-memory graph and has
//! no hop walk, and the store holds no bounded transitive walk to reuse, so
//! none of it fits a focus neighborhood. The bound is the impact machinery's
//! ([`MAX_DEPTH`](super::MAX_DEPTH)), enforced by the parameter contract.
//!
//! Direction is not a parameter of the contract, so v1 serves both: every
//! edge is labelled `out` (the walk reached its callee) or `in` (it reached
//! its caller), and keeps the stored orientation as `from` -> `to`.
//!
//! # Evidence
//!
//! An edge's `evidence` is its call site, `relpath:line`, from the edge's
//! `relpath` and `source_location` (the projection's `L<n>`). A node's
//! `evidence` is its file: the code graph's symbol nodes carry no line of
//! their own, so a node is cited by its file.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{NEEDS_FOCUS, Params, View, mermaid_label, run, text};
use crate::git_history::queries::Section;
use crate::store_graph::GraphStore;
use crate::supply_chain::resolve::line_of;
use crate::{Error, Result};

/// The most nodes one walk holds. Past it the walk stops adding nodes, in
/// the sorted order the hop's rows are taken in, and says so (`truncated`).
pub const MAX_NODES: usize = 500;

/// The focus's seed symbols: a stable id, a qualified name or a file path
/// (a file seeds every symbol it holds).
const SEEDS: &str = "MATCH (s:Symbol {repository_id: $repo}) \
     WHERE s.stable_id = $focus OR s.qualified_name = $focus OR s.relpath = $focus \
     RETURN s.stable_id AS id, s.qualified_name AS name, s.relpath AS path";

/// The live calls and imports edges leaving the anchors of one hop. The
/// projection writes the extractors' relation strings upper-cased, and a
/// Rust `use` as `IMPORTS_FROM` (the history pack's relation list).
const OUT: &str = "MATCH (a:Symbol {repository_id: $repo})-[r]->(b:Symbol {repository_id: $repo}) \
     WHERE a.stable_id IN $ids AND type(r) IN ['CALLS', 'IMPORTS', 'IMPORTS_FROM'] \
     AND r.valid_from_episode IS NOT NULL AND r.valid_to_episode IS NULL \
     RETURN a.stable_id AS anchor, a.relpath AS anchor_path, b.stable_id AS neighbor, \
     b.qualified_name AS name, b.relpath AS path, type(r) AS relation, \
     r.relpath AS edge_path, r.source_location AS location";

/// The same edges arriving at the anchors: the neighbor is the caller.
const IN: &str = "MATCH (a:Symbol {repository_id: $repo})<-[r]-(b:Symbol {repository_id: $repo}) \
     WHERE a.stable_id IN $ids AND type(r) IN ['CALLS', 'IMPORTS', 'IMPORTS_FROM'] \
     AND r.valid_from_episode IS NOT NULL AND r.valid_to_episode IS NULL \
     RETURN a.stable_id AS anchor, a.relpath AS anchor_path, b.stable_id AS neighbor, \
     b.qualified_name AS name, b.relpath AS path, type(r) AS relation, \
     r.relpath AS edge_path, r.source_location AS location";

/// What a walk reached: its nodes by id, the edges it crossed keyed by their
/// endpoints, relation and call site, and whether [`MAX_NODES`] cut it short.
struct Walk {
    nodes: BTreeMap<String, Node>,
    edges: BTreeMap<(String, String, String, String), Edge>,
    truncated: bool,
}

/// A node the walk reached: its name, its file, the hop it was first reached.
struct Node {
    name: String,
    path: String,
    depth: usize,
}

/// A live edge the walk crossed, in its stored orientation.
struct Edge {
    from: String,
    to: String,
    relation: String,
    /// `out` when the walk followed the edge to its callee, `in` to its caller.
    direction: &'static str,
    /// The call site, `relpath:line` (the file alone when no line is stored).
    evidence: String,
    /// The hop the edge was first crossed.
    depth: usize,
}

/// One row of a hop's statements, parsed.
struct Hit {
    anchor: String,
    neighbor: String,
    name: String,
    path: String,
    relation: String,
    edge_path: String,
    line: Option<i64>,
    anchor_path: String,
    out: bool,
}

/// The family's view of `(repository, params)`: the neighborhood of the
/// focus within `params.depth` hops, nodes and edges cut to `params.cap`.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let focus = params
        .focus
        .as_deref()
        .filter(|focus| !focus.is_empty())
        .ok_or_else(|| Error::bad(NEEDS_FOCUS))?;
    let seeds = run(graph, SEEDS, &json!({"repo": repository, "focus": focus}))?;
    let found = !seeds.is_empty();

    let Walk {
        nodes,
        edges,
        truncated: walk_truncated,
    } = walk(graph, repository, &seeds, params.depth)?;

    // Nodes first, the focus and nearer hops ahead of farther ones, each by
    // its file and id; then the edges whose both ends are kept.
    let mut kept: Vec<(&String, &Node)> = nodes.iter().collect();
    kept.sort_by(|a, b| (a.1.depth, &a.1.path, a.0).cmp(&(b.1.depth, &b.1.path, b.0)));
    let node_total = kept.len();
    kept.truncate(params.cap);
    let kept_ids: BTreeSet<&str> = kept.iter().map(|(id, _)| id.as_str()).collect();

    let mut kept_edges: Vec<&Edge> = edges
        .values()
        .filter(|edge| kept_ids.contains(edge.from.as_str()) && kept_ids.contains(edge.to.as_str()))
        .collect();
    kept_edges.sort_by(|a, b| {
        (a.depth, &a.evidence, &a.from, &a.to, &a.relation).cmp(&(
            b.depth,
            &b.evidence,
            &b.from,
            &b.to,
            &b.relation,
        ))
    });
    let edge_total = kept_edges.len();
    kept_edges.truncate(params.cap);

    let node_rows: Vec<Value> = kept
        .iter()
        .map(|(id, node)| {
            json!({"evidence": node.path, "id": id, "name": node.name,
                "path": node.path, "depth": node.depth})
        })
        .collect();
    let edge_rows: Vec<Value> = kept_edges
        .iter()
        .map(|edge| {
            json!({"evidence": edge.evidence, "from": edge.from, "to": edge.to,
                "relation": edge.relation, "direction": edge.direction,
                "depth": edge.depth})
        })
        .collect();
    let truncated = walk_truncated || node_total > params.cap || edge_total > params.cap;

    let model = json!({"focus": focus, "found": found, "depth": params.depth,
        "direction": "both", "truncated": truncated,
        "nodes": node_rows, "edges": edge_rows});
    let mermaid = found.then(|| mermaid(&kept, &kept_edges));
    Ok(View {
        sections: vec![
            ("symbols in the neighborhood", node_rows),
            ("calls and imports", edge_rows),
        ] as Vec<Section>,
        model,
        mermaid,
    })
}

/// The bounded walk: the seeds at depth 0, then `depth` hops of live edges
/// in both directions. Returns the nodes reached, the edges crossed (each
/// keyed by its endpoints, relation and call site) and whether
/// [`MAX_NODES`] cut the walk short.
fn walk(graph: &GraphStore, repository: &str, seeds: &[Value], depth: usize) -> Result<Walk> {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    for seed in seeds {
        let id = text(seed, "id");
        if id.is_empty() {
            continue;
        }
        let name = match text(seed, "name") {
            name if name.is_empty() => id.clone(),
            name => name,
        };
        nodes.entry(id).or_insert(Node {
            name,
            path: text(seed, "path"),
            depth: 0,
        });
    }
    let mut edges = BTreeMap::new();
    let mut truncated = false;
    let mut frontier: Vec<String> = nodes.keys().cloned().collect();
    for hop in 1..=depth {
        if frontier.is_empty() {
            break;
        }
        let mut hits = Vec::new();
        for (cypher, out) in [(OUT, true), (IN, false)] {
            for row in run(graph, cypher, &json!({"repo": repository, "ids": frontier}))? {
                hits.push(hit(&row, out));
            }
        }
        // The rows are sorted before the walk takes them, so the cut at
        // MAX_NODES is the same whatever order the store returns them in.
        hits.sort_by(|a, b| {
            (
                &a.neighbor,
                &a.anchor,
                &a.relation,
                a.out,
                &a.edge_path,
                a.line,
            )
                .cmp(&(
                    &b.neighbor,
                    &b.anchor,
                    &b.relation,
                    b.out,
                    &b.edge_path,
                    b.line,
                ))
        });
        let mut next = BTreeSet::new();
        for hit in hits {
            if hit.neighbor == hit.anchor {
                continue;
            }
            let (from, to) = if hit.out {
                (hit.anchor.clone(), hit.neighbor.clone())
            } else {
                (hit.neighbor.clone(), hit.anchor.clone())
            };
            let evidence = call_site(&hit);
            let key = (
                from.clone(),
                to.clone(),
                hit.relation.clone(),
                evidence.clone(),
            );
            edges.entry(key).or_insert(Edge {
                from,
                to,
                relation: hit.relation.clone(),
                direction: if hit.out { "out" } else { "in" },
                evidence,
                depth: hop,
            });
            if nodes.contains_key(&hit.neighbor) {
                continue;
            }
            if nodes.len() >= MAX_NODES {
                truncated = true;
                continue;
            }
            nodes.insert(
                hit.neighbor.clone(),
                Node {
                    name: if hit.name.is_empty() {
                        hit.neighbor.clone()
                    } else {
                        hit.name
                    },
                    path: hit.path,
                    depth: hop,
                },
            );
            next.insert(hit.neighbor);
        }
        frontier = next.into_iter().collect();
    }
    Ok(Walk {
        nodes,
        edges,
        truncated,
    })
}

/// The mermaid flowchart of the kept neighborhood: one quoted node per
/// node, its name and file in the label; one quoted edge per crossed edge,
/// labelled with its relation and call site.
fn mermaid(nodes: &[(&String, &Node)], edges: &[&Edge]) -> String {
    let index: BTreeMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();
    let mut lines = vec!["flowchart LR".to_string()];
    for (i, (_, node)) in nodes.iter().enumerate() {
        lines.push(format!(
            "  n{i}[\"{}<br/>{} (depth {})\"]",
            mermaid_label(&node.name),
            mermaid_label(&node.path),
            node.depth
        ));
    }
    for edge in edges {
        let (Some(from), Some(to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        lines.push(format!(
            "  n{from} -->|\"{}\"| n{to}",
            mermaid_label(&format!("{} {}", edge.relation, edge.evidence))
        ));
    }
    lines.join("\n") + "\n"
}

/// The call site of a hit: the edge's own file and line, else the caller's
/// file (the anchor on an out edge, the neighbor on an in edge).
fn call_site(hit: &Hit) -> String {
    let path = if hit.edge_path.is_empty() {
        if hit.out {
            hit.anchor_path.as_str()
        } else {
            hit.path.as_str()
        }
    } else {
        hit.edge_path.as_str()
    };
    match hit.line {
        Some(line) => format!("{path}:{line}"),
        None => path.to_string(),
    }
}

fn hit(row: &Value, out: bool) -> Hit {
    Hit {
        anchor: text(row, "anchor"),
        neighbor: text(row, "neighbor"),
        name: text(row, "name"),
        path: text(row, "path"),
        relation: text(row, "relation"),
        edge_path: text(row, "edge_path"),
        line: line_of(&text(row, "location")),
        anchor_path: text(row, "anchor_path"),
        out,
    }
}
