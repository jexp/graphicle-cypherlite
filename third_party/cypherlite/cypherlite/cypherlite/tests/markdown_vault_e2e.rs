//! End-to-end: an Obsidian-style vault is a CypherLite database
//! (ADR-0406). External markdown files load as a queryable graph, Cypher
//! writes materialize as markdown files, and the same folder stays a
//! valid vault — with the engine sidecar hidden under `.cypherlite/`.

mod common;

use std::path::{Path, PathBuf};

use cypherlite::SnapshotCodecKind;
use cypherlite::{Database, Value};

use common::{opts, row, string};

/// The set-but-ignored refusal assertion the two contract tests share:
/// the error names the ignored-configuration refusal.
fn assert_refuses_ignored_config(err: cypherlite::Error) {
    assert!(
        err.to_string()
            .contains("the configuration would be ignored"),
        "{err}"
    );
}

/// The two-file vault fixture the roundtrip and the closed-engine-edit
/// tests share: `Person/Ada.md` (frontmatter plus body) and
/// `Person/Grace.md` (frontmatter only), both keyed by `name`.
fn seed_two_person_vault(root: &Path) -> PathBuf {
    let vault = root.join("vault");
    let person = vault.join("Person");
    std::fs::create_dir_all(&person).unwrap();
    std::fs::write(
        person.join("Ada.md"),
        "---\ntype: Person\nname: Ada\n---\n\nAnalytical engine pioneer.\n",
    )
    .unwrap();
    std::fs::write(
        person.join("Grace.md"),
        "---\ntype: Person\nname: Grace\n---\n",
    )
    .unwrap();
    vault
}

#[test]
fn vault_open_query_write_roundtrip_through_database() {
    let dir = tempfile::tempdir().unwrap();
    let vault = seed_two_person_vault(dir.path());
    let uri = vault.to_str().unwrap();

    {
        let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
        let mut conn = db.connect().unwrap();

        // The external files are the graph.
        let result = conn
            .query("MATCH (p:Person) RETURN p.name AS name ORDER BY name")
            .unwrap();
        assert_eq!(result.header.columns, vec!["name"]);
        assert_eq!(
            result.rows,
            vec![vec![string("Ada")], vec![string("Grace")]]
        );
        // The body is data.
        assert_eq!(
            row(
                &mut conn,
                "MATCH (p:Person {name: 'Ada'}) RETURN p.body AS body"
            ),
            vec![string("Analytical engine pioneer.")]
        );

        // Cypher writes materialize as markdown files.
        conn.query(
            "MATCH (a:Person {name: 'Ada'}) CREATE (a)-[:MENTORS {since: 1935}]->(b:Person {name: 'Mary'})",
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    // The vault now holds Mary and the relationship, still a valid vault.
    let mary = std::fs::read_to_string(vault.join("Person/Mary.md")).unwrap();
    assert!(mary.contains("type: Person"));
    assert!(mary.contains("name: Mary"));
    let ada = std::fs::read_to_string(vault.join("Person/Ada.md")).unwrap();
    assert!(
        ada.contains("MENTORS:\n  - since: 1935\n    target: \"[[Person/Mary]]\""),
        "relationship properties render beside the link: {ada}"
    );
    // The ADR-0406 §3 engine-sidecar paths come from the codec, not
    // hand-spelled literals.
    assert!(vault
        .join(SnapshotCodecKind::Markdown.checkpoint_image())
        .exists());
    assert!(vault.join(SnapshotCodecKind::Markdown.wal_dir()).exists());

    // Reopen: the graph is the vault, WAL replay included.
    let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        row(
            &mut conn,
            "MATCH (a:Person {name: 'Ada'})-[m:MENTORS]->(b) \
             RETURN b.name AS name, m.since AS since"
        ),
        vec![string("Mary"), Value::Int(1935)]
    );
}

/// A vault edit while the engine is closed lands at the next open: the
/// first block performs no writes, so the WAL stays at the checkpoint LSN
/// and the closed-engine edit is authoritative (ADR-0406 §5's rebase).
#[test]
fn closed_engine_vault_edit_lands_at_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let vault = seed_two_person_vault(dir.path());
    let uri = vault.to_str().unwrap();

    // Establish the guarded checkpoint; no writes follow it.
    {
        let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
        db.checkpoint().unwrap();
    }

    std::fs::write(
        vault.join("Person/Grace.md"),
        "---\ntype: Person\nname: Grace\nyears: 1906\n---\n",
    )
    .unwrap();
    let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        row(
            &mut conn,
            "MATCH (p:Person {name: 'Grace'}) RETURN p.years AS years"
        ),
        vec![Value::Int(1906)],
        "closed-engine vault edits are picked up"
    );
}

#[test]
fn vault_external_edits_with_dirty_wal_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let uri = vault.to_str().unwrap();
    std::fs::create_dir_all(vault.join("Person")).unwrap();
    std::fs::write(
        vault.join("Person/Ada.md"),
        "---\ntype: Person\nname: Ada\n---\n",
    )
    .unwrap();

    // Establish the baseline checkpoint, then edit a file while the
    // engine is open: the external-write guard at the next checkpoint
    // refuses with the close-edit-reopen advisory (ADR-0406 §5).
    let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
    db.checkpoint().unwrap();
    {
        let mut conn = db.connect().unwrap();
        conn.query("CREATE (:Note {name: 'scratch'})").unwrap();
    }
    std::fs::write(
        vault.join("Person/Ada.md"),
        "---\ntype: Person\nname: Ada\nyears: 1906\n---\n",
    )
    .unwrap();
    let err = db.checkpoint().unwrap_err();
    assert!(
        err.to_string()
            .contains("changed on disk while the database was open"),
        "{err}"
    );
}

/// ADR-0460 end-to-end: `OpenOptions.markdown_body_properties` selects
/// which properties render as the note body — the first bare, the rest
/// as `## <property>` sections — and the sidecar records the
/// configuration so the plain reopen (no options) keeps the layout.
#[test]
fn body_properties_flow_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let uri = vault.to_str().unwrap();

    {
        let db = Database::open_with(
            uri,
            cypherlite::OpenOptions {
                checkpoint_interval: Some(std::time::Duration::ZERO),
                snapshot_codec: Some(SnapshotCodecKind::Markdown),
                markdown_body_properties: Some(
                    cypherlite::MarkdownBodyProperties::parse("summary,notes").unwrap(),
                ),
                ..cypherlite::OpenOptions::default()
            },
        )
        .unwrap();
        let mut conn = db.connect().unwrap();
        conn.query(
            "CREATE (:Person {name: 'Ada', summary: 'The primary text.', \
             notes: 'Scratch thoughts.'})",
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    let ada = std::fs::read_to_string(vault.join("Person/Ada.md")).unwrap();
    assert_eq!(
        ada,
        "---\ntype: Person\nname: Ada\n---\n\nThe primary text.\n\n## notes\n\nScratch \
         thoughts.\n"
    );

    // The plain reopen — no options at all — reads the recorded
    // configuration from the sidecar (ADR-0460: the vault is
    // self-describing) and a hand edit to a section lands as data.
    std::fs::write(
        vault.join("Person/Ada.md"),
        "---\ntype: Person\nname: Ada\n---\n\nThe primary text.\n\n## notes\n\nEdited \
         while closed.\n",
    )
    .unwrap();
    let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        row(
            &mut conn,
            "MATCH (p:Person {name: 'Ada'}) RETURN p.summary AS summary, p.notes AS notes"
        ),
        vec![string("The primary text."), string("Edited while closed.")]
    );
}

/// A set body-property configuration on a non-markdown store fails the
/// open (the set-but-ignored contract), not a silent no-op.
#[test]
fn body_properties_on_a_non_markdown_store_fail_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let err = Database::open_with(
        dir.path().join("db.json-store").to_str().unwrap(),
        cypherlite::OpenOptions {
            snapshot_codec: Some(SnapshotCodecKind::Json),
            markdown_body_properties: Some(
                cypherlite::MarkdownBodyProperties::parse("summary").unwrap(),
            ),
            ..cypherlite::OpenOptions::default()
        },
    )
    .err()
    .expect("the open must refuse the ignored configuration");
    assert_refuses_ignored_config(err);
}

/// The fail-before-lease contract for the body-property configuration
/// (ADR-0460): a fresh directory with no codec flag resolves the build's
/// default codec, so the set-but-ignored refusal fires **before** the
/// lease — the failed open leaves no directory behind.
#[test]
fn a_body_property_config_on_a_fresh_non_markdown_open_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh-db");
    let err = Database::open_with(
        fresh.to_str().unwrap(),
        cypherlite::OpenOptions {
            markdown_body_properties: Some(
                cypherlite::MarkdownBodyProperties::parse("summary").unwrap(),
            ),
            ..cypherlite::OpenOptions::default()
        },
    )
    .err()
    .expect("the open must refuse the ignored configuration");
    assert_refuses_ignored_config(err);
    assert!(
        !fresh.exists(),
        "the failed open must not leave the store directory behind"
    );
}

/// The pre-lease refusal never blocks a sidecar-classified markdown
/// vault: an explicit body-property configuration on an existing vault
/// opens with **no** codec flag at all (the probe classifies the
/// directory), and replaces the recorded configuration.
#[test]
fn a_body_property_config_on_an_existing_markdown_vault_needs_no_codec_flag() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let uri = vault.to_str().unwrap();
    {
        let db = Database::open_with(uri, opts(SnapshotCodecKind::Markdown)).unwrap();
        db.checkpoint().unwrap();
    }
    // No snapshot_codec: the directory's sidecar is the evidence.
    let db = Database::open_with(
        uri,
        cypherlite::OpenOptions {
            checkpoint_interval: Some(std::time::Duration::ZERO),
            markdown_body_properties: Some(
                cypherlite::MarkdownBodyProperties::parse("summary").unwrap(),
            ),
            ..cypherlite::OpenOptions::default()
        },
    )
    .unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("CREATE (:Person {name: 'Ada', summary: 'The primary text.'})")
        .unwrap();
    db.checkpoint().unwrap();
    let ada = std::fs::read_to_string(vault.join("Person/Ada.md")).unwrap();
    assert_eq!(
        ada,
        "---\ntype: Person\nname: Ada\n---\n\nThe primary text.\n"
    );
}
