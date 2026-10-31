//! The structural-depth screen (ADR-0227 §3): documents nesting deeper
//! than 128 levels are rejected with a typed [`OntologyError::NestingTooDeep`].
//! On the YAML path the screen counts collection starts in the
//! (non-recursive) `saphyr_parser` event stream before the recursive
//! `Yaml::load_from_str` tree builder runs — without it, saphyr recurses
//! per nesting level with no cap and the `"- "` × 100_000 payload below
//! aborts the process with a stack overflow, pre-auth, via any query
//! surface. On the JSON path serde_json's own 128-level recursion limit
//! fires (its strict-JSON sniff is non-recursive) and is mapped to the
//! same typed error, so both syntaxes share one error surface.
//!
//! The same event pass screens anchors and aliases (ADR-0227 §4): saphyr's
//! loader resolves aliases eagerly — cloning the anchored subtree for each
//! reference — so a billion-laughs alias chain inside a *fully valid*
//! document (riding an extension payload, which validation carries
//! untouched) expands into gigabytes during `load_from_str`, past the
//! depth screen (event depth ~4) and before any post-parse envelope check
//! can run. The screen rejects at the first anchor or alias event, with a
//! typed [`OntologyError::UnsupportedConstruct`], without expanding
//! anything; the value model has no alias concept, so no representable
//! document is lost.

use cypherlite_ontology::{
    parse_document_value, parse_ontology_document, DocumentFormat, OntologyError, Value,
    PINNED_SCHEMA_URI,
};

/// The nesting cap, matching serde_json's recursion limit.
const LIMIT: usize = 128;

fn expect_too_deep(result: Result<impl std::fmt::Debug, OntologyError>) -> OntologyError {
    let err = result.expect_err("a too-deep document must be rejected");
    assert!(
        matches!(err, OntologyError::NestingTooDeep { limit } if limit == LIMIT),
        "expected NestingTooDeep {{ limit: {LIMIT} }}, got: {err}"
    );
    err
}

/// `depth` nested block mappings (`k:` lines, two-space indentation) with
/// a scalar leaf: true nesting equals the pre-screen's estimate.
fn nested_yaml_maps(maps: usize) -> String {
    let mut doc = String::new();
    for i in 0..maps {
        doc.push_str(&"  ".repeat(i));
        doc.push_str("k:\n");
    }
    doc.push_str(&"  ".repeat(maps));
    doc.push_str("1\n");
    doc
}

/// `depth` nested JSON objects `{"a": … }` with a scalar leaf.
fn nested_json_maps(depth: usize) -> String {
    format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth))
}

/// A schema-valid document whose `custom` extension payload (a free-form
/// JSON object, ADR-0364 §2) nests `arrays` levels deep.
fn valid_doc_with_deep_extension(arrays: usize) -> String {
    let nest = format!("{}1{}", "[".repeat(arrays), "]".repeat(arrays));
    format!(
        "{{\"schema\":\"{PINNED_SCHEMA_URI}\",\"id\":\"deep\",\"version\":\"1\",\
         \"extensions\":{{\"custom\":{{\"deep\":{nest}}}}}}}"
    )
}

// ---------------------------------------------------------------------------
// The regression: the attack payload fails fast with the typed error
// ---------------------------------------------------------------------------

#[test]
fn compact_dash_run_100k_fails_fast_with_typed_error() {
    // One line, no indentation: 100_000 nested block sequences in saphyr's
    // compact notation. Without the pre-screen this aborts the process.
    let payload = format!("{}1", "- ".repeat(100_000));
    expect_too_deep(parse_document_value(payload.as_bytes()));
    // Every public parse entry point is screened.
    expect_too_deep(parse_ontology_document(payload.as_bytes()));
}

#[test]
fn json_deep_nesting_fails_fast_with_typed_error() {
    // serde_json's own recursion cap rejects this (its `IgnoredAny` sniff
    // is non-recursive, so the sniff is safe too); the error is mapped to
    // the typed depth error, not an abort.
    let payload = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
    expect_too_deep(parse_document_value(payload.as_bytes()));
    expect_too_deep(parse_ontology_document(payload.as_bytes()));
}

#[test]
fn yaml_flow_deep_nesting_fails_with_typed_error() {
    // Flow style is YAML too: collection starts are counted the same in
    // block and flow style. (saphyr's scanner additionally hard-caps flow
    // nesting at 256 with a scan error, also surfaced as NestingTooDeep.)
    let payload = format!("a: {}1{}", "[".repeat(100_000), "]".repeat(100_000));
    expect_too_deep(parse_document_value(payload.as_bytes()));
}

// ---------------------------------------------------------------------------
// Boundary: exactly 128 collection levels pass, 129 fail (event counting
// counts every collection start as one level; scalar leaves are not
// collections). JSON's boundary sits one level lower: serde_json counts
// the leaf value as a recursion level, so 127 collections pass and 128
// fail — both surfaces raise the same typed error at the same cap value.
// ---------------------------------------------------------------------------

#[test]
fn json_boundary_127_passes_128_fails() {
    // 127 nested objects parse end-to-end: serde_json's native 128-level
    // recursion limit counts the scalar leaf as a level.
    let (value, format) = parse_document_value(nested_json_maps(LIMIT - 1).as_bytes())
        .expect("127 nested objects are within serde_json's limit");
    assert_eq!(format, DocumentFormat::Json);
    assert!(value.as_object().is_some());
    // 128 nested objects trip serde_json's recursion limit, mapped to the
    // typed depth error; 129 fail the same way.
    expect_too_deep(parse_document_value(nested_json_maps(LIMIT).as_bytes()));
    expect_too_deep(parse_document_value(nested_json_maps(LIMIT + 1).as_bytes()));
}

#[test]
fn yaml_block_boundary_128_passes_129_fails() {
    // 128 `k:` lines are 128 nested mappings: exactly at the limit.
    parse_document_value(nested_yaml_maps(LIMIT).as_bytes())
        .expect("128 levels of block nesting are within the limit");
    // One more `k:` line is 129 nested mappings.
    expect_too_deep(parse_document_value(nested_yaml_maps(LIMIT + 1).as_bytes()));
}

#[test]
fn yaml_compact_dash_run_boundary_128_passes_129_fails() {
    let ok = format!("{}1", "- ".repeat(LIMIT));
    parse_document_value(ok.as_bytes()).expect("128 compact sequences are within the limit");
    let too_deep = format!("{}1", "- ".repeat(LIMIT + 1));
    expect_too_deep(parse_document_value(too_deep.as_bytes()));
}

// ---------------------------------------------------------------------------
// Legitimate deep documents still parse
// ---------------------------------------------------------------------------

#[test]
fn hundred_level_yaml_document_validates() {
    let (value, _) = parse_document_value(nested_yaml_maps(100).as_bytes())
        .expect("a ~100-level document is well within the limit");
    // Spot-check the shape: nested single-key maps down to the leaf.
    let mut current = &value;
    for _ in 0..100 {
        current = &current.as_object().expect("mapping")["k"];
    }
    assert_eq!(current.as_integer(), Some(1));
}

#[test]
fn valid_document_with_deep_extension_payload_validates() {
    // Root + 'extensions' + 'custom' + 96 nested arrays: 99 levels, within
    // the limit.
    let doc = valid_doc_with_deep_extension(96);
    let parsed = parse_ontology_document(doc.as_bytes()).expect("a ~100-level document validates");
    assert_eq!(parsed.id(), "deep");
}

// ---------------------------------------------------------------------------
// Non-structural bytes cannot spoof the count
// ---------------------------------------------------------------------------

#[test]
fn bracket_runs_inside_json_strings_do_not_count() {
    let doc = format!(
        "{{\"schema\":\"{PINNED_SCHEMA_URI}\",\"id\":\"s\",\"version\":\"1\",\"name\":\"{}\"}}",
        "[".repeat(10_000)
    );
    parse_ontology_document(doc.as_bytes()).expect("string content is not structure");
}

#[test]
fn dashes_and_brackets_inside_yaml_strings_do_not_count() {
    let doc = format!(
        "schema: {PINNED_SCHEMA_URI}\nid: s\nversion: \"1\"\ndescription: \"{}\"\n",
        "- ".repeat(10_000)
    );
    parse_ontology_document(doc.as_bytes()).expect("quoted scalar content is not structure");
}

#[test]
fn block_scalar_content_does_not_count() {
    let doc = format!(
        "schema: {PINNED_SCHEMA_URI}\nid: s\nversion: \"1\"\ndescription: |\n  {}\n  {}[[[[\n",
        "- ".repeat(10_000),
        "- ".repeat(10_000)
    );
    parse_ontology_document(doc.as_bytes()).expect("block scalar content is not structure");
}

#[test]
fn comment_content_does_not_count() {
    let doc = format!(
        "schema: {PINNED_SCHEMA_URI} # {}\nid: s\nversion: \"1\"\n",
        "[".repeat(10_000)
    );
    parse_ontology_document(doc.as_bytes()).expect("comment content is not structure");
}

// ---------------------------------------------------------------------------
// The anchor/alias screen (the same event pass): a billion-laughs alias
// chain riding a fully valid document is rejected at the first anchor
// event, before saphyr's loader expands it
// ---------------------------------------------------------------------------

/// The typed rejection every anchored or aliased document gets.
fn expect_no_anchor(result: Result<impl std::fmt::Debug, OntologyError>) -> OntologyError {
    let err = result.expect_err("an anchored or aliased document must be rejected");
    assert!(
        matches!(
            err,
            OntologyError::UnsupportedConstruct(ref msg) if msg.contains("anchors and aliases")
        ),
        "expected the anchor/alias UnsupportedConstruct, got: {err}"
    );
    err
}

/// Billion-laughs shape: `l0` is an anchored 10-int sequence; every later
/// level is an anchored sequence of 10 alias references to the previous
/// level. saphyr's loader clones the anchored subtree per reference while
/// building the tree, so the *loaded* node count is 10^levels — the
/// ~600 B document measured 37.5 GB expanded at 8 levels, an OOM at 10 —
/// while the event depth stays ~4 and validation carries the extension
/// payload untouched, so nothing else on the path stops it.
fn alias_bomb_yaml(levels: usize) -> String {
    let mut doc = format!(
        "schema: {PINNED_SCHEMA_URI}\nid: bomb\nversion: \"1\"\nextensions:\n  custom:\n    x-bomb:\n"
    );
    doc.push_str("    l0: &l0 [1,2,3,4,5,6,7,8,9,10]\n");
    for i in 1..levels {
        let refs = std::iter::repeat_n(format!("*l{}", i - 1), 10)
            .collect::<Vec<_>>()
            .join(",");
        doc.push_str(&format!("    l{i}: &l{i} [{refs}]\n"));
    }
    doc
}

#[test]
fn valid_document_alias_bomb_fails_fast_with_typed_error() {
    for levels in [6, 8, 10] {
        let payload = alias_bomb_yaml(levels);
        // Fails fast: the screen rejects at the first anchor event (the
        // `&l0` sequence start) without expanding anything — elapsed time
        // past this bound means the loader ran first.
        let t0 = std::time::Instant::now();
        expect_no_anchor(parse_document_value(payload.as_bytes()));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "a {levels}-level bomb must be rejected in the event screen, not expanded"
        );
        // The full `db.ontology.set` path is screened too: the document
        // is schema-valid — the alias chain rides an extension payload,
        // which validation carries untouched — so only the screen stops it.
        expect_no_anchor(parse_ontology_document(payload.as_bytes()));
    }
}

#[test]
fn anchored_nodes_without_alias_references_are_rejected() {
    // The anchor event fires even when no alias ever references it, on
    // every anchored event kind: sequence, scalar and mapping.
    expect_no_anchor(parse_document_value(b"a: &a [1,2]\n"));
    expect_no_anchor(parse_document_value(b"a: &x 1\n"));
    expect_no_anchor(parse_document_value(b"a: &m {b: 1}\n"));
}

#[test]
fn alias_to_undefined_anchor_is_rejected_typed_not_syntax() {
    // saphyr's parser fails on the undefined-anchor reference before it
    // could emit an alias event; the screen reports that error as the
    // same construct rejection (previously it surfaced from the loader
    // as a generic YAML syntax error).
    expect_no_anchor(parse_document_value(b"b: [*zzz]\n"));
    expect_no_anchor(parse_ontology_document(b"b: [*zzz]\n"));
}

#[test]
fn alias_to_defined_anchor_is_rejected_too() {
    // Even the well-defined two-level chain is refused — the screen
    // rejects at `&a`, the first anchor event, before any reference.
    expect_no_anchor(parse_document_value(b"a: &a [1,2]\nb: [*a, *a]\n"));
}

#[test]
fn the_same_fanout_written_literally_parses() {
    // Control: the same header, extension payload and fan-out semantics
    // with the copies written out literally — no anchor, no alias. No
    // representable document is lost to the screen; only the alias
    // construct is refused. (Two literal levels: 10^2 leaves.)
    let mut level = format!(
        "[{}]",
        (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    for _ in 0..2 {
        level = format!(
            "[{}]",
            std::iter::repeat_n(level, 10).collect::<Vec<_>>().join(",")
        );
    }
    let doc = format!(
        "schema: {PINNED_SCHEMA_URI}\nid: bomb\nversion: \"1\"\n\
         extensions:\n  custom:\n    x-bomb:\n      l0: {level}\n"
    );
    let parsed = parse_ontology_document(doc.as_bytes())
        .expect("the literal fan-out has no anchors and must parse");
    assert_eq!(parsed.id(), "bomb");
}

// ---------------------------------------------------------------------------
// Serialization depth cap (hand-built values bypass the parse screen, so
// the serializers enforce the same 128-level bound themselves).

#[test]
fn hand_built_deep_value_fails_serialization_typed_not_abort() {
    // 130 levels of arrays built by hand — no parser ever saw it, so only
    // the serializer's own cap can stop the recursive writers.
    let mut value = Value::Null;
    for _ in 0..130 {
        value = Value::Array(vec![value]);
    }
    let err = cypherlite_ontology::to_json_string(&value).unwrap_err();
    assert!(
        err.to_string().contains("128"),
        "depth error names the limit: {err}"
    );
    assert!(
        cypherlite_ontology::to_yaml_string(&value).is_err(),
        "the YAML path is capped by the same check"
    );
    assert!(
        cypherlite_ontology::to_json_sorted_string(&value).is_err(),
        "the sorted-JSON path is capped by the same check"
    );
}

#[test]
fn hand_built_value_within_the_cap_serializes() {
    let mut value = Value::Null;
    for _ in 0..128 {
        value = Value::Array(vec![value]);
    }
    assert!(cypherlite_ontology::to_json_string(&value).is_ok());
    assert!(cypherlite_ontology::to_yaml_string(&value).is_ok());
    assert!(cypherlite_ontology::to_json_sorted_string(&value).is_ok());
}

#[test]
fn hand_built_nan_value_fails_serialization_typed_not_panic() {
    // Value::Float is publicly constructible; a NaN must fail the
    // Result-returning serializers typed, not panic the expect guards.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let value = Value::Float(bad);
        let err = cypherlite_ontology::to_json_string(&value).unwrap_err();
        assert!(
            err.to_string().contains("non-finite"),
            "names the non-finite rule: {err}"
        );
        assert!(cypherlite_ontology::to_yaml_string(&value).is_err());
        assert!(cypherlite_ontology::to_json_sorted_string(&value).is_err());
    }
    // A finite float serializes.
    assert!(cypherlite_ontology::to_json_string(&Value::Float(1.5)).is_ok());
}
