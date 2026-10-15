//! Exact file-group fingerprints for incremental projection persistence.
//!
//! The complete assembled graph is still the authority. Hash every final
//! record, including global normalization effects, then load/compare stored
//! records only for groups that differ. This is a cache, not a second
//! authority.
//!
//! The graph store has no triggers, so projection-group invalidation is
//! explicit write-path logic: any write that inserts, updates or deletes
//! projection rows calls [`invalidate`] with the owner groups it touched, in
//! the same transaction, and the projector publishes fresh digests with
//! [`Plan::finish`] after its row writes. Groups are
//! `(:ProjectionGroup {repository_id, owner, digest})` nodes; a missing
//! `digest` marks the group dirty. Digests are hex strings (the engine has
//! no byte-string value type). Bulk work is plain `UNWIND $rows` batches
//! inside the caller's explicit transaction — no SAVEPOINT dance, and no
//! `CALL {} IN TRANSACTIONS` (spike E6 measured it ~300× slower on
//! `:memory:` in v0.12.2).
use crate::{
    Result,
    graph_projection::CanonicalRecord,
    store_graph::{Params, json_to_value, rows},
};
use cypherlite::Connection;
use graphicle::{graph::Graph, record::Attributes};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
};

pub fn owner(attributes: &Attributes) -> &str {
    attributes
        .get("relpath")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            attributes
                .get("source_file")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("")
}

/// Explicit write-path replacement for the legacy SQL triggers: a write that
/// inserts, updates or deletes projection rows marks every owner group it
/// touched dirty by clearing the cached digest. Runs inside the same
/// transaction as the mutation itself; there is no trigger to do it.
// Used by the projection write path as it migrates (and by tests).
#[allow(dead_code)]
pub fn invalidate(
    db: &mut Connection,
    repository: &str,
    owners: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<()> {
    let owners: Vec<String> = owners.into_iter().map(|o| o.as_ref().to_string()).collect();
    if owners.is_empty() {
        return Ok(());
    }
    let mut params = Params::new();
    params.insert("repository".into(), json_to_value(&json!(repository)));
    params.insert("owners".into(), json_to_value(&json!(owners)));
    rows(
        db,
        "UNWIND $owners AS owner
         MERGE (g:ProjectionGroup {repository_id: $repository, owner: owner})
         SET g.digest = NULL",
        params,
    )?;
    Ok(())
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

struct HashWriter<'a>(&'a mut Sha256);
impl io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub groups: usize,
    pub dirty_groups: usize,
    pub nodes_compared: usize,
    pub edges_compared: usize,
    pub total_nodes: usize,
    pub total_edges: usize,
}

pub struct Plan {
    pub stats: Stats,
    pub dirty_json: String,
    dirty: BTreeSet<String>,
    next: BTreeMap<String, [u8; 32]>,
}

impl Plan {
    pub fn new(db: &mut Connection, repository: &str, graph: &Graph) -> Result<Self> {
        let mut groups: HashMap<&str, (Sha256, usize, usize)> = HashMap::new();
        for node in graph.nodes() {
            let group = groups.entry(owner(&node.attributes)).or_default();
            serde_json::to_writer(
                HashWriter(&mut group.0),
                &(0, &node.id, CanonicalRecord(&node.attributes)),
            )?;
            group.1 += 1;
        }
        for (source, target, edge) in graph.iter_edges() {
            let group = groups.entry(owner(&edge.attributes)).or_default();
            serde_json::to_writer(
                HashWriter(&mut group.0),
                &(
                    1,
                    &graph.nodes()[source].id,
                    &graph.nodes()[target].id,
                    &edge.key,
                    CanonicalRecord(&edge.attributes),
                ),
            )?;
            group.2 += 1;
        }
        let mut params = Params::new();
        params.insert("repository".into(), json_to_value(&json!(repository)));
        let mut previous = BTreeMap::new();
        for row in rows(
            db,
            "MATCH (g:ProjectionGroup {repository_id: $repository})
             RETURN g.owner AS owner, g.digest AS digest",
            params,
        )? {
            previous.insert(
                row["owner"].as_str().unwrap_or("").to_string(),
                row["digest"].as_str().and_then(unhex),
            );
        }
        let mut next = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        let mut stats = Stats {
            groups: groups.len(),
            dirty_groups: 0,
            nodes_compared: 0,
            edges_compared: 0,
            total_nodes: graph.node_count(),
            total_edges: graph.edge_count(),
        };
        for (owner, (hash, nodes, edges)) in groups {
            let digest: [u8; 32] = hash.finalize().into();
            if previous.remove(owner).flatten().as_deref() != Some(digest.as_slice()) {
                dirty.insert(owner.to_owned());
                stats.nodes_compared += nodes;
                stats.edges_compared += edges;
            }
            next.insert(owner.to_owned(), digest);
        }
        dirty.extend(previous.into_keys());
        stats.dirty_groups = dirty.len();
        Ok(Self {
            dirty_json: serde_json::to_string(&dirty)?,
            stats,
            dirty,
            next,
        })
    }

    pub fn changed(&self, attributes: &Attributes) -> bool {
        self.dirty.contains(owner(attributes))
    }

    /// The owner groups (files) this projection rewrites, removed ones
    /// included.
    pub fn dirty(&self) -> &BTreeSet<String> {
        &self.dirty
    }

    /// Publish the cache only after all authoritative rows have been written.
    /// The caller keeps the plan and the row writes in the same transaction.
    pub fn finish(&self, db: &mut Connection, repository: &str) -> Result<()> {
        let updates: Vec<_> = self
            .dirty
            .iter()
            .filter_map(|owner| {
                self.next
                    .get(owner)
                    .map(|digest| json!({"owner": owner, "digest": hex(digest)}))
            })
            .collect();
        if !updates.is_empty() {
            let mut params = Params::new();
            params.insert("repository".into(), json_to_value(&json!(repository)));
            params.insert("rows".into(), json_to_value(&json!(updates)));
            rows(
                db,
                "UNWIND $rows AS row
                 MERGE (g:ProjectionGroup {repository_id: $repository, owner: row.owner})
                 SET g.digest = row.digest",
                params,
            )?;
        }
        let removed: Vec<&String> = self
            .dirty
            .iter()
            .filter(|owner| !self.next.contains_key(*owner))
            .collect();
        if !removed.is_empty() {
            let mut params = Params::new();
            params.insert("repository".into(), json_to_value(&json!(repository)));
            params.insert("owners".into(), json_to_value(&json!(removed)));
            rows(
                db,
                "UNWIND $owners AS owner
                 MATCH (g:ProjectionGroup {repository_id: $repository, owner: owner})
                 DELETE g",
                params,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_graph::GraphStore;

    fn graph(files: &[(&str, &str)]) -> Graph {
        let nodes: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, (id, file))| {
                json!({"id": id, "stable_id": id, "label": id, "relpath": file, "seq": i})
            })
            .collect();
        Graph::from_node_link(
            &json!({"directed": true, "multigraph": true, "nodes": nodes, "links": []}),
            "links",
        )
        .unwrap()
    }

    fn attrs(file: &str) -> Attributes {
        Attributes::from(json!({"relpath": file}).as_object().unwrap().clone())
    }

    #[test]
    fn dirty_groups_track_writes_without_triggers() {
        let store = GraphStore::open_in_memory().unwrap();
        let first = graph(&[("a", "a.py"), ("b", "b.py")]);
        let plan = store
            .transaction(|db| {
                let plan = Plan::new(db, "repo", &first)?;
                assert_eq!(plan.stats.dirty_groups, 2);
                plan.finish(db, "repo")?;
                Ok(plan)
            })
            .unwrap();
        assert!(plan.changed(&attrs("a.py")));
        // Unchanged graph: nothing dirty once digests are published.
        store
            .transaction(|db| {
                let plan = Plan::new(db, "repo", &first)?;
                assert_eq!(plan.stats.dirty_groups, 0);
                Ok(())
            })
            .unwrap();
        // An explicit invalidation (the trigger replacement) dirties a group.
        store
            .transaction(|db| {
                invalidate(db, "repo", ["a.py"])?;
                let plan = Plan::new(db, "repo", &first)?;
                assert_eq!(plan.dirty().iter().collect::<Vec<_>>(), ["a.py"]);
                plan.finish(db, "repo")?;
                Ok(())
            })
            .unwrap();
        // A removed file's group disappears on finish.
        let shrunk = graph(&[("a", "a.py")]);
        store
            .transaction(|db| {
                let plan = Plan::new(db, "repo", &shrunk)?;
                assert_eq!(plan.dirty().iter().collect::<Vec<_>>(), ["b.py"]);
                plan.finish(db, "repo")?;
                Ok(())
            })
            .unwrap();
        store
            .read(|db| {
                let plan = Plan::new(db, "repo", &shrunk)?;
                assert_eq!(plan.stats.dirty_groups, 0);
                Ok(())
            })
            .unwrap();
    }
}
