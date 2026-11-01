//! The history writer: the git-to-graph plan §2 model written to the store
//! as MERGE-anchored, idempotent batches (§3), and the `history` job's body —
//! bounded back-fill, resume from the recorded head, and the rescan
//! semantics of §6.
//!
//! # The anchored-lookup rule (plan §2, the supernode caveat)
//!
//! No git fact is globally unique: the same sha appears in every fork, the
//! same path in every repository. Every statement this module runs therefore
//! anchors on one repository — `(:Project {id: $repo})` matched by key and
//! expanded through ONE typed edge, or a lookup scoped by the
//! `repository_id` property plus its index — and every node it writes
//! carries `repository_id`. No pattern traverses *through* the project node
//! to its far side: the project edge is a convenience for point reads, not
//! a join path, because the project is a supernode by design once every
//! commit, change, person, branch and tag hangs off it.
//!
//! # Batches and idempotence
//!
//! Writes go through [`BATCH`]-row `UNWIND` statements, one transaction per
//! batch, MERGE-anchored on natural keys (`(repository_id, oid)` for
//! commits, `(repository_id, email)` for persons, `(repository_id, commit,
//! path)` for changes). Re-running an unchanged repository writes nothing:
//! every MERGE matches, every SET lands the value the node already carries.
//! A run killed mid-way leaves only whole batches behind, and the re-run
//! converges on the same graph.
//!
//! # Resume and rescan (plan §4, §6)
//!
//! The head record is the `(:Branch {name: 'HEAD'})` node the refs pass
//! keeps pointed at the checkout's head commit. A later run queries it and
//! walks only `<recorded>..<head>` — the kontext-e pattern, chosen over
//! `--since`/`--max-count` because a revision range is exact: no clock-skew
//! gap, no double-read at the boundary. The record also carries the window
//! that wrote it: a run configured with a wider bound (or a different
//! `--since`) cannot resume onto it, because the older commits it now
//! reaches were never walked — it back-fills again, which is also what
//! fills the boundary stubs a bounded walk leaves behind. When the recorded
//! head is no longer
//! an ancestor of `HEAD` (`git merge-base --is-ancestor` fails) the history
//! was rewritten (rebase, squash, branch deletion) — not incrementally
//! detectable — so the run marks the head record `:Stale` and returns; the
//! next run sees the mark, deletes the project's git-anchored facts (their
//! changes, their `PARENT` edges, their `HAS_COMMIT` — persons and files
//! survive, they are shared) and re-ingests fully under a new `known_at`.
//!
//! Renames are resolved at load time (plan §2's deviation from kontext-e):
//! the `:Change` is written against the NEW path with `old_path` set, so no
//! churn query ever needs a rename special case.
//!
//! Merge commits carry no `:Change` rows (git lists no diff for a merge
//! without `--diff-merges`); they are identifiable by their `PARENT` degree.
use std::collections::BTreeMap;
use std::path::Path;

use super::reader;
use super::{CommitRecord, GitRef, Window};
use crate::store_graph::{self, GraphStore};
use crate::{Result, digest, now};
use serde_json::{Value, json};

/// The job kind of the history writer (plan §4).
pub const KIND: &str = "history";

/// Rows of one entity kind per transaction (plan §1's per-1,000-row batch).
pub const BATCH: usize = 1000;

/// The writer's model format, folded into the job's input hash: a schema
/// change re-ingests instead of resuming onto a stale shape.
pub const FORMAT: u32 = 1;

/// The head record's branch name: the checkout's `HEAD` as a
/// `(:Branch)-[:AT]->(:Commit)` pointer (plan §2's HEAD pointer), and the
/// resume marker the next run reads.
const HEAD_BRANCH: &str = "HEAD";

/// The `history` job's input hash (plan §4): the repository's `HEAD` sha,
/// the window configuration and the model format. `stale` is folded in so a
/// project whose history was marked stale queues a rescan run even though
/// its `HEAD` did not move.
pub fn input_hash(head: &str, window: &Window, stale: bool) -> String {
    digest(&json!({
        "kind": KIND, "format": FORMAT, "head": head, "stale": stale,
        "max_commits": window.max_commits, "since": window.since,
    }))
}

/// Whether the project's head record carries the `:Stale` mark (plan §6):
/// the signal that the next run re-ingests fully.
pub fn marked_stale(graph: &GraphStore, repository_id: &str) -> Result<bool> {
    Ok(graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (b:Branch {repository_id: $repo, name: $name}) \
                 WHERE b:Stale RETURN 1 AS stale",
                p(&json!({"repo": repository_id, "name": HEAD_BRANCH})),
            )
        })?
        .is_some())
}

/// Ingest the git history of the checkout at `dir` into `repository_id`,
/// resuming or rescanning as the recorded head dictates (see the module
/// doc). `still_leasing` is checked before every batch, so a job whose
/// lease was cancelled or taken over stops with that error. The result is
/// the `history` job's: `{"kind", "mode", "head", "ingested", "commits",
/// "changes", "persons", "branches", "tags", "known_at"}` (or `skipped`
/// with a reason for a folder that is not a git repository or has no
/// commits yet).
pub fn ingest(
    graph: &GraphStore,
    repository_id: &str,
    dir: &Path,
    window: &Window,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    if !dir.join(".git").exists() {
        // The enqueue point already skips these; a job that reaches one
        // anyway (the folder changed shape) skips too, never fails.
        return Ok(json!({"kind": KIND, "skipped": true,
            "reason": "not a git repository"}));
    }
    let stale = marked_stale(graph, repository_id)?;
    let head = reader::current_head(dir)?;
    let mut window = window.clone();
    let mode;
    if stale {
        // Plan §6: an earlier run found the recorded head unreachable and
        // marked the head record; this run forgets the project's
        // git-anchored facts and re-ingests fully under a new known_at.
        rescan(graph, repository_id, still_leasing)?;
        window.range = None;
        mode = "rescan";
        if head.is_none() {
            // The rewrite left nothing to ingest: the rescan emptied the
            // project's git facts and there is no head to record.
            let mut result = json!({"kind": KIND, "mode": mode, "head": Value::Null});
            merge(&mut result, &counts(graph, repository_id)?);
            return Ok(result);
        }
    } else {
        let Some(head) = head else {
            return Ok(json!({"kind": KIND, "skipped": true,
                "reason": "the repository has no commits"}));
        };
        if let Some(recorded) = recorded_head(graph, repository_id)? {
            if recorded.oid != head && !reader::is_ancestor(dir, &recorded.oid, &head)? {
                // Plan §6: a rewrite is not incrementally detectable. Mark
                // the head record and let the next run re-ingest; this run
                // leaves the graph exactly as it found it.
                mark_stale(graph, repository_id)?;
                return Ok(json!({"kind": KIND, "mode": "stale", "head": head,
                    "reason": "the recorded head is unreachable; \
                               the next run re-ingests fully"}));
            } else if !recorded.covers(&window) {
                // A window the record does not cover — a widened bound, a
                // different `--since`: the older commits it now reaches were
                // never walked, so back-fill again under the new bound.
                // MERGE idempotence keeps it cheap, and the boundary stubs
                // the bounded walk left gain their facts.
                mode = "backfill";
            } else if recorded.oid == head {
                // Nothing new: still refresh the refs (a branch or tag may
                // have moved while HEAD stood still); the commit walk is
                // empty by construction.
                mode = "unchanged";
                window.range = Some(format!("{head}..{head}"));
            } else {
                mode = "resume";
                window.range = Some(format!("{}..{head}", recorded.oid));
            }
        } else {
            mode = "backfill";
        }
    }
    let known_at = now();
    let history = reader::read_history_windowed(dir, &window)?;
    let commits = &history.commits;
    let pass = Pass {
        graph,
        repository_id,
        known_at: &known_at,
        leasing: still_leasing,
    };
    write_commits(&pass, commits)?;
    write_parents(&pass, commits)?;
    write_persons(&pass, commits)?;
    write_changes(&pass, commits)?;
    write_refs(
        &pass,
        &history.branches,
        &history.tags,
        &history.head,
        &window,
    )?;
    let mut result = json!({
        "kind": KIND, "mode": mode, "head": history.head,
        "known_at": known_at, "ingested": commits.len(),
    });
    merge(&mut result, &counts(graph, repository_id)?);
    Ok(result)
}

/// Fold `counts`' fields into `into`.
fn merge(into: &mut Value, counts: &Value) {
    if let (Some(target), Some(source)) = (into.as_object_mut(), counts.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// The head record as a run reads it back: the commit it points at and the
/// back-fill window that wrote it. A run configured with a different window
/// cannot resume onto the record — the older commits a wider bound reaches
/// were never walked — so it back-fills again (idempotent, and the boundary
/// stubs gain their facts).
struct HeadRecord {
    oid: String,
    max_commits: Option<i64>,
    since: Option<String>,
}

impl HeadRecord {
    /// Whether a run configured with `window` can resume onto this record:
    /// the same bound and `--since`, so everything outside the recorded
    /// walk stays outside the new one too.
    fn covers(&self, window: &Window) -> bool {
        self.max_commits == window.max_commits.map(|max| max as i64)
            && self.since.as_deref() == window.since.as_deref()
    }
}

/// The recorded head record. `None` before the first ingest (or after a
/// rescan of an emptied repository).
fn recorded_head(graph: &GraphStore, repository_id: &str) -> Result<Option<HeadRecord>> {
    Ok(graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (b:Branch {repository_id: $repo, name: $name})-[:AT]->\
                 (c:Commit {repository_id: $repo}) \
                 RETURN c.oid AS oid, b.max_commits AS max_commits, b.since AS since",
                p(&json!({"repo": repository_id, "name": HEAD_BRANCH})),
            )
        })?
        .map(|row| HeadRecord {
            oid: store_graph::s(&row, "oid").to_string(),
            max_commits: row["max_commits"].as_i64(),
            since: row["since"].as_str().map(str::to_string),
        }))
}

/// Mark the head record `:Stale` (plan §6): the label sits on the record
/// itself, so it is repository-scoped — it walks with the project in a
/// deletion and never outlives it as an orphan marker.
fn mark_stale(graph: &GraphStore, repository_id: &str) -> Result<()> {
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (b:Branch {repository_id: $repo, name: $name}) \
             SET b:Stale, b.stale_since = $now",
            p(&json!({"repo": repository_id, "name": HEAD_BRANCH, "now": now()})),
        )?;
        Ok(())
    })
}

/// Forget the project's git-anchored facts before a full re-ingest (plan
/// §6): the changes, the commits (with their `PARENT`, `HAS_COMMIT`,
/// `CONTAINS_CHANGE`, `AUTHORED`/`COMMITTED` and ref-pointing edges) and
/// the refs. Persons and files survive — they are shared with the content
/// store and with other runs' knowledge of the people.
fn rescan(
    graph: &GraphStore,
    repository_id: &str,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<()> {
    still_leasing()?;
    graph.transaction(|db| {
        for label in ["Change", "Commit", "Branch", "Tag"] {
            store_graph::write(
                db,
                &format!("MATCH (n:{label} {{repository_id: $repo}}) DETACH DELETE n"),
                p(&json!({"repo": repository_id})),
            )?;
        }
        Ok(())
    })
}

/// One ingest run's shared context: the store, the repository being
/// written, the run's record-time stamp and the lease check between
/// batches. The write passes take it instead of repeating the four.
pub(super) struct Pass<'a> {
    pub(super) graph: &'a GraphStore,
    pub(super) repository_id: &'a str,
    pub(super) known_at: &'a str,
    pub(super) leasing: &'a dyn Fn() -> Result<()>,
}

impl<'a> Pass<'a> {
    /// A pass that writes no commit facts, so the record-time stamp
    /// (`known_at`, owned by the commit pass) stays empty: the one
    /// constructor for the coupling and resolution passes.
    pub(super) fn derived(
        graph: &'a GraphStore,
        repository_id: &'a str,
        leasing: &'a dyn Fn() -> Result<()>,
    ) -> Pass<'a> {
        Pass {
            graph,
            repository_id,
            known_at: "",
            leasing,
        }
    }
}

/// Run `query` once per [`BATCH`]-row chunk of `rows`, one transaction per
/// chunk, the lease checked before each. Every write pass funnels through
/// here so the batch bound and the lease check live in exactly one place.
pub(super) fn batches(pass: &Pass, rows: &[Value], query: &str, extra: Value) -> Result<()> {
    for chunk in rows.chunks(BATCH) {
        (pass.leasing)()?;
        let mut params = json!({"repo": pass.repository_id, "rows": chunk});
        merge(&mut params, &extra);
        pass.graph.transaction(|db| {
            store_graph::write(db, query, p(&params))?;
            Ok(())
        })?;
    }
    Ok(())
}

/// The commit pass: commit nodes with their facts, `known_at` on first
/// sight, and the project's `HAS_COMMIT` edges.
fn write_commits(pass: &Pass, commits: &[CommitRecord]) -> Result<()> {
    let rows: Vec<Value> = commits
        .iter()
        .map(|commit| {
            json!({
                "oid": commit.oid, "tree_oid": commit.tree_oid,
                "subject": commit.subject, "message": message(commit),
                "author_date": commit.author_date.to_rfc3339(),
                "commit_date": commit.committer_date.to_rfc3339(),
            })
        })
        .collect();
    batches(
        pass,
        &rows,
        "UNWIND $rows AS row \
         MATCH (p:Project {id: $repo}) \
         MERGE (c:Commit {repository_id: $repo, oid: row.oid}) \
           ON CREATE SET c.known_at = $known_at \
         SET c.tree_oid = row.tree_oid, c.subject = row.subject, \
             c.message = row.message, c.author_date = row.author_date, \
             c.commit_date = row.commit_date \
         MERGE (p)-[:HAS_COMMIT]->(c)",
        json!({"known_at": pass.known_at}),
    )
}

/// A commit's full message: subject and body as git shows them, joined the
/// way `git log --format=%B` renders them.
fn message(commit: &CommitRecord) -> String {
    if commit.body.is_empty() {
        commit.subject.clone()
    } else {
        format!("{}\n\n{}", commit.subject, commit.body)
    }
}

/// The parent pass: `PARENT {position}` edges in git's order (first parent
/// first; more than one parent is a merge). A parent outside the emitted
/// set — the history-simplification caveat of the reader's contract, or the
/// boundary of a resume window — is written as a bare commit stub the next
/// wider walk fills in.
fn write_parents(pass: &Pass, commits: &[CommitRecord]) -> Result<()> {
    let oids: Vec<Value> = commits
        .iter()
        .flat_map(|commit| &commit.parents)
        .map(|oid| json!(oid.as_str()))
        .collect();
    batches(
        pass,
        &oids,
        "UNWIND $rows AS oid \
         MERGE (c:Commit {repository_id: $repo, oid: oid}) \
           ON CREATE SET c.known_at = $known_at",
        json!({"known_at": pass.known_at}),
    )?;
    let edges: Vec<Value> = commits
        .iter()
        .flat_map(|commit| {
            commit.parents.iter().enumerate().map(move |(position, parent)| {
                json!({"commit": commit.oid, "parent": parent, "position": position})
            })
        })
        .collect();
    batches(
        pass,
        &edges,
        "UNWIND $rows AS row \
         MATCH (c:Commit {repository_id: $repo, oid: row.commit}) \
         MATCH (p:Commit {repository_id: $repo, oid: row.parent}) \
         MERGE (c)-[:PARENT {position: row.position}]->(p)",
        json!({}),
    )
}

/// One person as the person pass resolves them: keyed by lowercased email
/// (plan §1's dedup rule), named by their newest commit, every other name
/// they used collected in `aliases`. `authored` and `committed` carry the
/// `(commit oid, date)` pairs of each side.
struct Person {
    name: String,
    aliases: Vec<String>,
    authored: Vec<(String, String)>,
    committed: Vec<(String, String)>,
}

/// The person pass: `(:Person)` nodes with `AUTHORED {at}` and
/// `COMMITTED {at}` edges. Names are resolved in Rust over the whole run —
/// a person spans batches, and their `aliases` must merge with what earlier
/// runs already collected rather than clobber it.
fn write_persons(pass: &Pass, commits: &[CommitRecord]) -> Result<()> {
    if commits.is_empty() {
        return Ok(());
    }
    // What the store already knows, so a resume run adds to it instead of
    // replacing it.
    let known: BTreeMap<String, (String, Vec<String>)> = pass
        .graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (a:Person {repository_id: $repo}) \
                 RETURN a.email AS email, a.name AS name, a.aliases AS aliases",
                p(&json!({"repo": pass.repository_id})),
            )
        })?
        .into_iter()
        .map(|row| {
            let aliases = row["aliases"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|name| name.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (
                store_graph::s(&row, "email").to_string(),
                (store_graph::s(&row, "name").to_string(), aliases),
            )
        })
        .collect();
    let mut people: BTreeMap<String, Person> = BTreeMap::new();
    for commit in commits {
        // The log is newest first: the first name seen for an email is the
        // newest one, every later (older) name an alias.
        for side in [
            (
                &commit.author_name,
                &commit.author_email,
                &commit.author_date,
                false,
            ),
            (
                &commit.committer_name,
                &commit.committer_email,
                &commit.committer_date,
                true,
            ),
        ] {
            let (name, email, at, committer) = side;
            let person = people
                .entry(email.to_lowercase())
                .or_insert_with(|| Person {
                    name: String::new(),
                    aliases: Vec::new(),
                    authored: Vec::new(),
                    committed: Vec::new(),
                });
            if person.name.is_empty() {
                person.name = name.clone();
            } else if name != &person.name && !person.aliases.contains(name) {
                person.aliases.push(name.clone());
            }
            let pair = (commit.oid.clone(), at.to_rfc3339());
            if committer {
                person.committed.push(pair);
            } else {
                person.authored.push(pair);
            }
        }
    }
    let rows: Vec<Value> = people
        .iter()
        .map(|(email, person)| {
            // Merge this run's names into what the store had: the run's
            // newest name wins (its commits are the newest), the rest join
            // the aliases, and the winning name leaves the aliases.
            let (old_name, mut aliases) = known
                .get(email)
                .cloned()
                .unwrap_or_else(|| (person.name.clone(), Vec::new()));
            let name = if person.name.is_empty() {
                old_name
            } else {
                person.name.clone()
            };
            for seen in &person.aliases {
                if !aliases.contains(seen) {
                    aliases.push(seen.clone());
                }
            }
            aliases.retain(|alias| alias != &name);
            json!({"email": email, "name": name, "aliases": aliases})
        })
        .collect();
    batches(
        pass,
        &rows,
        "UNWIND $rows AS row \
         MERGE (a:Person {repository_id: $repo, email: row.email}) \
           ON CREATE SET a.known_at = $known_at \
         SET a.name = row.name, a.aliases = row.aliases",
        json!({"known_at": pass.known_at}),
    )?;
    for (people_keys, query) in [
        (
            false,
            "UNWIND $rows AS row \
             MATCH (a:Person {repository_id: $repo, email: row.email}) \
             MATCH (c:Commit {repository_id: $repo, oid: row.oid}) \
             MERGE (a)-[:AUTHORED {at: row.at}]->(c)",
        ),
        (
            true,
            "UNWIND $rows AS row \
             MATCH (a:Person {repository_id: $repo, email: row.email}) \
             MATCH (c:Commit {repository_id: $repo, oid: row.oid}) \
             MERGE (a)-[:COMMITTED {at: row.at}]->(c)",
        ),
    ] {
        let rows: Vec<Value> = people
            .iter()
            .flat_map(|(email, person)| {
                let pairs = if people_keys {
                    &person.committed
                } else {
                    &person.authored
                };
                pairs
                    .iter()
                    .map(move |(oid, at)| json!({"email": email, "oid": oid, "at": at}))
            })
            .collect();
        batches(pass, &rows, query, json!({}))?;
    }
    Ok(())
}

/// The change pass: one `(:Change)` per (commit, path) — renames against
/// the NEW path with `old_path` (plan §2's load-time rename resolution) —
/// each `CONTAINS_CHANGE` from its commit and `MODIFIES` to the
/// content store's `(:File)` (a stub when the file is not projected yet;
/// the same key the content store MERGEs fills it).
fn write_changes(pass: &Pass, commits: &[CommitRecord]) -> Result<()> {
    let rows: Vec<Value> = commits
        .iter()
        .flat_map(|commit| {
            commit.changes.iter().map(move |change| {
                json!({
                    "commit": commit.oid, "path": change.path,
                    "kind": change.kind.letter().to_string(),
                    "old_path": change.old_path, "similarity": change.similarity,
                })
            })
        })
        .collect();
    batches(
        pass,
        &rows,
        "UNWIND $rows AS row \
         MATCH (c:Commit {repository_id: $repo, oid: row.commit}) \
         MERGE (ch:Change {repository_id: $repo, commit: row.commit, path: row.path}) \
         SET ch.kind = row.kind, ch.old_path = row.old_path, \
             ch.similarity = row.similarity \
         MERGE (c)-[:CONTAINS_CHANGE]->(ch) \
         MERGE (f:File {repository_id: $repo, path: row.path}) \
         MERGE (ch)-[:MODIFIES]->(f)",
        json!({}),
    )
}

/// The refs pass: branches and tags as own nodes with their commit pointers
/// (`(:Branch)-[:AT]->`, `(:Tag)-[:TAGS]->`), the `HEAD` record the next
/// run resumes from — carrying the window that wrote it, so a later run
/// knows whether it can resume or must re-walk under a changed bound — and
/// the pruning of refs git no longer lists. Ref targets outside the
/// emitted set are stubbed like parents.
fn write_refs(
    pass: &Pass,
    branches: &[GitRef],
    tags: &[GitRef],
    head: &str,
    window: &Window,
) -> Result<()> {
    let head_ref = GitRef {
        name: HEAD_BRANCH.to_string(),
        refname: HEAD_BRANCH.to_string(),
        target: head.to_string(),
    };
    let branches: Vec<&GitRef> = branches.iter().chain([&head_ref]).collect();
    let branch_rows: Vec<Value> = branches
        .iter()
        .map(|r| json!({"name": r.name, "ref": r.refname, "target": r.target}))
        .collect();
    let tag_rows: Vec<Value> = tags
        .iter()
        .map(|r| json!({"name": r.name, "target": r.target}))
        .collect();
    let targets: Vec<Value> = branches
        .iter()
        .copied()
        .chain(tags.iter())
        .map(|r| json!(r.target.as_str()))
        .collect();
    let branch_names: Vec<&str> = branches.iter().map(|r| r.name.as_str()).collect();
    let tag_names: Vec<&str> = tags.iter().map(|r| r.name.as_str()).collect();

    // Commit stubs for ref targets the walk did not emit.
    batches(
        pass,
        &targets,
        "UNWIND $rows AS oid \
         MERGE (c:Commit {repository_id: $repo, oid: oid}) \
           ON CREATE SET c.known_at = $known_at",
        json!({"known_at": pass.known_at}),
    )?;
    batches(
        pass,
        &branch_rows,
        "UNWIND $rows AS row \
         MATCH (p:Project {id: $repo}) \
         MERGE (b:Branch {repository_id: $repo, name: row.name}) \
         SET b.ref = row.ref \
         MERGE (p)-[:HAS_BRANCH]->(b)",
        json!({}),
    )?;
    batches(
        pass,
        &branch_rows,
        "UNWIND $rows AS row \
         MATCH (b:Branch {repository_id: $repo, name: row.name}) \
         MATCH (c:Commit {repository_id: $repo, oid: row.target}) \
         MERGE (b)-[:AT]->(c)",
        json!({}),
    )?;
    // The head record carries the window that wrote it: the resume
    // decision of the next run reads it back ([`HeadRecord::covers`]).
    (pass.leasing)()?;
    pass.graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (b:Branch {repository_id: $repo, name: $name}) \
             SET b.max_commits = $max, b.since = $since",
            p(&json!({"repo": pass.repository_id, "name": HEAD_BRANCH,
                "max": window.max_commits, "since": window.since})),
        )?;
        Ok(())
    })?;
    batches(
        pass,
        &tag_rows,
        "UNWIND $rows AS row \
         MATCH (p:Project {id: $repo}) \
         MERGE (t:Tag {repository_id: $repo, name: row.name}) \
         MERGE (p)-[:HAS_TAG]->(t)",
        json!({}),
    )?;
    batches(
        pass,
        &tag_rows,
        "UNWIND $rows AS row \
         MATCH (t:Tag {repository_id: $repo, name: row.name}) \
         MATCH (c:Commit {repository_id: $repo, oid: row.target}) \
         MERGE (t)-[:TAGS]->(c)",
        json!({}),
    )?;
    // Refs git no longer lists go, so the model tracks deletions too. The
    // property-scoped match keeps the prune off the project supernode.
    for (rows, label) in [(branch_names, "Branch"), (tag_names, "Tag")] {
        (pass.leasing)()?;
        pass.graph.transaction(|db| {
            store_graph::write(
                db,
                &format!(
                    "MATCH (n:{label} {{repository_id: $repo}}) \
                     WHERE NOT n.name IN $names DETACH DELETE n"
                ),
                p(&json!({"repo": pass.repository_id, "names": rows})),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Query parameters from a JSON object (the module's one-liner for
/// [`store_graph::params`]).
fn p(value: &Value) -> store_graph::Params {
    store_graph::params(value)
}

/// The repository's counts, for the job result.
fn counts(graph: &GraphStore, repository_id: &str) -> Result<Value> {
    Ok(graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (c:Commit {repository_id: $repo}) WITH count(c) AS commits \
                 MATCH (ch:Change {repository_id: $repo}) \
                 WITH commits, count(ch) AS changes \
                 MATCH (a:Person {repository_id: $repo}) \
                 WITH commits, changes, count(a) AS persons \
                 MATCH (b:Branch {repository_id: $repo}) \
                 WITH commits, changes, persons, count(b) AS branches \
                 MATCH (t:Tag {repository_id: $repo}) \
                 RETURN commits, changes, persons, branches, count(t) AS tags",
                p(&json!({"repo": repository_id})),
            )
        })?
        .unwrap_or_else(|| json!({})))
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{self, history_fixture};
    use super::super::reader;
    use super::*;
    use crate::store_graph::GraphStore;
    use serde_json::json;

    /// The fixture's two authors' authored-commit counts (see
    /// `history_fixture`: Ada writes first, main, merge, octopus and delete;
    /// Bob writes the rename, feat, o1 and o2).
    const ADA_AUTHORED: i64 = 5;
    const BOB_AUTHORED: i64 = 4;
    /// The distinct paths the fixture touches.
    const FILES: i64 = 8;

    /// One repository's project node in a fresh in-memory store.
    fn graph_for(repository_id: &str) -> GraphStore {
        let graph = GraphStore::open_in_memory().unwrap();
        project(&graph, repository_id);
        graph
    }

    fn project(graph: &GraphStore, repository_id: &str) {
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (p:Project {id: $id, workspace_id: $ws, project_key: $key})",
                    p(&json!({"id": repository_id, "ws": "ws", "key": repository_id})),
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// Ingest `dir` into `repository_id` with the default window.
    fn run(graph: &GraphStore, repository_id: &str, dir: &Path) -> Value {
        super::ingest(graph, repository_id, dir, &Window::default(), &|| Ok(())).unwrap()
    }

    /// The first row's `n` of a count query, with full parameters.
    fn found(graph: &GraphStore, cypher: &str, params: Value) -> i64 {
        graph
            .read(|db| store_graph::one(db, cypher, p(&params)))
            .unwrap()
            .and_then(|row| row["n"].as_i64())
            .unwrap_or_default()
    }

    /// `found` over the whole store.
    fn total(graph: &GraphStore, cypher: &str) -> i64 {
        found(graph, cypher, json!({}))
    }

    /// `found` scoped to one repository.
    fn of(graph: &GraphStore, repository_id: &str, cypher: &str) -> i64 {
        let query = cypher.replace(
            "$repo_scope",
            &format!("{{repository_id: '{repository_id}'}}"),
        );
        found(graph, &query, json!({"repo": repository_id}))
    }

    /// The store as one comparable value: every node's labels and
    /// properties, every relationship's type and properties.
    fn state(graph: &GraphStore) -> Value {
        graph
            .read(|db| {
                let nodes = store_graph::rows(
                    db,
                    "MATCH (n) RETURN labels(n) AS labels, properties(n) AS props",
                    p(&json!({})),
                )?;
                let rels = store_graph::rows(
                    db,
                    "MATCH ()-[r]->() RETURN type(r) AS type, properties(r) AS props",
                    p(&json!({})),
                )?;
                Ok(json!({"nodes": nodes, "rels": rels}))
            })
            .unwrap()
    }

    /// Node and edge counts per label and per relationship type, the
    /// interrupted-run comparison.
    fn tally(graph: &GraphStore) -> Value {
        graph
            .read(|db| {
                let nodes = store_graph::rows(
                    db,
                    "MATCH (n) UNWIND labels(n) AS label \
                     RETURN label, count(*) AS n ORDER BY label",
                    p(&json!({})),
                )?;
                let rels = store_graph::rows(
                    db,
                    "MATCH ()-[r]->() RETURN type(r) AS type, count(*) AS n ORDER BY type",
                    p(&json!({})),
                )?;
                Ok(json!({"nodes": nodes, "rels": rels}))
            })
            .unwrap()
    }

    /// `git rev-list --count HEAD` of a fixture repository.
    fn repo_count(dir: &Path) -> i64 {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn backfill_writes_the_full_model_anchored_on_the_project() {
        let (dir, repo, scenario) = history_fixture();
        let graph = graph_for("repo-git");
        let result = run(&graph, "repo-git", repo.root());

        assert_eq!(result["mode"], json!("backfill"), "{result:#}");
        assert_eq!(result["head"], json!(repo.rev("HEAD")));
        let expected = repo_count(repo.root());
        assert_eq!(result["commits"], json!(expected), "{result:#}");
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ),
            expected
        );
        assert_eq!(
            total(
                &graph,
                "MATCH (:Project {id: 'repo-git'})-[:HAS_COMMIT]->() RETURN count(*) AS n"
            ),
            expected,
            "one HAS_COMMIT edge per commit, from the project anchor"
        );

        // The first commit: every §2 property — immutable facts plus the
        // record-time known_at.
        let commit = graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (c:Commit {repository_id: $repo, oid: $oid}) RETURN c",
                    p(&json!({"repo": "repo-git", "oid": scenario.first})),
                )
            })
            .unwrap()
            .unwrap()["c"]
            .clone();
        assert_eq!(commit["subject"], json!("one: add the shared module"));
        assert_eq!(
            commit["message"],
            json!("one: add the shared module\n\nbody line 1\n\nbody line 2")
        );
        assert_eq!(commit["tree_oid"].as_str().map(str::len), Some(40));
        assert_eq!(commit["author_date"], json!("2026-01-01T00:00:00+00:00"));
        assert_eq!(commit["commit_date"], json!("2026-01-01T00:00:00+00:00"));
        assert!(commit["known_at"].as_str().is_some_and(|at| !at.is_empty()));
        assert_eq!(commit["repository_id"], json!("repo-git"));

        // Ordered parents: the merge keeps first parent first, the octopus
        // keeps three in git's order.
        let parents = |oid: &str| {
            graph
                .read(|db| {
                    store_graph::rows(
                        db,
                        "MATCH (:Commit {repository_id: $repo, oid: $oid})-[r:PARENT]->(p) \
                     RETURN r.position AS pos, p.oid AS oid ORDER BY pos",
                        p(&json!({"repo": "repo-git", "oid": oid})),
                    )
                })
                .unwrap()
                .into_iter()
                .map(|row| {
                    (
                        row["pos"].as_i64().unwrap_or_default(),
                        row["oid"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parents(&scenario.merge),
            vec![
                (0, repo.rev(&format!("{}^1", scenario.merge))),
                (1, scenario.feat.clone()),
            ]
        );
        assert_eq!(
            parents(&scenario.octopus),
            vec![
                (0, scenario.merge.clone()),
                (1, repo.rev("o1")),
                (2, repo.rev("o2")),
            ]
        );

        // Two authors, deduped by lowercased email, named by their newest
        // commit, one AUTHORED edge per commit they wrote, at their date.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (a:Person $repo_scope) RETURN count(a) AS n"
            ),
            2
        );
        let ada = graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (a:Person {repository_id: $repo, email: $email}) \
                     RETURN a.name AS name, a.aliases AS aliases",
                    p(&json!({"repo": "repo-git", "email": fixture::ADA_EMAIL})),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(ada["name"], json!(fixture::ADA));
        assert_eq!(ada["aliases"], json!([]));
        assert_eq!(
            found(
                &graph,
                "MATCH (:Person {repository_id: $repo, email: $email})-[r:AUTHORED]->(:Commit) \
                 RETURN count(r) AS n",
                json!({"repo": "repo-git", "email": fixture::ADA_EMAIL})
            ),
            ADA_AUTHORED
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Person {repository_id: $repo, email: $email})-[r:COMMITTED]->(:Commit) \
                 RETURN count(r) AS n",
                json!({"repo": "repo-git", "email": fixture::BOB_EMAIL})
            ),
            BOB_AUTHORED
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Person {repository_id: $repo, email: $email})-[r:AUTHORED {at: $at}]->(c) \
                 WHERE c.oid = $oid RETURN count(r) AS n",
                json!({"repo": "repo-git", "email": fixture::BOB_EMAIL,
                       "at": "2026-01-02T00:00:00+00:00", "oid": scenario.rename})
            ),
            1,
            "the rename is Bob's, at his author date, on the AUTHORED edge"
        );

        // The rename: one Change against the NEW path, old_path and
        // similarity on it, modifying the file at the new path.
        let rename_change = graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                     RETURN ch.kind AS kind, ch.old_path AS old_path, ch.similarity AS similarity",
                    p(&json!({"repo": "repo-git", "oid": scenario.rename, "path": "src/shared.rs"})),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(rename_change["kind"], json!("R"));
        assert_eq!(rename_change["old_path"], json!("src/common.rs"));
        assert_eq!(rename_change["similarity"], json!(100));
        assert_eq!(
            found(
                &graph,
                "MATCH (:Change {repository_id: $repo, path: $path})-[:MODIFIES]->\
                 (:File {repository_id: $repo, path: $path}) RETURN count(*) AS n",
                json!({"repo": "repo-git", "path": "src/shared.rs"})
            ),
            1
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Commit {repository_id: $repo, oid: $oid})-[:CONTAINS_CHANGE]->\
                 (:Change {repository_id: $repo, path: $path}) RETURN count(*) AS n",
                json!({"repo": "repo-git", "oid": scenario.rename, "path": "src/shared.rs"})
            ),
            1
        );
        // The old path appears only as the first commit's add: the rename
        // commit itself has no Change at it — the old path lives in
        // `old_path`, load-time rename resolution.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (ch:Change $repo_scope) WHERE ch.path = 'src/common.rs' RETURN count(ch) AS n"
            ),
            1
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (ch:Change {repository_id: $repo, commit: $oid}) \
                 WHERE ch.path = 'src/common.rs' RETURN count(ch) AS n",
                json!({"repo": "repo-git", "oid": scenario.rename})
            ),
            0
        );

        // Branches and tags as own nodes, with the HEAD record among them.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (b:Branch $repo_scope) RETURN count(b) AS n"
            ),
            5,
            "feat, main, o1, o2 and the HEAD record"
        );
        assert_eq!(
            total(
                &graph,
                "MATCH (:Project {id: 'repo-git'})-[:HAS_BRANCH]->(:Branch) RETURN count(*) AS n"
            ),
            5
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Branch {repository_id: $repo, name: 'feat'})-[:AT]->(c) \
                 WHERE c.oid = $oid RETURN count(*) AS n",
                json!({"repo": "repo-git", "oid": scenario.feat})
            ),
            1
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Branch {repository_id: $repo, name: 'HEAD'})-[:AT]->(c) \
                 WHERE c.oid = $oid RETURN count(*) AS n",
                json!({"repo": "repo-git", "oid": repo.rev("HEAD")})
            ),
            1,
            "the head record points at the checkout's head"
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Project {id: $repo})-[:HAS_TAG]->(:Tag {repository_id: $repo, name: 'v1'})-[:TAGS]->(c) \
                 WHERE c.oid = $oid RETURN count(*) AS n",
                json!({"repo": "repo-git", "oid": scenario.rename})
            ),
            1,
            "an annotated tag peels to its commit"
        );

        // Every touched path has its File (a stub here; the content store
        // MERGEs the same key when it projects).
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (f:File $repo_scope) RETURN count(f) AS n"
            ),
            FILES
        );
        drop(dir);
    }

    #[test]
    fn re_running_an_unchanged_repository_writes_nothing() {
        let (dir, repo, _scenario) = history_fixture();
        let graph = graph_for("repo-git");
        run(&graph, "repo-git", repo.root());
        let before = state(&graph);

        let again = run(&graph, "repo-git", repo.root());
        assert_eq!(again["mode"], json!("unchanged"), "{again:#}");
        assert_eq!(again["ingested"], json!(0));
        assert_eq!(state(&graph), before, "the second run must write nothing");
        drop(dir);
    }

    #[test]
    fn an_interrupted_run_resumed_produces_identical_counts() {
        let (dir, repo, _scenario) = history_fixture();
        // The interrupted run: killed after the commit and parent batches
        // of its first pass (whole transactions — the kill points of a
        // per-batch writer).
        let interrupted = graph_for("repo-a");
        let history = reader::read_history(repo.root()).unwrap();
        let leasing = || Ok(());
        let partial = Pass {
            graph: &interrupted,
            repository_id: "repo-a",
            known_at: "t1",
            leasing: &leasing,
        };
        write_commits(&partial, &history.commits).unwrap();
        write_parents(&partial, &history.commits).unwrap();

        // The re-run of the same history.
        let result = run(&interrupted, "repo-a", repo.root());
        assert_eq!(result["mode"], json!("backfill"), "{result:#}");

        // A clean single run of the same history, for comparison.
        let clean = graph_for("repo-b");
        run(&clean, "repo-b", repo.root());

        assert_eq!(
            tally(&interrupted),
            tally(&clean),
            "no duplicates, no loss: the same node and edge counts per label and type"
        );
        drop(dir);
    }

    #[test]
    fn resume_walks_only_the_new_commits_from_the_recorded_head() {
        let (dir, repo, _scenario) = history_fixture();
        let graph = graph_for("repo-git");
        run(&graph, "repo-git", repo.root());
        let before = of(
            &graph,
            "repo-git",
            "MATCH (c:Commit $repo_scope) RETURN count(c) AS n",
        );

        repo.write("src/later.rs", "later\n");
        repo.commit(fixture::ada(fixture::DAY_1), "later: add", "");
        let new_head = repo.rev("HEAD");

        let result = run(&graph, "repo-git", repo.root());
        assert_eq!(result["mode"], json!("resume"), "{result:#}");
        assert_eq!(result["ingested"], json!(1));
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ),
            before + 1
        );
        // The new commit's parent edge reaches the recorded head, and the
        // head record moved with it.
        assert_eq!(
            found(
                &graph,
                "MATCH (:Commit {repository_id: $repo, oid: $new})-[:PARENT]->\
                 (:Commit {repository_id: $repo, oid: $old}) RETURN count(*) AS n",
                json!({"repo": "repo-git", "new": new_head, "old": repo.rev("HEAD~1")})
            ),
            1
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Branch {repository_id: $repo, name: 'HEAD'})-[:AT]->(c) \
                 WHERE c.oid = $oid RETURN count(*) AS n",
                json!({"repo": "repo-git", "oid": new_head})
            ),
            1
        );
        // No duplicate persons for the old commits.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (a:Person $repo_scope) RETURN count(a) AS n"
            ),
            2
        );
        assert!(!marked_stale(&graph, "repo-git").unwrap());
        drop(dir);
    }

    #[test]
    fn a_rewritten_history_marks_stale_and_the_next_run_rescans() {
        let (dir, repo, _scenario) = history_fixture();
        let graph = graph_for("repo-git");
        run(&graph, "repo-git", repo.root());
        let old_tip = repo.rev("HEAD");
        let commits = of(
            &graph,
            "repo-git",
            "MATCH (c:Commit $repo_scope) RETURN count(c) AS n",
        );
        let changes = of(
            &graph,
            "repo-git",
            "MATCH (ch:Change $repo_scope) RETURN count(ch) AS n",
        );

        // A history rewrite: amending the tip leaves the recorded head
        // unreachable (its object survives, its ref does not).
        let new_tip = repo.amend(fixture::ada(fixture::DAY_1), "remove a, rewritten");
        assert_ne!(old_tip, new_tip);

        // The detecting run: marks the head record stale and touches
        // nothing else.
        let detected = run(&graph, "repo-git", repo.root());
        assert_eq!(detected["mode"], json!("stale"), "{detected:#}");
        assert!(marked_stale(&graph, "repo-git").unwrap());
        assert_eq!(
            total(
                &graph,
                "MATCH (b:Branch {repository_id: 'repo-git', name: 'HEAD'}) WHERE b:Stale RETURN count(b) AS n"
            ),
            1,
            "the mark sits on the head record itself"
        );
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ),
            commits
        );
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (ch:Change $repo_scope) RETURN count(ch) AS n"
            ),
            changes
        );

        // The next run: a full re-ingest under a new known_at.
        let rescanned = run(&graph, "repo-git", repo.root());
        assert_eq!(rescanned["mode"], json!("rescan"), "{rescanned:#}");
        assert_eq!(rescanned["head"], json!(new_tip));
        assert!(
            !marked_stale(&graph, "repo-git").unwrap(),
            "the rescan clears the mark"
        );
        // The vanished tip's anchored facts are gone; the rewritten tip is
        // in, with the same history length (an amend rewrites one commit).
        assert_eq!(
            found(
                &graph,
                "MATCH (c:Commit {repository_id: $repo, oid: $oid}) RETURN count(c) AS n",
                json!({"repo": "repo-git", "oid": old_tip})
            ),
            0,
            "the vanished tip is gone with its anchored facts"
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (c:Commit {repository_id: $repo, oid: $oid}) RETURN count(c) AS n",
                json!({"repo": "repo-git", "oid": new_tip})
            ),
            1
        );
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ),
            commits
        );
        assert_eq!(
            total(
                &graph,
                "MATCH (:Change {repository_id: 'repo-git'})-[:MODIFIES]->(:File {repository_id: 'repo-git'}) RETURN count(*) AS n"
            ),
            of(
                &graph,
                "repo-git",
                "MATCH (ch:Change $repo_scope) RETURN count(ch) AS n"
            ),
            "every surviving change still modifies its file"
        );
        // Persons and files survive the rescan: they are shared.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (a:Person $repo_scope) RETURN count(a) AS n"
            ),
            2
        );
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (f:File $repo_scope) RETURN count(f) AS n"
            ),
            FILES
        );
        drop(dir);
    }

    #[test]
    fn forked_repositories_with_overlapping_shas_never_cross_link() {
        let (dir, repo, scenario) = history_fixture();
        // The fork: a clone of the fixture plus one commit of its own.
        let fork_dir = tempfile::tempdir().unwrap();
        let fork_path = fork_dir.path().join("fork");
        let fork = fixture::clone(repo.root(), &fork_path);
        fork.write("src/fork-only.rs", "fork\n");
        fork.commit(fixture::bob(fixture::DAY_1), "fork: own history", "");

        let graph = graph_for("repo-a");
        project(&graph, "repo-b");
        run(&graph, "repo-a", repo.root());
        run(&graph, "repo-b", &fork_path);

        // The same sha on both sides: two commit nodes, one per repository.
        assert_eq!(
            found(
                &graph,
                "MATCH (c:Commit {oid: $oid}) RETURN count(c) AS n",
                json!({"oid": scenario.first})
            ),
            2,
            "the shared sha exists once per repository, never merged"
        );
        // The anchored lookup finds each repository's copy, and only it.
        assert_eq!(
            found(
                &graph,
                "MATCH (:Project {id: $repo})-[:HAS_COMMIT]->(c:Commit {oid: $oid}) \
                 WHERE c.repository_id = $repo RETURN count(*) AS n",
                json!({"repo": "repo-a", "oid": scenario.first})
            ),
            1
        );
        assert_eq!(
            found(
                &graph,
                "MATCH (:Project {id: $repo})-[:HAS_COMMIT]->(c:Commit {oid: $oid}) \
                 WHERE c.repository_id = $repo RETURN count(*) AS n",
                json!({"repo": "repo-b", "oid": scenario.first})
            ),
            1
        );
        // No edge crosses repositories — the fork shape's whole point.
        for edge in [
            "PARENT",
            "CONTAINS_CHANGE",
            "MODIFIES",
            "AUTHORED",
            "COMMITTED",
            "AT",
            "TAGS",
        ] {
            assert_eq!(
                total(
                    &graph,
                    &format!(
                        "MATCH (a)-[:{edge}]->(b) \
                     WHERE (a.repository_id = 'repo-a' AND b.repository_id = 'repo-b') \
                        OR (a.repository_id = 'repo-b' AND b.repository_id = 'repo-a') \
                     RETURN count(*) AS n"
                    )
                ),
                0,
                "a {edge} edge crossed repositories"
            );
        }
        // Nor does a project edge reach the other repository's commits.
        for (project, other) in [("repo-a", "repo-b"), ("repo-b", "repo-a")] {
            assert_eq!(
                found(
                    &graph,
                    "MATCH (:Project {id: $repo})-[:HAS_COMMIT]->(c:Commit) \
                     WHERE c.repository_id = $other RETURN count(*) AS n",
                    json!({"repo": project, "other": other})
                ),
                0
            );
        }
        // The fork's own commit and its parent edge into the shared history.
        assert_eq!(
            found(
                &graph,
                "MATCH (:Commit {repository_id: $repo, oid: $oid})-[:PARENT]->\
                 (:Commit {repository_id: $repo, oid: $parent}) RETURN count(*) AS n",
                json!({"repo": "repo-b", "oid": fork.rev("HEAD"), "parent": scenario.delete})
            ),
            1
        );
        assert_eq!(
            of(
                &graph,
                "repo-b",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ),
            repo_count(repo.root()) + 1
        );
        drop(dir);
    }

    #[test]
    fn the_backfill_window_bounds_the_walk() {
        let (dir, repo, _scenario) = history_fixture();
        let graph = graph_for("repo-git");
        let window = Window {
            max_commits: Some(3),
            ..Window::default()
        };
        let result = super::ingest(&graph, "repo-git", repo.root(), &window, &|| Ok(())).unwrap();
        assert_eq!(result["mode"], json!("backfill"), "{result:#}");
        // Three commits came back with their facts; their parents outside
        // the window exist as boundary stubs.
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) WHERE c.subject IS NOT NULL RETURN count(c) AS n"
            ),
            3
        );
        assert!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) RETURN count(c) AS n"
            ) > 3,
            "the window's boundary parents are stubbed so the DAG stays connected"
        );
        drop(dir);
    }

    #[test]
    fn a_wider_window_back_fills_again_and_fills_the_boundary_stubs() {
        let (dir, repo, _scenario) = history_fixture();
        let graph = graph_for("repo-git");
        let bounded = Window {
            max_commits: Some(3),
            ..Window::default()
        };
        super::ingest(&graph, "repo-git", repo.root(), &bounded, &|| Ok(())).unwrap();
        // The o1 branch's tip is one of the bounded walk's boundary stubs:
        // present, but with no facts of its own.
        let o1_tip = repo.rev("o1");
        let stub = |graph: &GraphStore| {
            graph
                .read(|db| {
                    store_graph::one(
                        db,
                        "MATCH (c:Commit {repository_id: $repo, oid: $oid}) \
                     RETURN c.subject AS subject, c.author_date AS author_date",
                        p(&json!({"repo": "repo-git", "oid": o1_tip})),
                    )
                })
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            stub(&graph)["subject"],
            Value::Null,
            "a stub carries no facts"
        );

        // A wider window cannot resume onto the bounded record: it
        // back-fills again, and the stubs gain their facts.
        let widened = run(&graph, "repo-git", repo.root());
        assert_eq!(widened["mode"], json!("backfill"), "{widened:#}");
        let filled = stub(&graph);
        assert_eq!(filled["subject"], json!("o1: add o1"));
        assert!(
            filled["author_date"]
                .as_str()
                .is_some_and(|at| !at.is_empty()),
            "the stub gained its dates too"
        );
        assert_eq!(
            of(
                &graph,
                "repo-git",
                "MATCH (c:Commit $repo_scope) WHERE c.subject IS NOT NULL RETURN count(c) AS n"
            ),
            repo_count(repo.root())
        );
        // The record now covers the default window: nothing more to walk.
        let settled = run(&graph, "repo-git", repo.root());
        assert_eq!(settled["mode"], json!("unchanged"), "{settled:#}");
        drop(dir);
    }

    #[test]
    fn a_folder_that_is_not_a_git_repository_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plain.txt"), "not a repository\n").unwrap();
        let graph = graph_for("repo-git");
        let result = run(&graph, "repo-git", dir.path());
        assert_eq!(result["skipped"], json!(true), "{result:#}");
    }

    #[test]
    fn the_input_hash_keys_on_head_window_and_staleness() {
        let window = Window::default();
        let head = "a".repeat(40);
        let plain = input_hash(&head, &window, false);
        assert_eq!(plain, input_hash(&head, &window, false));
        assert_ne!(
            plain,
            input_hash(&"b".repeat(40), &window, false),
            "a new HEAD is a new job"
        );
        assert_ne!(
            plain,
            input_hash(&head, &window, true),
            "a stale history re-queues even though HEAD did not move"
        );
        let bounded = Window {
            max_commits: Some(100),
            ..Window::default()
        };
        assert_ne!(plain, input_hash(&head, &bounded, false));
    }
}
