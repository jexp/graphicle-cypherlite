//! The reader's parse corpus. Real-repository cases run git against the
//! fixture of [`super::fixture`]; the malformed-record cases are hand-written
//! bytes in the exact `-z` shape, since git never emits them.

use super::fixture::{DAY_1, Repo, ada, history_fixture};
use super::reader::{
    ReadError, current_head, is_ancestor, parse_log, read_history, read_history_capped,
    read_history_windowed,
};
use super::{ChangeKind, CommitRecord, Window};

fn hex(digit: char) -> String {
    digit.to_string().repeat(40)
}

/// One `-z` record as `git log` writes it, with `changes` appended verbatim
/// (empty, or `\n` followed by NUL-terminated tokens).
fn record(sha: &str, parents: &str, date: &str, subject: &str, changes: &str) -> String {
    let tree = hex('f');
    format!(
        "\x1e{sha}\x1f{tree}\x1f{parents}\x1fAda\x1fada@example.com\x1f{date}\x1fAda\x1fada@example.com\x1f{date}\x1f{subject}\x1f\x1f\0{changes}"
    )
}

fn parse(stdout: &str) -> Result<Vec<CommitRecord>, super::ParseError> {
    parse_log(stdout.as_bytes())
}

fn parse_error(stdout: &str) -> super::ParseError {
    parse(stdout).expect_err("the record must not parse")
}

const GOOD_DATE: &str = "2026-01-01T00:00:00Z";

#[test]
fn empty_stdout_is_an_empty_history() {
    assert!(parse_log(b"").unwrap().is_empty());
}

#[test]
fn normal_commit_parses_every_header_field() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();

    let first = history
        .commits
        .iter()
        .find(|commit| commit.oid == scenario.first)
        .expect("the first commit is in the log");
    assert_eq!(first.tree_oid.len(), 40);
    assert!(first.parents.is_empty(), "a root commit has no parents");
    assert!(!first.is_merge());
    assert_eq!(first.subject, "one: add the shared module");
    assert_eq!(first.body, "body line 1\n\nbody line 2");
    assert_eq!(first.author_name, "Ada Lovelace");
    assert_eq!(first.author_email, "ada@example.com");
    assert_eq!(first.author_date.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    assert_eq!(first.committer_name, "Ada Lovelace");
    assert_eq!(first.committer_date, first.author_date);
    assert!(first.changes.iter().all(|c| c.kind == ChangeKind::Added));
    let paths: Vec<&str> = first.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/a.rs", "src/common.rs"]);
}

#[test]
fn rename_is_one_change_carrying_old_and_new_path() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();
    let rename = history
        .commits
        .iter()
        .find(|commit| commit.oid == scenario.rename)
        .unwrap();

    assert_eq!(rename.author_name, "Bob Builder");
    let renamed: Vec<_> = rename
        .changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Renamed)
        .collect();
    assert_eq!(
        renamed.len(),
        1,
        "R100 is one change, not a delete and an add"
    );
    assert_eq!(renamed[0].path, "src/shared.rs");
    assert_eq!(renamed[0].old_path.as_deref(), Some("src/common.rs"));
    assert_eq!(renamed[0].similarity, Some(100));
    assert!(
        rename
            .changes
            .iter()
            .any(|c| c.kind == ChangeKind::Modified && c.path == "src/a.rs")
    );
    assert!(
        rename
            .changes
            .iter()
            .any(|c| c.kind == ChangeKind::Added && c.path == "src/new.rs" && c.old_path.is_none())
    );
    assert!(
        !rename.changes.iter().any(|c| c.path == "src/common.rs"),
        "the old path appears only as a rename's old_path"
    );
}

#[test]
fn merge_keeps_first_parent_first() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();
    let merge = history
        .commits
        .iter()
        .find(|commit| commit.oid == scenario.merge)
        .unwrap();

    assert!(merge.is_merge());
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(
        merge.parents[0],
        repo.rev(&format!("{}^1", scenario.merge)),
        "first parent is main"
    );
    assert_eq!(merge.parents[1], scenario.feat, "second parent is feat");
    assert!(merge.changes.is_empty(), "git lists no changes for a merge");
}

#[test]
fn octopus_merge_has_three_ordered_parents() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();
    let octopus = history
        .commits
        .iter()
        .find(|commit| commit.oid == scenario.octopus)
        .unwrap();

    assert_eq!(
        octopus.parents,
        [scenario.merge.clone(), repo.rev("o1"), repo.rev("o2")]
    );
    assert!(octopus.is_merge());
}

#[test]
fn deletion_is_a_deleted_change() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();
    let delete = history
        .commits
        .iter()
        .find(|commit| commit.oid == scenario.delete)
        .unwrap();

    assert_eq!(delete.changes.len(), 1);
    assert_eq!(delete.changes[0].kind, ChangeKind::Deleted);
    assert_eq!(delete.changes[0].path, "src/a.rs");
}

#[test]
fn log_lists_every_fixture_commit_newest_first() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();

    assert_eq!(history.head, repo.rev("HEAD"));
    assert_eq!(history.commits[0].oid, history.head);
    let count = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.root())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    let expected: usize = String::from_utf8(count.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(history.commits.len(), expected);
    for sha in [
        &scenario.first,
        &scenario.rename,
        &scenario.feat,
        &scenario.merge,
        &scenario.octopus,
    ] {
        assert!(history.commits.iter().any(|commit| &commit.oid == sha));
    }
}

#[test]
fn branches_and_tags_are_read_with_peeled_targets() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();

    let mut names: Vec<&str> = history.branches.iter().map(|b| b.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["feat", "main", "o1", "o2"]);
    let feat = history.branches.iter().find(|b| b.name == "feat").unwrap();
    assert_eq!(feat.refname, "refs/heads/feat");
    assert_eq!(feat.target, scenario.feat);

    assert_eq!(history.tags.len(), 1);
    let tag = &history.tags[0];
    assert_eq!(tag.name, "v1");
    assert_eq!(tag.refname, "refs/tags/v1");
    assert_eq!(
        tag.target, scenario.rename,
        "an annotated tag peels to its commit"
    );
}

#[test]
fn repository_without_commits_is_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    Repo::init(dir.path());

    assert!(matches!(
        read_history(dir.path()),
        Err(ReadError::NoCommits)
    ));
}

#[test]
fn stdout_past_the_cap_fails_before_any_parse() {
    let (_dir, repo, _scenario) = history_fixture();
    assert!(matches!(
        read_history_capped(repo.root(), 1),
        Err(ReadError::LogTooLarge { limit: 1 })
    ));
}

#[test]
fn two_records_with_a_rename_and_a_similarity_parse() {
    let stdout = format!(
        "{}{}",
        record(&hex('1'), "", GOOD_DATE, "first", "\nA\0src/a.rs\0"),
        record(
            &hex('2'),
            &hex('1'),
            GOOD_DATE,
            "second",
            "\nR075\0src/old.rs\0src/new.rs\0M\0src/b.rs\0",
        ),
    );
    let commits = parse(&stdout).unwrap();

    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].oid, hex('1'));
    assert_eq!(commits[1].parents, [hex('1')]);
    let change = &commits[1].changes[0];
    assert_eq!(change.kind, ChangeKind::Renamed);
    assert_eq!(change.path, "src/new.rs");
    assert_eq!(change.old_path.as_deref(), Some("src/old.rs"));
    assert_eq!(change.similarity, Some(75));
    assert_eq!(commits[1].changes[1].kind, ChangeKind::Modified);
}

#[test]
fn octopus_parents_keep_their_order_in_a_handcrafted_record() {
    let parents = format!("{} {} {}", hex('a'), hex('b'), hex('c'));
    let commits = parse(&record(&hex('d'), &parents, GOOD_DATE, "octo", "")).unwrap();

    assert_eq!(commits[0].parents, [hex('a'), hex('b'), hex('c')]);
    assert!(commits[0].changes.is_empty());
}

#[test]
fn a_date_with_a_zone_offset_parses_to_that_instant() {
    let commits = parse(&record(
        &hex('e'),
        "",
        "2026-06-01T10:30:00+02:00",
        "zoned",
        "",
    ))
    .unwrap();
    assert_eq!(
        commits[0].author_date.to_rfc3339(),
        "2026-06-01T10:30:00+02:00"
    );
}

#[test]
fn malformed_second_record_is_named_by_position_and_sha() {
    let stdout = format!(
        "{}{}",
        record(&hex('1'), "", GOOD_DATE, "fine", ""),
        record(&hex('2'), "zz", GOOD_DATE, "bad parent", ""),
    );
    let error = parse_error(&stdout);

    assert_eq!(error.record, 2);
    assert_eq!(error.sha.as_deref(), Some(hex('2').as_str()));
    assert!(error.reason.contains("parent"), "{}", error.reason);
    assert!(error.to_string().contains("record 2"));
}

#[test]
fn record_with_too_few_header_fields_has_no_sha() {
    let stdout = format!("\x1e{}\x1f{}\x1f\x1f\0", hex('1'), hex('f'));
    let error = parse_error(&stdout);

    assert_eq!(error.record, 1);
    assert_eq!(error.sha, None);
    assert!(
        error.reason.contains("expected 11 header fields, found 3"),
        "{}",
        error.reason
    );
}

#[test]
fn invalid_dates_are_rejected() {
    let cases = [
        ("2026-01-01 00:00:00Z", "space separator"),
        ("2026-01-01T00:00:00.000Z", "fractional seconds"),
        ("2026-01-01T00:00:60Z", "leap second"),
        ("2026-01-01T00:00:00+24:00", "zone hour past 23"),
        ("2026-01-01T00:00:00+0100", "zone without colon"),
        ("2026-02-30T00:00:00Z", "no such calendar day"),
        ("2026-01-01T24:00:00Z", "hour past 23"),
        ("2026-01-01", "date only"),
    ];
    for (date, why) in cases {
        let error = parse_error(&record(&hex('1'), "", date, "s", ""));
        assert!(
            error.reason.contains("invalid date"),
            "{why}: {}",
            error.reason
        );
        assert_eq!(error.sha.as_deref(), Some(hex('1').as_str()), "{why}");
    }
}

#[test]
fn unknown_or_copy_status_is_rejected() {
    for status in ["X", "C100", "AM"] {
        let stdout = record(
            &hex('1'),
            "",
            GOOD_DATE,
            "s",
            &format!("\n{status}\0src/a.rs\0"),
        );
        let error = parse_error(&stdout);
        assert!(
            error.reason.contains("unknown change status") || error.reason.contains("extra bytes"),
            "{status}: {}",
            error.reason
        );
    }
}

#[test]
fn rename_without_its_new_path_is_rejected() {
    let error = parse_error(&record(
        &hex('1'),
        "",
        GOOD_DATE,
        "s",
        "\nR100\0src/old.rs\0",
    ));
    assert!(
        error.reason.contains("ends before a path"),
        "{}",
        error.reason
    );
}

#[test]
fn similarity_above_100_is_rejected() {
    let error = parse_error(&record(&hex('1'), "", GOOD_DATE, "s", "\nR101\0a\0b\0"));
    assert!(error.reason.contains("similarity"), "{}", error.reason);
}

#[test]
fn empty_path_is_rejected() {
    let error = parse_error(&record(&hex('1'), "", GOOD_DATE, "s", "\nA\0\0"));
    assert!(error.reason.contains("empty path"), "{}", error.reason);
}

#[test]
fn change_list_must_start_with_newline_and_end_with_nul() {
    let no_newline = parse_error(&record(&hex('1'), "", GOOD_DATE, "s", "A\0src/a.rs\0"));
    assert!(
        no_newline.reason.contains("start with a newline"),
        "{}",
        no_newline.reason
    );

    let no_nul = parse_error(&record(&hex('1'), "", GOOD_DATE, "s", "\nA\0src/a.rs"));
    assert!(
        no_nul.reason.contains("NUL-terminated"),
        "{}",
        no_nul.reason
    );
}

#[test]
fn header_without_its_nul_is_rejected() {
    let stdout = format!("\x1e{}\x1f{}\x1f\x1fAda", hex('1'), hex('f'));
    let error = parse_error(&stdout);
    assert!(
        error.reason.contains("not NUL-terminated"),
        "{}",
        error.reason
    );
}

#[test]
fn output_without_leading_separator_is_rejected() {
    let error = parse_error("garbage");
    assert_eq!(error.record, 1);
    assert!(
        error.reason.contains("record separator"),
        "{}",
        error.reason
    );
}

#[test]
fn trailing_separator_is_an_empty_record() {
    let stdout = format!("{}\x1e", record(&hex('1'), "", GOOD_DATE, "s", ""));
    let error = parse_error(&stdout);
    assert_eq!(error.record, 2);
}

#[test]
fn uppercase_sha_is_not_a_commit_id() {
    let stdout = record(&"A".repeat(40), "", GOOD_DATE, "s", "");
    let error = parse_error(&stdout);
    assert_eq!(error.sha, None);
    assert!(
        error.reason.contains("sha is not a commit id"),
        "{}",
        error.reason
    );
}

#[test]
fn parsed_commit_dates_compare_as_instants() {
    let (_dir, repo, scenario) = history_fixture();
    let history = read_history(repo.root()).unwrap();
    let first = history
        .commits
        .iter()
        .find(|c| c.oid == scenario.first)
        .unwrap();
    assert_eq!(
        first.author_date,
        chrono::DateTime::parse_from_rfc3339(DAY_1).unwrap()
    );
}

#[test]
fn current_head_is_none_for_an_unborn_repository() {
    let dir = tempfile::tempdir().unwrap();
    Repo::init(dir.path());
    assert_eq!(current_head(dir.path()).unwrap(), None);
}

#[test]
fn current_head_reads_the_checkout_head() {
    let (_dir, repo, _scenario) = history_fixture();
    assert_eq!(current_head(repo.root()).unwrap().as_deref(), Some(repo.rev("HEAD").as_str()));
}

#[test]
fn is_ancestor_follows_reachability() {
    let (dir, repo, scenario) = history_fixture();
    let head = repo.rev("HEAD");
    assert!(is_ancestor(repo.root(), &scenario.first, &head).unwrap());
    assert!(!is_ancestor(repo.root(), &head, &scenario.first).unwrap());
    // A rewritten tip: the old head keeps its object but loses reachability.
    let old_tip = head.clone();
    repo.amend(ada(DAY_1), "rewritten");
    assert!(!is_ancestor(repo.root(), &old_tip, &repo.rev("HEAD")).unwrap());
    drop(dir);
}

#[test]
fn a_resume_range_reads_only_the_new_commits() {
    let (dir, repo, _scenario) = history_fixture();
    let recorded = repo.rev("HEAD");
    repo.write("src/later.rs", "later\n");
    repo.commit(ada(DAY_1), "later: add", "");
    let head = repo.rev("HEAD");

    let window = Window {
        range: Some(format!("{recorded}..{head}")),
        ..Window::default()
    };
    let history = read_history_windowed(repo.root(), &window).unwrap();
    assert_eq!(history.commits.len(), 1);
    assert_eq!(history.commits[0].oid, head);
    assert_eq!(history.commits[0].parents, [recorded]);
    assert_eq!(history.head, head);
    // The refs come back whole: a resume run still refreshes them.
    assert_eq!(history.branches.len(), 4);

    // An empty range (nothing new) reads nothing.
    let same = Window {
        range: Some(format!("{head}..{head}")),
        ..Window::default()
    };
    assert!(read_history_windowed(repo.root(), &same).unwrap().commits.is_empty());
    drop(dir);
}

#[test]
fn a_max_count_window_bounds_the_walk() {
    let (_dir, repo, _scenario) = history_fixture();
    let window = Window {
        max_commits: Some(2),
        ..Window::default()
    };
    let history = read_history_windowed(repo.root(), &window).unwrap();
    assert_eq!(history.commits.len(), 2);
    assert_eq!(history.commits[0].oid, repo.rev("HEAD"));
    // The head is the checkout's, not the window's oldest commit.
    assert_eq!(history.head, repo.rev("HEAD"));
}

#[test]
fn a_window_value_that_poses_as_an_option_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    Repo::init(dir.path());
    let repo = Repo::wrap(dir.path());
    repo.write("a.txt", "a\n");
    repo.commit(ada(DAY_1), "first", "");
    for window in [
        Window {
            since: Some("--exec=touch /tmp/pwned".into()),
            ..Window::default()
        },
        Window {
            range: Some("HEAD --all".into()),
            ..Window::default()
        },
    ] {
        assert!(matches!(
            read_history_windowed(repo.root(), &window),
            Err(ReadError::Malformed(_))
        ));
    }
}
