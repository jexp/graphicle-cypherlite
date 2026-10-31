//! Real git repositories for the reader's tests, scripted with plain git
//! commands (the model is cypherlite's `cypherlite-test-support/src/git.rs`).
//!
//! Every command runs with an empty global config and `GIT_CONFIG_NOSYSTEM`,
//! so a developer's `~/.gitconfig` (signing, hooks, mailmap) cannot change a
//! fixture. Commits take an explicit author and committer identity and date.
//!
//! This lives in the crate rather than `tests/it`: the reader's parse corpus
//! is a set of unit tests (`cargo test --lib git_history`), and those cannot
//! see integration-test helpers — nor can the writer's and the enqueue
//! point's, which share it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One identity and the date both its author and committer lines carry.
#[derive(Clone, Copy)]
pub struct Ident {
    pub name: &'static str,
    pub email: &'static str,
    pub date: &'static str,
}

pub const ADA: &str = "Ada Lovelace";
pub const ADA_EMAIL: &str = "ada@example.com";
pub const BOB: &str = "Bob Builder";
pub const BOB_EMAIL: &str = "bob@example.com";

/// Fixture dates, one day apart, in git's `%aI` input form.
pub const DAY_1: &str = "2026-01-01T00:00:00+00:00";
pub const DAY_2: &str = "2026-01-02T00:00:00+00:00";
pub const DAY_3: &str = "2026-01-03T00:00:00+00:00";
pub const DAY_4: &str = "2026-01-04T00:00:00+00:00";
pub const DAY_5: &str = "2026-01-05T00:00:00+00:00";
pub const DAY_6: &str = "2026-01-06T00:00:00+00:00";

pub fn ada(date: &'static str) -> Ident {
    Ident {
        name: ADA,
        email: ADA_EMAIL,
        date,
    }
}

pub fn bob(date: &'static str) -> Ident {
    Ident {
        name: BOB,
        email: BOB_EMAIL,
        date,
    }
}

/// A fixture repository. Its empty global config file is removed on drop.
pub struct Repo {
    root: PathBuf,
    config: tempfile::NamedTempFile,
}

impl Repo {
    /// Create a repository at `root` on branch `main`.
    pub fn init(root: &Path) -> Self {
        std::fs::create_dir_all(root).expect("create the fixture repository directory");
        let repo = Self::wrap(root);
        repo.run(&["init", "-q", "-b", "main"], ada(DAY_1));
        repo
    }

    /// Wrap a repository that already exists at `root` (a clone, say) with
    /// this fixture's isolated config and identity helpers.
    pub fn wrap(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            config: tempfile::NamedTempFile::new().expect("create the empty git config"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write `contents` to `relative` under the root, creating parents.
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the fixture directory");
        }
        std::fs::write(path, contents).expect("write the fixture file");
    }

    /// `git mv from to`.
    pub fn mv(&self, from: &str, to: &str) {
        self.run(&["mv", from, to], ada(DAY_1));
    }

    /// `git rm path`.
    pub fn remove(&self, path: &str) {
        self.run(&["rm", "-q", path], ada(DAY_1));
    }

    /// Stage every change and commit it as `ident`, with an optional body.
    /// Returns the new commit's sha.
    pub fn commit(&self, ident: Ident, subject: &str, body: &str) -> String {
        self.run(&["add", "-A"], ident);
        let mut args = vec!["commit", "-q", "--allow-empty", "-m", subject];
        if !body.is_empty() {
            args.extend(["-m", body]);
        }
        self.run(&args, ident);
        self.rev("HEAD")
    }

    /// `git checkout -q -b name start`.
    pub fn branch_at(&self, name: &str, start: &str) {
        self.run(&["checkout", "-q", "-b", name, start], ada(DAY_1));
    }

    /// `git checkout -q name`.
    pub fn checkout(&self, name: &str) {
        self.run(&["checkout", "-q", name], ada(DAY_1));
    }

    /// `git merge --no-ff -q -m subject branches...` as `ident`; returns the
    /// merge commit's sha.
    pub fn merge(&self, ident: Ident, subject: &str, branches: &[&str]) -> String {
        let mut args = vec!["merge", "--no-ff", "-q", "-m", subject];
        args.extend(branches);
        self.run(&args, ident);
        self.rev("HEAD")
    }

    /// `git tag -a name -m name target` as `ident`.
    pub fn annotated_tag(&self, ident: Ident, name: &str, target: &str) {
        self.run(&["tag", "-a", name, "-m", name, target], ident);
    }

    /// `git commit --amend -q -m subject` as `ident` over the staged or
    /// unchanged tree; returns the rewritten tip. A history rewrite with no
    /// side branches: the recorded head becomes unreachable.
    pub fn amend(&self, ident: Ident, subject: &str) -> String {
        self.run(&["commit", "-q", "--amend", "-m", subject], ident);
        self.rev("HEAD")
    }

    /// The sha `rev` names.
    pub fn rev(&self, rev: &str) -> String {
        self.run(&["rev-parse", rev], ada(DAY_1))
    }

    /// One git command with the isolated config and `ident` as author and
    /// committer. Returns trimmed stdout; a failed command panics with git's
    /// stderr.
    fn run(&self, args: &[&str], ident: Ident) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.config.path())
            .env("GIT_AUTHOR_NAME", ident.name)
            .env("GIT_AUTHOR_EMAIL", ident.email)
            .env("GIT_AUTHOR_DATE", ident.date)
            .env("GIT_COMMITTER_NAME", ident.name)
            .env("GIT_COMMITTER_EMAIL", ident.email)
            .env("GIT_COMMITTER_DATE", ident.date)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command
            .output()
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

/// `git clone -q source dest` with the fixture's isolated config: a fork
/// shape — every sha of the source, its own history from here on. The
/// clone comes back wrapped as a [`Repo`].
pub fn clone(source: &Path, dest: &Path) -> Repo {
    let config = tempfile::NamedTempFile::new().expect("create the empty git config");
    let output = Command::new("git")
        .args(["clone", "-q"])
        .arg(source)
        .arg(dest)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", config.path())
        .env("GIT_AUTHOR_NAME", ADA)
        .env("GIT_AUTHOR_EMAIL", ADA_EMAIL)
        .env("GIT_COMMITTER_NAME", ADA)
        .env("GIT_COMMITTER_EMAIL", ADA_EMAIL)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("clone the fixture repository");
    assert!(
        output.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Repo::wrap(dest)
}

/// The shas and names of [`history_fixture`]'s commits.
pub struct Scenario {
    /// `one: add the shared module`, Ada, with a two-line body.
    pub first: String,
    /// `two: rename`, Bob: renames `src/common.rs` to `src/shared.rs`.
    pub rename: String,
    /// `feat: add f`, Bob, on the `feat` branch.
    pub feat: String,
    /// `merge feat`, Ada: two parents, `main` first.
    pub merge: String,
    /// `octopus o1 o2`, Ada: three parents, `main`'s merge first.
    pub octopus: String,
    /// `remove a`, Ada: deletes `src/a.rs`.
    pub delete: String,
}

/// A repository with two authors, a rename, a plain merge, an octopus merge,
/// a deletion, a branch (`feat`, plus `o1` and `o2`) and an annotated tag
/// `v1` on the rename commit. Returns the temp dir (keep it alive), the repo
/// and the commit shas.
pub fn history_fixture() -> (tempfile::TempDir, Repo, Scenario) {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let repo = Repo::init(dir.path());

    repo.write("src/common.rs", "common one\n");
    repo.write("src/a.rs", "a one\n");
    let first = repo.commit(
        ada(DAY_1),
        "one: add the shared module",
        "body line 1\n\nbody line 2",
    );

    repo.mv("src/common.rs", "src/shared.rs");
    repo.write("src/a.rs", "a two\n");
    repo.write("src/new.rs", "new\n");
    let rename = repo.commit(bob(DAY_2), "two: rename", "");
    repo.annotated_tag(ada(DAY_2), "v1", &rename);

    repo.branch_at("feat", &rename);
    repo.write("src/f.rs", "f\n");
    let feat = repo.commit(bob(DAY_3), "feat: add f", "");

    repo.checkout("main");
    repo.write("src/m.rs", "m\n");
    repo.commit(ada(DAY_3), "main: add m", "");
    let merge = repo.merge(ada(DAY_4), "merge feat", &["feat"]);

    repo.branch_at("o1", &rename);
    repo.write("o1.txt", "o1\n");
    repo.commit(bob(DAY_4), "o1: add o1", "");
    repo.branch_at("o2", &rename);
    repo.write("o2.txt", "o2\n");
    repo.commit(bob(DAY_4), "o2: add o2", "");

    repo.checkout("main");
    let octopus = repo.merge(ada(DAY_4), "octopus o1 o2", &["o1", "o2"]);

    repo.remove("src/a.rs");
    let delete = repo.commit(ada(DAY_4), "remove a", "");

    let scenario = Scenario {
        first,
        rename,
        feat,
        merge,
        octopus,
        delete,
    };
    (dir, repo, scenario)
}

/// [`history_fixture`] plus two commits by Ada that always change
/// `src/pay.rs` and `src/tax.rs` together: the known-coupled pair of the
/// coupling tests. The base history is untouched, so its own pairs stay
/// what [`history_fixture`] makes them.
pub fn coupled_fixture() -> (tempfile::TempDir, Repo, Scenario) {
    let (dir, repo, scenario) = history_fixture();
    for (day, note) in [(DAY_5, "pay one"), (DAY_6, "pay two")] {
        repo.write("src/pay.rs", &format!("pay {note}\n"));
        repo.write("src/tax.rs", &format!("tax {note}\n"));
        repo.commit(ada(day), &format!("{note}: pay and tax"), "");
    }
    (dir, repo, scenario)
}
