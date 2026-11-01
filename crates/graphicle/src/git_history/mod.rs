//! The commit history of a git checkout, read into plain structs: the output
//! contract of the git log reader and the input of the history writer
//! (git-to-graph plan §2 and §3).
//!
//! [`reader::read_history`] runs git and returns a [`History`]. Nothing in
//! the reader touches the graph store: [`writer`] maps these structs onto
//! `(:Commit)`, `(:Change)`, `(:Branch)`, `(:Tag)`, `(:Person)` and `(:File)`,
//! and [`writer::ingest`] is the `history` job's body (resume and rescan,
//! plan §4 and §6).
//!
//! Field contract:
//! - [`History`]: `head` (the `HEAD` commit oid), `branches` and `tags`
//!   (from `git for-each-ref`), `commits` (git log's default order, newest
//!   first, merges included).
//! - [`CommitRecord`]: `oid` (`%H`), `tree_oid` (`%T`), `parents` (`%P`,
//!   first parent first; more than one parent is a merge), `author_name` and
//!   `author_email` (`%aN`, `%aE`, mailmap-resolved), `author_date` (`%aI`),
//!   `committer_name`, `committer_email`, `committer_date` (`%cN`, `%cE`,
//!   `%cI`), `subject` (`%s`), `body` (`%b` without its trailing newlines),
//!   `changes` (`--name-status -M`). A merge's `changes` are empty: git lists
//!   no diff for a merge without `--diff-merges`.
//! - [`Change`]: one path the commit touched. `path` is the path after the
//!   change (the new path for a rename), `old_path` is set for renames only,
//!   and `similarity` (0 to 100) is set for renames only. Paths are relative
//!   to the repository root, and a non-UTF-8 path is decoded lossily, as
//!   cypherlite's reader does.
//! - [`GitRef`]: `name` is the short name (`main`, `v1`), `refname` the full
//!   one (`refs/heads/main`), and `target` the commit oid, peeled through an
//!   annotated tag.
//! - Dates are [`chrono::DateTime<FixedOffset>`] parsed from git's strict
//!   ISO 8601 (`%aI`/`%cI`). Values git never emits are rejected.

use chrono::{DateTime, FixedOffset};

pub mod correlation;
pub mod coupling;
pub mod queries;
pub mod reader;
pub mod resolution;
pub mod writer;

#[cfg(test)]
pub(crate) mod fixture;
#[cfg(test)]
mod tests;

pub use reader::{ParseError, ReadError, parse_log, read_history};
pub use writer::KIND;

/// How a history read is bounded (plan §4): the first run's back-fill bound
/// (`max_commits`, `since`) and the resume range `<recorded head>..<head>`
/// that walks only the commits the store does not have yet. The default is
/// generous on purpose — everything, capped only by the reader's log-byte
/// budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Window {
    /// `--max-count`: the back-fill bound. `None` reads all of it.
    pub max_commits: Option<usize>,
    /// `--since` as git takes it (a date). `None` reads all of it.
    pub since: Option<String>,
    /// A revision range (`<old>..<new>`) for a resume run, set by
    /// [`writer::ingest`] — never by configuration.
    pub range: Option<String>,
}

/// What a commit did to one path, git's name-status letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

impl ChangeKind {
    /// git's name-status letter: `A`, `M`, `D`, `R` or `T`.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::TypeChanged => 'T',
        }
    }
}

/// One path a commit touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    /// The path after the change; for a rename, the new path.
    pub path: String,
    /// The path before the change, for a rename only.
    pub old_path: Option<String>,
    /// The rename similarity, 0 to 100, for a rename only.
    pub similarity: Option<u8>,
}

/// One commit as `git log` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: String,
    pub tree_oid: String,
    /// Parent oids in git's order; the first parent comes first.
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_date: DateTime<FixedOffset>,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: DateTime<FixedOffset>,
    pub subject: String,
    /// The message body, without its trailing newlines.
    pub body: String,
    pub changes: Vec<Change>,
}

impl CommitRecord {
    /// Whether the commit has more than one parent.
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// A branch or tag and the commit it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    /// The short name, without `refs/heads/` or `refs/tags/`.
    pub name: String,
    /// The full ref name.
    pub refname: String,
    /// The commit oid, peeled through an annotated tag.
    pub target: String,
}

/// A checkout's history, read by [`reader::read_history`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// The `HEAD` commit oid.
    pub head: String,
    pub branches: Vec<GitRef>,
    pub tags: Vec<GitRef>,
    /// The commits `git log -- .` reports, newest first. The pathspec applies
    /// git's history simplification, so a commit that leaves the tree as it
    /// was (an empty commit, or a merge identical to one parent) is absent,
    /// and a parent oid may name a commit outside this list.
    pub commits: Vec<CommitRecord>,
}
