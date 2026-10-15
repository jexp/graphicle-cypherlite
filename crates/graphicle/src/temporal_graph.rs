//! Repository-scoped, append-only valid/recorded-time graph assertions.
//!
//! Graph model (migration plan §2.4):
//! - `(:Assertion {repository_id, branch, episode, valid_us, recorded_us,
//!   origin, seq})-[:ON_TIMELINE]->(:Project)` replaces `gf_timeline`.
//! - `(:Alias {repository_id, proposal_id, from_node, to_node, active,
//!   valid_us, recorded_us, seq})-[:OF]->(:Project)` replaces `gf_aliases`.
//! - `(:Hyperedge {repository_id, hyperedge_id, episode, relation, members,
//!   attributes, active, valid_us, recorded_us, seq})-[:OF]->(:Project)`
//!   replaces `gf_hyperedges`; `members` is a list property. `attributes`
//!   stays stringified JSON: the engine rejects map-valued properties
//!   (v0.12.2), only scalars and lists of scalars.
//!
//! `seq` comes from a store-wide `(:Counter {name: 'temporal_seq'})` and
//! preserves insertion order for assertions sharing `(valid_us,
//! recorded_us)` — the role the SQL rowid played. The bi-temporal
//! `row_number() OVER (PARTITION BY …)` window reads become
//! `ORDER BY valid_us DESC, recorded_us DESC, seq DESC` + `collect(row)[0]`
//! per group.
use crate::{
    Error, Result, graph_projection,
    store_graph::{Params, json_to_value, one, rows, s},
};
use chrono::{DateTime, SecondsFormat, Utc};
use cypherlite::Connection;
use graphicle::graph::Graph;
use graphicle_memory::graph::canonical::AliasSet;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn instant(value: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.timestamp_micros())
        .map_err(|_| Error::bad("Use an RFC3339 timestamp with an explicit time zone"))
}
pub fn formatted(value: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(value)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Micros, true))
        .unwrap_or_default()
}
pub fn clock() -> i64 {
    Utc::now().timestamp_micros()
}

/// The next assertion sequence number (insertion-order tiebreak). Runs in
/// the caller's transaction.
fn next_seq(db: &mut Connection) -> Result<i64> {
    let row = one(
        db,
        "MERGE (c:Counter {name: 'temporal_seq'})
         ON CREATE SET c.value = 0
         SET c.value = c.value + 1
         RETURN c.value AS seq",
        Params::new(),
    )?
    .ok_or_else(|| Error::bad("Temporal counter did not advance"))?;
    Ok(row["seq"].as_i64().unwrap_or(0))
}

/// The project an assertion attaches to. Assertions never predate their
/// project; the id is the repository id, and the graph type's mandatory
/// properties are satisfied when a test asserts before registering.
fn project(db: &mut Connection, repository: &str) -> Result<()> {
    let mut params = Params::new();
    params.insert("repository".into(), json_to_value(&json!(repository)));
    rows(
        db,
        "MERGE (p:Project {id: $repository})
         ON CREATE SET p.workspace_id = $repository, p.project_key = $repository",
        params,
    )?;
    Ok(())
}

fn valid_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 256 || branch.chars().any(char::is_control) {
        return Err(Error::bad("Invalid timeline branch"));
    }
    Ok(())
}

pub fn observe(
    db: &mut Connection,
    repository: &str,
    episode: &str,
    branch: &str,
    valid: i64,
    recorded: i64,
    origin: &str,
) -> Result<()> {
    valid_branch(branch)?;
    project(db, repository)?;
    let seq = next_seq(db)?;
    let mut params = Params::new();
    params.insert(
        "props".into(),
        json_to_value(&json!({
            "repository_id": repository, "branch": branch, "episode": episode,
            "valid_us": valid, "recorded_us": recorded, "origin": origin, "seq": seq,
        })),
    );
    params.insert("repository".into(), json_to_value(&json!(repository)));
    rows(
        db,
        "MATCH (p:Project {id: $repository})
         CREATE (a:Assertion)-[:ON_TIMELINE]->(p)
         SET a += $props",
        params,
    )?;
    Ok(())
}

/// Record a hyperedge assertion (created or retracted) at a valid and a
/// recorded time. `members` is stored as a list property. `attributes`
/// scalar fields flatten to dotted properties (`attributes.<key>`, which
/// stay queryable/indexable — the engine has no map-valued properties);
/// nested or non-scalar values keep a stringified `attributes` JSON blob.
pub fn hyperedge_event(
    db: &mut Connection,
    repository: &str,
    hyperedge: &Value,
    active: bool,
    valid: i64,
    recorded: i64,
) -> Result<()> {
    project(db, repository)?;
    let seq = next_seq(db)?;
    let mut props = json!({
        "repository_id": repository,
        "hyperedge_id": s(hyperedge, "hyperedge_id"),
        "episode": s(hyperedge, "episode"),
        "relation": s(hyperedge, "relation"),
        "members": hyperedge.get("members").cloned().unwrap_or(json!([])),
        "active": active, "valid_us": valid, "recorded_us": recorded, "seq": seq,
    });
    let attributes = hyperedge.get("attributes").cloned().unwrap_or(json!({}));
    let flattened: Vec<(String, Value)> = match attributes.as_object() {
        Some(fields)
            if !fields.is_empty()
                && fields.iter().all(|(key, value)| {
                    !key.contains('`') && !value.is_object() && !value.is_array()
                }) =>
        {
            fields
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }
        _ => {
            props["attributes"] = json!(serde_json::to_string(&attributes)?);
            Vec::new()
        }
    };
    let mut params = Params::new();
    params.insert("props".into(), json_to_value(&props));
    params.insert("repository".into(), json_to_value(&json!(repository)));
    // Dotted property names are set with backtick-quoted keys (verified on
    // v0.12.2); the map merge does not carry them.
    let dotted = flattened
        .iter()
        .enumerate()
        .map(|(i, (key, value))| {
            params.insert(format!("attr{i}"), json_to_value(value));
            format!("h.`attributes.{key}` = $attr{i}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    rows(
        db,
        &format!(
            "MATCH (p:Project {{id: $repository}})
             CREATE (h:Hyperedge)-[:OF]->(p)
             SET h += $props{}",
            if dotted.is_empty() {
                String::new()
            } else {
                format!(", {dotted}")
            }
        ),
        params,
    )?;
    Ok(())
}

pub struct View {
    pub episode: String,
    pub valid: i64,
    pub known: i64,
    pub metadata: Value,
}

fn optional_timestamp(args: &Value, key: &str) -> Result<Option<i64>> {
    args.get(key)
        .map(|v| {
            v.as_str()
                .ok_or_else(|| Error::bad(format!("{key} must be an RFC3339 timestamp")))
                .and_then(instant)
        })
        .transpose()
}
fn branch(args: &Value) -> Result<&str> {
    let branch = match args.get("branch") {
        None => "default",
        Some(v) => v
            .as_str()
            .ok_or_else(|| Error::bad("branch must be a string"))?,
    };
    valid_branch(branch)?;
    Ok(branch)
}

/// Episode selectors on the graph store: `latest`/`working`/explicit resolve
/// against `Episode` nodes directly; anything else is a workspace label.
fn resolve_episode(
    db: &mut Connection,
    workspace: &str,
    repository: &str,
    selector: &str,
) -> Result<String> {
    if selector.is_empty() || selector.len() > 128 {
        return Err(Error::bad("Invalid episode selector"));
    }
    let params = || {
        let mut params = Params::new();
        params.insert("repository".into(), json_to_value(&json!(repository)));
        params
    };
    match selector {
        "working" => {
            let heads: std::collections::BTreeMap<String, String> = rows(
                db,
                "MATCH (f:File)-[:IN_PROJECT]->(:Project {id: $repository})
                 MATCH (f)-[:HEAD]->(o:Object)
                 RETURN f.path AS path, o.sha256 AS sha256",
                params(),
            )?
            .into_iter()
            .map(|row| (s(&row, "path").to_string(), s(&row, "sha256").to_string()))
            .collect();
            let current = graph_projection::episode_of(&heads);
            if episode_exists(db, repository, &current)? {
                return Ok(current);
            }
            return Err(Error::bad(
                "The working head set has not been projected yet",
            ));
        }
        "latest" => {
            return one(
                db,
                "MATCH (e:Episode {repository_id: $repository})
                 RETURN e.episode AS episode
                 ORDER BY e.sequence DESC LIMIT 1",
                params(),
            )?
            .map(|row| s(&row, "episode").to_string())
            .ok_or_else(|| Error::bad("No graph episode recorded for this repository"));
        }
        _ => {}
    }
    if episode_exists(db, repository, selector)? {
        return Ok(selector.to_string());
    }
    let mut params = params();
    params.insert("workspace".into(), json_to_value(&json!(workspace)));
    params.insert("label".into(), json_to_value(&json!(selector)));
    one(
        db,
        "MATCH (l:EpisodeLabel {workspace: $workspace, repository_id: $repository,
                                label: $label})
         RETURN l.episode AS episode LIMIT 1",
        params,
    )?
    .map(|row| s(&row, "episode").to_string())
    .ok_or_else(|| Error::bad("Unknown episode or label for this repository"))
}

fn episode_exists(db: &mut Connection, repository: &str, episode: &str) -> Result<bool> {
    let mut params = Params::new();
    params.insert("repository".into(), json_to_value(&json!(repository)));
    params.insert("episode".into(), json_to_value(&json!(episode)));
    Ok(one(
        db,
        "MATCH (e:Episode {repository_id: $repository, episode: $episode})
         RETURN e.episode AS episode LIMIT 1",
        params,
    )?
    .is_some())
}

pub fn select(
    db: &mut Connection,
    workspace: &str,
    repository: &str,
    args: &Value,
) -> Result<View> {
    let known = optional_timestamp(args, "known_at")?.unwrap_or_else(clock);
    let branch = branch(args)?;
    if args
        .get("as_of")
        .is_some_and(|v| !v.is_string() && !v.is_null())
    {
        return Err(Error::bad("as_of must be a string"));
    }
    if args.get("canonical").is_some_and(|v| !v.is_boolean()) {
        return Err(Error::bad("canonical must be boolean"));
    }
    let temporal = args.get("valid_at").is_some()
        || args.get("known_at").is_some()
        || args.get("branch").is_some();
    if temporal && args.get("as_of").is_some_and(|v| !v.is_null()) {
        return Err(Error::bad(
            "Choose an episode selector or valid_at/known_at/branch",
        ));
    }
    if temporal {
        let valid = optional_timestamp(args, "valid_at")?.unwrap_or_else(clock);
        let mut params = Params::new();
        params.insert("repository".into(), json_to_value(&json!(repository)));
        params.insert("branch".into(), json_to_value(&json!(branch)));
        params.insert("valid".into(), json_to_value(&json!(valid)));
        params.insert("known".into(), json_to_value(&json!(known)));
        let row = one(
            db,
            "MATCH (a:Assertion {repository_id: $repository, branch: $branch})
             WHERE a.valid_us <= $valid AND a.recorded_us <= $known
             RETURN a.episode AS episode, a.valid_us AS valid_us,
                    a.recorded_us AS recorded_us, a.origin AS origin
             ORDER BY a.valid_us DESC, a.recorded_us DESC, a.seq DESC
             LIMIT 1",
            params,
        )?
        .ok_or_else(|| Error::bad("No graph known at the requested valid/recorded time"))?;
        Ok(View {
            episode: s(&row, "episode").into(),
            valid,
            known,
            metadata: json!({"valid_at":formatted(valid),"known_at":formatted(known),"branch":branch,"assertion_valid_from":formatted(row["valid_us"].as_i64().unwrap_or(0)),"recorded_at":formatted(row["recorded_us"].as_i64().unwrap_or(0)),"origin":row["origin"]}),
        })
    } else {
        let selector = args["as_of"].as_str().unwrap_or("latest");
        let episode = resolve_episode(db, workspace, repository, selector)?;
        let mut params = Params::new();
        params.insert("repository".into(), json_to_value(&json!(repository)));
        params.insert("episode".into(), json_to_value(&json!(episode)));
        let row = one(
            db,
            "MATCH (e:Episode {repository_id: $repository, episode: $episode})
             RETURN e.created_at AS created_at",
            params,
        )?
        .ok_or_else(|| Error::bad("Unknown graph episode"))?;
        let valid = if matches!(selector, "latest" | "working") {
            clock()
        } else {
            instant(s(&row, "created_at"))?
        };
        Ok(View {
            episode,
            valid,
            known,
            metadata: json!({"valid_at":formatted(valid),"known_at":formatted(known),"selection":"episode"}),
        })
    }
}

pub fn alias_event(
    db: &mut Connection,
    repository: &str,
    proposal: &Value,
    active: bool,
    valid: i64,
    recorded: i64,
) -> Result<()> {
    project(db, repository)?;
    let seq = next_seq(db)?;
    let mut params = Params::new();
    params.insert(
        "props".into(),
        json_to_value(&json!({
            "repository_id": repository, "proposal_id": s(proposal, "id"),
            "from_node": s(proposal, "from_node"), "to_node": s(proposal, "to_node"),
            "active": active, "valid_us": valid, "recorded_us": recorded, "seq": seq,
        })),
    );
    params.insert("repository".into(), json_to_value(&json!(repository)));
    rows(
        db,
        "MATCH (p:Project {id: $repository})
         CREATE (a:Alias)-[:OF]->(p)
         SET a += $props",
        params,
    )?;
    Ok(())
}

pub fn aliases_at(
    db: &mut Connection,
    repository: &str,
    valid: i64,
    known: i64,
) -> Result<AliasSet> {
    let mut params = Params::new();
    params.insert("repository".into(), json_to_value(&json!(repository)));
    params.insert("valid".into(), json_to_value(&json!(valid)));
    params.insert("known".into(), json_to_value(&json!(known)));
    // Latest assertion per proposal: order the candidates, then take the
    // head of each group (replaces row_number() OVER (PARTITION BY …)).
    let active = rows(
        db,
        "MATCH (a:Alias {repository_id: $repository})
         WHERE a.valid_us <= $valid AND a.recorded_us <= $known
         WITH a ORDER BY a.valid_us DESC, a.recorded_us DESC, a.seq DESC
         WITH a.proposal_id AS proposal, collect(a)[0] AS latest
         WHERE latest.active
         RETURN latest.from_node AS from_node, latest.to_node AS to_node
         ORDER BY proposal
         LIMIT 10001",
        params,
    )?;
    if active.len() > 10000 {
        return Err(Error::bad("Historical alias view exceeds 10000 aliases"));
    }
    let mut set = AliasSet::new();
    for row in active {
        set.union(s(&row, "from_node"), s(&row, "to_node"));
    }

    Ok(set)
}

pub fn hyperedges_at(
    db: &mut Connection,
    repository: &str,
    valid: i64,
    known: i64,
    graph: &Graph,
) -> Result<(Vec<Value>, bool)> {
    let mut params = Params::new();
    params.insert("repository".into(), json_to_value(&json!(repository)));
    params.insert("valid".into(), json_to_value(&json!(valid)));
    params.insert("known".into(), json_to_value(&json!(known)));
    let mut latest = rows(
        db,
        "MATCH (h:Hyperedge {repository_id: $repository})
         WHERE h.valid_us <= $valid AND h.recorded_us <= $known
         WITH h ORDER BY h.valid_us DESC, h.recorded_us DESC, h.seq DESC
         WITH h.hyperedge_id AS hyperedge, collect(h)[0] AS latest
         WHERE latest.active
         RETURN latest
         ORDER BY hyperedge
         LIMIT 1001",
        params,
    )?;
    let truncated = latest.len() > 1000;
    latest.truncate(1000);
    // The node carries scalar attribute fields as dotted `attributes.<key>`
    // properties; a stringified `attributes` blob holds nested payloads.
    // Both fold back into one `attributes` object.
    let mut rows_out = Vec::with_capacity(latest.len());
    for row in &latest {
        let node = &row["latest"];
        let mut out = serde_json::Map::new();
        let mut attributes = serde_json::Map::new();
        for (key, value) in node.as_object().into_iter().flatten() {
            if let Some(field) = key.strip_prefix("attributes.") {
                attributes.insert(field.to_string(), value.clone());
            } else if !key.starts_with('_') && key != "repository_id" && key != "seq" {
                out.insert(key.clone(), value.clone());
            }
        }
        if let Some(text) = out.get("attributes").and_then(Value::as_str) {
            if let Value::Object(blob) = serde_json::from_str::<Value>(text)? {
                for (key, value) in blob {
                    attributes.entry(key).or_insert(value);
                }
            }
        }
        out.remove("attributes");
        out.insert("attributes".into(), Value::Object(attributes));
        rows_out.push(Value::Object(out));
    }
    let latest = rows_out;
    let known_nodes = graph
        .nodes()
        .iter()
        .filter_map(|n| n.id.as_str())
        .collect::<BTreeSet<_>>();
    Ok((
        latest
            .into_iter()
            .map(|mut r| {
                r["unresolved_members"] = json!(
                    r["members"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|v| !v.as_str().is_some_and(|id| known_nodes.contains(id)))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                r["valid_from"] = json!(formatted(r["valid_us"].as_i64().unwrap_or(0)));
                r["recorded_at"] = json!(formatted(r["recorded_us"].as_i64().unwrap_or(0)));
                r
            })
            .collect(),
        truncated,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_graph::GraphStore;

    fn at(value: &str) -> i64 {
        instant(value).unwrap()
    }

    fn fixture() -> GraphStore {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .transaction(|db| {
                let mut params = Params::new();
                params.insert("id".into(), json_to_value(&json!("repo")));
                rows(
                    db,
                    "CREATE (p:Project {id: $id, workspace_id: $id, project_key: $id})",
                    params,
                )?;
                Ok(())
            })
            .unwrap();
        store
    }

    fn graph() -> Graph {
        Graph::from_node_link(
            &json!({"directed": true, "multigraph": true,
                "nodes": [{"id": "a", "stable_id": "a"}, {"id": "b", "stable_id": "b"}],
                "links": []}),
            "links",
        )
        .unwrap()
    }

    #[test]
    fn retroactive_correction_preserves_what_was_known_and_branch_scope() {
        let store = fixture();
        let (first, second) = (at("2021-01-01T00:00:00Z"), at("2021-01-02T00:00:00Z"));
        let valid = at("2020-01-01T00:00:00Z");
        store
            .transaction(|db| {
                for (episode, recorded) in [("e1", first), ("e2", second)] {
                    observe(db, "repo", episode, "release", valid, recorded, "test")?;
                }
                Ok(())
            })
            .unwrap();
        let seen = |known: i64| {
            store.read(|db| {
                select(
                    db,
                    "ws",
                    "repo",
                    &json!({"branch": "release",
                        "valid_at": "2020-01-02T00:00:00Z", "known_at": formatted(known)}),
                )
            })
        };
        assert_eq!(seen(first).unwrap().episode, "e1");
        assert_eq!(seen(second).unwrap().episode, "e2");
        // An unknown branch knows nothing.
        assert!(
            store
                .read(|db| select(
                    db,
                    "ws",
                    "repo",
                    &json!({"branch": "other", "valid_at": "2020-01-02T00:00:00Z"})
                ))
                .is_err()
        );
    }

    #[test]
    fn aliases_follow_the_latest_assertion_per_proposal() {
        let store = fixture();
        let valid = at("2020-01-01T00:00:00Z");
        store
            .transaction(|db| {
                let proposal = json!({"id": "p", "from_node": "a", "to_node": "b"});
                alias_event(db, "repo", &proposal, true, valid, valid + 10)?;
                alias_event(db, "repo", &proposal, false, valid, valid + 20)?;
                assert!(aliases_at(db, "repo", valid, valid + 9)?.is_empty());
                assert!(!aliases_at(db, "repo", valid, valid + 10)?.is_empty());
                assert!(aliases_at(db, "repo", valid, valid + 20)?.is_empty());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn hyperedges_survive_retraction_without_future_leakage() {
        let store = fixture();
        let valid = at("2020-01-01T00:00:00Z");
        let (created, retracted) = (valid + 10, valid + 20);
        let graph = graph();
        store
            .transaction(|db| {
                let edge = json!({"hyperedge_id": "contract", "episode": "e1",
                    "relation": "requires", "members": ["a", "b", "c"],
                    "attributes": {"priority": "high", "weight": 2}});
                hyperedge_event(db, "repo", &edge, true, valid, created)?;
                hyperedge_event(db, "repo", &edge, false, valid, retracted)?;
                let mut known = |recorded| hyperedges_at(db, "repo", valid, recorded, &graph);
                assert!(known(created - 1)?.0.is_empty());
                let (edges, truncated) = known(created)?;
                assert!(!truncated);
                assert_eq!(edges.len(), 1);
                // "c" is not a node of the graph: unresolved.
                assert_eq!(edges[0]["unresolved_members"], json!(["c"]));
                // Scalar attribute fields round-trip through dotted properties.
                assert_eq!(
                    edges[0]["attributes"],
                    json!({"priority": "high", "weight": 2})
                );
                assert!(known(retracted)?.0.is_empty());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn malformed_temporal_selectors_are_rejected() {
        let store = fixture();
        for args in [
            json!({"valid_at":1}),
            json!({"known_at":null}),
            json!({"branch":1}),
            json!({"branch":""}),
            json!({"canonical":"yes"}),
            json!({"as_of":false}),
            json!({"valid_at":"2020-01-01"}),
            json!({"branch":"bad\nbranch"}),
            json!({"as_of":"e1","valid_at":"2020-01-01T00:00:00Z"}),
        ] {
            assert!(
                store.read(|db| select(db, "ws", "repo", &args)).is_err(),
                "{args}"
            );
        }
    }

    #[test]
    fn hyperedges_report_truncation() {
        let store = fixture();
        let graph = graph();
        store
            .transaction(|db| {
                let seq = next_seq(db)?;
                let batch: Vec<_> = (0..1001)
                    .map(|i| {
                        json!({"repository_id": "repo",
                            "hyperedge_id": format!("h{i:04}"), "episode": "e1",
                            "relation": "requires", "members": ["a", "b"], "attributes": "{}",
                            "active": true, "valid_us": 0, "recorded_us": 0,
                            "seq": seq + i as i64})
                    })
                    .collect();
                let mut params = Params::new();
                params.insert("repository".into(), json_to_value(&json!("repo")));
                params.insert("rows".into(), json_to_value(&json!(batch)));
                rows(
                    db,
                    "MATCH (p:Project {id: $repository})
                     UNWIND $rows AS row
                     CREATE (h:Hyperedge)-[:OF]->(p)
                     SET h += row",
                    params,
                )?;
                Ok(())
            })
            .unwrap();
        let (edges, truncated) = store
            .read(|db| hyperedges_at(db, "repo", 0, 0, &graph))
            .unwrap();
        assert!(truncated);
        assert_eq!(edges.len(), 1000);
        assert_eq!(edges[0]["unresolved_members"], json!([]));
        assert_eq!(edges[0]["hyperedge_id"], "h0000");
    }
}
