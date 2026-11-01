use super::*;
use serde_json::json;

#[test]
fn parses_core_schema_scalars() {
    let value =
        parse_yaml_value("a: 1\nb: -2\nc: 1.5\nd: true\ne: null\nf: yes\ng: 2020-01-02\n").unwrap();
    assert_eq!(
        value,
        json!({
            "a": 1, "b": -2, "c": 1.5, "d": true, "e": null,
            // YAML 1.2 core: yes/no are strings, and timestamps are not
            // resolved — the temporal vocabulary is the tagged encoding.
            "f": "yes", "g": "2020-01-02",
        })
    );
}

#[test]
fn quoted_scalars_are_always_strings() {
    let value = parse_yaml_value("a: \"1\"\nb: 'true'").unwrap();
    assert_eq!(value, json!({"a": "1", "b": "true"}));
}

#[test]
fn parses_nested_lists_and_maps() {
    let text = "list:\n  - 1\n  - - 2\n    - 3\nmap:\n  inner:\n    deep: value\n";
    assert_eq!(
        parse_yaml_value(text).unwrap(),
        json!({"list": [1, [2, 3]], "map": {"inner": {"deep": "value"}}})
    );
}

#[test]
fn empty_input_is_an_empty_mapping() {
    assert_eq!(parse_yaml_value("").unwrap(), json!({}));
}

#[test]
fn duplicate_keys_fail_closed() {
    let err = parse_yaml_value("a: 1\na: 2\n").unwrap_err();
    assert!(err.to_string().contains("duplicate mapping key 'a'"));
}

#[test]
fn anchors_and_aliases_are_rejected() {
    assert!(parse_yaml_value("a: &x 1\nb: *x\n").is_err());
    assert!(parse_yaml_value("a: *undefined\n").is_err());
}

#[test]
fn explicit_tags_are_rejected() {
    assert!(parse_yaml_value("a: !!str 5\n").is_err());
}

#[test]
fn non_string_keys_are_rejected() {
    assert!(parse_yaml_value("1: a\n").is_err());
}

#[test]
fn multiple_documents_are_rejected() {
    assert!(parse_yaml_value("---\na: 1\n---\nb: 2\n").is_err());
}

#[test]
fn deeper_than_the_cap_is_rejected() {
    let depth = MAX_YAML_DEPTH + 2;
    let mut text = String::new();
    for _ in 0..depth {
        text.push_str("- ");
    }
    text.push('1');
    assert!(parse_yaml_value(&text).is_err());
}

#[test]
fn emitted_yaml_round_trips_through_the_parser() {
    let value = json!({
        "wikilink-list": ["[[Person/Grace]]", "[[Place/Ithaca]]"],
        "plain": "text",
        "needs-quoting": "a: b # c",
        "number-like": "42",
        "n": null,
        "b": false,
        "i": 7,
        "f": 0.5,
        "nested": {"list": [1, {"tag": "date", "value": "2020-01-01"}]},
        "empty-list": [],
        "empty-map": {},
    });
    let text = emit_yaml_value(&value);
    assert_eq!(parse_yaml_value(&text).unwrap(), value);
}

#[test]
fn wikilinks_always_quote() {
    let text = emit_yaml_value(&json!({"KNOWS": ["[[Person/Grace]]"]}));
    assert_eq!(text, "KNOWS:\n  - \"[[Person/Grace]]\"\n");
}

#[test]
fn emit_entries_preserves_caller_order() {
    let entries = vec![
        ("type".to_string(), json!("Person")),
        ("zeta".to_string(), json!(1)),
        ("alpha".to_string(), json!(2)),
    ];
    let text = emit_yaml_entries(&entries);
    assert_eq!(text, "type: Person\nzeta: 1\nalpha: 2\n");
}

#[test]
fn split_and_join_frontmatter() {
    let text = "---\ntype: Person\n---\n\nBody.\n";
    let (yaml, body) = split_frontmatter(text);
    assert_eq!(yaml.unwrap(), "type: Person\n");
    assert_eq!(body, "\nBody.\n");
    assert_eq!(join_frontmatter(yaml.unwrap(), body), text);
}

#[test]
fn split_frontmatter_without_fence_is_all_body() {
    let text = "Just a note.\n";
    let (yaml, body) = split_frontmatter(text);
    assert!(yaml.is_none());
    assert_eq!(body, text);
}

#[test]
fn unclosed_fence_is_all_body() {
    let text = "---\ntype: Person\n";
    let (yaml, _) = split_frontmatter(text);
    assert!(yaml.is_none());
}

#[test]
fn crlf_fences_split() {
    let text = "---\r\ntype: Person\r\n---\r\n\r\nBody.\r\n";
    let (yaml, body) = split_frontmatter(text);
    assert!(yaml.is_some());
    assert!(body.starts_with("\r\nBody."));
}

#[test]
fn a_carriage_return_round_trips_through_the_quoted_form() {
    // The block form would lose the CR to YAML's CR/CRLF→LF
    // normalization on reload; the double-quoted escape keeps it.
    let value = "line one\r\nline two with more than eighty bytes of content to pass the block-scalar threshold and reach the block-form arm";
    let emitted = emit_yaml_value(&json!(value));
    assert!(
        emitted.contains("\\r"),
        "the CR is escaped, not blocked: {emitted:?}"
    );
    let reparsed = parse_yaml_value(&emitted).unwrap();
    assert_eq!(reparsed, json!(value));
}

#[test]
fn control_characters_and_del_round_trip_through_the_quoted_form() {
    // Every C0 control and DEL takes the double-quoted form — the
    // literal-block form cannot carry them — both below the threshold
    // and above it, where the block form would otherwise win.
    for value in [
        "a\u{0001}b",
        "a\u{7f}b",
        "a\u{007f} long line that passes the block-scalar threshold on \
         length but is disqualified by the control character it carries",
    ] {
        let emitted = emit_yaml_value(&json!(value));
        assert!(
            emitted.contains("\\u"),
            "the control is escaped, not raw: {emitted:?}"
        );
        let reparsed = parse_yaml_value(&emitted).unwrap();
        assert_eq!(reparsed, json!(value));
    }
}
