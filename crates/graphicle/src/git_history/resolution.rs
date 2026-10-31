//! Change resolution (git-to-graph plan §2): each `(:Change)` is resolved
//! against the content-store world it was made in, and the result is two
//! kinds of edge:
//!
//! ```text
//! (:Change)-[:MODIFIES {basis, episode?, sha256}]->(:File)
//! (:Change)-[:TOUCHES {line_start, line_end}]->(:Symbol)
//! ```
//!
//! # Episode-lagged by design
//!
//! A change resolves against the episode world as it was when the commit
//! was made, never against a live re-derivation of that commit's symbols.
//! Concretely:
//!
//! - **MODIFIES, basis `episode`**: the commit has an `(:Episode)-[:AT_COMMIT]`
//!   edge (its episode's head set is the commit's tree, see
//!   [`super::correlation`]) and the path is in that tree. The edge carries
//!   the episode id and the blob's sha256 at the commit.
//! - **MODIFIES, basis `head`**: no such episode, so the file's then-current
//!   head ([`crate::store_content::heads`]) stands in. This is the lag: the
//!   head is the newest version the store holds, which is exact for tip
//!   commits and an approximation for older ones. The edge carries its sha256
//!   and no episode.
//! - **Unresolved**: a deletion (the path has no version at its own commit),
//!   or a path with no head in the store. The path is gone from every episode
//!   the store knows, so there is nothing to resolve against. Unresolved
//!   changes are counted in the result, never errors.
//!
//! Symbol spans come from the `:SymbolHead` table, which describes one
//! file version by its sha256. A span is used only when that sha256 is the
//! change's basis sha, so a span is never applied to a version it does not
//! describe. That is what keeps the lag honest: a change older than the
//! symbol table's version gets no `TOUCHES`, rather than a wrong one.
//!
//! # TOUCHES: the span rule
//!
//! Candidates are the `:SymbolHead` rows of the change's path at the basis
//! sha. For a rename (`old_path` set), the survivors of `:SymbolMove` chains
//! from the `:Symbol` identities under `old_path` are added too, so a symbol
//! that moved with its file resolves to the symbol that survived. A candidate
//! is touched when a changed line range (git's `-U0` hunks in the new file,
//! 1-based and inclusive) overlaps its span `[line_start, line_next - 1]`;
//! with no `line_next` the span runs to the end of the file. The edge carries
//! the overlap. A candidate with no `:Symbol` node gets no edge.
//!
//! Line information is best-effort. When the provider has none for a change
//! (a binary file, say), the change gets no `TOUCHES` edge and is counted in
//! `no_line_info`; that is not an error.
//!
//! # Supernode and idempotence
//!
//! Every statement matches by key (`repository_id` with `commit`, `path`,
//! `stable_id` or `relpath`), never through `(:Project)`. The edges are MERGE
//! or SET writes, so a repeated run converges on the same graph; they are
//! written through [`super::writer`]'s batches, so the lease is checked
//! between them.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::{Value, json};

use super::correlation;
use super::reader::{self, LOG_LIMIT};
use super::writer::{self, Pass};
use crate::store_graph::{self, GraphStore};
use crate::{Result, store_content};

/// A changed line range in the new file, 1-based and inclusive.
pub type LineRange = (u64, u64);

/// Rename chains followed through `:SymbolMove`, per change. A chain longer
/// than this keeps its first [`MAX_HOPS`] survivors and no more.
pub const MAX_HOPS: usize = 8;

/// The version a change resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Basis {
    /// The episode whose head set is the commit's tree, when there is one.
    episode: Option<String>,
    /// The blob sha256 of the path at that version.
    sha256: String,
}

/// One change as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeRow {
    commit: String,
    path: String,
    kind: String,
    old_path: Option<String>,
}

/// A symbol whose span can be matched against changed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    stable_id: String,
    line_start: u64,
    line_next: Option<u64>,
}

impl Span {
    /// The inclusive last line of the span, `None` when it runs to the end.
    fn last_line(&self) -> Option<u64> {
        self.line_next.map(|next| next.saturating_sub(1))
    }

    /// The overlap of `range` with this span, when there is one.
    fn overlap(&self, range: LineRange) -> Option<LineRange> {
        let (first, last) = range;
        let end = self.last_line().unwrap_or(u64::MAX);
        if first <= end && last >= self.line_start {
            Some((first.max(self.line_start), last.min(end)))
        } else {
            None
        }
    }
}

/// The counts a resolution run reports (the `resolution` field of the
/// `history` job's result).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Counts {
    changes: usize,
    modifies_episode: usize,
    modifies_head: usize,
    touches: usize,
    no_line_info: usize,
    unresolved: usize,
}

impl Counts {
    fn json(&self) -> Value {
        json!({
            "changes": self.changes,
            "modifies": {"episode": self.modifies_episode, "head": self.modifies_head},
            "touches": self.touches,
            "no_line_info": self.no_line_info,
            "unresolved": self.unresolved,
        })
    }
}

/// Resolve every change of `repository_id` against the episode world at
/// `folder`, with line information from git. The `history` job's body.
pub fn resolve(
    graph: &GraphStore,
    repository_id: &str,
    folder: &Path,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    resolve_with(
        graph,
        repository_id,
        folder,
        still_leasing,
        &mut |commit, old, path| git_hunks(folder, commit, old, path),
    )
}

/// [`resolve`] with the line provider injected: `lines(commit, old_path,
/// path)` returns the changed ranges of the new file, or `None` when the
/// change has no line information. Called only for changes that have
/// candidate symbols, so no git runs for the rest.
pub fn resolve_with(
    graph: &GraphStore,
    repository_id: &str,
    folder: &Path,
    still_leasing: &dyn Fn() -> Result<()>,
    lines: &mut dyn FnMut(&str, Option<&str>, &str) -> Result<Option<Vec<LineRange>>>,
) -> Result<Value> {
    let changes = changes(graph, repository_id)?;
    let episodes = episodes_at_commit(graph, repository_id)?;
    let heads = store_content::heads(graph, repository_id)?;
    let mut trees: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    let mut counts = Counts {
        changes: changes.len(),
        ..Counts::default()
    };
    let mut modifies: Vec<Value> = Vec::new();
    let mut touches: Vec<Value> = Vec::new();
    for change in &changes {
        let basis = if change.kind == "D" {
            None
        } else {
            match episodes.get(&change.commit) {
                Some(episode) => {
                    if !trees.contains_key(&change.commit) {
                        let tree = correlation::tree_blobs(folder, &change.commit)?;
                        let digests = correlation::blob_digests(folder, &tree)?;
                        trees.insert(change.commit.clone(), digests);
                    }
                    trees[&change.commit]
                        .get(&change.path)
                        .map(|sha| Basis {
                            episode: Some(episode.clone()),
                            sha256: sha.clone(),
                        })
                        .or_else(|| head_basis(&heads, &change.path))
                }
                None => head_basis(&heads, &change.path),
            }
        };
        let Some(basis) = basis else {
            counts.unresolved += 1;
            continue;
        };
        match basis.episode {
            Some(_) => counts.modifies_episode += 1,
            None => counts.modifies_head += 1,
        }
        modifies.push(json!({
            "commit": change.commit, "path": change.path,
            "basis": if basis.episode.is_some() { "episode" } else { "head" },
            "episode": basis.episode, "sha256": basis.sha256,
        }));
        let candidates = symbol_candidates(graph, repository_id, change, &basis.sha256)?;
        if candidates.is_empty() {
            continue;
        }
        match lines(&change.commit, change.old_path.as_deref(), &change.path)? {
            None => counts.no_line_info += 1,
            Some(ranges) => {
                for span in &candidates {
                    let hits: Vec<LineRange> = ranges
                        .iter()
                        .filter_map(|range| span.overlap(*range))
                        .collect();
                    let (Some(first), Some(last)) = (
                        hits.iter().map(|hit| hit.0).min(),
                        hits.iter().map(|hit| hit.1).max(),
                    ) else {
                        continue;
                    };
                    counts.touches += 1;
                    touches.push(json!({
                        "commit": change.commit, "path": change.path,
                        "stable_id": span.stable_id,
                        "line_start": first, "line_end": last,
                    }));
                }
            }
        }
    }
    still_leasing()?;
    write_modifies(graph, repository_id, &modifies, still_leasing)?;
    write_touches(graph, repository_id, &touches, still_leasing)?;
    Ok(counts.json())
}

fn head_basis(heads: &BTreeMap<String, String>, path: &str) -> Option<Basis> {
    heads.get(path).map(|sha| Basis {
        episode: None,
        sha256: sha.clone(),
    })
}

/// Every change of the repository, through its commit (the supernode rule:
/// the scan starts at the `:Commit` by its `repository_id`).
fn changes(graph: &GraphStore, repository_id: &str) -> Result<Vec<ChangeRow>> {
    let rows = read(
        graph,
        "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->(ch:Change {repository_id: $repo}) \
         RETURN ch.commit AS commit, ch.path AS path, ch.kind AS kind, ch.old_path AS old_path \
         ORDER BY ch.commit, ch.path",
        json!({"repo": repository_id}),
    )?;
    Ok(rows
        .iter()
        .map(|row| ChangeRow {
            commit: text(row, "commit"),
            path: text(row, "path"),
            kind: text(row, "kind"),
            old_path: row["old_path"].as_str().map(str::to_string),
        })
        .collect())
}

/// The episode that is AT_COMMIT at each commit, by commit oid.
fn episodes_at_commit(graph: &GraphStore, repository_id: &str) -> Result<BTreeMap<String, String>> {
    let rows = read(
        graph,
        "MATCH (e:Episode {repository_id: $repo})-[:AT_COMMIT]->(c:Commit {repository_id: $repo}) \
         RETURN c.oid AS oid, e.episode AS episode",
        json!({"repo": repository_id}),
    )?;
    Ok(rows
        .iter()
        .map(|row| (text(row, "oid"), text(row, "episode")))
        .collect())
}

/// The symbols a change can touch: the `:SymbolHead` rows of its path at
/// the basis sha, plus the survivors of the rename chains from the old
/// path's `:Symbol` identities (rename changes only). Sorted by stable id.
fn symbol_candidates(
    graph: &GraphStore,
    repository_id: &str,
    change: &ChangeRow,
    sha256: &str,
) -> Result<Vec<Span>> {
    let mut spans: BTreeMap<String, Span> = BTreeMap::new();
    let direct = read(
        graph,
        "MATCH (s:SymbolHead {repository_id: $repo, relpath: $path}) \
         WHERE s.sha256 = $sha AND s.stable_id <> '' \
         RETURN s.stable_id AS stable_id, s.line_start AS line_start, s.line_next AS line_next",
        json!({"repo": repository_id, "path": change.path, "sha": sha256}),
    )?;
    for row in &direct {
        let span = span_of(row);
        spans.insert(span.stable_id.clone(), span);
    }
    if let Some(old) = &change.old_path {
        let survivors = follow_moves(graph, repository_id, old)?;
        if !survivors.is_empty() {
            let rows = read(
                graph,
                "MATCH (s:SymbolHead {repository_id: $repo, relpath: $path}) \
                 WHERE s.sha256 = $sha AND s.stable_id IN $ids \
                 RETURN s.stable_id AS stable_id, s.line_start AS line_start, s.line_next AS line_next",
                json!({"repo": repository_id, "path": change.path, "sha": sha256,
                       "ids": survivors.iter().collect::<Vec<_>>()}),
            )?;
            for row in &rows {
                let span = span_of(row);
                spans.entry(span.stable_id.clone()).or_insert(span);
            }
        }
    }
    Ok(spans.into_values().collect())
}

/// The surviving symbol ids of the `:Symbol` identities under `relpath`:
/// every id a `:SymbolMove` chain from them reaches, at most [`MAX_HOPS`]
/// hops deep. A symbol that did not move has no survivors.
fn follow_moves(
    graph: &GraphStore,
    repository_id: &str,
    relpath: &str,
) -> Result<BTreeSet<String>> {
    let mut survivors = BTreeSet::new();
    let mut frontier: Vec<String> = read(
        graph,
        "MATCH (s:Symbol {repository_id: $repo, relpath: $path}) RETURN s.stable_id AS id",
        json!({"repo": repository_id, "path": relpath}),
    )?
    .iter()
    .map(|row| text(row, "id"))
    .collect();
    for _ in 0..MAX_HOPS {
        if frontier.is_empty() {
            break;
        }
        let moved = read(
            graph,
            "MATCH (m:SymbolMove {repository_id: $repo}) WHERE m.from_stable_id IN $ids \
             RETURN m.to_stable_id AS id",
            json!({"repo": repository_id, "ids": frontier}),
        )?;
        frontier = moved
            .iter()
            .map(|row| text(row, "id"))
            .filter(|id| survivors.insert(id.clone()))
            .collect();
    }
    Ok(survivors)
}

fn span_of(row: &Value) -> Span {
    Span {
        stable_id: text(row, "stable_id"),
        line_start: row["line_start"].as_u64().unwrap_or(1).max(1),
        line_next: row["line_next"].as_u64(),
    }
}

/// The changed line ranges of `commit` in the new file, from git's `-U0`
/// hunks. A pure deletion marks the line it fell after. A rename is diffed
/// across both paths, so only its content edits count.
fn git_hunks(
    folder: &Path,
    commit: &str,
    old_path: Option<&str>,
    path: &str,
) -> Result<Option<Vec<LineRange>>> {
    let mut args = vec!["show", "-U0", "-M", "--format=", "--no-color", commit, "--"];
    if let Some(old) = old_path {
        args.push(old);
    }
    args.push(path);
    let captured = reader::run_git(folder, &args, LOG_LIMIT)?;
    let stdout = reader::stdout_or_error(captured)?;
    Ok(Some(parse_hunks(&String::from_utf8_lossy(&stdout))))
}

/// The new-side ranges of a `-U0` diff: `@@ -a[,b] +c[,d] @@`.
fn parse_hunks(diff: &str) -> Vec<LineRange> {
    diff.lines()
        .filter(|line| line.starts_with("@@ "))
        .filter_map(|line| {
            let new_side = line
                .split_whitespace()
                .find(|field| field.starts_with('+'))?;
            let (start, count) = match new_side[1..].split_once(',') {
                Some((start, count)) => (start.parse::<u64>().ok()?, count.parse::<u64>().ok()?),
                None => (new_side[1..].parse::<u64>().ok()?, 1),
            };
            if count == 0 {
                let line = start.max(1);
                Some((line, line))
            } else {
                Some((start, start + count - 1))
            }
        })
        .collect()
}

fn write_modifies(
    graph: &GraphStore,
    repository_id: &str,
    rows: &[Value],
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let pass = pass(graph, repository_id, still_leasing);
    writer::batches(
        &pass,
        rows,
        "UNWIND $rows AS row \
         MATCH (ch:Change {repository_id: $repo, commit: row.commit, path: row.path})\
               -[m:MODIFIES]->(f:File {repository_id: $repo, path: row.path}) \
         SET m.basis = row.basis, m.episode = row.episode, m.sha256 = row.sha256",
        json!({}),
    )
}

fn write_touches(
    graph: &GraphStore,
    repository_id: &str,
    rows: &[Value],
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let pass = pass(graph, repository_id, still_leasing);
    writer::batches(
        &pass,
        rows,
        "UNWIND $rows AS row \
         MATCH (ch:Change {repository_id: $repo, commit: row.commit, path: row.path}) \
         MATCH (s:Symbol {repository_id: $repo, stable_id: row.stable_id}) \
         MERGE (ch)-[t:TOUCHES]->(s) \
         SET t.line_start = row.line_start, t.line_end = row.line_end",
        json!({}),
    )
}

/// The writer's batch context. A resolution writes no commit facts, so the
/// record-time stamp (`known_at`, owned by the commit pass) stays empty.
fn pass<'a>(
    graph: &'a GraphStore,
    repository_id: &'a str,
    still_leasing: &'a dyn Fn() -> Result<()>,
) -> Pass<'a> {
    Pass {
        graph,
        repository_id,
        known_at: "",
        leasing: still_leasing,
    }
}

fn read(graph: &GraphStore, cypher: &str, params: Value) -> Result<Vec<Value>> {
    graph.read(|db| store_graph::rows(db, cypher, store_graph::params(&params)))
}

fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{DAY_4, Repo, ada, history_fixture};
    use super::super::{Window, writer};
    use super::*;
    use crate::graph_projection;
    use crate::store_graph::GraphStore;

    fn project(graph: &GraphStore, repository_id: &str) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (p:Project {id: $id, workspace_id: $ws, project_key: $key})",
                    store_graph::params(
                        &json!({"id": repository_id, "ws": "ws", "key": repository_id}),
                    ),
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// The fixture, with one more commit on top that modifies `src/shared.rs`
    /// (so HEAD carries a modified file and an episode can sit there).
    fn fixture_with_touch() -> (
        tempfile::TempDir,
        Repo,
        super::super::fixture::Scenario,
        String,
    ) {
        let (dir, repo, scenario) = history_fixture();
        repo.write("src/shared.rs", "common one\nmodified\n");
        let touch = repo.commit(ada(DAY_4), "touch shared", "");
        (dir, repo, scenario, touch)
    }

    /// Ingest the repository and record its HEAD as a synced episode: heads
    /// in the content store, an `Episode` keyed by their digest, and the
    /// `AT_COMMIT` edge that `correlate_head` writes. Returns the episode id.
    fn synced_head(graph: &GraphStore, repository_id: &str, repo: &Repo) -> String {
        writer::ingest(
            graph,
            repository_id,
            repo.root(),
            &Window::default(),
            &|| Ok(()),
        )
        .unwrap();
        let tree = correlation::tree_blobs(repo.root(), "HEAD").unwrap();
        let heads = correlation::blob_digests(repo.root(), &tree).unwrap();
        let objects: Vec<Value> = heads
            .iter()
            .map(|(path, sha)| {
                let content = std::fs::read_to_string(repo.root().join(path)).unwrap();
                json!({"sha256": sha, "content": content, "bytes": content.len()})
            })
            .collect();
        // The project node `project` made: the content store MERGEs on these.
        let repository = json!({"id": repository_id, "workspace_id": "ws",
            "project_key": repository_id, "name": repository_id});
        store_content::put_objects(graph, &repository, &json!(objects)).unwrap();
        for (path, sha) in &heads {
            store_content::put_head(graph, &repository, path, sha).unwrap();
        }
        let episode = graph_projection::episode_of(&heads);
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (e:Episode {repository_id: $repo, episode: $episode, sequence: 1})",
                    store_graph::params(&json!({"repo": repository_id, "episode": episode})),
                )?;
                Ok(())
            })
            .unwrap();
        assert!(correlation::correlate_head(graph, repository_id, repo.root()).unwrap());
        episode
    }

    fn setup(
        repository_id: &str,
    ) -> (
        tempfile::TempDir,
        Repo,
        super::super::fixture::Scenario,
        String,
        GraphStore,
        String,
    ) {
        let (dir, repo, scenario, touch) = fixture_with_touch();
        let graph = GraphStore::open_in_memory().unwrap();
        project(&graph, repository_id);
        let episode = synced_head(&graph, repository_id, &repo);
        (dir, repo, scenario, touch, graph, episode)
    }

    fn query(graph: &GraphStore, cypher: &str, params: Value) -> Vec<Value> {
        read(graph, cypher, params).unwrap()
    }

    /// Resolve with no line information anywhere.
    fn no_lines(_: &str, _: Option<&str>, _: &str) -> Result<Option<Vec<LineRange>>> {
        Ok(None)
    }

    fn resolve_no_lines(graph: &GraphStore, repository_id: &str, repo: &Repo) -> Value {
        resolve_with(graph, repository_id, repo.root(), &|| Ok(()), &mut no_lines).unwrap()
    }

    fn head_sha(graph: &GraphStore, repository_id: &str, path: &str) -> String {
        store_content::head_sha(graph, repository_id, path)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_change_at_an_episode_commit_resolves_to_that_episode_file() {
        let (_dir, repo, _scenario, touch, graph, episode) = setup("repo-git");
        let counts = resolve_no_lines(&graph, "repo-git", &repo);

        let rows = query(
            &graph,
            "MATCH (:Change {repository_id: $repo, commit: $commit, path: 'src/shared.rs'})\
             -[m:MODIFIES]->(f:File {repository_id: $repo}) \
             RETURN m.basis AS basis, m.episode AS episode, m.sha256 AS sha256, f.path AS path",
            json!({"repo": "repo-git", "commit": touch}),
        );
        assert_eq!(rows.len(), 1, "one MODIFIES edge to the file");
        assert_eq!(rows[0]["basis"], "episode");
        assert_eq!(rows[0]["episode"], episode);
        assert_eq!(
            rows[0]["sha256"],
            head_sha(&graph, "repo-git", "src/shared.rs")
        );
        assert_eq!(rows[0]["path"], "src/shared.rs");
        assert_eq!(counts["modifies"]["episode"], 1);
    }

    #[test]
    fn a_change_without_an_episode_falls_back_to_the_then_current_head() {
        let (_dir, repo, scenario, _touch, graph, _episode) = setup("repo-git");
        resolve_no_lines(&graph, "repo-git", &repo);

        let rows = query(
            &graph,
            "MATCH (:Change {repository_id: $repo, commit: $commit, path: 'src/new.rs'})\
             -[m:MODIFIES]->(:File) RETURN m.basis AS basis, m.episode AS episode",
            json!({"repo": "repo-git", "commit": scenario.rename}),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["basis"], "head");
        assert!(
            rows[0]["episode"].is_null(),
            "a head basis names no episode"
        );
    }

    #[test]
    fn a_change_with_line_information_touches_the_symbols_it_overlaps() {
        let (_dir, repo, _scenario, touch, graph, _episode) = setup("repo-git");
        let sha = head_sha(&graph, "repo-git", "src/shared.rs");
        identity(&graph, "repo-git", "alpha", "src/shared.rs");
        identity(&graph, "repo-git", "beta", "src/shared.rs");
        live(
            &graph,
            "repo-git",
            "alpha",
            "src/shared.rs",
            &sha,
            1,
            Some(4),
        );
        live(&graph, "repo-git", "beta", "src/shared.rs", &sha, 4, None);

        // The touch commit changed line 2 only; no other commit has lines.
        let counts = resolve_with(
            &graph,
            "repo-git",
            repo.root(),
            &|| Ok(()),
            &mut |commit, _, _| Ok((commit == touch).then(|| vec![(2, 2)])),
        )
        .unwrap();

        let rows = query(
            &graph,
            "MATCH (ch:Change {repository_id: $repo, commit: $commit})-[t:TOUCHES]->(s:Symbol) \
             RETURN s.stable_id AS stable_id, t.line_start AS line_start, t.line_end AS line_end",
            json!({"repo": "repo-git", "commit": touch}),
        );
        assert_eq!(rows.len(), 1, "only the overlapping symbol is touched");
        assert_eq!(rows[0]["stable_id"], "alpha");
        assert_eq!(rows[0]["line_start"], 2);
        assert_eq!(rows[0]["line_end"], 2);
        assert_eq!(counts["touches"], 1);
    }

    #[test]
    fn a_change_without_line_information_gets_no_touches_and_no_error() {
        let (_dir, repo, _scenario, _touch, graph, _episode) = setup("repo-git");
        let sha = head_sha(&graph, "repo-git", "src/shared.rs");
        identity(&graph, "repo-git", "alpha", "src/shared.rs");
        live(&graph, "repo-git", "alpha", "src/shared.rs", &sha, 1, None);

        let counts = resolve_no_lines(&graph, "repo-git", &repo);

        let touches = query(
            &graph,
            "MATCH ()-[t:TOUCHES]->() RETURN count(t) AS n",
            json!({}),
        );
        assert_eq!(touches[0]["n"], 0);
        // The two changes of shared.rs (its edit and the rename into it) have
        // a candidate symbol and no line information.
        assert_eq!(counts["no_line_info"], 2);
        assert_eq!(counts["touches"], 0);
    }

    #[test]
    fn a_rename_chain_touches_the_surviving_symbol() {
        let (_dir, repo, scenario, _touch, graph, _episode) = setup("repo-git");
        let sha = head_sha(&graph, "repo-git", "src/shared.rs");
        // `alpha` lived in src/common.rs and moved with the rename, through an
        // intermediate id, to `shared_alpha` in src/shared.rs.
        identity(&graph, "repo-git", "common_alpha", "src/common.rs");
        identity(&graph, "repo-git", "mid_alpha", "src/common.rs");
        identity(&graph, "repo-git", "shared_alpha", "src/shared.rs");
        live(
            &graph,
            "repo-git",
            "shared_alpha",
            "src/shared.rs",
            &sha,
            1,
            Some(4),
        );
        movement(&graph, "repo-git", "common_alpha", "mid_alpha");
        movement(&graph, "repo-git", "mid_alpha", "shared_alpha");

        let survivors = follow_moves(&graph, "repo-git", "src/common.rs").unwrap();
        assert_eq!(
            survivors.iter().map(String::as_str).collect::<Vec<_>>(),
            ["mid_alpha", "shared_alpha"],
            "the chain reaches the survivor through the intermediate id"
        );

        resolve_with(
            &graph,
            "repo-git",
            repo.root(),
            &|| Ok(()),
            &mut |commit, _, _| Ok((commit == scenario.rename).then(|| vec![(2, 2)])),
        )
        .unwrap();

        let rows = query(
            &graph,
            "MATCH (:Change {repository_id: $repo, commit: $commit})-[t:TOUCHES]->(s:Symbol) \
             RETURN s.stable_id AS stable_id",
            json!({"repo": "repo-git", "commit": scenario.rename}),
        );
        let touched: Vec<&str> = rows
            .iter()
            .filter_map(|row| row["stable_id"].as_str())
            .collect();
        assert_eq!(
            touched,
            ["shared_alpha"],
            "the vanished identities get no edge"
        );
    }

    #[test]
    fn a_path_gone_from_every_episode_is_counted_not_an_error() {
        let (_dir, repo, scenario, _touch, graph, _episode) = setup("repo-git");

        let counts = resolve_no_lines(&graph, "repo-git", &repo);

        // Unresolved: the two files `first` added and later removed (`a.rs`,
        // `common.rs`, which the rename left), `rename`'s edit of `a.rs`, and
        // the deletion of `a.rs` in `delete`.
        assert_eq!(counts["changes"], 11);
        assert_eq!(counts["unresolved"], 4);
        assert_eq!(counts["modifies"]["episode"], 1);
        assert_eq!(counts["modifies"]["head"], 6);
        // The writer's MODIFIES stub is there, but resolution names no basis.
        let deletion = query(
            &graph,
            "MATCH (:Change {repository_id: $repo, commit: $commit, kind: 'D'})-[m:MODIFIES]->() \
             RETURN m.basis AS basis",
            json!({"repo": "repo-git", "commit": scenario.delete}),
        );
        assert_eq!(deletion.len(), 1);
        assert!(
            deletion[0]["basis"].is_null(),
            "a deletion has no version to resolve against"
        );
    }

    #[test]
    fn resolving_twice_converges_on_the_same_graph() {
        let (_dir, repo, _scenario, _touch, graph, _episode) = setup("repo-git");
        let first = resolve_no_lines(&graph, "repo-git", &repo);
        let edges = || {
            query(
                &graph,
                "MATCH ()-[r]->() WHERE type(r) IN ['MODIFIES', 'TOUCHES'] RETURN count(r) AS n",
                json!({}),
            )[0]["n"]
                .clone()
        };
        let edges_once = edges();
        let second = resolve_no_lines(&graph, "repo-git", &repo);
        assert_eq!(first, second);
        assert_eq!(edges_once, edges());
    }

    #[test]
    fn a_zero_line_hunk_marks_the_line_it_fell_after() {
        // `+11,0`: a pure deletion after line 11; `+1,3`: three added lines.
        let diff = "@@ -3 +3 @@\n-x\n+y\n@@ -10,2 +11,0 @@\n@@ -0,0 +1,3 @@\n";
        assert_eq!(parse_hunks(diff), vec![(3, 3), (11, 11), (1, 3)]);
    }

    /// A `:Symbol` identity under `relpath`.
    fn identity(graph: &GraphStore, repository_id: &str, stable_id: &str, relpath: &str) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (:Symbol {repository_id: $repo, stable_id: $id, relpath: $path, kind: 'fn', qualified_name: $id})",
                    store_graph::params(&json!({"repo": repository_id, "id": stable_id, "path": relpath})),
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// A live `:SymbolHead` row: the symbol's span in the file at `sha256`.
    fn live(
        graph: &GraphStore,
        repository_id: &str,
        stable_id: &str,
        relpath: &str,
        sha256: &str,
        line_start: u64,
        line_next: Option<u64>,
    ) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (:SymbolHead {repository_id: $repo, stable_id: $id, relpath: $path, sha256: $sha, \
                     line_start: $start, line_next: $next, kind: 'fn', qualified_name: $id, ordinal: 0, \
                     node_id: $id, revision: 'r'})",
                    store_graph::params(&json!({"repo": repository_id, "id": stable_id, "path": relpath,
                        "sha": sha256, "start": line_start, "next": line_next})),
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn movement(graph: &GraphStore, repository_id: &str, from: &str, to: &str) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (:SymbolMove {repository_id: $repo, from_stable_id: $from, to_stable_id: $to, \
                     revision: 'r', episode: 'e', created_at: 'now'})",
                    store_graph::params(&json!({"repo": repository_id, "from": from, "to": to})),
                )?;
                Ok(())
            })
            .unwrap();
    }
}
