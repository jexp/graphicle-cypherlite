//! Reads a checkout's history with one bounded `git log` (git-to-graph plan
//! §3), plus `git rev-parse` for `HEAD` and `git for-each-ref` for branches
//! and tags.
//!
//! The subprocess discipline follows cypherlite's `lab/git_log.rs`: git runs
//! with `-C DIR`, a null stdin and no shell, and its stdout is capped before
//! any parse. [`parse_log`] is a pure function over the stdout bytes, and a
//! malformed record is a [`ParseError`] naming its position and sha.
//!
//! The `-z` shape this parser reads, as real git 2.56 writes it: a record
//! starts with `0x1e`; the header is eleven fields separated by `0x1f`, each
//! followed by a `0x1f`, then a NUL. After the NUL comes either nothing (no
//! changes, as for a merge) or a newline and NUL-terminated tokens. An `A`,
//! `M`, `D` or `T` token is followed by one path. A rename is one token
//! `R<similarity>` followed by the old path and then the new path.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use chrono::{DateTime, FixedOffset};

use super::{Change, ChangeKind, CommitRecord, GitRef, History, Window};

/// The most bytes any one git call may write to stdout (cypherlite's 128 MiB
/// log cap). The byte past it kills the child and fails the read, so nothing
/// past the cap is parsed.
pub const LOG_LIMIT: u64 = 128 * 1024 * 1024;

/// The most stderr bytes kept for an error message.
const STDERR_LIMIT: u64 = 64 * 1024;

const RECORD_SEP: u8 = 0x1e;
const FIELD_SEP: u8 = 0x1f;

/// The header's field names, in `--format` order.
const FIELD_NAMES: [&str; 11] = [
    "sha",
    "tree",
    "parents",
    "author name",
    "author email",
    "author date",
    "committer name",
    "committer email",
    "committer date",
    "subject",
    "body",
];

/// The `git log` format: the eleven header fields, each followed by `0x1f`.
const LOG_FORMAT: &str =
    "--format=%x1e%H%x1f%T%x1f%P%x1f%aN%x1f%aE%x1f%aI%x1f%cN%x1f%cE%x1f%cI%x1f%s%x1f%b%x1f";

/// `refname<TAB>objectname<TAB>peeled objectname` per ref. Git ref names
/// cannot contain a tab, so the split is unambiguous.
const REF_FORMAT: &str = "--format=%(refname)%09%(objectname)%09%(*objectname)";

/// Why a history could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// No `git` executable on the PATH.
    GitNotInstalled,
    /// The repository has no commits yet (`HEAD` is unborn), so there is no
    /// history to read.
    NoCommits,
    /// A git call wrote more than `limit` bytes to stdout. Nothing was
    /// parsed.
    LogTooLarge { limit: u64 },
    /// git exited non-zero; the message is its trimmed stderr.
    Git(String),
    /// Spawning or reading git failed.
    Io(String),
    /// git's `HEAD` or ref output did not have the expected shape.
    Malformed(String),
    /// A `git log` record did not parse.
    Parse(ParseError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitNotInstalled => write!(f, "git is not installed or not on the PATH"),
            Self::NoCommits => write!(f, "the repository has no commits yet"),
            Self::LogTooLarge { limit } => {
                write!(
                    f,
                    "git wrote more than {limit} bytes; the history is too large"
                )
            }
            Self::Git(message) => write!(f, "git failed: {message}"),
            Self::Io(message) => write!(f, "{message}"),
            Self::Malformed(message) => write!(f, "malformed git output: {message}"),
            Self::Parse(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// A read failure as the crate's error (the writer's job fails with git's
/// own message; the enqueue point reports it instead of failing the sync).
impl From<ReadError> for crate::Error {
    fn from(error: ReadError) -> Self {
        crate::Error::bad(error.to_string())
    }
}

/// A `git log` record that did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The record's one-based position in the log output.
    pub record: usize,
    /// The commit's sha, when the record's sha field was read and valid.
    pub sha: Option<String>,
    /// What was wrong with the record.
    pub reason: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.sha {
            Some(sha) => write!(
                f,
                "malformed git log record {} (commit {sha}): {}",
                self.record, self.reason
            ),
            None => write!(
                f,
                "malformed git log record {}: {}",
                self.record, self.reason
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Read the full history of the repository at `dir`.
///
/// Runs `git rev-parse --verify -q HEAD`, `git for-each-ref`, and the one
/// `git log` call of plan §3. An unborn `HEAD` is [`ReadError::NoCommits`].
pub fn read_history(dir: &Path) -> Result<History, ReadError> {
    read_history_bounded(dir, LOG_LIMIT, &Window::default())
}

/// [`read_history`] under a [`Window`] (plan §4): the back-fill bound and
/// the resume range become `--max-count`/`--since` and the log's revision
/// range.
pub fn read_history_windowed(dir: &Path, window: &Window) -> Result<History, ReadError> {
    read_history_bounded(dir, LOG_LIMIT, window)
}

/// [`read_history`] with the stdout cap made a parameter, so tests can
/// exercise the cap without a 128 MiB history.
pub(crate) fn read_history_capped(dir: &Path, limit: u64) -> Result<History, ReadError> {
    read_history_bounded(dir, limit, &Window::default())
}

fn read_history_bounded(
    dir: &Path,
    limit: u64,
    window: &Window,
) -> Result<History, ReadError> {
    let head = current_head(dir)?;
    let Some(head) = head else {
        return Err(ReadError::NoCommits);
    };

    let refs = run_git(
        dir,
        &["for-each-ref", REF_FORMAT, "refs/heads", "refs/tags"],
        limit,
    )?;
    let (branches, tags) = parse_refs(&stdout_or_error(refs)?)?;

    let args = log_args(window)?;
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = run_git(dir, &argv, limit)?;
    let commits = parse_log(&stdout_or_error(log)?).map_err(ReadError::Parse)?;

    Ok(History {
        head,
        branches,
        tags,
        commits,
    })
}

/// The `HEAD` commit oid of the repository at `dir`: `None` when `HEAD` is
/// unborn (a repository with no commits). This is the cheap call the
/// history job's enqueue point makes for its input hash.
pub fn current_head(dir: &Path) -> Result<Option<String>, ReadError> {
    let head = run_git(dir, &["rev-parse", "--verify", "-q", "HEAD"], LOG_LIMIT)?;
    if !head.status.success() {
        // `--verify -q` exits 1 silently for an unborn HEAD; anything else
        // (not a repository, say) is a real failure.
        if head.status.code() == Some(1) && head.stderr.is_empty() {
            return Ok(None);
        }
        return Err(git_failure(&head));
    }
    oid_from_output(&head.stdout)
        .map(Some)
        .ok_or_else(|| ReadError::Malformed("HEAD is not a commit id".to_string()))
}

/// Whether `ancestor` is reachable from `descendant`
/// (`git merge-base --is-ancestor`): the resumability check of plan §6. A
/// recorded head that is no longer an ancestor of `HEAD` means the history
/// was rewritten (rebase, squash, branch deletion) and only a full re-ingest
/// can catch it.
pub fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool, ReadError> {
    if !is_oid(ancestor) || !is_oid(descendant) {
        return Err(ReadError::Malformed(
            "merge-base needs two commit ids".to_string(),
        ));
    }
    let check = run_git(
        dir,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        LOG_LIMIT,
    )?;
    match check.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&check)),
    }
}

/// The `git log` argv of plan §3, program name excluded (`-C DIR` is added
/// by [`run_git`]). `-- .` scopes the walk to the directory git runs in.
/// A [`Window`] adds `--max-count`/`--since` and the revision range before
/// the pathspec; a window value that could pose as a git option is
/// rejected here, before any subprocess runs.
fn log_args(window: &Window) -> Result<Vec<String>, ReadError> {
    let mut args: Vec<String> = [
        "-c",
        "log.showSignature=false",
        "--no-pager",
        "log",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--name-status",
        "-z",
        "-M",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if let Some(max) = window.max_commits {
        args.push(format!("--max-count={max}"));
    }
    if let Some(since) = &window.since {
        check_revision(since)?;
        args.push(format!("--since={since}"));
    }
    if let Some(range) = &window.range {
        check_revision(range)?;
        args.push(range.clone());
    }
    args.push(LOG_FORMAT.to_string());
    args.push("--".into());
    args.push(".".into());
    Ok(args)
}

/// A `--since` value or revision range git receives on its own argv slot:
/// it must not pose as an option or smuggle a second argument. A value that
/// starts with `-` or contains whitespace is rejected before git runs.
fn check_revision(value: &str) -> Result<(), ReadError> {
    let bad = || ReadError::Malformed(format!("bad revision or date: {value:?}"));
    (!value.is_empty() && !value.starts_with('-') && !value.chars().any(char::is_whitespace))
        .then_some(())
        .ok_or_else(bad)
}

/// What one git call produced.
struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

/// Run `git -C dir <args>` with a null stdin and no shell. Stdout is read
/// through `take(limit + 1)`, so memory is bounded by the cap and the byte
/// past it is detected without buffering more. Stderr drains on its own
/// thread, so git never blocks on a full pipe.
fn run_git(dir: &Path, args: &[&str], limit: u64) -> Result<Captured, ReadError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReadError::GitNotInstalled
        } else {
            ReadError::Io(format!("failed to run git: {error}"))
        }
    })?;
    let stdout_pipe = child
        .stdout
        .take()
        .expect("a child spawned with piped stdout has a stdout handle");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("a child spawned with piped stderr has a stderr handle");
    let stderr_reader = std::thread::spawn(move || {
        let mut pipe = stderr_pipe;
        let mut retained = Vec::new();
        let _ = (&mut pipe).take(STDERR_LIMIT).read_to_end(&mut retained);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        String::from_utf8_lossy(&retained).trim().to_string()
    });

    let mut stdout = Vec::new();
    let read = stdout_pipe.take(limit + 1).read_to_end(&mut stdout);
    if let Err(error) = read {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Err(ReadError::Io(format!("failed to read git output: {error}")));
    }
    if stdout.len() as u64 > limit {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Err(ReadError::LogTooLarge { limit });
    }
    let status = child
        .wait()
        .map_err(|error| ReadError::Io(format!("failed to run git: {error}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

/// The stdout of a call that must succeed.
fn stdout_or_error(captured: Captured) -> Result<Vec<u8>, ReadError> {
    if captured.status.success() {
        Ok(captured.stdout)
    } else {
        Err(git_failure(&captured))
    }
}

fn git_failure(captured: &Captured) -> ReadError {
    if captured.stderr.is_empty() {
        ReadError::Git(format!("git exited with {}", captured.status))
    } else {
        ReadError::Git(captured.stderr.clone())
    }
}

/// The trimmed stdout as a commit id, when it is one.
fn oid_from_output(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    is_oid(text).then(|| text.to_string())
}

/// A full object id: 40 lowercase hex digits (SHA-1) or 64 (SHA-256).
fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Split `for-each-ref` output into branches and tags.
fn parse_refs(stdout: &[u8]) -> Result<(Vec<GitRef>, Vec<GitRef>), ReadError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| ReadError::Malformed("for-each-ref output is not UTF-8".to_string()))?;
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [refname, object, peeled] = fields.as_slice() else {
            return Err(ReadError::Malformed(format!(
                "for-each-ref line has {} fields: {line}",
                fields.len()
            )));
        };
        if !is_oid(object) || !(peeled.is_empty() || is_oid(peeled)) {
            return Err(ReadError::Malformed(format!(
                "for-each-ref line has a bad object id: {line}"
            )));
        }
        let target = if peeled.is_empty() { object } else { peeled };
        let (name, bucket) = if let Some(name) = refname.strip_prefix("refs/heads/") {
            (name, &mut branches)
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            (name, &mut tags)
        } else {
            return Err(ReadError::Malformed(format!("unexpected ref: {refname}")));
        };
        bucket.push(GitRef {
            name: name.to_string(),
            refname: (*refname).to_string(),
            target: (*target).to_string(),
        });
    }
    Ok((branches, tags))
}

/// Parse the `-z` stdout of the plan §3 `git log` into commits, in git's
/// order.
///
/// Empty stdout is an empty history. Anything that does not match the format
/// is a [`ParseError`] naming the record: a missing leading separator, a
/// header that is not NUL-terminated or does not have eleven fields, an
/// invalid oid, date or change status, or a rename without its new path.
pub fn parse_log(stdout: &[u8]) -> Result<Vec<CommitRecord>, ParseError> {
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    if stdout[0] != RECORD_SEP {
        return Err(ParseError {
            record: 1,
            sha: None,
            reason: "output does not start with the record separator".to_string(),
        });
    }
    stdout[1..]
        .split(|byte| *byte == RECORD_SEP)
        .enumerate()
        .map(|(index, record)| parse_record(index + 1, record))
        .collect()
}

/// Parse one record (the bytes after its `0x1e`).
fn parse_record(record: usize, bytes: &[u8]) -> Result<CommitRecord, ParseError> {
    let fail = |sha: Option<&str>, reason: String| ParseError {
        record,
        sha: sha.map(str::to_string),
        reason,
    };
    let header_end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| fail(None, "header is not NUL-terminated".to_string()))?;
    let header = bytes[..header_end]
        .strip_suffix(&[FIELD_SEP])
        .ok_or_else(|| {
            fail(
                None,
                "header does not end with a field separator".to_string(),
            )
        })?;
    let raw: Vec<&[u8]> = header.split(|byte| *byte == FIELD_SEP).collect();
    if raw.len() != FIELD_NAMES.len() {
        return Err(fail(
            None,
            format!(
                "expected {} header fields, found {}",
                FIELD_NAMES.len(),
                raw.len()
            ),
        ));
    }
    let mut fields = Vec::with_capacity(raw.len());
    for (name, bytes) in FIELD_NAMES.iter().zip(&raw) {
        let text =
            std::str::from_utf8(bytes).map_err(|_| fail(None, format!("{name} is not UTF-8")))?;
        fields.push(text);
    }

    let sha = fields[0];
    if !is_oid(sha) {
        return Err(fail(None, format!("sha is not a commit id: {sha:?}")));
    }
    let at = |reason: String| fail(Some(sha), reason);
    if !is_oid(fields[1]) {
        return Err(at(format!("tree is not an object id: {:?}", fields[1])));
    }
    let parents = parse_parents(fields[2]).map_err(&at)?;
    let author_date = parse_git_date(fields[5]).map_err(&at)?;
    let committer_date = parse_git_date(fields[8]).map_err(&at)?;
    let changes = parse_changes(&bytes[header_end + 1..]).map_err(&at)?;

    Ok(CommitRecord {
        oid: sha.to_string(),
        tree_oid: fields[1].to_string(),
        parents,
        author_name: fields[3].to_string(),
        author_email: fields[4].to_string(),
        author_date,
        committer_name: fields[6].to_string(),
        committer_email: fields[7].to_string(),
        committer_date,
        subject: fields[9].to_string(),
        body: fields[10].trim_end_matches('\n').to_string(),
        changes,
    })
}

/// `%P`: space-separated parent oids, or nothing for a root commit.
fn parse_parents(value: &str) -> Result<Vec<String>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(' ')
        .map(|parent| {
            if is_oid(parent) {
                Ok(parent.to_string())
            } else {
                Err(format!("parent is not a commit id: {parent:?}"))
            }
        })
        .collect()
}

/// Strict `%aI`/`%cI`: `YYYY-MM-DDTHH:MM:SS` then `Z` or `±HH:MM`, with
/// fixed widths and separators. Calendar and clock ranges are checked here
/// too, and a leap second (`:60`) is rejected, because git never emits one.
/// Only then is the value parsed by chrono, which validates the calendar.
fn parse_git_date(value: &str) -> Result<DateTime<FixedOffset>, String> {
    let bad = || format!("invalid date {value:?}: expected strict ISO 8601 as git's %aI emits it");
    let bytes = value.as_bytes();
    let zone_ok = match bytes.len() {
        20 => bytes[19] == b'Z',
        25 => matches!(bytes[19], b'+' | b'-') && bytes[22] == b':',
        _ => return Err(bad()),
    };
    let separators_ok = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':';
    let digit_spans: &[(usize, usize)] = if bytes.len() == 25 {
        &[
            (0, 4),
            (5, 7),
            (8, 10),
            (11, 13),
            (14, 16),
            (17, 19),
            (20, 22),
            (23, 25),
        ]
    } else {
        &[(0, 4), (5, 7), (8, 10), (11, 13), (14, 16), (17, 19)]
    };
    let digits_ok = digit_spans
        .iter()
        .all(|&(start, end)| bytes[start..end].iter().all(u8::is_ascii_digit));
    if !(zone_ok && separators_ok && digits_ok) {
        return Err(bad());
    }
    let number = |start: usize, end: usize| -> u32 {
        value[start..end].parse().expect("checked ASCII digits")
    };
    let in_range = number(11, 13) <= 23
        && number(14, 16) <= 59
        && number(17, 19) <= 59
        && (bytes.len() == 20 || (number(20, 22) <= 23 && number(23, 25) <= 59));
    if !in_range {
        return Err(bad());
    }
    DateTime::parse_from_rfc3339(value).map_err(|_| bad())
}

/// The change list after a header's NUL: empty, or a newline and
/// NUL-terminated tokens (see the module docs for the shapes).
fn parse_changes(rest: &[u8]) -> Result<Vec<Change>, String> {
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let list = rest
        .strip_prefix(b"\n")
        .ok_or("the change list does not start with a newline")?;
    let list = list
        .strip_suffix(&[0])
        .ok_or("the change list is not NUL-terminated")?;

    let mut tokens = list.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(status) = tokens.next() {
        let (kind, similarity) = match status.first() {
            Some(b'A') => (ChangeKind::Added, None),
            Some(b'M') => (ChangeKind::Modified, None),
            Some(b'D') => (ChangeKind::Deleted, None),
            Some(b'T') => (ChangeKind::TypeChanged, None),
            Some(b'R') => (ChangeKind::Renamed, Some(parse_similarity(&status[1..])?)),
            _ => {
                return Err(format!(
                    "unknown change status {:?}",
                    String::from_utf8_lossy(status)
                ));
            }
        };
        if kind != ChangeKind::Renamed && status.len() != 1 {
            return Err(format!(
                "change status {:?} has extra bytes",
                String::from_utf8_lossy(status)
            ));
        }
        if kind == ChangeKind::Renamed {
            let old_path = next_path(&mut tokens)?;
            let path = next_path(&mut tokens)?;
            changes.push(Change {
                kind,
                path,
                old_path: Some(old_path),
                similarity,
            });
        } else {
            changes.push(Change {
                kind,
                path: next_path(&mut tokens)?,
                old_path: None,
                similarity,
            });
        }
    }
    Ok(changes)
}

/// The similarity digits of an `R` status: one to three, at most 100.
fn parse_similarity(digits: &[u8]) -> Result<u8, String> {
    let well_formed = (1..=3).contains(&digits.len()) && digits.iter().all(u8::is_ascii_digit);
    let value = well_formed
        .then(|| std::str::from_utf8(digits).ok()?.parse::<u8>().ok())
        .flatten()
        .filter(|value| *value <= 100);
    value.ok_or_else(|| {
        format!(
            "rename similarity {:?} is not 0 to 100",
            String::from_utf8_lossy(digits)
        )
    })
}

/// The next NUL-terminated path token, which must exist and not be empty.
fn next_path<'a>(tokens: &mut impl Iterator<Item = &'a [u8]>) -> Result<String, String> {
    let path = tokens.next().ok_or("the change list ends before a path")?;
    if path.is_empty() {
        return Err("a change names an empty path".to_string());
    }
    Ok(String::from_utf8_lossy(path).into_owned())
}
