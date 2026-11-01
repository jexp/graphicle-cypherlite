use super::*;
use crate::body_props;
use serde_json::json;
use std::collections::BTreeSet;

fn rel_types(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn doc_with_properties(properties: &[(&str, Value)]) -> NodeDoc {
    NodeDoc {
        primary_label: "Person".into(),
        extra_labels: vec![],
        properties: properties
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        links: BTreeMap::new(),
    }
}

#[test]
fn renders_the_deterministic_frontmatter_order() {
    let mut doc = doc_with_properties(&[("name", json!("Ada")), ("age", json!(36))]);
    doc.links.insert(
        "KNOWS".into(),
        vec![
            LinkEntry {
                target: parse_link("[[Person/Mary]]").unwrap(),
                properties: BTreeMap::new(),
            },
            LinkEntry {
                target: parse_link("[[Person/Grace]]").unwrap(),
                properties: BTreeMap::new(),
            },
        ],
    );
    doc.links.insert(
        "PARTNERS".into(),
        vec![LinkEntry {
            target: parse_link("[[Company/X]]").unwrap(),
            properties: BTreeMap::new(),
        }],
    );
    let text = render_node_doc(
        &doc,
        &rel_types(&["KNOWS", "PARTNERS"]),
        &body_props(&["body"]),
    )
    .unwrap();
    assert_eq!(
        text,
        "---\ntype: Person\nage: 36\nname: Ada\nKNOWS:\n  - \"[[Person/Grace]]\"\n  - \"[[Person/Mary]]\"\nPARTNERS:\n  - \"[[Company/X]]\"\n---\n"
    );
}

#[test]
fn render_parse_round_trip_with_map_form_entries() {
    let doc = NodeDoc {
        primary_label: "Person".into(),
        extra_labels: vec!["Mathematician".into()],
        properties: BTreeMap::from([
            ("name".to_string(), json!("Ada")),
            ("tags".to_string(), json!(["pioneer", "math"])),
            (
                "born".to_string(),
                json!({"__type": "date", "value": "1815-12-10"}),
            ),
            ("body".to_string(), json!("Analytical engine pioneer.")),
        ]),
        links: BTreeMap::from([
            (
                "KNOWS".to_string(),
                vec![LinkEntry {
                    target: parse_link("[[Person/Grace]]").unwrap(),
                    properties: BTreeMap::new(),
                }],
            ),
            (
                "WORKED_WITH".to_string(),
                vec![LinkEntry {
                    target: parse_link("[[Person/Charles]]").unwrap(),
                    properties: BTreeMap::from([("since".to_string(), Value::Number(1833.into()))]),
                }],
            ),
        ]),
    };
    let types = rel_types(&["KNOWS", "WORKED_WITH"]);
    let text = render_node_doc(&doc, &types, &body_props(&["body"])).unwrap();
    // Entry-map keys render in the value model's sorted order
    // (`since` before `target`); `target` leads only in the ADR's
    // illustrative example, not as a format rule.
    assert!(text.contains("WORKED_WITH:\n  - since: 1833\n    target: \"[[Person/Charles]]\"\n"));
    assert_eq!(
        parse_node_doc(
            "Person",
            "Person/Ada.md",
            &text,
            &types,
            &body_props(&["body"])
        )
        .unwrap(),
        doc
    );
}

#[test]
fn structural_type_must_match_the_directory() {
    let text = "---\ntype: Person\nname: Ada\n---\n";
    let parsed =
        parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert!(!parsed.properties.contains_key(KEY_TYPE));

    // A foreign `type` (Obsidian metadata) is a property, and re-renders
    // demoted under the mirror key.
    let text = "---\ntype: book\ntitle: Dune\n---\n";
    let parsed =
        parse_node_doc("Book", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert_eq!(parsed.properties.get("type"), Some(&json!("book")));
    let rendered = render_node_doc(&parsed, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert!(rendered.contains("type: Book\ntype_property: book\n"));
    assert_eq!(
        parse_node_doc(
            "Book",
            "f",
            &rendered,
            &rel_types(&[]),
            &body_props(&["body"])
        )
        .unwrap(),
        parsed
    );
}

#[test]
fn labels_key_adds_extra_labels_and_the_property_demotes() {
    let text = "---\ntype: Person\nlabels:\n  - Engineer\n  - Person\nname: Ada\n---\n";
    let parsed =
        parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert_eq!(parsed.extra_labels, vec!["Engineer".to_string()]);

    let mut doc = doc_with_properties(&[("labels", json!("a,b"))]);
    doc.extra_labels = vec!["Engineer".into()];
    let rendered = render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert!(rendered.contains("labels_property: a,b\n"));
    assert!(rendered.contains("labels:\n  - Engineer\n"));
    assert_eq!(
        parse_node_doc(
            "Person",
            "f",
            &rendered,
            &rel_types(&[]),
            &body_props(&["body"])
        )
        .unwrap(),
        doc,
        "the labels property survives demotion and reload"
    );
}

#[test]
fn classification_is_schema_driven_not_shape_sniffed() {
    // `records` looks like links but is not a rel type: it stays a
    // property, verbatim.
    let text = "---\ntype: Person\nrecords:\n  - \"[[Person/Grace]]\"\nname: Ada\n---\n";
    let parsed =
        parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert_eq!(
        parsed.properties.get("records"),
        Some(&json!(["[[Person/Grace]]"]))
    );
    assert!(parsed.links.is_empty());

    // The same key classified as a rel type parses as links.
    let parsed = parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["records"]),
        &body_props(&["body"]),
    )
    .unwrap();
    assert!(!parsed.links.is_empty());
    assert!(!parsed.properties.contains_key("records"));
}

#[test]
fn a_property_key_colliding_with_a_rel_type_fails_at_render() {
    let doc = doc_with_properties(&[("KNOWS", json!([1, 2]))]);
    let err = render_node_doc(&doc, &rel_types(&["KNOWS"]), &body_props(&["body"])).unwrap_err();
    assert!(err.to_string().contains("KNOWS"));
}

#[test]
fn a_link_key_absent_from_the_vocabulary_fails_at_render() {
    // The dual of the property-collision refusal: a link key the
    // vocabulary does not declare would re-parse as a property, so the
    // link-shaped key set must equal the declared rel types exactly.
    let mut doc = doc_with_properties(&[]);
    doc.links.insert(
        "KNOWS".into(),
        vec![LinkEntry {
            target: parse_link("[[Person/Grace]]").unwrap(),
            properties: BTreeMap::new(),
        }],
    );
    let err = render_node_doc(&doc, &rel_types(&["WORKS_AT"]), &body_props(&["body"])).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("KNOWS"),
        "message names the stray key: {message}"
    );
    assert!(
        message.contains("not a relationship type"),
        "message states the refusal: {message}"
    );
}

#[test]
fn map_entries_need_a_parseable_target() {
    let text = "---\ntype: Person\nKNOWS:\n  - target: Grace\n---\n";
    assert!(parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"])
    )
    .is_err());
    let text = "---\ntype: Person\nKNOWS:\n  - url: https://x\n---\n";
    assert!(parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"])
    )
    .is_err());
    // Not a list at all:
    let text = "---\ntype: Person\nKNOWS: 7\n---\n";
    assert!(parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"])
    )
    .is_err());
}

#[test]
fn short_form_links_parse_and_malformed_fail_closed() {
    // The short form parses (the codec resolves it through the schema).
    let text = "---\ntype: Person\nKNOWS:\n  - \"[[Grace]]\"\n---\n";
    let parsed = parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"]),
    )
    .unwrap();
    assert_eq!(
        parsed.links["KNOWS"][0].target,
        parse_link("[[Grace]]").unwrap()
    );
    // Empty targets and empty path segments stay malformed.
    let text = "---\ntype: Person\nKNOWS:\n  - \"[[]]\"\n---\n";
    assert!(parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"])
    )
    .is_err());
    let text = "---\ntype: Person\nKNOWS:\n  - \"[[Person/]]\"\n---\n";
    assert!(parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"])
    )
    .is_err());
}

#[test]
fn exact_duplicate_entries_deduplicate_both_ways() {
    let text =
        "---\ntype: Person\nKNOWS:\n  - \"[[Person/Grace]]\"\n  - \"[[Person/Grace]]\"\n---\n";
    let parsed = parse_node_doc(
        "Person",
        "f",
        text,
        &rel_types(&["KNOWS"]),
        &body_props(&["body"]),
    )
    .unwrap();
    assert_eq!(parsed.links["KNOWS"].len(), 1);

    let mut doc = doc_with_properties(&[]);
    doc.links.insert(
        "KNOWS".into(),
        vec![
            LinkEntry {
                target: parse_link("[[Person/Grace]]").unwrap(),
                properties: BTreeMap::new(),
            };
            2
        ],
    );
    let rendered = render_node_doc(&doc, &rel_types(&["KNOWS"]), &body_props(&["body"])).unwrap();
    assert_eq!(rendered.matches("[[Person/Grace]]").count(), 1);
}

#[test]
fn parallel_edges_differ_by_properties() {
    let mut doc = doc_with_properties(&[]);
    doc.links.insert(
        "KNOWS".into(),
        vec![
            LinkEntry {
                target: parse_link("[[Person/Grace]]").unwrap(),
                properties: BTreeMap::new(),
            },
            LinkEntry {
                target: parse_link("[[Person/Grace]]").unwrap(),
                properties: BTreeMap::from([("since".to_string(), json!(2020))]),
            },
        ],
    );
    let types = rel_types(&["KNOWS"]);
    let text = render_node_doc(&doc, &types, &body_props(&["body"])).unwrap();
    let parsed = parse_node_doc("Person", "f", &text, &types, &body_props(&["body"])).unwrap();
    assert_eq!(
        parsed.links["KNOWS"].len(),
        2,
        "distinct properties = distinct edges"
    );
}

#[test]
fn relationship_property_named_target_fails_at_render() {
    let mut doc = doc_with_properties(&[]);
    doc.links.insert(
        "KNOWS".into(),
        vec![LinkEntry {
            target: parse_link("[[Person/Grace]]").unwrap(),
            properties: BTreeMap::from([("target".to_string(), json!("x"))]),
        }],
    );
    assert!(render_node_doc(&doc, &rel_types(&["KNOWS"]), &body_props(&["body"])).is_err());
}

#[test]
fn reserved_property_names_fail_closed_at_render() {
    for key in [KEY_TYPE_PROPERTY, KEY_LABELS_PROPERTY] {
        let doc = doc_with_properties(&[(key, json!("x"))]);
        let err = render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).unwrap_err();
        assert!(err.to_string().contains(key), "missing error for {key}");
    }
}

#[test]
fn mirror_double_collision_fails_closed() {
    let doc = doc_with_properties(&[("type", json!("book")), ("type_property", json!("z"))]);
    assert!(render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).is_err());
}

#[test]
fn duplicate_property_arrivals_fail_closed() {
    let text = "---\ntype: book\ntype_property: novel\n---\n";
    assert!(parse_node_doc("Book", "f", text, &rel_types(&[]), &body_props(&["body"])).is_err());
    let text = "---\ntype: Person\nbody: a\n---\n\nb\n";
    assert!(parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).is_err());
}

#[test]
fn no_frontmatter_is_a_bare_body_node() {
    let parsed = parse_node_doc(
        "Person",
        "f",
        "Just prose.\n",
        &rel_types(&[]),
        &body_props(&["body"]),
    )
    .unwrap();
    assert_eq!(parsed.properties.get("body"), Some(&json!("Just prose.")));
}

#[test]
fn reserved_view_names_match_through_the_case_fold() {
    // The codec's one path-identity relation is the case fold
    // (ADR-0406 §2): a case variant of a view name is the same file as
    // the generated view on a case-insensitive filesystem, so it is
    // reserved exactly like the canonical spelling. An ordinary note
    // whose name merely ends like a view is not reserved.
    for name in [
        "index.md", "Index.md", "INDEX.MD", "log.md", "Log.md", "LOG.MD",
    ] {
        assert!(is_reserved_view_file_name(name), "{name} must be reserved");
    }
    assert!(!is_reserved_view_file_name("Blog.md"));
    assert!(!is_reserved_view_file_name("myindex.md"));
    assert!(!is_reserved_view_file_name("index.mdx"));
}

#[test]
fn empty_and_blank_frontmatter_blocks_are_absent_frontmatter() {
    // `parse_yaml_value`'s tail owns the empty-block rule: empty and
    // whitespace-only frontmatter both normalize to the empty mapping.
    for text in ["---\n---\n", "---\n   \n---\n"] {
        let parsed =
            parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
        assert!(
            parsed.properties.is_empty(),
            "unexpected properties for {text:?}"
        );
        assert!(parsed.extra_labels.is_empty());
    }
}

#[test]
fn explicit_null_frontmatter_fails_closed() {
    // A `~`/`null` document is a non-mapping root, not absent
    // frontmatter.
    for text in ["---\n~\n---\n", "---\nnull\n---\n"] {
        assert!(
            parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).is_err(),
            "null frontmatter must fail closed: {text:?}"
        );
    }
}

#[test]
fn rel_types_spelled_like_reserved_keys_fail_closed() {
    // A rel type named like a reserved key would swallow the structural
    // key at parse and double-emit it at render; both entries refuse
    // the vocabulary instead. `body` joins them only through the body
    // list (the test above), not as a frontmatter-reserved name.
    for name in [KEY_TYPE, KEY_LABELS, KEY_TYPE_PROPERTY, KEY_LABELS_PROPERTY] {
        let types = rel_types(&[name]);
        assert!(
            parse_node_doc(
                "Person",
                "f",
                "---\ntype: Person\n---\n",
                &types,
                &body_props(&["body"])
            )
            .is_err(),
            "parse accepted reserved rel type '{name}'"
        );
        let doc = doc_with_properties(&[]);
        assert!(
            render_node_doc(&doc, &types, &body_props(&["body"])).is_err(),
            "render accepted reserved rel type '{name}'"
        );
    }
}

#[test]
fn properties_render_byte_sorted() {
    let doc = doc_with_properties(&[("b", json!(1)), ("a", json!(2)), ("C", json!(3))]);
    let text = render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).unwrap();
    let a = text.find("C: 3\n").unwrap();
    let b = text.find("a: 2\n").unwrap();
    let c = text.find("b: 1\n").unwrap();
    assert!(a < b && b < c, "byte-sorted: uppercase first");
}

/// The ADR-0460 body-properties suite, split from this file's
/// frontmatter/link tests (the no-1k-line rule; the storage side's
/// codec/markdown/tests/bodies.rs is the twin split).
mod body_properties;
