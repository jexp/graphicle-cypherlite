//! Git fixture repositories for the CLI's `lab git-to-graph` tests
//! (ADR-0376).
//!
//! Every repository this module creates runs git with an empty
//! `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_NOSYSTEM=1`, so a developer's
//! `~/.gitconfig` (commit hooks, signing keys, mailmap, default branch) can
//! never change a fixture. Commits always carry a fixed author, committer and
//! date, and signing is disabled on the command line. The empty global config
//! is a `tempfile` scratch file removed when the [`Repo`] drops.
//!
//! The fixture kits ([`history_fixture`], [`joined_fixture`]) and the
//! [`iso_days_ago`] clock helper live here, once, so the unit and end-to-end
//! layers build the same repositories instead of re-deriving them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use cypherlite_core::temporal::{datetime_from_epoch, format_pattern};

/// The author and committer name every fixture commit uses.
pub const AUTHOR_NAME: &str = "Ada Lovelace";

/// The author and committer email every fixture commit uses.
pub const AUTHOR_EMAIL: &str = "ada@example.com";

/// The author and committer date every fixture commit uses unless a caller
/// passes its own (`%aI` renders this as `2026-01-02T03:04:05Z`).
pub const FIXTURE_DATE: &str = "2026-01-02T03:04:05+00:00";

/// Fixture commit dates, one day apart, so a `--since` can fall between two
/// commits (git filters on the committer date, which [`Repo::commit_as`]
/// sets).
pub const JAN_1: &str = "2026-01-01T00:00:00+00:00";
pub const JAN_2: &str = "2026-01-02T00:00:00+00:00";
pub const JAN_3: &str = "2026-01-03T00:00:00+00:00";
pub const JAN_4: &str = "2026-01-04T00:00:00+00:00";

/// One git identity: the name, email and date its author or committer
/// environment variables carry.
#[derive(Debug, Clone, Copy)]
struct Identity<'a> {
    name: &'a str,
    email: &'a str,
    date: &'a str,
}

/// The fixed fixture identity (ADR-0376) every command runs as unless a
/// caller passes its own — for one role or both.
fn fixture_identity() -> Identity<'static> {
    Identity {
        name: AUTHOR_NAME,
        email: AUTHOR_EMAIL,
        date: FIXTURE_DATE,
    }
}

/// A fixture git repository with its own isolated git configuration.
pub struct Repo {
    /// The work-tree root; every fixture path is resolved against it.
    root: PathBuf,
    /// An empty file used as `GIT_CONFIG_GLOBAL` for every git invocation.
    /// `NamedTempFile` removes it when the `Repo` drops.
    config: tempfile::NamedTempFile,
}

impl Repo {
    /// Create a repository at `root` on branch `main`.
    ///
    /// Missing directories are created. The repository uses an empty global
    /// config, no system config and no commit signing (ADR-0376).
    pub fn init(root: &Path) -> Self {
        std::fs::create_dir_all(root).expect("create the fixture repository directory");
        let repo = Self {
            root: root.to_path_buf(),
            config: tempfile::NamedTempFile::new().expect("create the empty git config"),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// Run one git command in the repository and return its trimmed stdout.
    ///
    /// The command runs with the isolated config and the fixed fixture
    /// identity and date. A non-zero exit or a non-UTF-8 stdout is a broken
    /// fixture and panics with git's stderr.
    pub fn git(&self, args: &[&str]) -> String {
        let fixture = fixture_identity();
        self.run(args, fixture, fixture, &[])
    }

    /// Run one git command in the repository with `stdin` piped to it, and
    /// return its trimmed stdout.
    ///
    /// For plumbing that reads its request from stdin (`git mktree`); the
    /// command runs with the same isolated config, fixed identity and date as
    /// [`Repo::git`].
    pub fn git_stdin(&self, args: &[&str], stdin: &[u8]) -> String {
        let fixture = fixture_identity();
        self.run(args, fixture, fixture, stdin)
    }

    /// Write `contents` to `relative` under the repository root, creating
    /// missing parent directories.
    pub fn write(&self, relative: &str, contents: &[u8]) {
        crate::write_file(&self.root.join(relative), contents);
    }

    /// Delete the file at `relative`.
    pub fn remove(&self, relative: &str) {
        std::fs::remove_file(self.root.join(relative)).expect("remove the fixture file");
    }

    /// Commit every working-tree change as the default fixture author and
    /// date, and return the new commit's sha.
    pub fn commit(&self, subject: &str) -> String {
        self.commit_as(AUTHOR_NAME, AUTHOR_EMAIL, FIXTURE_DATE, subject)
    }

    /// The add→commit→head sequence every commit helper shares: stage every
    /// working-tree change, commit it with the given author and committer
    /// identities, and return the new commit's sha.
    fn commit_with(&self, author: Identity<'_>, committer: Identity<'_>, subject: &str) -> String {
        self.git(&["add", "-A"]);
        self.run(
            &["commit", "-q", "--allow-empty", "-m", subject],
            author,
            committer,
            &[],
        );
        self.head()
    }

    /// Commit every working-tree change with an explicit identity and date,
    /// and return the new commit's sha.
    pub fn commit_as(&self, name: &str, email: &str, date: &str, subject: &str) -> String {
        let identity = Identity { name, email, date };
        self.commit_with(identity, identity, subject)
    }

    /// Commit every working-tree change with an explicit author identity
    /// and author date while the committer keeps the fixture identity and
    /// date, and return the new commit's sha.
    ///
    /// This is the rebase/cherry-pick shape (ADR-0376): the author and
    /// committer differ, so git log's committer-date order can disagree
    /// with the author date — the disagreement the greatest-author-date
    /// person-name rule resolves.
    pub fn commit_as_author(
        &self,
        name: &str,
        email: &str,
        author_date: &str,
        subject: &str,
    ) -> String {
        let author = Identity {
            name,
            email,
            date: author_date,
        };
        self.commit_with(author, fixture_identity(), subject)
    }

    /// Merge `branch` with a `--no-ff` merge commit authored by the default
    /// fixture identity and date, and return the merge commit's sha.
    pub fn merge(&self, branch: &str, subject: &str) -> String {
        self.git(&["merge", "--no-ff", "-q", "-m", subject, branch]);
        self.head()
    }

    /// The current `HEAD` commit sha.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// The one git invocation: isolated config, the author and committer
    /// identities, and `stdin` piped to the command — an empty write is a
    /// no-op and the handle's drop closes the pipe, so every command sees
    /// EOF after its input bytes, the same EOF a no-input command always
    /// saw.
    fn run(
        &self,
        args: &[&str],
        author: Identity<'_>,
        committer: Identity<'_>,
        stdin: &[u8],
    ) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.config.path())
            .env("GIT_AUTHOR_NAME", author.name)
            .env("GIT_AUTHOR_EMAIL", author.email)
            .env("GIT_AUTHOR_DATE", author.date)
            .env("GIT_COMMITTER_NAME", committer.name)
            .env("GIT_COMMITTER_EMAIL", committer.email)
            .env("GIT_COMMITTER_DATE", committer.date)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("run git for the fixture repository");
        child
            .stdin
            .take()
            .expect("a child spawned with piped stdin has a stdin handle")
            .write_all(stdin)
            .expect("write the fixture git command's stdin");
        let output = child
            .wait_with_output()
            .expect("run git for the fixture repository");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        String::from_utf8(output.stdout)
            .expect("git fixture output is UTF-8")
            .trim()
            .to_string()
    }
}

/// The canonical git-to-graph history fixture (ADR-0376): three non-merge
/// commits by two authors — Ada under two spellings of one email — touching
/// four files.
///
/// * `one: add the shared module` — `src/common.rs`, `src/a.rs`
/// * `fix: repair the shared module` — `src/common.rs`, `src/b.rs`
/// * `three: extend a` — `src/a.rs`, `src/extra.rs`
///
/// One pair per commit, so an import must produce exactly 3 commits,
/// 2 people, 4 files, 6 `TOUCHED` and 3 `CO_CHANGED` edges, each pair with
/// `count` 1.
pub fn history_fixture() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().expect("history fixture tempdir");
    let repo = Repo::init(dir.path());
    repo.write("src/common.rs", b"common one");
    repo.write("src/a.rs", b"a one");
    repo.commit_as(
        AUTHOR_NAME,
        AUTHOR_EMAIL,
        JAN_1,
        "one: add the shared module",
    );
    repo.write("src/common.rs", b"common two");
    repo.write("src/b.rs", b"b one");
    repo.commit_as(
        "Bob",
        "bob@example.com",
        JAN_2,
        "fix: repair the shared module",
    );
    repo.write("src/a.rs", b"a two");
    repo.write("src/extra.rs", b"extra");
    repo.commit_as("Ada Newest", "Ada@Example.COM", JAN_3, "three: extend a");
    (dir, repo)
}

/// The canonical fs-to-graph × git-to-graph join fixture: two tracked files
/// still on disk with text content, one file deleted in history, and one
/// visible untracked file on disk.
///
/// Importing DIR with both `lab` actions, in either order, must leave exactly
/// four `:File` nodes: the two tracked files (content, incoming `TOUCHED`),
/// the deleted-in-history file (git-only, no content) and the untracked file
/// (fs-only, no `TOUCHED`).
pub fn joined_fixture() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().expect("joined fixture tempdir");
    let repo = Repo::init(dir.path());
    repo.write("tracked.txt", b"tracked one");
    repo.write("src/nested.txt", b"nested");
    repo.write("deleted.txt", b"gone");
    repo.commit("add files");
    repo.write("tracked.txt", b"tracked two");
    repo.remove("deleted.txt");
    repo.commit("modify and delete");
    repo.write("untracked.txt", b"untracked");
    (dir, repo)
}

/// ISO-8601 noon-UTC date `days` days before `now_secs` (Unix seconds), for
/// git fixture dates. The catalog's time questions compare against
/// `datetime()` at test time, so a fixture's dates must be relative to the
/// clock, not pinned constants.
///
/// The calendar conversion and rendering are the engine's
/// (`cypherlite_core::temporal`), so fixture dates and the importer's
/// timestamp parser cannot drift into two implementations.
fn iso_days_ago_at(now_secs: u64, days: u64) -> String {
    let day_start = now_secs.saturating_sub(days * 86_400) / 86_400 * 86_400;
    let noon = i64::try_from(day_start + 12 * 3_600)
        .expect("a Unix second count for a representable date fits i64");
    let datetime = datetime_from_epoch(noon, 0).expect("noon UTC is a representable datetime");
    format_pattern(&datetime, "yyyy-MM-dd'T'HH:mm:ssxxx")
        .expect("the fixture timestamp pattern is supported")
}

/// ISO-8601 noon-UTC date `days` days ago, for fixture dates that must sit
/// inside a relative window (e.g. the catalog's `$days` questions).
pub fn iso_days_ago(days: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_secs();
    iso_days_ago_at(now, days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_carry_the_fixed_identity_and_date() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path());
        repo.write("a.txt", b"a");
        repo.commit("first");

        let log = repo.git(&["log", "-1", "--format=%aN|%aE|%aI"]);
        assert_eq!(log, "Ada Lovelace|ada@example.com|2026-01-02T03:04:05Z");
    }

    #[test]
    fn iso_days_ago_renders_known_dates() {
        // 2026-01-01T00:00:00Z is 1_767_225_600 Unix seconds.
        assert_eq!(
            iso_days_ago_at(1_767_225_600, 0),
            "2026-01-01T12:00:00+00:00"
        );
        assert_eq!(
            iso_days_ago_at(1_767_225_600, 1),
            "2025-12-31T12:00:00+00:00"
        );
        assert_eq!(iso_days_ago_at(0, 0), "1970-01-01T12:00:00+00:00");
        // 400 days back crosses a leap day, so the engine's calendar
        // arithmetic is actually exercised.
        assert_eq!(
            iso_days_ago_at(1_767_225_600, 400),
            "2024-11-27T12:00:00+00:00"
        );
    }

    #[test]
    fn commit_as_overrides_the_fixture_identity() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path());
        repo.write("a.txt", b"a");
        repo.commit_as(
            "Grace Hopper",
            "grace@example.com",
            "2026-02-03T04:05:06+00:00",
            "second",
        );

        let log = repo.git(&["log", "-1", "--format=%aN|%aE|%aI"]);
        assert_eq!(log, "Grace Hopper|grace@example.com|2026-02-03T04:05:06Z");
    }

    #[test]
    fn commit_as_author_overrides_only_the_author() {
        // The rebase/cherry-pick shape (ADR-0376): the author identity and
        // date are the caller's while the committer keeps the fixture's, so
        // committer-date log order can disagree with the author date.
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path());
        repo.write("a.txt", b"a");
        repo.commit_as_author("Jan First", "ada@example.com", JAN_1, "rebased");

        let log = repo.git(&["log", "-1", "--format=%aN|%aI|%cN|%cI"]);
        assert_eq!(
            log,
            "Jan First|2026-01-01T00:00:00Z|Ada Lovelace|2026-01-02T03:04:05Z"
        );
    }

    #[test]
    fn git_stdin_feeds_the_command_and_returns_its_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path());
        let hash = repo.git_stdin(&["hash-object", "-w", "--stdin"], b"piped blob\n");
        assert_eq!(repo.git(&["cat-file", "-p", &hash]), "piped blob");
    }

    #[test]
    fn init_creates_the_main_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path());
        repo.write("a.txt", b"a");
        repo.commit("first");
        assert_eq!(repo.git(&["symbolic-ref", "--short", "HEAD"]), "main");
    }
}
