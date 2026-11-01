//! The ADR-0460 body-properties suite: the default single-`body` rule,
//! the first-bare-then-sections render and its round trips, the marker
//! boundary rules, the fail-closed guards, and the demotion rules.

use super::*;

#[test]
fn body_is_one_bare_property_headings_are_text() {
    // The default list is `body` alone, so no section boundaries exist
    // and every heading — including one spelled like a configured
    // section elsewhere — is the body's own text (ADR-0406's rule,
    // unchanged by ADR-0460).
    let text = "---\ntype: Person\n---\n\n## Notes\n\nAda, countess of Lovelace.\n";
    let parsed =
        parse_node_doc("Person", "f", text, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert_eq!(
        parsed.properties.get("body"),
        Some(&json!("## Notes\n\nAda, countess of Lovelace."))
    );
    let rendered = render_node_doc(&parsed, &rel_types(&[]), &body_props(&["body"])).unwrap();
    assert!(rendered.contains("---\n\n## Notes\n\nAda, countess of Lovelace.\n"));
    assert_eq!(
        parse_node_doc(
            "Person",
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
fn body_properties_render_first_bare_then_sections_and_round_trip() {
    // ADR-0460: the first present property is the bare body, each
    // subsequent one a `## <name>` section, in list order.
    let list = body_props(&["summary", "notes", "review"]);
    let doc = doc_with_properties(&[
        ("name", json!("Ada")),
        ("summary", json!("The primary text.")),
        ("notes", json!("Scratch thoughts.")),
        ("review", json!("Verdict: keep.")),
    ]);
    let text = render_node_doc(&doc, &rel_types(&[]), &list).unwrap();
    assert_eq!(
        text,
        "---\ntype: Person\nname: Ada\n---\n\nThe primary text.\n\n## notes\n\nScratch \
         thoughts.\n\n## review\n\nVerdict: keep.\n"
    );
    assert_eq!(
        parse_node_doc("Person", "f", &text, &rel_types(&[]), &list).unwrap(),
        doc
    );
}

#[test]
fn a_partially_present_body_property_list_renders_only_the_present() {
    // The absent properties render nothing — neither a section nor a
    // frontmatter key. The list's first entry alone is the bare body
    // 1:1; a later entry alone keeps its heading (rendered bare it
    // would re-parse as the first entry's text).
    let list = body_props(&["summary", "notes"]);
    let doc = doc_with_properties(&[("name", json!("Ada")), ("summary", json!("Only summary."))]);
    let text = render_node_doc(&doc, &rel_types(&[]), &list).unwrap();
    assert_eq!(text, "---\ntype: Person\nname: Ada\n---\n\nOnly summary.\n");
    assert_eq!(
        parse_node_doc("Person", "f", &text, &rel_types(&[]), &list).unwrap(),
        doc
    );
    let doc = doc_with_properties(&[("name", json!("Ada")), ("notes", json!("Only notes."))]);
    let text = render_node_doc(&doc, &rel_types(&[]), &list).unwrap();
    assert_eq!(
        text,
        "---\ntype: Person\nname: Ada\n---\n\n## notes\n\nOnly notes.\n"
    );
    assert_eq!(
        parse_node_doc("Person", "f", &text, &rel_types(&[]), &list).unwrap(),
        doc
    );
    // No listed property present at all: an empty body.
    let doc = doc_with_properties(&[("name", json!("Ada"))]);
    let text = render_node_doc(&doc, &rel_types(&[]), &list).unwrap();
    assert_eq!(text, "---\ntype: Person\nname: Ada\n---\n");
    assert_eq!(
        parse_node_doc("Person", "f", &text, &rel_types(&[]), &list).unwrap(),
        doc
    );
}

#[test]
fn a_first_property_heading_is_content_a_section_heading_is_a_boundary() {
    // A heading naming the FIRST list entry is never a boundary (the
    // preamble belongs to it); one naming a later entry is.
    let list = body_props(&["summary", "notes"]);
    let text = "---\ntype: Person\n---\n\n## summary\n\nProse.\n\n## notes\n\nKept.\n";
    let parsed = parse_node_doc("Person", "f", text, &rel_types(&[]), &list).unwrap();
    assert_eq!(
        parsed.properties.get("summary"),
        Some(&json!("## summary\n\nProse."))
    );
    assert_eq!(parsed.properties.get("notes"), Some(&json!("Kept.")));
    // Re-rendering normalizes to the canonical form: first bare, then
    // the section, and the round trip is stable from there.
    let rendered = render_node_doc(&parsed, &rel_types(&[]), &list).unwrap();
    assert!(rendered.contains("---\n\n## summary\n\nProse.\n\n## notes\n\nKept.\n"));
    assert_eq!(
        parse_node_doc("Person", "f", &rendered, &rel_types(&[]), &list).unwrap(),
        parsed
    );
}

#[test]
fn a_body_section_and_its_frontmatter_twin_collide() {
    // A frontmatter key and a body section of the same name arrive
    // twice — the arrives-twice refusal, one grammar class wider.
    let list = body_props(&["summary", "notes"]);
    let text = "---\ntype: Person\nnotes: from frontmatter\n---\n\nS.\n\n## notes\n\nN.\n";
    let err = parse_node_doc("Person", "f", text, &rel_types(&[]), &list).unwrap_err();
    assert!(
        err.to_string().contains("property 'notes' arrives twice"),
        "{err}"
    );
}

#[test]
fn unrepresentable_body_property_values_stay_in_frontmatter() {
    // The demotion rule holds per listed property: a non-string, blank,
    // or outer-whitespace value stays a frontmatter property and the
    // body renders without its section.
    for (value, rendered) in [
        (json!(7), "notes: 7\n"),
        (json!(""), "notes: \"\"\n"),
        (json!(" x "), "notes: \" x \"\n"),
    ] {
        let list = body_props(&["summary", "notes"]);
        let doc = doc_with_properties(&[("summary", json!("S.")), ("notes", value.clone())]);
        let text = render_node_doc(&doc, &rel_types(&[]), &list).unwrap();
        assert!(text.contains(rendered), "missing {rendered:?} for {value}");
        assert_eq!(
            parse_node_doc("Person", "f", &text, &rel_types(&[]), &list).unwrap(),
            doc,
            "round trip for {value}"
        );
    }
}

#[test]
fn invalid_body_property_lists_fail_closed() {
    // The one validation every list passes: empty names, reserved
    // frontmatter keys, and duplicates all fail closed.
    for list in [
        body_props(&["summary", ""]),
        body_props(&["summary", KEY_TYPE]),
        body_props(&["summary", KEY_LABELS]),
        body_props(&["summary", KEY_TYPE_PROPERTY]),
        body_props(&["summary", KEY_LABELS_PROPERTY]),
        body_props(&["notes", "notes"]),
        // A name that can never be a boundary line — any whitespace,
        // interior included (the marker match is the heading's first
        // word) — would render a `## ` heading the next open silently
        // absorbs into the preceding segment; and a case-fold duplicate
        // is the same section twice.
        body_props(&["summary", "no\ntes"]),
        body_props(&["summary", "no\rtes"]),
        body_props(&["summary", " notes"]),
        body_props(&["summary", "notes "]),
        body_props(&["summary", "my notes"]),
        body_props(&["notes", "Notes"]),
    ] {
        assert!(
            validate_body_properties(&list).is_err(),
            "validate accepted {list:?}"
        );
        assert!(
            render_node_doc(&doc_with_properties(&[]), &rel_types(&[]), &list).is_err(),
            "render accepted {list:?}"
        );
        assert!(
            parse_node_doc(
                "Person",
                "f",
                "---\ntype: Person\n---\n",
                &rel_types(&[]),
                &list
            )
            .is_err(),
            "parse accepted {list:?}"
        );
    }
}

#[test]
fn a_rel_type_named_like_a_body_property_fails_closed() {
    // A rel type whose name is in the list would swallow the section
    // heading's property at parse; both entries refuse it.
    let list = body_props(&["summary", "notes"]);
    let types = rel_types(&["notes"]);
    assert!(parse_node_doc("Person", "f", "---\ntype: Person\n---\n", &types, &list).is_err());
    assert!(render_node_doc(&doc_with_properties(&[]), &types, &list).is_err());
}

#[test]
fn non_representable_body_values_stay_in_frontmatter() {
    // One contract, four values: a non-string, the empty string, a
    // whitespace-only string, and an outer-whitespace string are all
    // unrepresentable as the bare body (`representable_as_body` — the
    // render→parse cycle would normalize the outer whitespace away), so
    // each demotes to a frontmatter property and survives the round
    // trip there.
    for (value, rendered) in [
        (json!(7), "body: 7\n"),
        (json!(""), "body: \"\"\n"),
        (json!("   "), "body: \"   \"\n"),
        (json!(" x "), "body: \" x \"\n"),
    ] {
        let doc = doc_with_properties(&[("body", value.clone())]);
        let text = render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).unwrap();
        assert!(text.contains(rendered), "missing {rendered:?} for {value}");
        assert_eq!(
            parse_node_doc(
                "Person",
                "f",
                &text,
                &rel_types(&[]),
                &body_props(&["body"])
            )
            .unwrap(),
            doc,
            "round trip for {value}"
        );
    }
}

#[test]
fn nul_body_fails_closed_at_render() {
    // The parser refuses a NUL body on load; the writer refuses the
    // same text, so a checkpoint never emits a file the next open
    // cannot reload.
    let doc = doc_with_properties(&[("body", json!("a\u{0}b"))]);
    let err = render_node_doc(&doc, &rel_types(&[]), &body_props(&["body"])).unwrap_err();
    assert!(err.to_string().contains("NUL byte in body text"));
}

#[test]
fn with_body_property_ignores_blank_text() {
    let doc = NodeDoc::default().with_body_property("body", "  \n ".into());
    assert!(
        doc.properties.is_empty(),
        "blank body must not insert a property"
    );
    // The builder consults the same representability rule: an
    // outer-whitespace body demotes to no property, never a body the
    // next open would silently rewrite.
    let doc = NodeDoc::default().with_body_property("body", " x ".into());
    assert!(
        doc.properties.is_empty(),
        "outer-whitespace body must not insert a property"
    );
    let doc = NodeDoc::default().with_body_property("body", "text".into());
    assert_eq!(doc.properties.get("body"), Some(&json!("text")));
}
