//! The ADR-0460 body-properties suite: the SPEC grammar, the
//! first-bare-then-sections render, sidecar recording and the
//! config-free reopen, replace semantics, and the guard refusals.

use super::*;

/// The body-properties suite's graph: a Person carrying the default
/// list's two properties, a Person carrying only the later one, and a
/// Book carrying its per-label list.
fn body_graph() -> Graph {
    let graph = Graph::default();
    for (id, labels, properties) in [
        (
            1,
            vec!["Person".to_string()],
            props(&[
                ("name", string("Ada")),
                ("summary", string("The primary text.")),
                ("notes", string("Scratch thoughts.")),
            ]),
        ),
        (
            2,
            vec!["Person".to_string()],
            props(&[("name", string("Grace")), ("notes", string("Only notes."))]),
        ),
        (
            3,
            vec!["Book".to_string()],
            props(&[
                ("title", string("Dune")),
                ("summary", string("A desert novel.")),
                ("review", string("Kept the spice flowing.")),
            ]),
        ),
    ] {
        graph
            .load_checkpoint_node(Node {
                id,
                labels: Arc::new(labels),
                properties,
            })
            .unwrap();
    }
    graph.set_next_node_id(4);
    graph.set_next_rel_id(1);
    graph.set_commit_ts(7);
    graph
}

fn body_store(dir: &std::path::Path, bodies: Option<MarkdownBodyProperties>) -> StoreTarget {
    let uri = dir.to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    match bodies {
        Some(bodies) => store
            .with_codec_config(CodecOpenConfig::MarkdownBodies(bodies))
            .unwrap(),
        None => store,
    }
}

#[test]
fn body_properties_parse_resolve_and_reject_the_bad() {
    // The one grammar both the flag and the env var use: bare entries
    // accumulate as the default in order; Label= entries override.
    let config = MarkdownBodyProperties::parse("summary,notes;Book=summary,review").unwrap();
    assert_eq!(
        config.default,
        vec!["summary".to_string(), "notes".to_string()]
    );
    assert_eq!(
        config.per_label.get("Book").unwrap(),
        &vec!["summary".to_string(), "review".to_string()]
    );
    // Resolution: the plain per-label-else-default lookup. The legacy
    // `body` form is materialized at the boundaries — parse normalizes
    // a per-label-only spec, and resolve_bodies the sidecar's wire
    // form — never inside resolve.
    assert_eq!(config.resolve("Person"), vec!["summary", "notes"]);
    assert_eq!(config.resolve("Book"), vec!["summary", "review"]);
    let per_label_only = MarkdownBodyProperties::parse("Book=review").unwrap();
    assert_eq!(per_label_only.default, vec!["body".to_string()]);
    assert_eq!(per_label_only.resolve("Book"), vec!["review"]);
    assert_eq!(per_label_only.resolve("Person"), vec!["body"]);
    // The `;`-less single list is the common form.
    assert_eq!(
        MarkdownBodyProperties::parse("summary,notes")
            .unwrap()
            .default,
        vec!["summary".to_string(), "notes".to_string()]
    );
    // Fail-closed grammar: empty entries, empty names, nothing at all,
    // reserved names, duplicate names, and a duplicate label (never
    // silently last-wins).
    for bad in [
        "",
        "  ",
        ";",
        "summary,,notes",
        "summary,",
        "Book=",
        "=notes",
        "Book=,",
        "type,summary",
        "summary,summary",
        "Book=notes,notes",
        "Book=summary;Book=review",
        // '=' is the entry separator; a name carrying one can never be
        // expressed back by the grammar.
        "Book=summary=x",
    ] {
        assert!(
            MarkdownBodyProperties::parse(bad).is_err(),
            "parse accepted {bad:?}"
        );
    }
    // An empty per-label list is refused by validate too — resolve()
    // would return it and the grammar would drop the label's whole
    // body at the next checkpoint.
    let mut empty_list = MarkdownBodyProperties::default();
    empty_list.per_label.insert("Book".to_string(), Vec::new());
    assert!(
        empty_list.validate().is_err(),
        "validate accepted {empty_list:?}"
    );
}

#[test]
fn body_properties_render_first_bare_then_sections() {
    let dir = tempfile::tempdir().unwrap();
    let store = body_store(
        dir.path(),
        Some(MarkdownBodyProperties::parse("summary,notes;Book=summary,review").unwrap()),
    );
    crate::checkpoint(&store, &body_graph(), None).unwrap();
    let vfs = store.vfs();
    // The full two-property node: the first bare, the second sectioned;
    // neither listed property remains in frontmatter.
    let ada = String::from_utf8(read_to_vec(&*vfs, "Person/Ada.md").unwrap()).unwrap();
    assert_eq!(
        ada,
        "---\ntype: Person\nname: Ada\n---\n\nThe primary text.\n\n## notes\n\nScratch \
         thoughts.\n"
    );
    // A later entry present alone keeps its heading — rendered bare it
    // would re-parse as the first entry's text.
    let grace = String::from_utf8(read_to_vec(&*vfs, "Person/Grace.md").unwrap()).unwrap();
    assert_eq!(
        grace,
        "---\ntype: Person\nname: Grace\n---\n\n## notes\n\nOnly notes.\n"
    );
    // The per-label list overrides the default for the Book label.
    let dune = String::from_utf8(read_to_vec(&*vfs, "Book/Dune.md").unwrap()).unwrap();
    assert_eq!(
        dune,
        "---\ntype: Book\ntitle: Dune\n---\n\nA desert novel.\n\n## review\n\nKept the \
         spice flowing.\n"
    );
}

#[test]
fn body_properties_are_recorded_in_the_sidecar_and_survive_reopen_without_config() {
    // The vault is self-describing (ADR-0460): a fresh open with no
    // configuration parses the sections from the sidecar's recorded
    // list, and its checkpoint re-renders the identical files.
    let dir = tempfile::tempdir().unwrap();
    let config = MarkdownBodyProperties::parse("summary,notes;Book=summary,review").unwrap();
    let store = body_store(dir.path(), Some(config.clone()));
    crate::checkpoint(&store, &body_graph(), None).unwrap();

    let recorded = read_manifest(&store).unwrap().unwrap().bodies;
    assert_eq!(recorded, config);
    // The lease of the first open must be dropped before a second
    // target can lease the same directory.
    drop(store);

    let fresh = body_store(dir.path(), None);
    let (loaded, _) = crate::open_store(&fresh).unwrap();
    let ada = node_by_name(&loaded, "Ada");
    assert_eq!(
        ada.properties.get("summary"),
        Some(&string("The primary text."))
    );
    assert_eq!(
        ada.properties.get("notes"),
        Some(&string("Scratch thoughts."))
    );
    let grace = node_by_name(&loaded, "Grace");
    assert_eq!(grace.properties.get("summary"), None);
    assert_eq!(grace.properties.get("notes"), Some(&string("Only notes.")));
    assert_eq!(signature(&loaded), signature(&body_graph()));
    // The config-free checkpoint re-renders the identical node bytes
    // (the idempotence contract, through the sidecar-carried config).
    let vfs = fresh.vfs();
    let before: BTreeMap<String, Vec<u8>> = walk_vault(&*vfs)
        .unwrap()
        .into_iter()
        .map(|f| (f.walked.path, f.bytes))
        .collect();
    crate::checkpoint(&fresh, &loaded, None).unwrap();
    let after: BTreeMap<String, Vec<u8>> = walk_vault(&*vfs)
        .unwrap()
        .into_iter()
        .map(|f| (f.walked.path, f.bytes))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn an_explicit_body_property_config_replaces_the_recorded_one() {
    // ADR-0460's replace semantics: an explicit config at open replaces
    // the sidecar's wholesale — a section no longer in the list becomes
    // the first property's text (no text is lost, the structure is
    // reinterpreted) — and the next checkpoint records the new config.
    let dir = tempfile::tempdir().unwrap();
    let store = body_store(
        dir.path(),
        Some(MarkdownBodyProperties::parse("summary,notes").unwrap()),
    );
    crate::checkpoint(&store, &body_graph(), None).unwrap();
    drop(store);

    let replacement = body_store(
        dir.path(),
        Some(MarkdownBodyProperties::parse("summary").unwrap()),
    );
    let (loaded, _) = crate::open_store(&replacement).unwrap();
    let ada = node_by_name(&loaded, "Ada");
    // The whole body is the single configured property's text now.
    assert_eq!(
        ada.properties.get("summary"),
        Some(&string(
            "The primary text.\n\n## notes\n\nScratch thoughts."
        ))
    );
    assert_eq!(ada.properties.get("notes"), None);
    crate::checkpoint(&replacement, &loaded, None).unwrap();
    let recorded = read_manifest(&replacement).unwrap().unwrap().bodies;
    assert_eq!(recorded, MarkdownBodyProperties::parse("summary").unwrap());
}

#[test]
fn a_body_property_named_like_a_rel_type_fails_closed_at_checkpoint() {
    // The vocabulary disjointness (ADR-0460 generalizing ADR-0406's
    // `body` rule): a rel type whose name is in the list would swallow
    // the section heading's property at reload.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown)
        .unwrap()
        .with_codec_config(CodecOpenConfig::MarkdownBodies(
            MarkdownBodyProperties::parse("summary,KNOWS").unwrap(),
        ))
        .unwrap();
    let graph = person_graph();
    graph
        .load_checkpoint_node(Node {
            id: 4,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("X")), ("summary", string("S."))]),
        })
        .unwrap();
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(
        err.to_string().contains("is a body property of this label"),
        "{err}"
    );
}

#[test]
fn a_body_property_config_on_a_non_markdown_store_fails_the_install() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let err = store
        .with_codec_config(CodecOpenConfig::MarkdownBodies(
            MarkdownBodyProperties::parse("summary").unwrap(),
        ))
        .err()
        .expect("the install must refuse a non-markdown store");
    assert!(
        err.to_string()
            .contains("the configuration would be ignored"),
        "{err}"
    );
}
