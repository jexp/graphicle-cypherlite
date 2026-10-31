//! Episode correlation (git-to-graph plan, the episode ↔ commit edge): an
//! episode whose head set is exactly the tree of a recorded commit gets
//! `(:Episode)-[:AT_COMMIT]->(:Commit)`. A head set that is a working-tree
//! snapshot (uncommitted changes, untracked files, or content the git tree
//! does not hold) gets no edge.
//!
//! The head set is `path → sha256(content)` ([`crate::store_content::heads`]),
//! so a commit's tree is compared by hashing the blobs it names. The episode
//! id is itself a digest of the head set ([`crate::graph_projection::episode_of`]),
//! so the comparison is exact: equal path sets with equal digests.
//!
//! Both sides are matched by key (`repository_id` with `episode` or `oid`),
//! never through the `(:Project)` anchor (the supernode rule of this module).
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use super::reader::{self, LOG_LIMIT, ReadError};
use crate::store_graph::{self, GraphStore};
use crate::{Result, store_content};

/// Correlate the episode of the repository's current head set with `HEAD`
/// of the checkout at `folder`. Returns whether the episode is now
/// correlated with `HEAD`: `true` when the edge exists after the call, which
/// includes a repeated call (the edge is MERGE'd, so it never duplicates).
///
/// Nothing is written unless the checkout is clean (`git status` lists no
/// change, untracked files included), the `HEAD` commit and the episode are
/// both recorded, and the head set equals the `HEAD` tree exactly.
pub fn correlate_head(graph: &GraphStore, repository_id: &str, folder: &Path) -> Result<bool> {
    let Some(head) = reader::current_head(folder)? else {
        return Ok(false);
    };
    let heads = store_content::heads(graph, repository_id)?;
    let episode = crate::graph_projection::episode_of(&heads);
    if edge_exists(graph, repository_id, &episode, &head)? {
        return Ok(true);
    }
    if !episode_and_commit_recorded(graph, repository_id, &episode, &head)? {
        return Ok(false);
    }
    if !is_clean(folder)? {
        return Ok(false);
    }
    let tree = tree_blobs(folder, &head)?;
    // Cheap first: the same paths, before any blob is read.
    if tree.len() != heads.len() || !tree.keys().eq(heads.keys()) {
        return Ok(false);
    }
    let digests = blob_digests(folder, &tree)?;
    if digests != heads {
        return Ok(false);
    }
    write_edge(graph, repository_id, &episode, &head)?;
    Ok(true)
}

fn edge_exists(graph: &GraphStore, repository_id: &str, episode: &str, oid: &str) -> Result<bool> {
    graph.read(|db| {
        Ok(store_graph::one(
            db,
            "MATCH (e:Episode {repository_id: $repo, episode: $episode})\
             -[:AT_COMMIT]->(c:Commit {repository_id: $repo, oid: $oid}) \
             RETURN c.oid AS oid",
            store_graph::params(&json!({"repo": repository_id, "episode": episode, "oid": oid})),
        )?
        .is_some())
    })
}

fn episode_and_commit_recorded(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    oid: &str,
) -> Result<bool> {
    graph.read(|db| {
        let params = store_graph::params(&json!({"repo": repository_id, "episode": episode, "oid": oid}));
        let episode_row = store_graph::one(
            db,
            "MATCH (e:Episode {repository_id: $repo, episode: $episode}) RETURN e.episode AS episode",
            params.clone(),
        )?;
        let commit_row = store_graph::one(
            db,
            "MATCH (c:Commit {repository_id: $repo, oid: $oid}) RETURN c.oid AS oid",
            params,
        )?;
        Ok(episode_row.is_some() && commit_row.is_some())
    })
}

fn write_edge(graph: &GraphStore, repository_id: &str, episode: &str, oid: &str) -> Result<()> {
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (e:Episode {repository_id: $repo, episode: $episode}) \
             MATCH (c:Commit {repository_id: $repo, oid: $oid}) \
             MERGE (e)-[:AT_COMMIT]->(c)",
            store_graph::params(&json!({"repo": repository_id, "episode": episode, "oid": oid})),
        )?;
        Ok(())
    })
}

/// `git status` lists nothing: the working tree is the `HEAD` tree plus
/// nothing a sync would read (ignored files are not listed, as the sync's
/// own walk skips them).
fn is_clean(folder: &Path) -> std::result::Result<bool, ReadError> {
    let captured = reader::run_git(
        folder,
        &["status", "--porcelain", "--untracked-files=all"],
        LOG_LIMIT,
    )?;
    Ok(reader::stdout_or_error(captured)?.is_empty())
}

/// The blobs of `commit`'s tree: path → blob oid, submodules and trees left out.
fn tree_blobs(
    folder: &Path,
    commit: &str,
) -> std::result::Result<BTreeMap<String, String>, ReadError> {
    let captured = reader::run_git(
        folder,
        &["ls-tree", "-r", "-z", "--full-tree", commit],
        LOG_LIMIT,
    )?;
    let stdout = reader::stdout_or_error(captured)?;
    let mut blobs = BTreeMap::new();
    for entry in stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        // `<mode> SP <type> SP <oid> TAB <path>`
        let text = String::from_utf8_lossy(entry);
        let Some((meta, path)) = text.split_once('\t') else {
            return Err(ReadError::Malformed(format!("ls-tree entry: {text}")));
        };
        let mut fields = meta.split(' ');
        let (_mode, kind, oid) = (fields.next(), fields.next(), fields.next());
        match (kind, oid) {
            (Some("blob"), Some(oid)) => {
                blobs.insert(path.to_string(), oid.to_string());
            }
            (Some(_), Some(_)) => {}
            _ => return Err(ReadError::Malformed(format!("ls-tree entry: {text}"))),
        }
    }
    Ok(blobs)
}

/// The sha256 of each blob's content, keyed by path, as the sync records a
/// file's head. One `git cat-file --batch` process streams every blob.
fn blob_digests(
    folder: &Path,
    tree: &BTreeMap<String, String>,
) -> std::result::Result<BTreeMap<String, String>, ReadError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReadError::GitNotInstalled
            } else {
                ReadError::Io(format!("failed to run git: {error}"))
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let oids: Vec<String> = tree.values().cloned().collect();
    // Written on its own thread: git answers each request as it reads it,
    // so writing everything first could fill the stdout pipe and stall both.
    let writer = std::thread::spawn(move || {
        for oid in oids {
            if writeln!(stdin, "{oid}").is_err() {
                break;
            }
        }
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let mut answers = BufReader::new(stdout);
    let mut digests = BTreeMap::new();
    let mut result = Ok(());
    for path in tree.keys() {
        match read_blob(&mut answers) {
            Ok(content) => {
                digests.insert(path.clone(), graphicle_memory::text::sha256(&content));
            }
            Err(error) => {
                result = Err(error);
                break;
            }
        }
    }
    drop(answers);
    let _ = child.kill();
    let _ = child.wait();
    let _ = writer.join();
    result?;
    Ok(digests)
}

/// One `--batch` answer: `<oid> <type> <size>` then the content and a newline.
fn read_blob(reader: &mut impl BufRead) -> std::result::Result<Vec<u8>, ReadError> {
    let mut header = String::new();
    reader
        .read_line(&mut header)
        .map_err(|error| ReadError::Io(format!("failed to read git output: {error}")))?;
    let mut fields = header.split_whitespace();
    let (_oid, kind, size) = (fields.next(), fields.next(), fields.next());
    let size: usize = match (kind, size) {
        (Some("blob"), Some(size)) => size
            .parse()
            .map_err(|_| ReadError::Malformed(format!("cat-file header: {header}")))?,
        _ => {
            return Err(ReadError::Malformed(format!(
                "cat-file header: {}",
                header.trim_end()
            )));
        }
    };
    let mut content = vec![0u8; size];
    reader
        .read_exact(&mut content)
        .map_err(|error| ReadError::Io(format!("failed to read git output: {error}")))?;
    let mut newline = [0u8; 1];
    reader
        .read_exact(&mut newline)
        .map_err(|error| ReadError::Io(format!("failed to read git output: {error}")))?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::history_fixture;
    use super::*;

    /// A commit's tree digests are the sha256 of each file's content, the
    /// same digests a sync records as the head set, for any recorded commit
    /// and not only `HEAD`.
    #[test]
    fn a_commit_tree_digests_its_blobs_by_path() {
        let (_dir, repo, scenario) = history_fixture();
        let tree = tree_blobs(repo.root(), &scenario.first).unwrap();
        assert_eq!(
            tree.keys().cloned().collect::<Vec<_>>(),
            ["src/a.rs", "src/common.rs"]
        );
        let digests = blob_digests(repo.root(), &tree).unwrap();
        assert_eq!(
            digests["src/a.rs"],
            graphicle_memory::text::sha256(b"a one\n")
        );
        assert_eq!(
            digests["src/common.rs"],
            graphicle_memory::text::sha256(b"common one\n")
        );
    }
}
