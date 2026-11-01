//! YAML text ↔ tagged JSON values for markdown frontmatter (ADR-0406 §2).
//!
//! Parsing is YAML 1.2 via the workspace's `saphyr-parser` (the ADR-0227
//! parser, reused): one document, no anchors or aliases, no explicit
//! tags — the same restrictions the ontology document format applies,
//! because frontmatter has the same shape-safety needs. Plain scalars
//! resolve per the YAML 1.2 core schema (null / bool / integer / float /
//! string); quoted scalars are always strings; YAML timestamps are *not*
//! resolved — a date-like plain scalar stays a string, and the engine's
//! temporal values arrive as tagged maps from the canonical property
//! encoding, never as bare YAML dates.
//!
//! Emission is a hand-rolled deterministic printer for the restricted
//! value set the canonical encoding produces (`null`, booleans, JSON
//! numbers, strings, arrays, objects): fixed styles, no options, so the
//! same value always renders byte-identical (ADR-0406 §2). Strings are
//! plain when unambiguous and double-quoted otherwise — wikilinks always
//! quote, matching the lpgfs renderer's `"[[Label/name]]"` output.

use cypherlite_core::{Error, Result};
use saphyr_parser::{Event, Parser, ScalarStyle};

use crate::is_c0_del;

/// The deepest nesting a frontmatter document may use — the frontmatter
/// screen's own cap. It is equal by value, not by reference, to
/// serde_json's native recursion limit and to the ontology document
/// screens' 128 (ADR-0227 §3), which live in crates that cannot import
/// this one; if the caps are ever meant to move together, the number
/// needs a home both crates can see (cypherlite-core).
const MAX_YAML_DEPTH: usize = 128;

fn yaml_error(message: impl Into<String>) -> Error {
    Error::storage(format!("markdown frontmatter: {}", message.into()))
}

/// Parse a single YAML document into a JSON value.
///
/// Empty input is the empty mapping (a node file without frontmatter
/// content). Fails closed on anything the restricted grammar above does
/// not admit.
pub fn parse_yaml_value(text: &str) -> Result<serde_json::Value> {
    let mut parser = Parser::new_from_str(text);
    let mut stack: Vec<Frame> = vec![Frame::Root];
    let mut documents = 0usize;
    let mut done: Option<serde_json::Value> = None;

    loop {
        let (event, _marker) = match parser.next() {
            Some(Ok(item)) => item,
            Some(Err(e)) => {
                return Err(yaml_error(format!(
                    "line {}: {}",
                    e.marker().line(),
                    e.info()
                )));
            }
            None => break,
        };
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err(yaml_error("expected a single YAML document"));
                }
            }
            Event::DocumentEnd => {}
            Event::Scalar(value, style, anchor, tag) => {
                reject_anchor(anchor)?;
                if tag.is_some() {
                    return Err(yaml_error("explicit YAML tags are not supported"));
                }
                let resolved = match style {
                    ScalarStyle::Plain => resolve_plain(&value)?,
                    ScalarStyle::SingleQuoted
                    | ScalarStyle::DoubleQuoted
                    | ScalarStyle::Literal
                    | ScalarStyle::Folded => serde_json::Value::String(value.into_owned()),
                };
                // Under a mapping the scalar is either the value of the
                // pending key or the next key itself; anywhere else it
                // is a delivered value.
                let pending_key = take_pending_key(&mut stack);
                match pending_key {
                    Some(key) => deliver(&mut stack, &mut done, resolved, Some(key))?,
                    None => match stack.last_mut() {
                        Some(Frame::Map { pending, .. }) => *pending = Some(scalar_key(&resolved)?),
                        _ => deliver(&mut stack, &mut done, resolved, None)?,
                    },
                }
            }
            Event::SequenceStart(anchor, _) => {
                open_frame(&mut stack, anchor, |key| Frame::Seq {
                    items: Vec::new(),
                    key,
                })?;
            }
            Event::MappingStart(anchor, _) => {
                open_frame(&mut stack, anchor, |key| Frame::Map {
                    entries: Vec::new(),
                    pending: None,
                    key,
                })?;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (value, frame_key) = match stack.pop() {
                    Some(Frame::Seq { items, key }) => (serde_json::Value::Array(items), key),
                    Some(Frame::Map {
                        entries,
                        key,
                        pending,
                    }) => {
                        if let Some(key) = &pending {
                            return Err(yaml_error(format!("mapping key '{key}' has no value")));
                        }
                        let mut seen = std::collections::HashSet::new();
                        for (key, _) in &entries {
                            if !seen.insert(key.clone()) {
                                return Err(yaml_error(format!("duplicate mapping key '{key}'")));
                            }
                        }
                        (
                            serde_json::Value::Object(entries.into_iter().collect()),
                            key,
                        )
                    }
                    _ => return Err(inconsistent_state()),
                };
                deliver(&mut stack, &mut done, value, frame_key)?;
            }
            Event::Alias(anchor) => {
                // The same one refusal the anchor guards consult — a
                // delivered alias always names a registered anchor
                // (saphyr refuses unknown ones itself, and ids start at
                // 1), so this arm always refuses; no impossible tail
                // beside the handled form.
                reject_anchor(anchor)?;
                return Err(inconsistent_state());
            }
        }
    }

    // The root frame is never popped; a well-formed document ends with
    // exactly it on the stack.
    if stack.len() > 1 {
        return Err(yaml_error("unexpected end of YAML input"));
    }
    match done {
        Some(value) => Ok(value),
        None if text.trim().is_empty() => Ok(serde_json::Value::Object(Default::default())),
        None => Err(yaml_error("no YAML value in document")),
    }
}

/// Deterministically render ordered mapping entries as YAML block-style
/// text, preserving the caller's key order (frontmatter order is part of
/// the format contract; `serde_json::Map` would re-sort it).
pub(crate) fn emit_yaml_entries(entries: &[(String, serde_json::Value)]) -> String {
    let mut out = String::new();
    emit_mapping(entries.iter().map(|(k, v)| (k, v)), 0, &mut out);
    out
}

enum Frame {
    Root,
    Seq {
        items: Vec<serde_json::Value>,
        /// The mapping key this sequence is the value of.
        key: Option<String>,
    },
    Map {
        entries: Vec<(String, serde_json::Value)>,
        /// A key seen but not yet given its value.
        pending: Option<String>,
        /// The mapping key this mapping is the value of.
        key: Option<String>,
    },
}

/// Take the pending key off the frame a new collection opens into, so
/// the key travels with the collection to its close.
fn take_pending_key(stack: &mut [Frame]) -> Option<String> {
    match stack.last_mut() {
        Some(Frame::Map { pending, .. }) => pending.take(),
        _ => None,
    }
}

/// Open a collection frame: the shared entry guard for sequences and
/// mappings — reject anchors, cap the nesting depth, and carry the
/// pending mapping key into the new frame.
fn open_frame(
    stack: &mut Vec<Frame>,
    anchor: usize,
    frame: impl FnOnce(Option<String>) -> Frame,
) -> Result<()> {
    reject_anchor(anchor)?;
    if stack.len() > MAX_YAML_DEPTH {
        return Err(yaml_error(format!(
            "nesting deeper than {MAX_YAML_DEPTH} levels"
        )));
    }
    // A collection under a mapping key carries that key with it; the
    // key is consumed when the collection closes.
    let key = take_pending_key(stack);
    stack.push(frame(key));
    Ok(())
}

/// Deliver a completed value into its parent container: a keyed push
/// under a mapping, a plain push into a sequence, or the document's root
/// value — the one dispatch both the scalar path and the
/// collection-close path consult. `key` is the mapping key the value
/// was opened under, when there is one.
fn deliver(
    stack: &mut [Frame],
    done: &mut Option<serde_json::Value>,
    value: serde_json::Value,
    key: Option<String>,
) -> Result<()> {
    match stack.last_mut() {
        Some(Frame::Map { entries, .. }) => {
            let key = key.ok_or_else(|| yaml_error("mapping value without a key"))?;
            entries.push((key, value));
        }
        Some(Frame::Seq { items, .. }) => items.push(value),
        Some(Frame::Root) | None => {
            if key.is_some() {
                return Err(yaml_error("mapping key outside a mapping"));
            }
            if done.replace(value).is_some() {
                return Err(yaml_error("more than one root value in document"));
            }
        }
    }
    Ok(())
}

fn reject_anchor(anchor: usize) -> Result<()> {
    if anchor > 0 {
        return Err(yaml_error("anchors and aliases are not supported"));
    }
    Ok(())
}

fn inconsistent_state() -> Error {
    yaml_error("inconsistent parser state")
}

fn scalar_key(value: &serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(key) => Ok(key.clone()),
        other => Err(yaml_error(format!(
            "mapping keys must be strings, found {other}"
        ))),
    }
}

/// YAML 1.2 core-schema resolution for plain scalars.
fn resolve_plain(raw: &str) -> Result<serde_json::Value> {
    match raw {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(serde_json::Value::Null),
        "true" | "True" | "TRUE" => return Ok(serde_json::Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(serde_json::Value::Bool(false)),
        _ => {}
    }
    if let Some(int) = parse_yaml_integer(raw) {
        return Ok(serde_json::Value::Number(int));
    }
    if let Some(float) = parse_yaml_float(raw) {
        return serde_json::Number::from_f64(float)
            .map(serde_json::Value::Number)
            .ok_or_else(|| yaml_error(format!("number '{raw}' is not representable")));
    }
    Ok(serde_json::Value::String(raw.to_string()))
}

fn parse_yaml_integer(raw: &str) -> Option<serde_json::Number> {
    raw.parse::<i64>().ok().map(serde_json::Number::from)
}

fn parse_yaml_float(raw: &str) -> Option<f64> {
    let value: f64 = raw.parse().ok()?;
    value.is_finite().then_some(value)
}

/// Split a markdown file into its YAML frontmatter text and body.
///
/// Frontmatter is the leading `---` fence closed by a lone `---` line
/// (the Obsidian convention); a file without the leading fence is all
/// body. Returns `(None, whole)` when there is no frontmatter.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let after_open = match text.strip_prefix("---\n").or_else(|| {
        // A CRLF fence still ends its line at the `\n`; the `\r` rides
        // along as the first byte of the YAML block, where the parser's
        // own line handling covers it.
        text.strip_prefix("---\r\n")
    }) {
        Some(rest) => rest,
        None => return (None, text),
    };
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let yaml = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return (Some(yaml), body);
        }
        offset += line.len();
    }
    // An unclosed fence is not frontmatter; the whole file is body.
    (None, text)
}

/// Re-join frontmatter YAML and body into file text (the inverse of
/// [`split_frontmatter`]).
pub(crate) fn join_frontmatter(yaml: &str, body: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + body.len() + 10);
    out.push_str("---\n");
    out.push_str(yaml);
    out.push_str("---\n");
    out.push_str(body);
    out
}

/// Deterministically render a JSON value as YAML block-style text
/// (ADR-0406 §2). The value set is what the canonical property encoding
/// emits; other values are rejected rather than approximated.
/// Production callers go through [`emit_yaml_entries`] (frontmatter
/// order is part of the format contract); this whole-value entry point
/// serves the tests' round-trip checks.
#[cfg(test)]
fn emit_yaml_value(value: &serde_json::Value) -> String {
    let mut out = String::new();
    emit_block(value, 0, &mut out);
    out
}

/// How one nested value renders inside a mapping or sequence: a
/// non-empty mapping or sequence is a block at a deeper indent; every
/// other value — scalars and empty collections — is a flow scalar on
/// the current line. The single classification both emitters' arms
/// consult.
enum Nested<'a> {
    Mapping(&'a serde_json::Map<String, serde_json::Value>),
    Sequence(&'a [serde_json::Value]),
    Flow,
}

fn nested(value: &serde_json::Value) -> Nested<'_> {
    match value {
        serde_json::Value::Object(inner) if !inner.is_empty() => Nested::Mapping(inner),
        serde_json::Value::Array(items) if !items.is_empty() => Nested::Sequence(items),
        _ => Nested::Flow,
    }
}

fn emit_mapping<'a, I>(entries: I, indent: usize, out: &mut String)
where
    I: IntoIterator<Item = (&'a String, &'a serde_json::Value)>,
{
    let pad = " ".repeat(indent);
    for (key, value) in entries {
        out.push_str(&pad);
        out.push_str(&emit_key(key));
        match nested(value) {
            Nested::Mapping(inner) => {
                out.push_str(":\n");
                emit_mapping(inner.iter(), indent + 2, out);
            }
            Nested::Sequence(items) => {
                out.push_str(":\n");
                emit_sequence(items, indent + 2, out);
            }
            Nested::Flow => {
                out.push_str(": ");
                write_scalar(out, &emit_scalar(value), indent + 2);
            }
        }
    }
}

fn emit_sequence(items: &[serde_json::Value], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for item in items {
        out.push_str(&pad);
        out.push_str("- ");
        match nested(item) {
            Nested::Mapping(inner) => {
                let mut block = String::new();
                emit_mapping(inner.iter(), indent + 2, &mut block);
                out.push_str(inline_after_marker(&block, indent + 2));
            }
            Nested::Sequence(inner) => {
                let mut block = String::new();
                emit_sequence(inner, indent + 2, &mut block);
                out.push_str(inline_after_marker(&block, indent + 2));
            }
            Nested::Flow => write_scalar(out, &emit_scalar(item), indent + 2),
        }
    }
}

/// Place a nested block's first line inline after the `- ` marker: the
/// block was emitted fully padded at `indent`, so exactly the first
/// line's pad is sliced away (every emitted line begins with it).
fn inline_after_marker(block: &str, indent: usize) -> &str {
    block[indent..].trim_start_matches(' ')
}

/// Write one scalar's text, indenting a block scalar's continuation
/// lines to `indent` (the first line is already placed inline).
fn write_scalar(out: &mut String, scalar: &str, indent: usize) {
    if let Some((first, rest)) = scalar.split_once('\n') {
        out.push_str(first);
        out.push('\n');
        let pad = " ".repeat(indent);
        for line in rest.split('\n') {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&pad);
                out.push_str(line);
                out.push('\n');
            }
        }
    } else {
        out.push_str(scalar);
        out.push('\n');
    }
}

/// Emit one value as a whole block (the top-level position: any mapping
/// or sequence renders as its block, even an empty one).
#[cfg(test)]
fn emit_block(value: &serde_json::Value, indent: usize, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => emit_mapping(map.iter(), indent, out),
        serde_json::Value::Array(items) => emit_sequence(items, indent, out),
        other => {
            out.push_str(&" ".repeat(indent));
            write_scalar(out, &emit_scalar(other), indent);
        }
    }
}

fn emit_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(true) => "true".into(),
        serde_json::Value::Bool(false) => "false".into(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => emit_string(text),
        // Block styles handle non-empty collections at the call site; an
        // empty one renders as its flow form, preserving its kind.
        serde_json::Value::Object(_) => "{}".into(),
        serde_json::Value::Array(_) => "[]".into(),
    }
}

/// Strings at least this long whose every line is block-scalar-safe
/// render as `|-` literal blocks instead of escape-heavy double-quoted
/// lines (ADR-0406 §2: long strings as block scalars, escape-safe
/// quoting below the threshold). One constant, deterministic.
const BLOCK_SCALAR_THRESHOLD: usize = 80;

/// A mapping key's spelling: plain when safe, double-quoted otherwise —
/// never the `|-` block form, which is invalid in key position (a
/// checkpoint that wrote one would produce a file the next open
/// demotes to raw passthrough, silently losing the node).
fn emit_key(text: &str) -> String {
    if is_plain_safe(text) {
        text.to_string()
    } else {
        double_quoted(text)
    }
}

/// Render a string scalar: plain when unambiguous, a `|-` literal block
/// for long line-safe text, double-quoted with escapes otherwise.
fn emit_string(text: &str) -> String {
    if is_plain_safe(text) {
        text.to_string()
    } else if let Some(block) = block_scalar(text) {
        block
    } else {
        double_quoted(text)
    }
}

/// A `|-` literal block for a long string whose lines carry no leading
/// or trailing whitespace and no control characters beyond the interior
/// `\n`/`\t` a block legitimately holds — the shapes a literal block
/// cannot represent faithfully. The content lines are indented by the
/// caller's emitter.
fn block_scalar(text: &str) -> Option<String> {
    if text.len() < BLOCK_SCALAR_THRESHOLD {
        return None;
    }
    if text.ends_with('\n') || text.trim().is_empty() {
        return None;
    }
    // Every character the double-quoted form would escape disqualifies
    // the literal block: each C0-or-DEL control except the interior
    // \n/\t. That set includes the \r the line checks below cannot see
    // (`str::lines` splits on \n) — YAML's CR/CRLF→LF normalization
    // would silently rewrite a `|-` block on reload, a lossy round-trip
    // the double-quoted \r escape preserves instead — and the other
    // controls, which the quoted form escapes as \uXXXX. Both gates
    // consult the one control vocabulary (`is_c0_del`), so they cannot
    // drift apart.
    if text
        .chars()
        .any(|ch| is_c0_del(ch) && !matches!(ch, '\n' | '\t'))
    {
        return None;
    }
    for line in text.lines() {
        if line.starts_with([' ', '\t']) || line.ends_with([' ', '\t']) {
            return None;
        }
    }
    Some(format!("|-\n{text}"))
}

fn is_plain_safe(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let first = text.as_bytes()[0];
    if matches!(
        first,
        b'!' | b'&'
            | b'*'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'#'
            | b'|'
            | b'>'
            | b'@'
            | b'`'
            | b'"'
            | b'\''
            | b'%'
            | b','
            | b'-'
            | b'?'
            | b':'
            | b' '
    ) {
        return false;
    }
    if text.ends_with([':', ' ', '\t']) {
        return false;
    }
    if text.chars().any(is_c0_del) {
        return false;
    }
    if text.contains(": ") || text.contains(" #") {
        return false;
    }
    // A plain scalar that would resolve to null, a bool or a number on
    // reload is a different value — quote it so it stays a string.
    !matches!(
        resolve_plain(text),
        Ok(serde_json::Value::Null)
            | Ok(serde_json::Value::Bool(_))
            | Ok(serde_json::Value::Number(_))
    )
}

fn double_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_c0_del(c) => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;
