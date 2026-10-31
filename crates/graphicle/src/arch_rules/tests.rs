//! Both frontends through the shared entry points. The fixtures under
//! `tests/fixtures/arch_rules/jqa-git` declare the same rules twice: once in
//! jQAssistant's AsciiDoc dialect (`git.adoc`, with `include::`) and once in
//! the Markdown layout (`mirror.md`).
use super::*;
use crate::store_graph::{self, GraphStore, Params};
use serde_json::json;
use std::path::{Path, PathBuf};

/// The repository id the rule engine's in-memory fixtures share.
pub(crate) const REPO: &str = "repo";

/// An in-memory store holding `rules` (a Markdown rule document) and the
/// symbols the rules work on: the fixture of the executor's, the
/// provenance's and the constraints' tests.
pub(crate) fn rules_graph(rules: &str, symbols: &[&str]) -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    let document = parse(rules).unwrap();
    store(&graph, REPO, "rules.md", &document).unwrap();
    add_symbols(&graph, symbols);
    graph
}

/// The symbols `ids` as `src/lib.rs` functions of [`REPO`].
pub(crate) fn add_symbols(graph: &GraphStore, ids: &[&str]) {
    graph
        .transaction(|db| {
            for id in ids {
                store_graph::write(
                    db,
                    "CREATE (s:Symbol {repository_id: $repo, stable_id: $id, \
                     kind: 'function', qualified_name: $id, relpath: 'src/lib.rs'})",
                    store_graph::params(&json!({"repo": REPO, "id": id})),
                )?;
            }
            Ok(())
        })
        .unwrap();
}

/// The first row's `n` of a `$repo`-scoped count query.
pub(crate) fn count(graph: &GraphStore, cypher: &str) -> i64 {
    graph
        .read(|db| store_graph::rows(db, cypher, store_graph::params(&json!({"repo": REPO}))))
        .unwrap()[0]["n"]
        .as_i64()
        .unwrap()
}

/// The rules the engine's tests share: a label, a relationship, a property
/// and a created node, one rule each, plus a constraint.
pub(crate) const SHARED: &str = "````concept\nid: arch:Test/Label\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE s.stable_id STARTS WITH 'db'\nSET s:Persistence\n```\n````\n\
````concept\nid: arch:Test/Connect\nrequires: [arch:Test/Label]\n```cypher\nMATCH (a:Symbol {repository_id: $repo, stable_id: 'db_one'}), (b:Symbol {repository_id: $repo, stable_id: 'db_two'})\nMERGE (a)-[:PERSISTS_IN]->(b)\n```\n````\n\
````concept\nid: arch:Test/Confidence\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.arch_confidence = 0.8\n```\n````\n\
````concept\nid: arch:Test/Layer\n```cypher\nMERGE (l:Layer {repository_id: $repo, name: 'Persistence'})\n```\n````\n\
````constraint\nid: arch:Test/NoCycles\nseverity: BLOCKER\nrequires: [arch:Test/Connect]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/arch_rules/jqa-git")
        .join(name)
}

/// The document with block line numbers cleared: two frontends place the same
/// rule at different lines, so the comparison is on content.
fn without_lines(mut document: RuleDocument) -> RuleDocument {
    for rule in document
        .concepts
        .iter_mut()
        .chain(document.constraints.iter_mut())
    {
        rule.line = 0;
    }
    for group in &mut document.groups {
        group.line = 0;
    }
    document
}

fn rule<'a>(document: &'a RuleDocument, id: &str) -> &'a Rule {
    document
        .concepts
        .iter()
        .chain(document.constraints.iter())
        .find(|rule| rule.id == id)
        .unwrap_or_else(|| panic!("no rule {id}"))
}

fn adoc(text: &str) -> RuleError {
    parse_adoc(text, None).expect_err("document should be rejected")
}

#[test]
fn adoc_fixture_and_markdown_mirror_declare_the_same_rules() {
    let from_adoc = parse_file(&fixture("git.adoc")).unwrap();
    let from_markdown = parse_file(&fixture("mirror.md")).unwrap();

    assert_eq!(from_adoc.concepts.len(), 5);
    assert_eq!(from_adoc.constraints.len(), 2);
    assert_eq!(from_adoc.groups.len(), 1);
    assert_eq!(
        without_lines(from_adoc.clone()),
        without_lines(from_markdown),
        "the two frontends must produce the same rule structs"
    );
    assert_eq!(
        from_adoc.groups[0].overrides,
        vec![("fixture-git:ChurnPerFile".to_string(), Severity::Major)]
    );
}

#[test]
fn adoc_spells_severity_and_requires_as_jqa_does() {
    let document = parse_file(&fixture("git.adoc")).unwrap();

    // `severity=MINOR` as a bare attribute, `severity="MAJOR"` quoted.
    assert_eq!(
        rule(&document, "fixture-git:CommitsPerMonth").severity,
        Severity::Minor
    );
    assert_eq!(
        rule(&document, "fixture-git:BusFactorRisk").severity,
        Severity::Major
    );
    // No severity on a concept: INFO.
    assert_eq!(
        rule(&document, "fixture-git:PersonsByEmail").severity,
        Severity::Info
    );

    // `requiresConcept` (one reference) and `requiresConcepts` (a list).
    assert_eq!(
        rule(&document, "fixture-git:BusFactorRisk").requires,
        vec!["fixture-git:ChurnPerFile"]
    );
    assert_eq!(
        rule(&document, "fixture-git:CoupledFiles").requires,
        vec!["fixture-git:ChurnPerFile", "fixture-git:CommitsWithParents"]
    );
    // A jQA `role="concept"` (quoted) and `verify=` are read.
    assert_eq!(
        rule(&document, "fixture-git:ChurnPerFile").description,
        "Count the commits that change each file."
    );
}

#[test]
fn adoc_example_in_the_format_doc_parses() {
    // The AsciiDoc example in docs/architecture-rules.md must parse as shown.
    let doc = include_str!("../../../../docs/architecture-rules.md");
    let start =
        doc.find("```asciidoc\n[[arch:Layer/Persistence]]").unwrap() + "```asciidoc\n".len();
    let end = start + doc[start..].find("\n```\n").unwrap();
    let example = parse_adoc(&doc[start..end], None).unwrap();
    assert_eq!(example.concepts.len(), 1);
    assert_eq!(example.constraints.len(), 1);
    assert_eq!(example.groups.len(), 1);
    assert_eq!(example.concepts[0].severity, Severity::Minor);
    assert_eq!(example.concepts[0].requires, vec!["arch:Module/*"]);
    assert_eq!(
        example.groups[0].overrides,
        vec![("arch:NoCycles".to_string(), Severity::Major)]
    );
}

#[test]
fn adoc_skips_listings_without_a_role_and_comments() {
    // The fixture's unroled listing and the commented-out block declare no rule.
    let document = parse_file(&fixture("git.adoc")).unwrap();
    let ids: Vec<&str> = document
        .concepts
        .iter()
        .chain(document.constraints.iter())
        .map(|rule| rule.id.as_str())
        .collect();
    assert!(!ids.iter().any(|id| id.contains("MATCH")));
    assert_eq!(ids.len(), 7);
}

#[test]
fn store_writes_the_same_nodes_from_either_frontend() {
    let graph = GraphStore::open_in_memory().unwrap();
    let from_adoc = parse_file(&fixture("git.adoc")).unwrap();
    let from_markdown = parse_file(&fixture("mirror.md")).unwrap();
    store(&graph, "repo-a", "git.adoc", &from_adoc).unwrap();
    store(&graph, "repo-b", "mirror.md", &from_markdown).unwrap();

    for label in ["ConceptRule", "ConstraintRule", "RuleGroup"] {
        let rows = graph
            .read(|db| {
                store_graph::rows(
                    db,
                    &format!("MATCH (r:{label}) RETURN r.repository_id AS repo, count(r) AS n ORDER BY repo"),
                    Params::new(),
                )
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "{label}: {rows:?}");
        assert_eq!(
            rows[0]["n"], rows[1]["n"],
            "{label} counts match across frontends"
        );
    }

    let sha = |repo: &str| {
        graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (r:ConceptRule {id: 'fixture-git:ChurnPerFile'}) \
                     WHERE r.repository_id = $repo RETURN r.rule_sha AS sha, r.severity AS severity",
                    store_graph::params(&json!({ "repo": repo })),
                )
            })
            .unwrap()
            .expect("concept node")
    };
    assert_eq!(sha("repo-a")["sha"], sha("repo-b")["sha"]);
    assert_eq!(
        sha("repo-a")["sha"],
        json!(rule(&from_adoc, "fixture-git:ChurnPerFile").rule_sha)
    );
}

#[test]
fn store_merges_rules_per_project() {
    let graph = GraphStore::open_in_memory().unwrap();
    let document = parse_file(&fixture("git.adoc")).unwrap();
    store(&graph, "repo-a", "git.adoc", &document).unwrap();
    store(&graph, "repo-a", "git.adoc", &document).unwrap();
    store(&graph, "repo-b", "git.adoc", &document).unwrap();

    let counts = graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (r:ConceptRule) RETURN r.repository_id AS repo, count(r) AS n ORDER BY repo",
                Params::new(),
            )
        })
        .unwrap();
    assert_eq!(counts.len(), 2);
    assert_eq!(counts[0]["n"], json!(5));
    assert_eq!(counts[1]["n"], json!(5));
}

#[test]
fn parse_file_dispatches_on_extension() {
    let not_a_rule_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let error = parse_file(&not_a_rule_file).unwrap_err();
    assert!(
        error.to_string().contains("expected .md or .adoc"),
        "{error}"
    );
}

#[test]
fn adoc_rejects_unknown_and_repeated_attributes() {
    let unknown =
        adoc("[[arch:X]]\n[source,cypher,role=concept,severty=INFO]\n----\nRETURN 1\n----\n");
    assert_eq!(unknown.kind, "concept");
    assert_eq!(unknown.line, 1, "a block is reported at its first line");
    assert!(
        unknown.to_string().contains("unknown attribute 'severty'"),
        "{unknown}"
    );

    let repeated = adoc(
        "[[arch:X]]\n[source,cypher,role=constraint,severity=INFO,severity=MAJOR]\n----\nRETURN 1\n----\n",
    );
    assert!(
        repeated.to_string().contains("'severity' repeated"),
        "{repeated}"
    );
}

#[test]
fn adoc_rejects_bodies_and_references_it_cannot_honour() {
    let sql = adoc("[[arch:X]]\n[source,sql,role=concept]\n----\nSELECT 1\n----\n");
    assert!(sql.to_string().contains("[source,cypher] listing"), "{sql}");

    let reference =
        adoc("[[arch:X]]\n[source,cypher,role=concept,severity={level}]\n----\nRETURN 1\n----\n");
    assert!(
        reference.to_string().contains("attribute reference"),
        "{reference}"
    );

    let unclosed = adoc("[[arch:X]]\n[source,cypher,role=concept]\n----\nRETURN 1\n");
    assert_eq!(unclosed.reason, Reason::Unclosed);
    assert_eq!(unclosed.line, 3);

    let no_listing = adoc("[[arch:X]]\n[role=constraint,severity=INFO]\n\nJust prose.\n");
    assert_eq!(no_listing.reason, Reason::MissingBody);
}

#[test]
fn adoc_rejects_disagreeing_anchor_and_id_and_duplicates() {
    let disagree =
        adoc("[[arch:X]]\n[source,cypher,role=concept,id=arch:Y]\n----\nRETURN 1\n----\n");
    assert!(disagree.to_string().contains("disagree"), "{disagree}");

    let duplicate = adoc(
        "[[arch:X]]\n[source,cypher,role=concept]\n----\nRETURN 1\n----\n\n\
         [[arch:X]]\n[source,cypher,role=concept]\n----\nRETURN 2\n----\n",
    );
    assert_eq!(duplicate.reason, Reason::DuplicateId);
    assert_eq!(duplicate.id.as_deref(), Some("arch:X"));
}

#[test]
fn adoc_refuses_conditionals_and_includes_without_a_folder() {
    let conditional = adoc(
        "ifdef::env-github[]\n[[arch:X]]\n[source,cypher,role=concept]\n----\nRETURN 1\n----\nendif::[]\n",
    );
    assert!(
        conditional.to_string().contains("conditional directives"),
        "{conditional}"
    );

    let include = adoc("include::queries/churn.adoc[]\n");
    assert!(
        include.to_string().contains("needs the document's folder"),
        "{include}"
    );
}

#[test]
fn adoc_names_the_included_file_of_a_broken_block() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("broken.adoc"),
        "[[arch:Broken]]\n[source,cypher,role=constraint]\n----\nRETURN 1\n----\n",
    )
    .unwrap();
    let main = dir.path().join("main.adoc");
    std::fs::write(&main, "= Rules\n\ninclude::broken.adoc[]\n").unwrap();

    let error = parse_file(&main).unwrap_err();
    assert_eq!(error.kind, "constraint");
    assert_eq!(error.id.as_deref(), Some("arch:Broken"));
    assert_eq!(error.line, 1);
    assert!(
        error.file.as_deref().unwrap().ends_with("broken.adoc"),
        "{error:?}"
    );
    assert!(error.to_string().contains("broken.adoc"), "{error}");
}

#[test]
fn adoc_reports_a_missing_include() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.adoc");
    std::fs::write(&main, "include::gone.adoc[]\n").unwrap();
    let error = parse_file(&main).unwrap_err();
    assert!(error.to_string().contains("cannot read include"), "{error}");
}
