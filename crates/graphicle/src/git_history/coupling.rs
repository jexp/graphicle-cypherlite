//! Co-change coupling (git-to-graph plan §5): window-keyed `CO_CHANGED`
//! edges derived from shared commits, at file and at symbol level.
//!
//! ```text
//! (:File)-[:CO_CHANGED {window, count}]->(:File)
//! (:Symbol)-[:CO_CHANGED {window, count}]->(:Symbol)
//! ```
//!
//! Two files co-change when one commit's `:Change` set touches both; `count`
//! is the number of such shared commits inside the window. Two symbols
//! co-change when one commit touches both through a `TOUCHES` edge of the
//! `span` basis (a line-level touch). The `move` basis carries no lines and
//! only names the symbol that survived a rename, so it is not evidence that
//! the symbol changed and does not count.
//!
//! Each pair is written once, with the lexicographically smaller endpoint as
//! the source; a query reads it undirected.
//!
//! # The changeset cap
//!
//! A commit touching more than `max_changeset` files (or symbols, at symbol
//! level) contributes nothing at that level. Bulk imports and large merges
//! would otherwise tie every pair of their files together. A capped commit
//! has at most cap-choose-2 pairs, so enumeration stays bounded.
//!
//! # Windows: recompute, not update (plan §5)
//!
//! Every edge carries the key of the window that wrote it (`all`, `90d`).
//! A run recomputes each configured window: it deletes the edges carrying
//! that key, then writes the pairs the window now yields. The key scopes the
//! delete, so a narrower window's rerun removes its own pairs that have aged
//! out and never touches another window's pairs. Nothing accumulates across
//! runs: a rerun of an unchanged history writes the same counts.
//!
//! The 90-day window is time-based, but its job is keyed on the head and the
//! configuration, not the date. An unchanged head therefore replays the
//! finished job and never re-runs the window, so its edges freeze at the
//! moment they were first computed: pairs that have since aged out stay
//! present, and wrong, until the head or the window configuration changes.
//!
//! # Supernode
//!
//! Every statement matches by key (`repository_id` with `commit`, `path` or
//! `stable_id`) and never passes through `(:Project)`, as in [`super::writer`].
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use super::writer::{self, Pass};
use crate::store_graph::{self, GraphStore};
use crate::{Result, digest};

/// The job kind of the co-change derivation (plan §5).
pub const KIND: &str = "coupling";

/// The derivation's format, folded into the job's input hash: a change to
/// what a pair means re-derives instead of replaying a stale result.
pub const FORMAT: u32 = 1;

/// The default changeset cap: a commit touching more files (or symbols) than
/// this contributes no pairs.
pub const MAX_CHANGESET: usize = 50;

/// One window to recompute: its key (stored on every edge it writes) and its
/// trailing length in days. `days: None` is all of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoupledWindow {
    pub key: String,
    pub days: Option<i64>,
}

/// The derivation's configuration: the changeset cap and the windows to
/// recompute. Part of the job's input hash, so changing it enqueues a new job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub max_changeset: usize,
    pub windows: Vec<CoupledWindow>,
}

impl Default for Config {
    /// All of history, and the trailing 90 days (plan §5).
    fn default() -> Self {
        Self {
            max_changeset: MAX_CHANGESET,
            windows: vec![
                CoupledWindow {
                    key: "all".to_string(),
                    days: None,
                },
                CoupledWindow {
                    key: "90d".to_string(),
                    days: Some(90),
                },
            ],
        }
    }
}

impl Config {
    /// The payload form the enqueue point records.
    pub fn to_json(&self) -> Value {
        json!({
            "max_changeset": self.max_changeset,
            "windows": self.windows.iter()
                .map(|window| json!({"key": window.key, "days": window.days}))
                .collect::<Vec<_>>(),
        })
    }

    /// Read back from a job payload's `config`; a missing part takes its
    /// default, and a window without a key is dropped.
    pub fn from_json(value: &Value) -> Self {
        let defaults = Self::default();
        let max_changeset = value["max_changeset"]
            .as_u64()
            .map(|max| max as usize)
            .unwrap_or(defaults.max_changeset);
        let windows = match value["windows"].as_array() {
            Some(list) => list
                .iter()
                .filter_map(|window| {
                    Some(CoupledWindow {
                        key: window["key"].as_str()?.to_string(),
                        days: window["days"].as_i64(),
                    })
                })
                .collect(),
            None => defaults.windows,
        };
        Self {
            max_changeset,
            windows,
        }
    }
}

/// The `coupling` job's input hash (plan §5): the window configuration and
/// the history head it derives from.
pub fn input_hash(head: &str, config: &Config) -> String {
    digest(&json!({
        "kind": KIND, "format": FORMAT, "head": head, "config": config.to_json(),
    }))
}

/// One level of coupling: the node label the pairs connect, the key property
/// that names a node, and the query that lists each commit's members.
struct Level {
    label: &'static str,
    key: &'static str,
    members: &'static str,
}

const FILES: Level = Level {
    label: "File",
    key: "path",
    members: "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
              (ch:Change {repository_id: $repo}) \
              RETURN c.oid AS commit, c.commit_date AS at, ch.path AS member",
};

const SYMBOLS: Level = Level {
    label: "Symbol",
    key: "stable_id",
    members: "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
              (ch:Change {repository_id: $repo})-[t:TOUCHES]->\
              (s:Symbol {repository_id: $repo}) \
              WHERE t.basis = 'span' \
              RETURN c.oid AS commit, c.commit_date AS at, s.stable_id AS member",
};

/// Each commit's date (when it parses) and its distinct members, by oid.
type CommitSets = BTreeMap<String, (Option<DateTime<Utc>>, BTreeSet<String>)>;

/// Recompute every configured window of `repository_id`'s co-change coupling
/// at `now`, at file and at symbol level (see the module doc). The lease is
/// checked before every batch. The result is the `coupling` job's:
/// `{"kind", "max_changeset", "windows": [{"key", "files", "symbols"}]}`,
/// the pair counts written per window.
pub fn derive(
    graph: &GraphStore,
    repository_id: &str,
    now: DateTime<Utc>,
    config: &Config,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    let pass = Pass {
        graph,
        repository_id,
        known_at: "",
        leasing: still_leasing,
    };
    let files = commit_sets(graph, repository_id, &FILES)?;
    let symbols = commit_sets(graph, repository_id, &SYMBOLS)?;
    let mut windows = Vec::new();
    for window in &config.windows {
        let cutoff = window.days.map(|days| now - Duration::days(days));
        let file_pairs = pair_counts(&files, cutoff, config.max_changeset);
        let symbol_pairs = pair_counts(&symbols, cutoff, config.max_changeset);
        recompute(&pass, &FILES, &window.key, &file_pairs)?;
        recompute(&pass, &SYMBOLS, &window.key, &symbol_pairs)?;
        windows.push(json!({"key": window.key, "files": file_pairs.len(),
            "symbols": symbol_pairs.len()}));
    }
    Ok(json!({"kind": KIND, "max_changeset": config.max_changeset, "windows": windows}))
}

/// The members of each commit at `level`, read from the graph.
fn commit_sets(graph: &GraphStore, repository_id: &str, level: &Level) -> Result<CommitSets> {
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            level.members,
            store_graph::params(&json!({"repo": repository_id})),
        )
    })?;
    let mut sets = CommitSets::new();
    for row in rows {
        let at = row["at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        let (_, members) = sets
            .entry(store_graph::s(&row, "commit").to_string())
            .or_insert_with(|| (at, BTreeSet::new()));
        members.insert(store_graph::s(&row, "member").to_string());
    }
    Ok(sets)
}

/// The shared-commit count of every pair inside `cutoff` (all of history
/// when `None`), skipping commits outside the window, with fewer than two
/// members, or above `cap`. A pair is keyed `(smaller, larger)`.
fn pair_counts(
    sets: &CommitSets,
    cutoff: Option<DateTime<Utc>>,
    cap: usize,
) -> BTreeMap<(String, String), i64> {
    let mut counts = BTreeMap::new();
    for (at, set) in sets.values() {
        let in_window = match (cutoff, at) {
            (None, _) => true,
            (Some(cutoff), Some(at)) => *at >= cutoff,
            (Some(_), None) => false,
        };
        if !in_window || set.len() < 2 || set.len() > cap {
            continue;
        }
        // The set is ordered, so every `a` sorts before the `b`s after it.
        let members: Vec<&String> = set.iter().collect();
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                *counts.entry(((*a).clone(), (*b).clone())).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Replace the `window` edges of `level` with `pairs`: delete the edges that
/// carry the key, then write the new ones in batches. The key scopes the
/// delete to this window; the endpoints scope it to this repository.
fn recompute(
    pass: &Pass,
    level: &Level,
    window: &str,
    pairs: &BTreeMap<(String, String), i64>,
) -> Result<()> {
    (pass.leasing)()?;
    pass.graph.transaction(|db| {
        store_graph::write(
            db,
            &format!(
                "MATCH (a:{label} {{repository_id: $repo}})\
                 -[r:CO_CHANGED {{window: $window}}]->\
                 (b:{label} {{repository_id: $repo}}) \
                 DELETE r",
                label = level.label,
            ),
            store_graph::params(&json!({"repo": pass.repository_id, "window": window})),
        )?;
        Ok(())
    })?;
    let rows: Vec<Value> = pairs
        .iter()
        .map(|((a, b), count)| json!({"a": a, "b": b, "count": count}))
        .collect();
    writer::batches(
        pass,
        &rows,
        &format!(
            "UNWIND $rows AS row \
             MATCH (a:{label} {{repository_id: $repo, {key}: row.a}}) \
             MATCH (b:{label} {{repository_id: $repo, {key}: row.b}}) \
             MERGE (a)-[r:CO_CHANGED {{window: $window}}]->(b) \
             SET r.count = row.count",
            label = level.label,
            key = level.key,
        ),
        json!({"window": window}),
    )
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Repo, coupled_fixture};
    use super::super::writer;
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    const REPO: &str = "repo-git";

    /// Inside the trailing 90 days of the fixture's commits (January 2026).
    fn inside() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 16, 0, 0, 0).unwrap()
    }

    /// Today: the fixture's commits are months older than the 90-day window.
    fn outside() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, 0, 0, 0).unwrap()
    }

    /// A graph holding `repo`'s history, ingested the way the history job does.
    fn graph_with_history(repo: &Repo) -> GraphStore {
        let graph = GraphStore::open_in_memory().unwrap();
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (p:Project {id: $id, workspace_id: $ws, project_key: $key})",
                    store_graph::params(&json!({"id": REPO, "ws": "ws", "key": REPO})),
                )?;
                Ok(())
            })
            .unwrap();
        writer::ingest(&graph, REPO, repo.root(), &Default::default(), &|| Ok(())).unwrap();
        graph
    }

    /// The count on the `window` edge from `a` to `b` (nodes of `label`).
    fn pair(
        graph: &GraphStore,
        label: &str,
        key: &str,
        a: &str,
        b: &str,
        window: &str,
    ) -> Option<i64> {
        graph
            .read(|db| {
                store_graph::one(
                    db,
                    &format!(
                        "MATCH (x:{label} {{repository_id: $repo, {key}: $a}})\
                         -[r:CO_CHANGED {{window: $window}}]->\
                         (y:{label} {{repository_id: $repo, {key}: $b}}) \
                         RETURN r.count AS count"
                    ),
                    store_graph::params(&json!({"repo": REPO, "a": a, "b": b, "window": window})),
                )
            })
            .unwrap()
            .map(|row| row["count"].as_i64().unwrap())
    }

    /// How many `CO_CHANGED` edges of `window` the repository holds.
    fn edges(graph: &GraphStore, window: &str) -> i64 {
        graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (x:File {repository_id: $repo})-[r:CO_CHANGED {window: $window}]->\
                     (y:File {repository_id: $repo}) RETURN count(r) AS n",
                    store_graph::params(&json!({"repo": REPO, "window": window})),
                )
            })
            .unwrap()
            .map(|row| row["n"].as_i64().unwrap())
            .unwrap_or(0)
    }

    fn no_lease() -> Result<()> {
        Ok(())
    }

    fn window(key: &str, days: Option<i64>) -> Config {
        Config {
            max_changeset: MAX_CHANGESET,
            windows: vec![CoupledWindow {
                key: key.to_string(),
                days,
            }],
        }
    }

    #[test]
    fn a_known_coupled_pair_surfaces_with_its_count() {
        let (_dir, repo, _scenario) = coupled_fixture();
        let graph = graph_with_history(&repo);
        let result = derive(&graph, REPO, inside(), &Config::default(), &no_lease).unwrap();

        // Two commits change pay.rs and tax.rs together.
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "all"),
            Some(2)
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "90d"),
            Some(2)
        );
        // The base history's own pairs: the first commit, and the rename
        // commit's three files (shared.rs, a.rs, new.rs).
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/common.rs", "all"),
            Some(1)
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/shared.rs", "all"),
            Some(1)
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/new.rs", "all"),
            Some(1)
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/new.rs", "src/shared.rs", "all"),
            Some(1)
        );
        // Five distinct file pairs all-time; the 90-day window holds them too.
        assert_eq!(result["windows"][0]["key"], json!("all"), "{result:#}");
        assert_eq!(result["windows"][0]["files"], json!(5), "{result:#}");
        assert_eq!(result["windows"][1]["files"], json!(5), "{result:#}");
        assert_eq!(result["windows"][0]["symbols"], json!(0), "{result:#}");
    }

    #[test]
    fn a_commit_over_the_changeset_cap_contributes_nothing() {
        let (_dir, repo, _scenario) = coupled_fixture();
        let graph = graph_with_history(&repo);
        // The rename commit touches three files; a cap of two excludes it.
        let config = Config {
            max_changeset: 2,
            ..window("all", None)
        };
        let result = derive(&graph, REPO, inside(), &config, &no_lease).unwrap();

        assert_eq!(
            pair(&graph, "File", "path", "src/new.rs", "src/shared.rs", "all"),
            None
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/new.rs", "all"),
            None
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/shared.rs", "all"),
            None
        );
        // Commits within the cap still count: the two-file first commit and
        // the two pay/tax commits.
        assert_eq!(
            pair(&graph, "File", "path", "src/a.rs", "src/common.rs", "all"),
            Some(1)
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "all"),
            Some(2)
        );
        assert_eq!(result["windows"][0]["files"], json!(2), "{result:#}");
    }

    #[test]
    fn a_narrower_window_rerun_never_leaves_stale_pairs() {
        let (_dir, repo, _scenario) = coupled_fixture();
        let graph = graph_with_history(&repo);
        derive(&graph, REPO, inside(), &Config::default(), &no_lease).unwrap();
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "90d"),
            Some(2)
        );

        // Recompute, not update: the all-time pair stays at 2, never 4.
        derive(&graph, REPO, inside(), &Config::default(), &no_lease).unwrap();
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "all"),
            Some(2)
        );

        // Today, the 90-day window holds none of the fixture's commits. Its
        // rerun deletes its own pairs and writes none; the all-time pairs are
        // another window's edges and stay as they were.
        derive(&graph, REPO, outside(), &window("90d", Some(90)), &no_lease).unwrap();
        assert_eq!(
            edges(&graph, "90d"),
            0,
            "a 90d rerun leaves no 90d pair behind"
        );
        assert_eq!(
            pair(&graph, "File", "path", "src/pay.rs", "src/tax.rs", "all"),
            Some(2)
        );
        assert_eq!(edges(&graph, "all"), 5);
    }

    #[test]
    fn symbol_pairs_come_from_span_touches_and_moves_do_not_count() {
        let (_dir, repo, scenario) = coupled_fixture();
        let graph = graph_with_history(&repo);
        for stable_id in ["pay::charge", "tax::levy", "shared::moved"] {
            graph
                .transaction(|db| {
                    store_graph::write(
                        db,
                        "CREATE (s:Symbol {repository_id: $repo, stable_id: $id, kind: 'fn', \
                         qualified_name: $id, relpath: 'src'})",
                        store_graph::params(&json!({"repo": REPO, "id": stable_id})),
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        // Each pay/tax commit touches both symbols through a span.
        let pay_two = repo.rev("HEAD");
        let pay_one = repo.rev("HEAD~1");
        for commit in [&pay_one, &pay_two] {
            touch(&graph, commit, "src/pay.rs", "pay::charge", "span");
            touch(&graph, commit, "src/tax.rs", "tax::levy", "span");
        }
        // The rename commit: two spans, and one symbol that only moved.
        touch(&graph, &scenario.rename, "src/a.rs", "pay::charge", "span");
        touch(&graph, &scenario.rename, "src/new.rs", "tax::levy", "span");
        touch(
            &graph,
            &scenario.rename,
            "src/shared.rs",
            "shared::moved",
            "move",
        );

        let result = derive(&graph, REPO, inside(), &window("all", None), &no_lease).unwrap();
        assert_eq!(
            pair(
                &graph,
                "Symbol",
                "stable_id",
                "pay::charge",
                "tax::levy",
                "all"
            ),
            Some(3),
            "two pay/tax commits and the rename commit"
        );
        // A move names a survivor, not a change: the moved symbol pairs with nothing.
        assert_eq!(
            pair(
                &graph,
                "Symbol",
                "stable_id",
                "pay::charge",
                "shared::moved",
                "all"
            ),
            None
        );
        assert_eq!(
            pair(
                &graph,
                "Symbol",
                "stable_id",
                "shared::moved",
                "tax::levy",
                "all"
            ),
            None
        );
        assert_eq!(result["windows"][0]["symbols"], json!(1), "{result:#}");
    }

    /// Record a `TOUCHES` edge from `commit`'s change to `path` on `stable_id`.
    fn touch(graph: &GraphStore, commit: &str, path: &str, stable_id: &str, basis: &str) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "MATCH (ch:Change {repository_id: $repo, commit: $commit, path: $path}) \
                     MATCH (s:Symbol {repository_id: $repo, stable_id: $id}) \
                     MERGE (ch)-[t:TOUCHES]->(s) SET t.basis = $basis",
                    store_graph::params(&json!({
                        "repo": REPO, "commit": commit, "path": path,
                        "id": stable_id, "basis": basis,
                    })),
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn the_config_round_trips_through_its_payload() {
        let config = Config {
            max_changeset: 7,
            ..Config::default()
        };
        assert_eq!(Config::from_json(&config.to_json()), config);
        assert_eq!(Config::from_json(&json!({})), Config::default());
    }
}
