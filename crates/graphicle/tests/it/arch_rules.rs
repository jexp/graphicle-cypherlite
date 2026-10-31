//! The rule engine end to end (architecture-concepts.md §1.5, §2): a rule
//! document parses into rule nodes with their exact properties; a malformed
//! block and a requires cycle are reported with the block or the rules named;
//! applying a rule set twice to one episode changes nothing; rolling one rule
//! back removes exactly its writes; a constraint that finds rows writes
//! violations with severity and episode; and `graphicle analyze` exits 1 on a
//! gated violation and 0 on a clean or ungated run.
//!
//! The rule documents here are authored inline in temporary projects, so the
//! suite never reads the repository's own `docs/architecture/`.
use graphicle_runtime::{
    arch_rules::{self, Severity, analyze},
    limits,
    local,
    platform::CanonicalExt,
    store::s,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

use crate::arch::Fixture;

/// The fixture program's folders: the system's, and no tool of the machine
/// running the tests.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Labels every symbol, then a constraint that finds the unlabelled ones:
/// after the concept runs it finds none (passing).
const LABEL_RULES: &str = "# Architecture rules\n\n\
````concept\nid: arch:Test/Label\nseverity: MINOR\ndescription: Every symbol is test material\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s:ArchTest\n```\n````\n\n\
````constraint\nid: arch:Test/Bare\nseverity: MAJOR\nrequires: [arch:Test/Label]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE NOT s:ArchTest RETURN s\n```\n````\n";

/// Three independent concepts, for the rollback: a label, a property and a
/// node the rule creates.
const MANY_RULES: &str = "# Several rules\n\n\
````concept\nid: arch:Test/Label\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s:ArchTest\n```\n````\n\n\
````concept\nid: arch:Test/Mark\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.arch_mark = 'yes'\n```\n````\n\n\
````concept\nid: arch:Test/Note\n```cypher\nCREATE (n:ArchNote {repository_id: $repo, text: 'note'})\n```\n````\n";

/// A constraint that finds every symbol of the fixture's `src/lib.rs`
/// (MAJOR), and one that finds nothing (MINOR).
const VIOLATING_RULES: &str = "# Violations\n\n\
````constraint\nid: arch:Test/NoRun\nseverity: MAJOR\ndescription: The run function is not allowed\n```cypher\nMATCH (s:Symbol {repository_id: $repo, relpath: 'src/lib.rs'}) RETURN s\n```\n````\n\n\
````constraint\nid: arch:Test/Nothing\nseverity: MINOR\n```cypher\nMATCH (s:Symbol {repository_id: $repo, relpath: 'no/such/file.rs'}) RETURN s\n```\n````\n";

/// Two concepts that require each other.
const CYCLE_RULES: &str = "# Cycle\n\n\
````concept\nid: arch:Cycle/A\nrequires: [arch:Cycle/B]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n\n\
````concept\nid: arch:Cycle/B\nrequires: [arch:Cycle/A]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n";

/// The suite's own probes over the shared fixture: the current episode, the
/// enrich job's result, the store state the rules wrote, and the analysis.
impl Fixture {
    /// The episode the project is at (the latest recorded).
    fn episode(&self) -> String {
        self.query(
            "MATCH (e:Episode {repository_id: $repo}) RETURN e.episode AS episode ORDER BY e.sequence DESC",
            json!({}),
        )[0]["episode"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The result of the project's enrich job, decoded.
    fn enrich_result(&self) -> Value {
        let row = self
            .query(
                "MATCH (j:Job {repository_id: $repo, kind: 'enrich'}) RETURN j.result AS result",
                json!({}),
            )
            .into_iter()
            .next()
            .expect("the enrich job was queued");
        serde_json::from_str(s(&row, "result")).unwrap()
    }

    /// The store state the rules wrote, compared before and after a
    /// re-application: the labelled symbols, the application records with
    /// their provenance, the violations, and the jobs.
    fn state(&self) -> Value {
        json!({
            "labelled": self.count("MATCH (s:Symbol:ArchTest {repository_id: $repo}) RETURN count(s) AS n"),
            "marked": self.count("MATCH (s:Symbol {repository_id: $repo}) WHERE s.arch_mark IS NOT NULL RETURN count(s) AS n"),
            "notes": self.count("MATCH (n:ArchNote {repository_id: $repo}) RETURN count(n) AS n"),
            "labeled_edges": self.count("MATCH (:RuleApplication {repository_id: $repo})-[:LABELED]->(s:Symbol) RETURN count(s) AS n"),
            "applications": self.query(
                "MATCH (a:RuleApplication {repository_id: $repo}) \
                 RETURN a.rule AS rule, a.episode AS episode, a.provenance AS provenance \
                 ORDER BY rule, episode",
                json!({}),
            ),
            "violations": self.query(
                "MATCH (v:Violation {repository_id: $repo}) \
                 RETURN v.rule AS rule, v.episode AS episode, v.severity AS severity, \
                        v.path AS path, v.line AS line \
                 ORDER BY rule, path, line",
                json!({}),
            ),
            "jobs": self.query(
                "MATCH (j:Job {repository_id: $repo}) RETURN j.kind AS kind, j.state AS state, \
                        j.input_hash AS hash ORDER BY kind, hash",
                json!({}),
            ),
        })
    }

    /// The analysis of the project at its current episode, as the command
    /// computes it (the document errors of `root` come first).
    fn analyze(&self, min: Option<Severity>) -> Value {
        analyze::run(
            &self.store,
            &self.session,
            &self.repo,
            Some(&self.root),
            &analyze::Request { group: None, min },
            limits::current().file_bytes,
        )
        .unwrap()
    }
}

#[test]
fn a_rule_document_parses_into_rules_with_exact_properties() {
    let text = "# Rules\n\n\
````concept\nid: arch:Test/Base\n```cypher\nMATCH (s:Symbol) RETURN s\n```\n````\n\n\
````concept\nid: arch:Test/Label\nrequires: [arch:Test/Base, arch:Test/Layer/*]\nseverity: MINOR\ndescription: Labels the symbols\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s:ArchTest\n```\n````\n\n\
````constraint\nid: arch:Test/NoBare\nseverity: CRITICAL\nrequires: [arch:Test/Label]\ndescription: Bare symbols\n```cypher\nMATCH (s:Symbol) WHERE NOT s:ArchTest RETURN s\n```\n````\n\n\
````group\nid: arch:default\ninclude: [arch:Test/*]\noverrides: [arch:Test/NoBare=MAJOR]\ndescription: The test pack\n````\n";

    // The parse itself: every field of every rule, and the 1-based line of
    // each block's opening fence.
    let document = arch_rules::parse(text).unwrap();
    assert_eq!(document.concepts.len(), 2);
    assert_eq!(document.constraints.len(), 1);
    assert_eq!(document.groups.len(), 1);
    let label = &document.concepts[1];
    assert_eq!(label.id, "arch:Test/Label");
    assert_eq!(label.requires, vec!["arch:Test/Base", "arch:Test/Layer/*"]);
    assert_eq!(label.severity, Severity::Minor);
    assert_eq!(label.description, "Labels the symbols");
    assert_eq!(label.cypher, "MATCH (s:Symbol {repository_id: $repo}) SET s:ArchTest");
    assert_eq!(label.line, 10);
    let constraint = &document.constraints[0];
    assert_eq!(constraint.id, "arch:Test/NoBare");
    assert_eq!(constraint.severity, Severity::Critical);
    assert_eq!(constraint.requires, vec!["arch:Test/Label"]);
    assert_eq!(constraint.line, 20);
    let group = &document.groups[0];
    assert_eq!(group.id, "arch:default");
    assert_eq!(group.include, vec!["arch:Test/*"]);
    assert_eq!(group.overrides, vec![("arch:Test/NoBare".to_string(), Severity::Major)]);
    assert_eq!(group.description, "The test pack");

    // Synced, the same rules are nodes whose properties hold the same
    // values: the requires list as its JSON text, the severity as its name.
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", text);
    let synced = f.sync();
    assert_eq!(synced["rules"]["parsed"], json!(1), "{synced:#}");
    let node = f.query(
        "MATCH (r:ConceptRule {repository_id: $repo, id: 'arch:Test/Label'}) \
         RETURN r.description AS description, r.requires AS requires, \
                r.severity AS severity, r.line AS line, r.source AS source",
        json!({}),
    );
    assert_eq!(node.len(), 1, "{node:#?}");
    assert_eq!(node[0]["description"], json!("Labels the symbols"));
    assert_eq!(node[0]["requires"], json!(r#"["arch:Test/Base","arch:Test/Layer/*"]"#));
    assert_eq!(node[0]["severity"], json!("MINOR"));
    assert_eq!(node[0]["line"], json!(10));
    assert_eq!(node[0]["source"], json!("docs/architecture/rules.md"));
    let group = f.query(
        "MATCH (g:RuleGroup {repository_id: $repo, id: 'arch:default'}) \
         RETURN g.include AS include, g.overrides AS overrides",
        json!({}),
    );
    assert_eq!(group[0]["include"], json!(r#"["arch:Test/*"]"#));
    assert_eq!(group[0]["overrides"], json!(r#"["arch:Test/NoBare=MAJOR"]"#));
}

#[test]
fn a_malformed_block_is_reported_by_name_and_writes_no_rules() {
    let mut f = Fixture::new();
    // The first block is well formed; the second names its id but has no
    // cypher body, so the whole document is refused.
    f.write(
        "docs/architecture/rules.md",
        "````concept\nid: arch:Test/Fine\n```cypher\nMATCH (s:Symbol) RETURN s\n```\n````\n\n\
         ````concept\nid: arch:Test/Bad\nseverity: MINOR\n````\n",
    );
    let synced = f.sync();
    let skipped = &synced["rules"]["skipped"];
    assert_eq!(skipped[0]["path"], json!("docs/architecture/rules.md"), "{synced:#}");
    let reason = skipped[0]["reason"].as_str().unwrap();
    assert!(reason.contains("'arch:Test/Bad'"), "{reason}");
    assert!(reason.contains("missing cypher body"), "{reason}");
    assert!(reason.contains("line 8"), "{reason}");
    assert_eq!(f.count("MATCH (r:ConceptRule) RETURN count(r) AS n"), 0);

    // The same error is what the analysis reports first, and it is an error
    // (exit 2), not a clean run.
    let errors = arch_rules::document_errors(&f.root, limits::current().file_bytes);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("docs/architecture/rules.md: "), "{errors:?}");
    let report = f.analyze(None);
    assert_eq!(report["exit"], json!(2), "{report:#}");
    assert!(
        report["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("'arch:Test/Bad'")),
        "{report:#}"
    );
}

#[test]
fn a_requires_cycle_is_rejected_with_its_rules_named() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", CYCLE_RULES);
    // The document parses (a cycle is a resolution error, not a parse one),
    // so the sync writes its rules; the build's drain then runs the enrich job.
    let synced = f.sync();
    assert_eq!(synced["rules"]["parsed"], json!(1), "{synced:#}");
    local::build_graph(&f.store, &f.session, &f.repo, false).unwrap();
    assert_eq!(f.count("MATCH (r:ConceptRule {repository_id: $repo}) RETURN count(r) AS n"), 2);

    // The job's plan is the error: it names both rules, applies nothing.
    let result = f.enrich_result();
    let error = result["plans"][0]["error"].as_str().unwrap();
    assert_eq!(error, "rule cycle through 'arch:Cycle/A', 'arch:Cycle/B'", "{result:#}");
    assert_eq!(result["applied"], json!(0), "{result:#}");
    assert_eq!(f.count("MATCH (a:RuleApplication) RETURN count(a) AS n"), 0);

    // The analysis surfaces the same resolution error and exits 2.
    let report = f.analyze(None);
    assert_eq!(report["exit"], json!(2), "{report:#}");
    let errors = report["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| {
            let e = e.as_str().unwrap();
            e.contains("'arch:Cycle/A'") && e.contains("'arch:Cycle/B'") && e.contains("rule cycle")
        }),
        "{report:#}"
    );
}

#[test]
fn applying_a_rule_set_twice_to_one_episode_changes_nothing() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", LABEL_RULES);
    f.sync_and_build();
    let episode = f.episode();
    let first = f.state();
    assert!(first["labelled"].as_i64().unwrap() > 0, "{first:#}");
    assert_eq!(first["violations"], json!([]), "the constraint finds nothing once labelled");

    // The integrated path again: a sync of the same tree is the same episode,
    // so the completed job replays and the store is what it was.
    f.sync_and_build();
    assert_eq!(f.episode(), episode);
    assert_eq!(f.state(), first, "a replayed sync changes nothing");

    // The same episode applied a second time: the executor and the
    // constraints, called with the arguments the enrich job passes. A
    // completed job replays, so this is the application itself.
    let graph = f.graph();
    let never = || Ok(());
    let run = arch_rules::enrich(&graph, &f.repo, &episode, &never).unwrap();
    let constraints =
        arch_rules::constraints::evaluate(&graph, &f.repo, &episode, &run, &never).unwrap();
    let (report, constraints) = (run.to_value(), constraints.to_value());
    assert_eq!(report["applied"], json!(1), "{report:#}");
    assert_eq!(constraints["violations"], json!(0), "{constraints:#}");
    assert_eq!(f.state(), first, "the second application left the store as it was");
}

#[test]
fn rolling_back_one_rule_removes_exactly_its_writes() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", MANY_RULES);
    f.sync_and_build();
    let episode = f.episode();
    let symbols = f.count("MATCH (s:Symbol {repository_id: $repo}) RETURN count(s) AS n");
    assert!(symbols > 0);
    assert_eq!(f.count("MATCH (s:Symbol:ArchTest {repository_id: $repo}) RETURN count(s) AS n"), symbols);
    assert_eq!(f.count("MATCH (s:Symbol {repository_id: $repo}) WHERE s.arch_mark IS NOT NULL RETURN count(s) AS n"), symbols);
    assert_eq!(f.count("MATCH (n:ArchNote {repository_id: $repo}) RETURN count(n) AS n"), 1);
    assert_eq!(f.count("MATCH (a:RuleApplication {repository_id: $repo}) RETURN count(a) AS n"), 3);

    // Roll back the property rule through the store, as the executor's
    // rollback is called for one (rule, episode).
    let removed = arch_rules::rollback(&f.graph(), &f.repo, "arch:Test/Mark", &episode).unwrap();
    assert_eq!(removed["properties"], json!(symbols), "{removed:#}");
    assert_eq!(removed["labels"], json!(0), "{removed:#}");
    assert_eq!(removed["nodes"], json!(0), "{removed:#}");

    // Its writes are gone and nothing else moved: the property is off every
    // symbol, its application record is gone, and the other two rules'
    // labels, records and node are as they were.
    assert_eq!(f.count("MATCH (s:Symbol {repository_id: $repo}) WHERE s.arch_mark IS NOT NULL RETURN count(s) AS n"), 0);
    assert_eq!(
        f.count("MATCH (a:RuleApplication {repository_id: $repo, rule: 'arch:Test/Mark'}) RETURN count(a) AS n"),
        0
    );
    assert_eq!(f.count("MATCH (s:Symbol:ArchTest {repository_id: $repo}) RETURN count(s) AS n"), symbols);
    assert_eq!(f.count("MATCH (n:ArchNote {repository_id: $repo}) RETURN count(n) AS n"), 1);
    assert_eq!(
        f.count("MATCH (a:RuleApplication {repository_id: $repo, rule: 'arch:Test/Label'}) RETURN count(a) AS n"),
        1
    );
    assert_eq!(
        f.count("MATCH (a:RuleApplication {repository_id: $repo, rule: 'arch:Test/Note'}) RETURN count(a) AS n"),
        1
    );

    // A rule never applied to the episode rolls back nothing.
    let none = arch_rules::rollback(&f.graph(), &f.repo, "arch:Test/Missing", &episode).unwrap();
    assert_eq!(none["properties"], json!(0), "{none:#}");
}

#[test]
fn a_constraint_that_finds_rows_writes_violations_with_severity_and_episode() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", VIOLATING_RULES);
    f.sync_and_build();
    let episode = f.episode();

    // The MAJOR constraint's rows, one violation each, carrying the episode
    // and the severity the document gave it; the constraint that finds
    // nothing writes none.
    let violations = f.query(
        "MATCH (v:Violation {repository_id: $repo}) \
         RETURN v.rule AS rule, v.episode AS episode, v.severity AS severity, v.path AS path \
         ORDER BY rule, path",
        json!({}),
    );
    assert!(!violations.is_empty(), "the constraint finds the fixture's symbols");
    for row in &violations {
        assert_eq!(row["rule"], json!("arch:Test/NoRun"), "{row:#}");
        assert_eq!(row["episode"], json!(episode), "{row:#}");
        assert_eq!(row["severity"], json!("MAJOR"), "{row:#}");
        assert_eq!(row["path"], json!("src/lib.rs"), "{row:#}");
    }
    assert_eq!(f.count("MATCH (v:Violation {rule: 'arch:Test/Nothing'}) RETURN count(v) AS n"), 0);

    // The analysis reports the same rows, in the episode's report, with the
    // gate applied: a MAJOR gate sees them, a BLOCKER gate does not.
    let gated = f.analyze(Some(Severity::Major));
    assert_eq!(gated["episode"], json!(episode), "{gated:#}");
    assert_eq!(gated["violations"].as_array().unwrap().len(), violations.len(), "{gated:#}");
    assert_eq!(gated["violations"][0]["severity"], json!("MAJOR"), "{gated:#}");
    assert_eq!(gated["violations"][0]["path"], json!("src/lib.rs"), "{gated:#}");
    assert_eq!(gated["exit"], json!(1), "{gated:#}");
    let blocker = f.analyze(Some(Severity::Blocker));
    assert_eq!(blocker["violations"].as_array().unwrap().len(), violations.len(), "{blocker:#}");
    assert_eq!(blocker["exit"], json!(0), "{blocker:#}");
}

/// The `graphicle` program in a temporary HOME, with its own data folder and
/// a project folder: the command-line half of the suite.
struct Cli {
    _dir: TempDir,
    root: PathBuf,
}

impl Cli {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonical().unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        std::fs::write(root.join("project/src/lib.rs"), "pub fn run() {}\n").unwrap();
        Self { _dir: dir, root }
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn write_rules(&self, text: &str) {
        let folder = self.project().join("docs/architecture");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rules.md"), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.root.join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(self.project())
            .arg("--data-dir")
            .arg(self.root.join("data"))
            .args(args);
        crate::child_env::isolate(&mut command, &home, &[], PATH)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("GRAPHICLE_NO_BROWSER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmp);
        }
        crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap()
    }

    /// `graphicle analyze` in JSON, and its exit code.
    fn analyze(&self, extra: &[&str]) -> (i32, Value) {
        let mut args = vec!["analyze", "--format", "json"];
        args.extend_from_slice(extra);
        let output = self.run(&args);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "analyze printed no JSON ({error}): stderr {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        let code = output.status.code().expect("analyze exits with a code");
        assert_eq!(report["exit"], json!(code), "{report:#}");
        (code, report)
    }
}

#[test]
fn analyze_exits_1_on_a_gated_violation_and_0_when_clean_or_ungated() {
    let cli = Cli::new();
    cli.write_rules(VIOLATING_RULES);
    let synced = cli.run(&["sync"]);
    assert!(synced.status.success(), "{}", String::from_utf8_lossy(&synced.stderr));

    // A MAJOR violation gated at MAJOR: exit 1.
    let (code, gated) = cli.analyze(&["--min-severity", "MAJOR"]);
    assert_eq!(code, 1, "{gated:#}");
    assert!(gated["counts"]["violations"].as_i64().unwrap() > 0, "{gated:#}");
    assert_eq!(gated["counts"]["by_severity"]["MAJOR"], gated["counts"]["violations"], "{gated:#}");

    // The same violations, ungated: reported, and exit 0.
    let (code, ungated) = cli.analyze(&[]);
    assert_eq!(code, 0, "{ungated:#}");
    assert_eq!(ungated["counts"]["violations"], gated["counts"]["violations"], "{ungated:#}");

    // A gate above every violation is clean: exit 0.
    let (code, clean) = cli.analyze(&["--min-severity", "BLOCKER"]);
    assert_eq!(code, 0, "{clean:#}");

    // A project whose constraints find nothing is clean at any gate.
    let quiet = Cli::new();
    quiet.write_rules(LABEL_RULES);
    assert!(quiet.run(&["sync"]).status.success());
    let (code, report) = quiet.analyze(&["--min-severity", "MINOR"]);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["counts"]["violations"], json!(0), "{report:#}");
}
