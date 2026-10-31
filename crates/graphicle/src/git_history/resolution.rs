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
//! sha. A candidate is touched when a changed line range (git's `-U0` hunks
//! in the new file, 1-based and inclusive) overlaps its span
//! `[line_start, line_next - 1]`; with no `line_next` the span runs to the
//! end of the file. The edge carries `basis: 'span'` and the overlap. A
//! candidate with no `:Symbol` node gets no edge.
//!
//! **Rename by symbol move.** `:SymbolHead` is the current-version table, so
//! after a rename and further edits its rows for the new path describe a
//! newer sha than the change's basis. When the basis-sha candidate set is
//! empty and the change is a rename (`old_path` set), the rename's symbols
//! are followed through `:SymbolMove` chains from the `:Symbol` identities
//! under `old_path` to the chain's surviving identity. That identity gets a
//! `TOUCHES` edge with `basis: 'move'` and no line properties: the edge says
//! which symbol survived, but no span is applied, because the survivor's
//! span describes a version the change does not.
//!
//! Line information is best-effort. Hunks come from one batched `git log`
//! over the commits that have span candidates. When a change has no hunk
//! entry (a binary file, a pure rename with no content edit, a path git
//! quotes), it gets no span `TOUCHES` edge and is counted in `no_line_info`;
//! that is not an error.
//!
//! # Supernode and idempotence
//!
//! Every statement matches by key (`repository_id` with `commit`, `path`,
//! `stable_id` or `relpath`), never through `(:Project)`. The edges are MERGE
//! or SET writes, so a repeated run converges on the same graph; they are
//! written through [`super::writer`]'s batches, so the lease is checked
//! between them.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::correlation;
use super::reader::{LOG_LIMIT, ReadError};
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
    moved: usize,
    no_line_info: usize,
    unresolved: usize,
}

impl Counts {
    fn json(&self) -> Value {
        json!({
            "changes": self.changes,
            "modifies": {"episode": self.modifies_episode, "head": self.modifies_head},
            "touches": self.touches,
            "moved": self.moved,
            "no_line_info": self.no_line_info,
            "unresolved": self.unresolved,
        })
    }
}

/// The changed ranges per `(commit, new path)`, as one git pass reports
/// them. An entry with no ranges is a file with no changed lines.
pub type HunkMap = BTreeMap<(String, String), Vec<LineRange>>;

/// The symbols one change can touch: the spans to match against its
/// changed lines, and, when there are none at its basis, the line-less
/// survivors of its rename's symbol moves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Candidates {
    spans: Vec<Span>,
    moved: Vec<String>,
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
        &mut |commits| Ok(git_hunks(folder, commits)?),
    )
}

/// [`resolve`] with the hunk provider injected: `hunks(commits)` returns the
/// changed ranges of each `(commit, new path)` it covers. A change with no
/// entry has no line information. Called once, and only when some change
/// has span candidates, so no git runs for the rest.
pub fn resolve_with(
    graph: &GraphStore,
    repository_id: &str,
    folder: &Path,
    still_leasing: &dyn Fn() -> Result<()>,
    hunks: &mut dyn FnMut(&BTreeSet<String>) -> Result<HunkMap>,
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
    // Changes with span candidates, awaiting the one hunk pass.
    let mut spanned: Vec<(&ChangeRow, Vec<Span>)> = Vec::new();
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
        if !candidates.spans.is_empty() {
            spanned.push((change, candidates.spans));
        }
        for stable_id in candidates.moved {
            counts.moved += 1;
            counts.touches += 1;
            touches.push(json!({
                "commit": change.commit, "path": change.path, "stable_id": stable_id,
                "basis": "move", "line_start": null, "line_end": null,
            }));
        }
    }
    if !spanned.is_empty() {
        let commits: BTreeSet<String> = spanned
            .iter()
            .map(|(change, _)| change.commit.clone())
            .collect();
        let hunk_map = hunks(&commits)?;
        for (change, spans) in &spanned {
            let Some(ranges) = hunk_map.get(&(change.commit.clone(), change.path.clone())) else {
                counts.no_line_info += 1;
                continue;
            };
            for span in spans {
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
                    "stable_id": span.stable_id, "basis": "span",
                    "line_start": first, "line_end": last,
                }));
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

/// The symbols a change can touch. The `:SymbolHead` rows of its path at the
/// basis sha are the spans. When there are none, and the change is a rename,
/// the surviving identities of the rename's symbol moves are the `moved`
/// candidates, which get no span. Sorted by stable id.
fn symbol_candidates(
    graph: &GraphStore,
    repository_id: &str,
    change: &ChangeRow,
    sha256: &str,
) -> Result<Candidates> {
    let rows = read(
        graph,
        "MATCH (s:SymbolHead {repository_id: $repo, relpath: $path}) \
         WHERE s.sha256 = $sha AND s.stable_id <> '' \
         RETURN s.stable_id AS stable_id, s.line_start AS line_start, s.line_next AS line_next \
         ORDER BY s.stable_id",
        json!({"repo": repository_id, "path": change.path, "sha": sha256}),
    )?;
    let spans: Vec<Span> = rows.iter().filter_map(span_of).collect();
    let mut candidates = Candidates {
        spans,
        moved: Vec::new(),
    };
    if candidates.spans.is_empty() {
        if let Some(old) = &change.old_path {
            candidates.moved = follow_moves(graph, repository_id, old)?
                .into_iter()
                .collect();
        }
    }
    Ok(candidates)
}

/// The surviving symbol ids of the `:Symbol` identities under `relpath`: the
/// ids at the end of their `:SymbolMove` chains, that is, reached by a move
/// and not moved on again. A chain longer than [`MAX_HOPS`] stops there, and
/// its last reached ids count as survivors. A symbol that did not move has
/// no survivor.
fn follow_moves(
    graph: &GraphStore,
    repository_id: &str,
    relpath: &str,
) -> Result<BTreeSet<String>> {
    let mut reached = BTreeSet::new();
    let mut moved_from = BTreeSet::new();
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
             RETURN m.from_stable_id AS from, m.to_stable_id AS id",
            json!({"repo": repository_id, "ids": frontier}),
        )?;
        moved_from.extend(moved.iter().map(|row| text(row, "from")));
        frontier = moved
            .iter()
            .map(|row| text(row, "id"))
            .filter(|id| reached.insert(id.clone()))
            .collect();
    }
    Ok(reached.difference(&moved_from).cloned().collect())
}

/// A `:SymbolHead` row as a span. A row without a usable `line_start` (a
/// positive integer) or with a `line_next` that is not an integer is
/// malformed: it is skipped, never coerced to a span.
fn span_of(row: &Value) -> Option<Span> {
    let stable_id = text(row, "stable_id");
    let line_start = row["line_start"].as_u64().filter(|line| *line >= 1)?;
    let line_next = match &row["line_next"] {
        Value::Null => None,
        next => Some(next.as_u64()?),
    };
    if stable_id.is_empty() {
        return None;
    }
    Some(Span {
        stable_id,
        line_start,
        line_next,
    })
}

/// The changed ranges of every `(commit, new path)` in `commits`, from one
/// `git log --no-walk --stdin -p -U0 -M` process: the commits are fed on
/// stdin, so one git serves them all, as the reader's one `git log` and the
/// correlation's one `cat-file --batch` do. The diff prefixes are pinned, so
/// a user's diff configuration cannot change the headers read here.
fn git_hunks(folder: &Path, commits: &BTreeSet<String>) -> std::result::Result<HunkMap, ReadError> {
    if commits.is_empty() {
        return Ok(HunkMap::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args([
            "log",
            "--no-walk=unsorted",
            "--stdin",
            "-p",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--format=%H",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReadError::GitNotInstalled
            } else {
                ReadError::Io(format!("failed to run git: {error}"))
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let oids: Vec<String> = commits.iter().cloned().collect();
    // On its own thread: git reads each commit as it writes its diff, so
    // writing everything first could fill the stdout pipe and stall both.
    let writer = std::thread::spawn(move || {
        for oid in oids {
            if writeln!(stdin, "{oid}").is_err() {
                break;
            }
        }
    });
    let output = child
        .wait_with_output()
        .map_err(|error| ReadError::Io(format!("failed to read git output: {error}")))?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(ReadError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    if output.stdout.len() as u64 > LOG_LIMIT {
        return Err(ReadError::LogTooLarge { limit: LOG_LIMIT });
    }
    Ok(parse_hunk_log(&String::from_utf8_lossy(&output.stdout)))
}

/// A batched `-p -U0` log: a 40-hex line opens a commit, `diff --git` opens a
/// file, and the `+++ b/<path>` header of that file (before its first `@@`)
/// names the new path. Only the new side of each hunk counts. A file with no
/// `+++` header (binary, a pure rename, a deletion) gets no entry, and neither
/// does a path git quotes, so both count as having no line information.
fn parse_hunk_log(log: &str) -> HunkMap {
    let mut map = HunkMap::new();
    let mut commit: Option<String> = None;
    let mut path: Option<String> = None;
    let mut in_header = false;
    for line in log.lines() {
        if is_oid(line) {
            commit = Some(line.to_string());
            path = None;
            in_header = false;
        } else if line.starts_with("diff --git ") {
            path = None;
            in_header = true;
        } else if line.starts_with("@@ ") {
            in_header = false;
            if let (Some(commit), Some(path), Some(range)) = (&commit, &path, hunk_range(line)) {
                map.entry((commit.clone(), path.clone()))
                    .or_default()
                    .push(range);
            }
        } else if in_header {
            if let Some(rest) = line.strip_prefix("+++ ") {
                path = rest
                    .strip_prefix("b/")
                    .map(|new| new.trim_end_matches('\t').to_string());
                if let (Some(commit), Some(path)) = (&commit, &path) {
                    map.entry((commit.clone(), path.clone())).or_default();
                }
            }
        }
    }
    map
}

/// Whether `line` is a full commit id: 40 lowercase hex digits.
fn is_oid(line: &str) -> bool {
    line.len() == 40
        && line
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The new-side range of a hunk header `@@ -a[,b] +c[,d] @@`. A pure
/// deletion (`d` is 0) marks the line it fell after.
fn hunk_range(header: &str) -> Option<LineRange> {
    let new_side = header
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
         SET t.basis = row.basis, t.line_start = row.line_start, t.line_end = row.line_end",
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

    /// The hunk provider of a run with no line information anywhere.
    fn no_hunks(_: &BTreeSet<String>) -> Result<HunkMap> {
        Ok(HunkMap::new())
    }

    fn resolve_no_lines(graph: &GraphStore, repository_id: &str, repo: &Repo) -> Value {
        resolve_with(graph, repository_id, repo.root(), &|| Ok(()), &mut no_hunks).unwrap()
    }

    /// A hunk map from `(commit, new path, changed ranges)` entries.
    fn hunks_of(entries: &[(&str, &str, Vec<LineRange>)]) -> HunkMap {
        entries
            .iter()
            .map(|(commit, path, ranges)| ((commit.to_string(), path.to_string()), ranges.clone()))
            .collect()
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
        let hunks = hunks_of(&[(&touch, "src/shared.rs", vec![(2, 2)])]);
        let counts = resolve_with(&graph, "repo-git", repo.root(), &|| Ok(()), &mut |_| {
            Ok(hunks.clone())
        })
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
            ["shared_alpha"],
            "the chain's end is the survivor; the intermediate id moved on"
        );

        let hunks = hunks_of(&[(&scenario.rename, "src/shared.rs", vec![(2, 2)])]);
        resolve_with(&graph, "repo-git", repo.root(), &|| Ok(()), &mut |_| {
            Ok(hunks.clone())
        })
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

    /// The lagged rename: `src/shared.rs` carries a newer version than the
    /// rename's basis, so its `:SymbolHead` rows describe another sha and the
    /// basis-sha candidates are empty. The symbols moved from `src/common.rs`
    /// with `common_alpha`'s chain, when `with_move` is set.
    fn lagged_rename(with_move: bool) -> (Value, GraphStore) {
        let (_dir, repo, _scenario, _touch, graph, _episode) = setup("repo-git");
        identity(&graph, "repo-git", "common_alpha", "src/common.rs");
        identity(&graph, "repo-git", "shared_alpha", "src/shared.rs");
        live(
            &graph,
            "repo-git",
            "shared_alpha",
            "src/shared.rs",
            "a-newer-sha",
            1,
            Some(4),
        );
        if with_move {
            movement(&graph, "repo-git", "common_alpha", "shared_alpha");
        }
        let counts = resolve_no_lines(&graph, "repo-git", &repo);
        (counts, graph)
    }

    #[test]
    fn a_lagged_rename_touches_the_survivor_by_its_symbol_move() {
        let (counts, graph) = lagged_rename(true);

        let rows = query(
            &graph,
            "MATCH (ch:Change {repository_id: $repo, kind: 'R'})-[t:TOUCHES]->(s:Symbol) \
             RETURN s.stable_id AS stable_id, t.basis AS basis, \
                    t.line_start AS line_start, t.line_end AS line_end",
            json!({"repo": "repo-git"}),
        );
        assert_eq!(rows.len(), 1, "the rename's survivor is touched");
        assert_eq!(rows[0]["stable_id"], "shared_alpha");
        assert_eq!(rows[0]["basis"], "move");
        assert!(
            rows[0]["line_start"].is_null() && rows[0]["line_end"].is_null(),
            "no span is applied to a version the symbol table does not describe"
        );
        assert_eq!(counts["moved"], 1);
        assert_eq!(counts["no_line_info"], 0);
    }

    /// The mutation check for the test above: the same lagged rename without
    /// its symbol move has no candidate at all.
    #[test]
    fn a_lagged_rename_without_its_symbol_move_gets_no_touch() {
        let (counts, graph) = lagged_rename(false);

        let touches = query(
            &graph,
            "MATCH ()-[t:TOUCHES]->() RETURN count(t) AS n",
            json!({}),
        );
        assert_eq!(touches[0]["n"], 0);
        assert_eq!(counts["moved"], 0);
    }

    #[test]
    fn a_batched_log_splits_hunks_per_commit_and_path() {
        let commit = "a".repeat(40);
        let log = format!(
            "{commit}\n\
             \n\
             diff --git a/x.rs b/x.rs\n\
             index 1..2 100644\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -3 +3 @@\n\
             -x\n\
             +y\n\
             @@ -10,2 +11,0 @@\n\
             diff --git a/bin.dat b/bin.dat\n\
             Binary files a/bin.dat and b/bin.dat differ\n\
             diff --git a/gone.rs b/gone.rs\n\
             deleted file mode 100644\n\
             --- a/gone.rs\n\
             +++ /dev/null\n\
             @@ -1,2 +0,0 @@\n\
             -a\n\
             -b\n\
             diff --git a/t.rs b/t.rs\n\
             --- a/t.rs\n\
             +++ b/t.rs\n\
             @@ -1 +1 @@\n\
             -x\n\
             +++ y\n"
        );
        let map = parse_hunk_log(&log);
        let key = |path: &str| (commit.clone(), path.to_string());
        assert_eq!(map[&key("x.rs")], vec![(3, 3), (11, 11)]);
        assert!(!map.contains_key(&key("bin.dat")), "binary: no line info");
        assert!(!map.contains_key(&key("gone.rs")), "deletion: no new path");
        // A content line `+++ y` inside a hunk is content, not a file header.
        assert_eq!(map[&key("t.rs")], vec![(1, 1)]);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn a_malformed_span_row_is_skipped_not_coerced_to_line_one() {
        let ok = json!({"stable_id": "a", "line_start": 4, "line_next": null});
        assert_eq!(
            span_of(&ok),
            Some(Span {
                stable_id: "a".into(),
                line_start: 4,
                line_next: None
            })
        );
        assert_eq!(
            span_of(&json!({"stable_id": "a", "line_start": null})),
            None
        );
        assert_eq!(span_of(&json!({"stable_id": "a", "line_start": 0})), None);
        assert_eq!(
            span_of(&json!({"stable_id": "a", "line_start": 2, "line_next": "x"})),
            None
        );
    }

    #[test]
    fn hunk_headers_give_the_new_side_range() {
        // `+11,0`: a pure deletion after line 11; `+1,3`: three added lines.
        assert_eq!(hunk_range("@@ -10,2 +11,0 @@"), Some((11, 11)));
        assert_eq!(hunk_range("@@ -0,0 +1,3 @@ fn x"), Some((1, 3)));
        assert_eq!(hunk_range("@@ -3 +3 @@"), Some((3, 3)));
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
