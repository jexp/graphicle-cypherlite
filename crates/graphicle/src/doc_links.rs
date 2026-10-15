//! Documents linked to the code symbols they name.
//!
//! A document names code by marking it as code:
//! - a Markdown code span (`` `parse_header` ``, `` `wire.py::parse_header` ``);
//! - a reStructuredText role (``:func:`parse_header` ``), object directive or
//!   code-shaped inline literal;
//! - a connector's inline-code formatting: a Jira ADF `code` mark or a
//!   Notion `code` annotation. The converters keep the converted text
//!   byte-identical and record those spans as byte offsets in the document's
//!   metadata (`code_spans`), never as backticks in the text, so a document's
//!   sha256, and every memory that cites it, stay as they were.
//!   An imported JSONL document may carry its own `code_spans`;
//! - backticks a connector's text already carries: Confluence's HTML
//!   conversion has always written `<code>` as backticks, and GitHub and
//!   Slack text is Markdown.
//!
//! Plain words never make a mention here; an extension's rung may link them.
//!
//! Each mention resolves as the engine resolves Markdown mentions
//! (`graphicle::markdown_resolution`, the `--docs` resolver): to the one
//! code definition with that name, every
//! qualifier naming an enclosing definition or a part of its path, never a
//! builtin. The document's own project is searched first; only when it has
//! no candidate, the other projects of the workspace (a design-document
//! project naming code in a code project). A name with
//! several candidates links nothing and is recorded as ambiguous.
//!
//! The links are `LINKS_TO` edges of the store's graph (rung `mention`,
//! producer `core`; see the "link store" section below), never nodes of the
//! shared episode: the episode is read by every reader of
//! a project, while connector documents are filtered per reader. The
//! workspace's `link` job recomputes them after every
//! projection and connector sync, and `graph.link` on request ([`relink`]),
//! only for the documents a change can reach; a query only reads them,
//! filtered by what the reader may read now and checked against the bytes it
//! reads (queries stay read-only). An extension's rows
//! name the extension as their producer and are read only while it is
//! installed.
//!
//! The `doc_links` switch: absent, a query follows no link: it counts the
//! links of the selected windows (`withheld_repository`,
//! `withheld_connector`) and lifts nothing, since whether lifted windows
//! help answers has not been measured; documents take part in the code arm,
//! `rank` and `graph_local` as they did before links were recorded. `true`
//! follows the links of repository and connector documents; `false` reads
//! none, and the code arm also leaves out document nodes and their
//! `references` edges, as `rank` and `graph_local` leave out documents'
//! file edges to code, so no document reaches code.
use crate::{
    Error, Result, digest, graph_projection, now,
    query::Snapshot,
    store::{Principal, Store, one, rows, s},
    store_graph::{self, GraphStore, Params, json_to_value},
};
use graphicle::graph::Graph;
use graphicle_memory::graph::span;
use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, PoisonError},
};

/// The packet field and the evidence facts of the links followed.
pub const ARM: &str = "doc_links";
/// The rung of an explicit, uniquely resolved mention.
pub const RUNG: &str = "mention";
/// Rows this program writes; an extension's rows name their own producer.
pub const PRODUCER: &str = "core";
/// The extraction and resolution rules. Rows computed under other rules
/// are recomputed by the next `link` job, which opening the store queues
/// ([`migrate`]). `doc-links/2`: rows keep what they depend on, and the
/// SDK's conversation sessions are not read.
pub const RULES: &str = "doc-links/2";
/// Mentions recorded per document; the rest are counted as omitted.
pub const MAX_MENTIONS: usize = 2000;
/// Longest mention text kept (an identifier, a dotted name or a path).
pub const MAX_MENTION_BYTES: usize = 300;
/// Windows one query lifts through links, as the code arm's
/// `UNCHUNKED_LIMIT` bounds its anchors.
pub const LIFT_LIMIT: usize = 6;
/// Inline-code spans a connector document may record in its metadata.
pub const MAX_CODE_SPANS: usize = 1000;
/// A lifted paragraph of a document that no passage covers.
const PARAGRAPH_BYTES: usize = 1600;

// ---------------------------------------------------------------------------
// The link store (migration plan §2.5): `(:ConnectorDoc {repository_id, id})`
// nodes — one per document read, carrying what its rows were computed from
// (the SQL `gf_doc_link_sources` row as properties: `source_kind`,
// `document_sha256`, `input`, `counts`, `dependencies`, `updated_at`, and
// `connector` for a connector document) — with one `[:LINKS_TO]` edge per
// mention (the SQL `gf_doc_links` row as edge properties, plus `seq`, the
// mention's order in its document), from the document to the `(:Symbol)` a
// linked mention resolved to, or back to the document itself when it did
// not resolve to a recorded symbol. `(:DocLinkCheckpoint {repository_id})`
// nodes hold the episode of each project the workspace's rows were last
// checked against, and one `(:DocLinkState {workspace_id})` node per
// workspace the rules its rows were computed under. The DDL is the graph
// type applied at open ([`crate::store_graph`]); the rows stay out of the
// shared episode, as before.

/// The store's graph store, found from a SQL connection's database file
/// (the canonicalized registry in [`crate::store::graph_for`], so the one
/// engine is shared however the path is spelled).
fn graph_of(db: &Connection) -> Result<std::sync::Arc<GraphStore>> {
    crate::store::graph_for(db)
}

/// Graph query parameters from `(name, JSON)` pairs.
fn gparams(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Params {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), json_to_value(&value)))
        .collect()
}

/// The columns of one link row as readers expect them (the SQL rows'
/// shape): the edge's properties plus its document's identity. The target's
/// facts are fixed-key dotted properties (`target.node_id`, …), reassembled
/// by [`target_facts`]; only genuinely dynamic payloads (`dependencies`)
/// stay stringified JSON.
#[doc(hidden)]
pub const LINK_COLUMNS: &str = "d.repository_id AS repository_id, d.id AS document_id, \
     l.document_sha256 AS document_sha256, l.source_kind AS source_kind, \
     l.byte_start AS byte_start, l.byte_end AS byte_end, l.mention AS mention, l.mark AS mark, \
     l.state AS state, l.scope AS scope, l.candidates AS candidates, \
     l.target_repository_id AS target_repository_id, l.stable_id AS stable_id, \
     l.target_relpath AS target_relpath, l.target_sha256 AS target_sha256, \
     l.target_start AS target_start, l.target_end AS target_end, \
     l.`target.node_id` AS target_node_id, l.`target.revision` AS target_revision, \
     l.`target.qualified_name` AS target_qualified_name, l.`target.kind` AS target_kind, \
     l.`target.label` AS target_label, l.`target.episode` AS target_episode, \
     l.`target.confidence` AS target_confidence, l.`target.names` AS target_names, \
     l.`target.line` AS target_line, l.`target.end_line` AS target_end_line, \
     l.rung AS rung, l.producer AS producer";

/// A link row's target facts as the SQL row's `target` JSON held them
/// (`{}` when the mention never linked): the dotted properties
/// reassembled.
#[doc(hidden)]
pub fn target_facts(row: &Value) -> Value {
    if row["target_node_id"].is_null() {
        return json!({});
    }
    let mut facts = json!({"node_id":row["target_node_id"],"revision":row["target_revision"],
        "qualified_name":row["target_qualified_name"],"kind":row["target_kind"],
        "label":row["target_label"],"episode":row["target_episode"],
        "confidence":row["target_confidence"],"names":row["target_names"]});
    if !row["target_line"].is_null() {
        facts["line"] = row["target_line"].clone();
        facts["end_line"] = row["target_end_line"].clone();
    }
    facts
}

/// A workspace whose rows were not computed under [`RULES`] (a store from
/// before the links, from other rules, or from before the move to the graph
/// store, which has no state node) gets its latest `link` job queued again,
/// unless one is waiting or running, so the next drain or worker computes
/// them. The SQL link tables of a store from before the move are dropped;
/// the queued job recomputes the rows in the graph store.
pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "DROP TABLE IF EXISTS gf_doc_links; DROP TABLE IF EXISTS gf_doc_link_sources;
         DROP TABLE IF EXISTS gf_doc_link_checkpoints; DROP TABLE IF EXISTS gf_doc_link_state;",
    )?;
    let current: HashSet<String> = graph_of(db)?
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (t:DocLinkState) WHERE t.rules = $rules RETURN t.workspace_id AS id",
                gparams([("rules", json!(RULES))]),
            )
        })?
        .iter()
        .map(|row| s(row, "id").to_string())
        .collect();
    for row in rows(db, "SELECT w.id FROM workspaces w", [])? {
        let workspace = s(&row, "id");
        if current.contains(workspace) {
            continue;
        }
        let jobs = "FROM am_jobs j JOIN repo_projects r ON r.id=j.repository_id \
                    WHERE r.workspace_id=? AND j.kind='link'";
        if one(
            db,
            &format!("SELECT 1 AS yes {jobs} AND j.state IN ('queued','retry','running') LIMIT 1"),
            [workspace],
        )?
        .is_some()
        {
            continue;
        }
        if let Some(latest) = one(
            db,
            &format!("SELECT j.id {jobs} ORDER BY j.updated_at DESC,j.id LIMIT 1"),
            [workspace],
        )? {
            let stamp = now();
            db.execute(
                "UPDATE am_jobs SET state='queued',attempt=0,not_before=?,updated_at=?,error=NULL,\
                 lease_token=NULL,lease_until=NULL WHERE id=?",
                params![stamp, stamp, s(&latest, "id")],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mentions

/// One code mention in a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    /// The mention's text in the document (without its marks).
    pub byte_start: usize,
    pub byte_end: usize,
    /// How the document marks it as code: `code_span`, `role`, `directive`,
    /// `literal` or `code_mark` (a connector's inline-code formatting).
    pub mark: &'static str,
    /// A cited file (`wire.py::parse_header`), resolved first.
    pub path: Option<String>,
    /// Qualifiers, then the name.
    pub names: Vec<String>,
}

/// How a document's mentions are found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Rst,
    Connector,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Rst => "rst",
            Self::Connector => "connector",
        }
    }
}

/// The format of a repository file the resolver reads: Markdown (the
/// engine's Markdown suffixes) or reStructuredText. Sphinx `.txt` pages count
/// only when a `conf.py` declares them ([`sphinx_txt`]).
pub fn format_of(path: &str) -> Option<Format> {
    let lower = path.to_ascii_lowercase();
    if graphicle::markdown_resolution::MARKDOWN_MENTION_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        Some(Format::Markdown)
    } else if lower.ends_with(".rst") {
        Some(Format::Rst)
    } else {
        None
    }
}

fn stripped(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// The engine's `strip` (Python's): whitespace and the separators U+001C to
/// U+001F.
fn strip(text: &str) -> &str {
    text.trim_matches(stripped)
}

/// A document's lines as the engine reads it (`\r\n` and `\r` folded to line
/// breaks, then Python's line separators), each with its byte offset.
pub fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            out.push((start, &text[start..at]));
            let mut next = at + c.len_utf8();
            if c == '\r' && text.as_bytes().get(next) == Some(&b'\n') {
                chars.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < text.len() {
        out.push((start, &text[start..]));
    }
    out
}

/// The text of each Markdown code span of one line, as byte ranges in the
/// line: what the engine's pattern `(?<!`)(`+)(?!`)(.+?)(?<!`)\1(?!`)` finds,
/// a run of backticks, at least one character and the next run of exactly as
/// many backticks.
pub fn code_spans(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let run = |from: usize| {
        let mut to = from;
        while to < bytes.len() && bytes[to] == b'`' {
            to += 1;
        }
        to
    };
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let open = run(at);
        let width = open - at;
        // The first content character is not a backtick (the run is whole),
        // so any later backtick starts after it.
        let mut close = None;
        let mut probe = open + 1;
        while probe < bytes.len() {
            if bytes[probe] != b'`' {
                probe += 1;
                continue;
            }
            let end = run(probe);
            if end - probe == width {
                close = Some(probe);
                break;
            }
            probe = end;
        }
        match close {
            Some(close) if open < close => {
                out.push((open, close));
                at = close + width;
            }
            _ => at = open,
        }
    }
    out
}

/// Record `text[a..b]` as a mention when it names code the way a Markdown
/// code span does (`graphicle::extractors::markdown::code_span_mention`).
fn push_code(out: &mut Vec<Mention>, text: &str, a: usize, b: usize, mark: &'static str) {
    let Some(span) = text.get(a..b) else {
        return;
    };
    let Some((path, names)) = graphicle::extractors::markdown::code_span_mention(span) else {
        return;
    };
    let lead = span.len() - span.trim_start_matches(stripped).len();
    let length = strip(span).len();
    if length == 0 || length > MAX_MENTION_BYTES {
        return;
    }
    out.push(Mention {
        byte_start: a + lead,
        byte_end: a + lead + length,
        mark,
        path,
        names,
    });
}

/// Mentions of a Markdown document, as the engine's Markdown extractor finds
/// them: code spans outside fenced code blocks and the front matter.
pub fn markdown(text: &str) -> Vec<Mention> {
    let lines = lines(text);
    let bare: Vec<&str> = lines.iter().map(|(_, line)| *line).collect();
    let (_, body) = graphicle::extractors::markdown::split_frontmatter(&bare);
    let mut out = Vec::new();
    let mut fenced = false;
    for (index, (offset, line)) in lines.iter().enumerate() {
        if strip(line).starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || index < body {
            continue;
        }
        for (a, b) in code_spans(line) {
            push_code(&mut out, text, offset + a, offset + b, "code_span");
        }
    }
    out
}

/// Python's whitespace class, as the engine's patterns use it.
const PY_WHITESPACE: &str = r"[\t\n\r\x0B\x0C\x1C-\x1F\x20\x85\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}]";

fn py(pattern: &str) -> Regex {
    Regex::new(&pattern.replace(r"\s", PY_WHITESPACE)).expect("static reStructuredText pattern")
}

// The engine's reStructuredText patterns (`extractors/rst.rs`).
static ROLE: LazyLock<Regex> = LazyLock::new(|| {
    py(r":(?:py:)?(?:func|meth|class|attr|exc|data|obj|const|decorator|property):`([^`]+)`")
});
static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    py(
        r"^\s*\.\.\s+(?:py:)?(?:auto)?(?:function|class|method|attribute|exception|data|decorator|property|classmethod|staticmethod)::\s*([^\s(]+)",
    )
});
static INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| py(r"^\s*\.\.\s+(?:literalinclude|include)::\s*(\S+)"));
static LITERAL: LazyLock<Regex> = LazyLock::new(|| py(r"``([^`]+)``"));
static CODE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    py(
        r"^\s*\.\.\s+(?:code-block|code|sourcecode|doctest|testcode|testoutput|ipython|highlight-block|parsed-literal|math)::",
    )
});
const UNDERLINE: &str = "=-`:'\"~^_*+#<>.";

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_rule(line: &str, width: usize) -> Option<char> {
    let t = line.trim_end();
    let c = t.chars().next()?;
    (UNDERLINE.contains(c) && t.chars().all(|x| x == c) && t.chars().count() >= width.max(2))
        .then_some(c)
}

/// A role or directive target's dotted parts (the engine's `target_names`).
fn target_names(target: &str) -> Option<Vec<String>> {
    let t = target.trim();
    let t = match (t.rfind('<'), t.ends_with('>')) {
        (Some(at), true) => &t[at + 1..t.len() - 1],
        _ => t,
    };
    let t = t.trim_start_matches(['~', '!', '.']).trim_end_matches("()");
    let parts = t.split('.').filter(|p| !p.is_empty()).collect::<Vec<_>>();
    let ident = |p: &str| {
        p.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && p.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    if parts.is_empty() || !parts.iter().all(|p| ident(p)) {
        return None;
    }
    Some(parts.into_iter().map(str::to_string).collect())
}

/// Whether an inline literal is shaped like code (the engine's
/// `code_shaped`): a dotted path, a call or a CamelCase name.
fn code_shaped(literal: &str) -> bool {
    let text = literal.trim();
    let name = text.strip_suffix("()").unwrap_or(text);
    name.len() < text.len()
        || name.contains('.')
        || name.contains("::")
        || (name.starts_with(char::is_uppercase)
            && name.chars().filter(|c| c.is_uppercase()).count() >= 2
            && name.chars().any(char::is_lowercase))
}

/// `text` without its role mentions, as the engine scans literals, with the
/// offset in `text` of each kept segment: `(offset in the result, offset in
/// text)`.
fn without_roles(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut out = String::with_capacity(text.len());
    let mut segments = vec![(0, 0)];
    let mut last = 0;
    for found in ROLE.find_iter(text) {
        out.push_str(&text[last..found.start()]);
        last = found.end();
        segments.push((out.len(), last));
    }
    out.push_str(&text[last..]);
    (out, segments)
}

fn original(segments: &[(usize, usize)], at: usize) -> usize {
    let index = segments.partition_point(|(start, _)| *start <= at) - 1;
    let (start, from) = segments[index];
    from + (at - start)
}

fn rst_line(out: &mut Vec<Mention>, base: usize, text: &str, module: &dyn Fn(&[String]) -> bool) {
    for found in ROLE.captures_iter(text) {
        let target = found.get(1).expect("role target");
        if let Some(names) = target_names(target.as_str())
            && target.len() <= MAX_MENTION_BYTES
        {
            out.push(Mention {
                byte_start: base + target.start(),
                byte_end: base + target.end(),
                mark: "role",
                path: None,
                names,
            });
        }
    }
    let (scanned, segments) = without_roles(text);
    for found in LITERAL.captures_iter(&scanned) {
        let literal = found.get(1).expect("literal text");
        if !code_shaped(literal.as_str()) {
            continue;
        }
        let Some((path, names)) =
            graphicle::extractors::markdown::code_span_mention(literal.as_str())
        else {
            continue;
        };
        if path.is_none() && !literal.as_str().trim().ends_with("()") && module(&names) {
            continue;
        }
        let a = original(&segments, literal.start());
        let b = original(&segments, literal.end() - 1) + 1;
        let span = &text[a..b];
        let lead = span.len() - span.trim_start().len();
        let length = span.trim().len();
        if length == 0 || length > MAX_MENTION_BYTES {
            continue;
        }
        out.push(Mention {
            byte_start: base + a + lead,
            byte_end: base + a + lead + length,
            mark: "literal",
            path,
            names,
        });
    }
}

/// Mentions of a reStructuredText page, as the engine's `--docs` extractor
/// finds them: roles, object directives and code-shaped literals, in titles
/// and text, outside literal blocks. `module` says whether dotted names spell
/// a module or package of the page's project, which a literal never names.
pub fn rst(text: &str, module: &dyn Fn(&[String]) -> bool) -> Vec<Mention> {
    let lines = lines(text);
    let mut out = Vec::new();
    let mut literal: Option<Option<usize>> = None;
    let mut i = 0;
    while i < lines.len() {
        let (offset, line) = lines[i];
        if let Some(block) = literal {
            if line.trim().is_empty() {
                i += 1;
                continue;
            }
            match block {
                None if indent(line) > 0 => literal = Some(Some(indent(line))),
                Some(width) if indent(line) >= width => {}
                _ => literal = None,
            }
            if literal.is_some() {
                i += 1;
                continue;
            }
        }
        let title = line.trim();
        if !title.is_empty()
            && indent(line) == 0
            && is_rule(line, 1).is_none()
            && lines
                .get(i + 1)
                .and_then(|(_, next)| is_rule(next, title.chars().count()))
                .is_some()
        {
            rst_line(&mut out, offset, title, module);
            i += 2;
            continue;
        }
        if INCLUDE.is_match(line) {
            // A file reference, not a mention.
        } else if let Some(found) = DIRECTIVE.captures(line) {
            let target = found.get(1).expect("directive target");
            if let Some(names) = target_names(target.as_str())
                && target.len() <= MAX_MENTION_BYTES
            {
                out.push(Mention {
                    byte_start: offset + target.start(),
                    byte_end: offset + target.end(),
                    mark: "directive",
                    path: None,
                    names,
                });
            }
        } else {
            rst_line(&mut out, offset, line, module);
        }
        if CODE_DIRECTIVE.is_match(line)
            || (line.trim_end().ends_with("::") && !line.trim_start().starts_with(".."))
        {
            literal = Some(None);
        }
        i += 1;
    }
    out
}

/// Mentions of a connector document: its recorded inline-code spans and the
/// Markdown code spans its text carries, in document order, one per range.
pub fn connector(text: &str, spans: &[(usize, usize)]) -> Vec<Mention> {
    let mut out = Vec::new();
    for &(a, b) in spans {
        push_code(&mut out, text, a, b, "code_mark");
    }
    out.extend(markdown(text));
    out.sort_by_key(|m| (m.byte_start, m.byte_end));
    let mut kept: Vec<Mention> = Vec::with_capacity(out.len());
    for mention in out {
        if kept
            .last()
            .is_none_or(|last| last.byte_end <= mention.byte_start)
        {
            kept.push(mention);
        }
    }
    kept
}

/// Validated inline-code spans of a document's text: `[start, end]` byte
/// pairs on character boundaries, at most [`MAX_CODE_SPANS`].
pub fn parse_code_spans(value: &Value, content: &str) -> Result<Vec<(usize, usize)>> {
    let Some(list) = value.as_array() else {
        return Err(Error::bad(
            "code_spans must list [start, end] byte offsets into the content",
        ));
    };
    if list.len() > MAX_CODE_SPANS {
        return Err(Error::bad(format!(
            "code_spans lists at most {MAX_CODE_SPANS} spans"
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for pair in list {
        let bound = |index: usize| {
            pair.get(index)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        match (pair.as_array().map(Vec::len), bound(0), bound(1)) {
            (Some(2), Some(a), Some(b))
                if a < b
                    && b <= content.len()
                    && content.is_char_boundary(a)
                    && content.is_char_boundary(b) =>
            {
                out.push((a, b));
            }
            _ => {
                return Err(Error::bad(
                    "Each code span is [start, end], 0 <= start < end <= the content's bytes, on character boundaries",
                ));
            }
        }
    }
    Ok(out)
}

/// A converter's inline-code spans as a document's `code_spans`: `None`
/// without any, else the first [`MAX_CODE_SPANS`].
pub fn spans_json(spans: &[(usize, usize)]) -> Option<Value> {
    (!spans.is_empty()).then(|| {
        json!(
            spans
                .iter()
                .take(MAX_CODE_SPANS)
                .map(|(a, b)| [a, b])
                .collect::<Vec<_>>()
        )
    })
}

/// The recorded inline-code spans of a stored connector document, ignoring
/// any that no longer fit its text.
fn stored_spans(value: &Value, content: &str) -> Vec<(usize, usize)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let a = usize::try_from(pair.get(0)?.as_u64()?).ok()?;
            let b = usize::try_from(pair.get(1)?.as_u64()?).ok()?;
            (a < b && content.get(a..b).is_some()).then_some((a, b))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Resolution

fn node_text<'a>(attributes: &'a impl graphicle::record::Properties, key: &str) -> &'a str {
    attributes.get(key).and_then(Value::as_str).unwrap_or("")
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(list)) => !list.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn symbol_label(label: &str) -> &str {
    label.trim_matches(['(', ')']).trim_start_matches('.')
}

fn posix(path: &str) -> String {
    path.replace('\\', "/")
}

/// A recorded code definition a mention resolved to.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub repository: String,
    pub episode: String,
    pub node_id: String,
    pub stable_id: Option<String>,
    pub revision: Option<String>,
    pub relpath: String,
    pub qualified_name: Option<String>,
    pub kind: Option<String>,
    pub label: String,
    /// `EXTRACTED` for a cited file, `INFERRED` for a name (the engine's).
    pub confidence: &'static str,
}

/// The resolution view of one project's episode: its code definitions by
/// name and by file, and their containment, as the engine's
/// `resolve_markdown_mentions` builds them over an extraction.
struct Index {
    repository: String,
    episode: String,
    graph: graphicle::session::GraphHandle,
    callable: HashMap<String, Vec<usize>>,
    by_file: HashMap<String, HashMap<String, Vec<usize>>>,
    files: HashSet<String>,
    parents: HashMap<usize, BTreeSet<usize>>,
    /// Definition start lines per file (file nodes out), as the code arm
    /// collects them: a definition's window runs to the next start.
    starts: HashMap<String, Vec<usize>>,
    /// The episode's source manifest, read once when a window needs it.
    manifest: std::cell::OnceCell<Option<BTreeMap<String, String>>>,
}

fn node_relpath(attributes: &impl graphicle::record::Properties) -> String {
    posix(
        Some(node_text(attributes, "relpath"))
            .filter(|v| !v.is_empty())
            .unwrap_or(node_text(attributes, "source_file")),
    )
}

impl Index {
    fn new(repository: &str, episode: &str, graph: graphicle::session::GraphHandle) -> Self {
        let view: &Graph = &graph;
        let mut callable: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut starts: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, node) in view.nodes().iter().enumerate() {
            let a = &node.attributes;
            if node_text(a, "kind") != "file"
                && let Some(line) = span::start_line(a)
            {
                let path = node_relpath(a);
                if !path.is_empty() {
                    starts.entry(path).or_default().push(line);
                }
            }
            let source = node_text(a, "source_file");
            let file_node = !source.is_empty()
                && node_text(a, "label") == source.rsplit('/').next().unwrap_or("");
            if node.id.as_str().is_none_or(str::is_empty)
                || node_text(a, "file_type") != "code"
                || file_node
            {
                continue;
            }
            let label = symbol_label(node_text(a, "label"));
            if label.is_empty() {
                continue;
            }
            if truthy(a.get("_callable")) {
                callable.entry(label.to_string()).or_default().push(index);
            }
            let file = posix(source);
            if !file.is_empty() {
                by_file
                    .entry(file)
                    .or_default()
                    .entry(label.to_string())
                    .or_default()
                    .push(index);
            }
        }
        let mut parents: HashMap<usize, BTreeSet<usize>> = HashMap::new();
        for (source, target, edge) in view.iter_edges() {
            if graphicle::markdown_resolution::CONTAINMENT_RELATIONS
                .contains(&node_text(&edge.attributes, "relation"))
            {
                parents.entry(target).or_default().insert(source);
            }
        }
        let files = by_file.keys().cloned().collect();
        for list in starts.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        Self {
            repository: repository.to_string(),
            episode: episode.to_string(),
            graph,
            callable,
            by_file,
            files,
            parents,
            starts,
            manifest: std::cell::OnceCell::new(),
        }
    }

    /// The episode's manifest (path → sha256), read once.
    fn manifest(&self, db: &Connection) -> Result<Option<&BTreeMap<String, String>>> {
        if self.manifest.get().is_none() {
            let loaded = graph_projection::episode_heads(db, &self.repository, &self.episode)?;
            let _ = self.manifest.set(loaded);
        }
        Ok(self.manifest.get().and_then(Option::as_ref))
    }

    /// Labels of a node's enclosing definitions and the parts of its path
    /// (the engine's `evidence`).
    fn evidence(&self, node: usize) -> HashSet<String> {
        let graph: &Graph = &self.graph;
        let mut found = HashSet::new();
        let mut frontier = vec![node];
        let mut seen = HashSet::new();
        while let Some(at) = frontier.pop() {
            if !seen.insert(at) {
                continue;
            }
            for &owner in self.parents.get(&at).into_iter().flatten() {
                found.insert(
                    symbol_label(node_text(&graph.nodes()[owner].attributes, "label")).to_string(),
                );
                frontier.push(owner);
            }
        }
        for segment in posix(node_text(&graph.nodes()[node].attributes, "source_file"))
            .split('/')
            .filter(|s| !s.is_empty())
        {
            found.insert(segment.to_string());
            let stem = segment
                .rfind('.')
                .filter(|&at| segment[..at].chars().any(|c| c != '.'))
                .map_or(segment, |at| &segment[..at]);
            found.insert(stem.to_string());
        }
        found
    }

    /// The engine's `resolve_chain` over one file's labels, keeping every
    /// candidate rather than only a unique one.
    fn chain(&self, labels: &HashMap<String, Vec<usize>>, names: &[String]) -> Vec<usize> {
        let Some(last) = names.last() else {
            return Vec::new();
        };
        let mut candidates = labels.get(last).cloned().unwrap_or_default();
        for (index, owner) in names[..names.len() - 1].iter().rev().enumerate() {
            let owners: HashSet<usize> = labels.get(owner).into_iter().flatten().copied().collect();
            candidates.retain(|candidate| {
                let mut chain = HashSet::from([*candidate]);
                for _ in 0..=index {
                    chain = chain
                        .iter()
                        .flat_map(|node| self.parents.get(node).into_iter().flatten().copied())
                        .collect();
                }
                !chain.is_disjoint(&owners)
            });
        }
        candidates
    }

    /// Every definition `mention` may name in this project: a cited file's
    /// definitions down its qualifiers, else the callable definitions of that
    /// name whose qualifiers all appear in their evidence. `doc_file` is the
    /// document's path in this project, for relative citations.
    fn candidates(&self, mention: &Mention, doc_file: &str) -> Vec<usize> {
        let mut names = mention.names.clone();
        let Some(last) = names.last_mut() else {
            return Vec::new();
        };
        *last = strip(last).to_string();
        if last.is_empty() {
            return Vec::new();
        }
        if let Some(path) = &mention.path {
            let Some(file) =
                graphicle::markdown_resolution::match_cited_file(path, doc_file, &self.files)
            else {
                return Vec::new();
            };
            return self.chain(&self.by_file[&file], &names);
        }
        let (callee, qualifiers) = names.split_last().expect("one name");
        self.callable
            .get(callee)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&node| {
                let found = self.evidence(node);
                qualifiers.iter().all(|q| found.contains(q))
            })
            .collect()
    }

    fn target(&self, node: usize, confidence: &'static str) -> Target {
        let graph: &Graph = &self.graph;
        let a = &graph.nodes()[node].attributes;
        let optional = |key: &str| {
            Some(node_text(a, key))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Target {
            repository: self.repository.clone(),
            episode: self.episode.clone(),
            node_id: graphicle::serve_search::id(graph, node),
            stable_id: optional("stable_id"),
            revision: optional("revision"),
            relpath: node_relpath(a),
            qualified_name: optional("qualified_name"),
            kind: optional("kind"),
            label: node_text(a, "label").to_string(),
            confidence,
        }
    }
}

/// How one mention resolved.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// One definition, in the document's project (`project`) or, when that
    /// has none, in another project of the workspace (`workspace`).
    Linked(Box<Target>, &'static str),
    /// Several definitions in one scope: nothing is linked.
    Ambiguous(usize, &'static str),
    /// No definition, or a builtin name.
    Unresolved,
}

fn confidence(mention: &Mention) -> &'static str {
    if mention.path.is_some() {
        "EXTRACTED"
    } else {
        "INFERRED"
    }
}

/// Resolve `mention` of a document in `own`, its project (`doc_file` its
/// path there): `None` when that project has no candidate, so the other
/// projects of the workspace are searched ([`resolve_elsewhere`]). A builtin
/// name never resolves (the engine's rule).
fn resolve_own(mention: &Mention, doc_file: &str, own: Option<&Index>) -> Option<Outcome> {
    if mention.path.is_none()
        && mention.names.last().is_some_and(|name| {
            graphicle::extractors::base::LANGUAGE_BUILTIN_GLOBALS.contains(&strip(name))
        })
    {
        return Some(Outcome::Unresolved);
    }
    let own = own?;
    let found = own.candidates(mention, doc_file);
    match found.len() {
        0 => None,
        1 => Some(Outcome::Linked(
            Box::new(own.target(found[0], confidence(mention))),
            "project",
        )),
        n => Some(Outcome::Ambiguous(n, "project")),
    }
}

/// Resolve `mention` across the other projects of the workspace, as one
/// scope: unique there or nothing.
fn resolve_elsewhere(mention: &Mention, others: &[&Index]) -> Outcome {
    let confidence = confidence(mention);
    let mut found: Vec<(&Index, usize)> = Vec::new();
    for index in others {
        found.extend(
            index
                .candidates(mention, "")
                .into_iter()
                .map(|node| (*index, node)),
        );
    }
    match found.len() {
        0 => Outcome::Unresolved,
        1 => Outcome::Linked(
            Box::new(found[0].0.target(found[0].1, confidence)),
            "workspace",
        ),
        n => Outcome::Ambiguous(n, "workspace"),
    }
}

// ---------------------------------------------------------------------------
// The link job

/// The metadata `schema` of the conversation sessions that the Python SDK's
/// `Memory` facade stores (`graphicle_sdk.memory.SCHEMA`). Sessions are
/// conversation memory, which is not linked to code: they are never read
/// for links, and importing them queues no `link` job.
pub const SDK_SESSION_SCHEMA: &str = "graphicle.memory-session.v1";

/// Documents written per transaction, so that relinking a large workspace
/// never holds the store's write lock for long.
const WRITE_BATCH: usize = 200;

/// A document the `link` job reads.
#[derive(Clone, Debug)]
struct Document {
    project: String,
    id: String,
    kind: &'static str,
    format: Format,
    sha: String,
    /// The repository path, for a repository file.
    path: Option<String>,
    /// `(connector, document)`, for a connector document.
    connector: Option<(String, String)>,
    spans: Value,
    input: String,
}

/// One project of the workspace as the job reads it.
struct Project {
    id: String,
    heads: BTreeMap<String, String>,
    /// The episode its links resolve against: the current head set's when
    /// recorded, else the latest recorded one (the lag fallback's reading).
    episode: Option<String>,
}

/// What a document's rows depend on besides its own bytes, its inline-code
/// spans and the rules, kept beside them (`gf_doc_link_sources.
/// dependencies`): every name its mentions look up (qualifiers, names and a
/// cited file's base name), the names that found no candidate in its own
/// project and were looked up in the workspace's other projects, and the
/// file each linked window was read from, with its sha256, by project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Dependencies {
    names: BTreeSet<String>,
    elsewhere: BTreeSet<String>,
    targets: BTreeMap<String, BTreeMap<String, String>>,
}

/// The names `mention`'s resolution reads: its qualifiers and name as
/// definitions' labels compare, and the base name of the file it cites.
fn mention_names(mention: &Mention) -> impl Iterator<Item = String> + '_ {
    mention
        .names
        .iter()
        .map(|name| symbol_label(strip(name)).to_string())
        .chain(mention.path.iter().filter_map(|path| {
            posix(path)
                .rsplit('/')
                .next()
                .filter(|base| !base.is_empty())
                .map(str::to_string)
        }))
        .filter(|name| !name.is_empty())
}

/// What changed in one project between the episode the workspace's links
/// were last checked against (its checkpoint) and the one they resolve
/// against now.
#[derive(Debug)]
enum Changed {
    Nothing,
    /// Not known: every document that depends on the project is recomputed.
    All,
    /// The labels, and the files' base names, of the definitions whose
    /// recorded rows changed.
    Names(Arc<HashSet<String>>),
}

impl Changed {
    fn touches(&self, names: &BTreeSet<String>) -> bool {
        match self {
            Self::Nothing => false,
            Self::All => true,
            Self::Names(changed) => names.iter().any(|name| changed.contains(name)),
        }
    }
}

/// Note a node row's label and the base name of its file.
fn note_row(found: &mut HashSet<String>, label: rusqlite::types::ValueRef<'_>, owner: &str) {
    if let rusqlite::types::ValueRef::Text(text) = label {
        let text = String::from_utf8_lossy(text);
        let label = symbol_label(text.trim());
        if !label.is_empty() {
            found.insert(label.to_string());
        }
    }
    if let Some(base) = posix(owner).rsplit('/').next().filter(|b| !b.is_empty()) {
        found.insert(base.to_string());
    }
}

/// `project`'s recorded episodes and their sequence numbers.
fn sequences(db: &Connection, project: &str) -> Result<HashMap<String, i64>> {
    let mut select =
        db.prepare_cached("SELECT episode,sequence FROM gf_episodes WHERE repository_id=?")?;
    let mut found = HashMap::new();
    let mut listed = select.query([project])?;
    while let Some(row) = listed.next()? {
        found.insert(row.get(0)?, row.get(1)?);
    }
    Ok(found)
}

/// The labels, and files' base names, of every node of `project`'s
/// `episode`. Episodes never change once recorded, so a few are kept per
/// process.
fn labels_at(db: &Connection, project: &str, episode: &str) -> Result<Arc<HashSet<String>>> {
    static KEPT: LazyLock<Mutex<HashMap<String, Arc<HashSet<String>>>>> =
        LazyLock::new(Default::default);
    let key = graph_projection::graph_ref(project, episode);
    if let Some(found) = KEPT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Ok(found.clone());
    }
    let known = sequences(db, project)?;
    let at = *known
        .get(episode)
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    // The latest episode's nodes are the live rows; an older one's are
    // read as of it, as `graph_projection::load` reads them.
    let sql = if known.values().all(|&sequence| sequence <= at) {
        "SELECT json_extract(attributes,'$.label'),owner,valid_from_episode,valid_to_episode \
         FROM gf_nodes WHERE repository_id=? AND valid_to_episode IS NULL"
    } else {
        "SELECT json_extract(attributes,'$.label'),owner,valid_from_episode,valid_to_episode \
         FROM gf_nodes WHERE repository_id=?"
    };
    let mut select = db.prepare_cached(sql)?;
    let mut listed = select.query([project])?;
    let mut found = HashSet::new();
    while let Some(row) = listed.next()? {
        let from: String = row.get(2)?;
        let to: Option<String> = row.get(3)?;
        let live = known.get(&from).is_some_and(|&f| f <= at)
            && to.is_none_or(|to| known.get(&to).is_some_and(|&t| t > at));
        if live {
            note_row(&mut found, row.get_ref(0)?, &row.get::<_, String>(1)?);
        }
    }
    let found = Arc::new(found);
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if kept.len() >= 32 {
        kept.clear();
    }
    kept.insert(key, found.clone());
    Ok(found)
}

/// What changed in `project` between episode `from` (its checkpoint) and
/// `to` (its reading now): the names of every node row that the episodes
/// recorded between them opened or closed. That covers any difference
/// between the two, whichever way the reading moved. A first reading counts
/// everything it defines as new.
fn changed(
    db: &Connection,
    project: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Changed> {
    let (from, to) = match (from, to) {
        (from, to) if from == to => return Ok(Changed::Nothing),
        (_, None) => return Ok(Changed::All),
        (None, Some(to)) => return Ok(Changed::Names(labels_at(db, project, to)?)),
        (Some(from), Some(to)) => (from, to),
    };
    let known = sequences(db, project)?;
    let (Some(&a), Some(&b)) = (known.get(from), known.get(to)) else {
        return Ok(Changed::All);
    };
    let mut select = db.prepare_cached(
        "SELECT json_extract(attributes,'$.label'),owner FROM gf_nodes WHERE repository_id=?1 AND (\
         valid_from_episode IN (SELECT episode FROM gf_episodes WHERE repository_id=?1 AND sequence>?2 AND sequence<=?3) \
         OR valid_to_episode IN (SELECT episode FROM gf_episodes WHERE repository_id=?1 AND sequence>?2 AND sequence<=?3))",
    )?;
    let mut listed = select.query(params![project, a.min(b), a.max(b)])?;
    let mut found = HashSet::new();
    while let Some(row) = listed.next()? {
        note_row(&mut found, row.get_ref(0)?, &row.get::<_, String>(1)?);
    }
    Ok(Changed::Names(Arc::new(found)))
}

/// The workspace's projects as writers last left them: each one's sync
/// generation and its latest recorded episode. While it holds, no project's
/// head set or recorded episodes, and so no project's reading, changed.
fn fingerprint(db: &Connection, workspace: &str) -> Result<Value> {
    Ok(json!(rows(
        db,
        "SELECT r.id,r.generation,(SELECT coalesce(max(e.sequence),0) FROM gf_episodes e WHERE e.repository_id=r.id) AS sequence \
         FROM repo_projects r WHERE r.workspace_id=? ORDER BY r.created_at,r.id",
        [workspace],
    )?))
}

/// The episode each project of the workspace resolves against now, PR
/// scratch projects and projects without an episode left out.
fn readings(db: &Connection, workspace: &str) -> Result<BTreeMap<String, String>> {
    let scratch = graph_projection::pr_scratch_projects(db, workspace)?;
    let mut out = BTreeMap::new();
    for row in rows(
        db,
        "SELECT id FROM repo_projects WHERE workspace_id=?",
        [workspace],
    )? {
        let id = s(&row, "id");
        if scratch.contains(id) {
            continue;
        }
        let graph = graph_of(db)?;
        let heads = crate::store_content::heads(&graph, id)?;
        let current = graph_projection::episode_of(&heads);
        if let Some(reading) = crate::code_graph_arm::reading(db, id, &heads, &current)? {
            out.insert(id.to_string(), reading.episode);
        }
    }
    Ok(out)
}

/// One computed row of `gf_doc_links`.
#[derive(Clone, Debug)]
struct Row {
    mention: Mention,
    text: String,
    outcome: Outcome,
    /// The target's file and bytes, when its source is stored.
    window: Option<Window>,
}

struct Computed {
    document: Document,
    rows: Vec<Row>,
    counts: Value,
    dependencies: Dependencies,
}

/// The documents whose links the job recomputes, and what they resolve
/// against.
struct Plan {
    projects: Vec<Project>,
    by_id: HashMap<String, usize>,
    documents: Vec<Document>,
    /// `(project, document)` of documents that are gone or no longer read.
    gone: Vec<(String, String)>,
    /// What every stored row was last checked against, by project.
    checkpoints: HashMap<String, Option<String>>,
    /// Projects with a checkpoint that are no longer resolution targets
    /// (they became PR scratch projects).
    retired: Vec<String>,
    /// Whether the workspace's rows were last computed under [`RULES`].
    rules_current: bool,
    /// [`fingerprint`] as the plan read it.
    fingerprint: Value,
}

fn object(db: &Connection, repository: &str, sha: &str) -> Result<Option<String>> {
    let graph = graph_of(db)?;
    crate::store_content::object_content(&graph, repository, sha)
}

/// Whether a `.txt` page is a Sphinx source: a `conf.py` in one of its
/// folders declares `.txt` sources (the engine's `is_sphinx_txt`).
fn sphinx_txt(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
    path: &str,
    cache: &mut HashMap<String, bool>,
) -> Result<bool> {
    let mut dir = path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    for _ in 0..8 {
        let conf = if dir.is_empty() {
            "conf.py".to_string()
        } else {
            format!("{dir}/conf.py")
        };
        let declares = match cache.get(&conf) {
            Some(known) => *known,
            None => {
                let found = match heads.get(&conf) {
                    Some(sha) => object(db, repository, sha)?.is_some_and(|text| {
                        text.lines().any(|line| {
                            line.trim_start().starts_with("source_suffix") && line.contains(".txt")
                        })
                    }),
                    None => false,
                };
                cache.insert(conf, found);
                found
            }
        };
        if declares {
            return Ok(true);
        }
        if dir.is_empty() {
            break;
        }
        dir = dir
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_string();
    }
    Ok(false)
}

/// Whether dotted `names` spell a module or package of the project, seen
/// from the page's folder and up to seven folders above it (the engine's
/// `names_module`, over the head set instead of a checkout).
fn names_module(heads: &BTreeMap<String, String>, page: &str, names: &[String]) -> bool {
    if names.len() < 2 {
        return false;
    }
    let relative = names.join("/");
    let mut dir = page.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    for _ in 0..8 {
        let base = if dir.is_empty() {
            relative.clone()
        } else {
            format!("{dir}/{relative}")
        };
        let folder = format!("{base}/");
        if heads.contains_key(&format!("{base}.py"))
            || heads
                .range(folder.clone()..)
                .next()
                .is_some_and(|(path, _)| path.starts_with(&folder))
        {
            return true;
        }
        if dir.is_empty() {
            break;
        }
        dir = dir
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_string();
    }
    false
}

/// The connector whose documents are captured agent sessions: never linked
/// (observer notes and captured sessions are not linked to code).
fn agent_sessions(kind: &str, name: &str) -> bool {
    kind == "jsonl" && name == crate::capture::CONNECTOR_NAME
}

/// The documents of `project` the job reads: its Markdown and
/// reStructuredText files (Sphinx `.txt` pages when a `conf.py` declares
/// them) and the present documents of its active connectors, except
/// captured agent sessions and the SDK's conversation sessions.
fn documents_of(db: &Connection, project: &Project) -> Result<Vec<Document>> {
    let mut found = Vec::new();
    let mut conf = HashMap::new();
    for (path, sha) in &project.heads {
        let format = match format_of(path) {
            Some(format) => format,
            None if path.to_ascii_lowercase().ends_with(".txt")
                && sphinx_txt(db, &project.id, &project.heads, path, &mut conf)? =>
            {
                Format::Rst
            }
            None => continue,
        };
        found.push(Document {
            project: project.id.clone(),
            id: format!("file:{path}"),
            kind: "repository_file",
            format,
            sha: sha.clone(),
            path: Some(path.clone()),
            connector: None,
            spans: Value::Null,
            input: String::new(),
        });
    }
    for row in rows(
        db,
        "SELECT d.connector_id,d.id,d.sha256,json_extract(d.metadata,'$.code_spans') AS spans,c.kind,c.name \
         FROM am_connector_docs d JOIN am_connectors c ON c.id=d.connector_id \
         WHERE c.repository_id=? AND c.state='active' AND d.present=1 \
         AND coalesce(json_extract(d.metadata,'$.provider_metadata.schema'),'')<>? ORDER BY d.connector_id,d.id",
        params![project.id, SDK_SESSION_SCHEMA],
    )? {
        if agent_sessions(s(&row, "kind"), s(&row, "name")) {
            continue;
        }
        let spans = row["spans"]
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        found.push(Document {
            project: project.id.clone(),
            id: format!("connector:{}:{}", s(&row, "connector_id"), s(&row, "id")),
            kind: "connector_source",
            format: Format::Connector,
            sha: s(&row, "sha256").to_string(),
            path: None,
            connector: Some((
                s(&row, "connector_id").to_string(),
                s(&row, "id").to_string(),
            )),
            spans,
            input: String::new(),
        });
    }
    Ok(found)
}

/// Decides whether a document's stored rows still hold, reading each
/// project's changes and manifest once per job.
struct Checker<'a> {
    db: &'a Connection,
    projects: &'a [Project],
    by_id: &'a HashMap<String, usize>,
    checkpoints: &'a HashMap<String, Option<String>>,
    retired: bool,
    changes: HashMap<usize, Changed>,
    manifests: HashMap<usize, Option<BTreeMap<String, String>>>,
}

impl Checker<'_> {
    /// What changed in project `index` since its checkpoint; a project whose
    /// changes cannot be read counts as wholly changed.
    fn changes(&mut self, index: usize) -> &Changed {
        if !self.changes.contains_key(&index) {
            let project = &self.projects[index];
            let from = self.checkpoints.get(&project.id).cloned().flatten();
            let found = changed(
                self.db,
                &project.id,
                from.as_deref(),
                project.episode.as_deref(),
            )
            .unwrap_or(Changed::All);
            self.changes.insert(index, found);
        }
        &self.changes[&index]
    }
    /// The manifest of project `index`'s reading.
    fn manifest(&mut self, index: usize) -> Option<&BTreeMap<String, String>> {
        if !self.manifests.contains_key(&index) {
            let project = &self.projects[index];
            let found = project.episode.as_ref().and_then(|episode| {
                graph_projection::episode_heads(self.db, &project.id, episode)
                    .ok()
                    .flatten()
            });
            self.manifests.insert(index, found);
        }
        self.manifests[&index].as_ref()
    }
    /// Whether a document of project `own` must be recomputed: a name it
    /// looks up changed in its own project, a name it looked up elsewhere
    /// changed in another project, or a file a link's window was read from
    /// has other bytes now.
    fn stale(&mut self, dependencies: &Dependencies, own: usize) -> bool {
        if self.changes(own).touches(&dependencies.names) {
            return true;
        }
        if !dependencies.elsewhere.is_empty() {
            if self.retired {
                return true;
            }
            for index in (0..self.projects.len()).filter(|&index| index != own) {
                if self.changes(index).touches(&dependencies.elsewhere) {
                    return true;
                }
            }
        }
        for (project, files) in &dependencies.targets {
            let Some(&index) = self.by_id.get(project) else {
                return true;
            };
            let Some(manifest) = self.manifest(index) else {
                return true;
            };
            if files
                .iter()
                .any(|(path, sha)| manifest.get(path) != Some(sha))
            {
                return true;
            }
        }
        false
    }
}

fn plan(db: &Connection, workspace: &str) -> Result<Plan> {
    let fingerprint = fingerprint(db, workspace)?;
    let scratch = graph_projection::pr_scratch_projects(db, workspace)?;
    let mut projects = Vec::new();
    for row in fingerprint.as_array().into_iter().flatten() {
        let id = s(row, "id").to_string();
        // A PR scratch project holds copies of its folder project's code:
        // as a target it would make every name ambiguous.
        if scratch.contains(&id) {
            continue;
        }
        let graph = graph_of(db)?;
        let heads = crate::store_content::heads(&graph, &id)?;
        let current = graph_projection::episode_of(&heads);
        let episode = crate::code_graph_arm::reading(db, &id, &heads, &current)?
            .map(|reading| reading.episode);
        projects.push(Project { id, heads, episode });
    }
    let by_id: HashMap<String, usize> = projects
        .iter()
        .enumerate()
        .map(|(index, project)| (project.id.clone(), index))
        .collect();
    let graph = graph_of(db)?;
    // Every project of the workspace, scratch ones included: their
    // checkpoints are read so a project that became scratch is retired.
    let all: Vec<Value> = fingerprint
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| json!(s(row, "id")))
        .collect();
    let checkpoints: HashMap<String, Option<String>> = graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (c:DocLinkCheckpoint) WHERE c.repository_id IN $ids \
                 RETURN c.repository_id AS repository_id, c.episode AS episode",
                gparams([("ids", json!(all))]),
            )
        })?
        .into_iter()
        .map(|row| {
            (
                s(&row, "repository_id").to_string(),
                row["episode"].as_str().map(str::to_string),
            )
        })
        .collect();
    let retired: Vec<String> = checkpoints
        .keys()
        .filter(|id| !by_id.contains_key(*id))
        .cloned()
        .collect();
    let rules_current = graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (t:DocLinkState {workspace_id: $workspace, rules: $rules}) RETURN 1 AS yes",
                gparams([("workspace", json!(workspace)), ("rules", json!(RULES))]),
            )
        })?
        .is_some();
    let mut checker = Checker {
        db,
        projects: &projects,
        by_id: &by_id,
        checkpoints: &checkpoints,
        retired: !retired.is_empty(),
        changes: HashMap::new(),
        manifests: HashMap::new(),
    };
    let mut documents = Vec::new();
    let mut gone = Vec::new();
    for (index, project) in projects.iter().enumerate() {
        let found = documents_of(db, project)?;
        let stored: HashMap<String, (String, String)> = graph
            .read(|db| {
                store_graph::rows(
                    db,
                    "MATCH (d:ConnectorDoc {repository_id: $repo}) \
                     RETURN d.id AS document_id, d.input AS input, d.dependencies AS dependencies",
                    gparams([("repo", json!(project.id))]),
                )
            })?
            .into_iter()
            .map(|row| {
                (
                    s(&row, "document_id").to_string(),
                    (
                        s(&row, "input").to_string(),
                        s(&row, "dependencies").to_string(),
                    ),
                )
            })
            .collect();
        let present: HashSet<&str> = found.iter().map(|d| d.id.as_str()).collect();
        for document in stored.keys().filter(|id| !present.contains(id.as_str())) {
            gone.push((project.id.clone(), document.clone()));
        }
        // A reStructuredText page's literals depend on which paths exist
        // (`names_module`), not on their bytes.
        let paths = digest(&json!(project.heads.keys().collect::<Vec<_>>()));
        for mut document in found {
            // What the rows depend on in the document itself. Project ids
            // and episodes stay out: two stores holding the same content
            // record the same inputs.
            document.input = digest(&json!({"rules":RULES,"sha256":document.sha,
                "format":document.format.name(),"spans":document.spans,
                "paths":if document.format == Format::Rst { json!(paths) } else { Value::Null }}));
            let current = match stored.get(&document.id) {
                Some((input, dependencies)) if *input == document.input => {
                    serde_json::from_str::<Dependencies>(dependencies)
                        .is_ok_and(|dependencies| !checker.stale(&dependencies, index))
                }
                _ => false,
            };
            if !current {
                documents.push(document);
            }
        }
    }
    drop(checker);
    Ok(Plan {
        projects,
        by_id,
        documents,
        gone,
        checkpoints,
        retired,
        rules_current,
        fingerprint,
    })
}

/// One project's resolution view as the job opened it.
enum View {
    Ready(Box<Index>),
    /// The project has no recorded episode.
    Empty,
    /// Its episode could not be read: its documents, and documents that
    /// would have looked a name up in it, are left as they are.
    Unavailable,
}

/// Open the resolution view of `project`'s episode once per job; one that
/// cannot be read leaves the project out instead of failing the job.
fn open(db: &Connection, project: &Project, views: &mut HashMap<String, View>) {
    if views.contains_key(&project.id) {
        return;
    }
    let view = match &project.episode {
        Some(episode) => match graph_projection::resident(db, &project.id, episode) {
            Ok(graph) => View::Ready(Box::new(Index::new(&project.id, episode, graph))),
            Err(_) => View::Unavailable,
        },
        None => View::Empty,
    };
    views.insert(project.id.clone(), view);
}

/// A linked definition's file sha256 and its bytes and lines there.
type Window = (String, usize, usize, usize, usize);
/// Stored files by `(project, sha256)`: their length and line starts.
type Lines = HashMap<(String, String), Option<(usize, Vec<usize>)>>;

/// What `compute` found: the rows of the documents it read, the documents
/// it could not read or resolve (left as they are and read again by the
/// next job), and the projects whose episode could not be read.
#[derive(Default)]
struct Resolved {
    computed: Vec<Computed>,
    failed: Vec<(String, String)>,
    unavailable: BTreeSet<String>,
}

/// Resolve the mentions of the planned documents (a read-only step). The
/// document's own project is opened when it has mentions; another project
/// only when a mention found no candidate in its own and the other project
/// defines that name at all (its labels, read without opening its graph).
fn compute(db: &Connection, plan: &Plan) -> Resolved {
    let mut views: HashMap<String, View> = HashMap::new();
    let mut windows: HashMap<(String, String), Option<Window>> = HashMap::new();
    let mut files: Lines = HashMap::new();
    let mut out = Resolved::default();
    for document in &plan.documents {
        let project = &plan.projects[plan.by_id[&document.project]];
        match compute_one(
            db,
            plan,
            project,
            document,
            &mut views,
            &mut windows,
            &mut files,
        ) {
            Ok(Some(computed)) => out.computed.push(computed),
            Ok(None) | Err(_) => out
                .failed
                .push((document.project.clone(), document.id.clone())),
        }
    }
    out.unavailable = views
        .iter()
        .filter(|(_, view)| matches!(view, View::Unavailable))
        .map(|(id, _)| id.clone())
        .collect();
    out
}

fn compute_one(
    db: &Connection,
    plan: &Plan,
    project: &Project,
    document: &Document,
    views: &mut HashMap<String, View>,
    windows: &mut HashMap<(String, String), Option<Window>>,
    files: &mut Lines,
) -> Result<Option<Computed>> {
    let content = match (&document.path, &document.connector) {
        (Some(_), _) => object(db, &project.id, &document.sha)?,
        (None, Some((connector, id))) => one(
            db,
            "SELECT content FROM am_connector_docs WHERE connector_id=? AND id=?",
            params![connector, id],
        )?
        .map(|row| s(&row, "content").to_string()),
        _ => None,
    };
    let Some(content) = content else {
        return Ok(None);
    };
    let page = document.path.clone().unwrap_or_default();
    let mut mentions = match document.format {
        Format::Markdown => markdown(&content),
        Format::Rst => rst(&content, &|names| {
            names_module(&project.heads, &page, names)
        }),
        Format::Connector => connector(&content, &stored_spans(&document.spans, &content)),
    };
    let omitted = mentions.len().saturating_sub(MAX_MENTIONS);
    mentions.truncate(MAX_MENTIONS);
    let mut dependencies = Dependencies::default();
    for mention in &mentions {
        dependencies.names.extend(mention_names(mention));
    }
    // The document's own project first.
    if !mentions.is_empty() {
        open(db, project, views);
    }
    let own: Vec<Option<Outcome>> = match views.get(&project.id) {
        Some(View::Unavailable) => return Ok(None),
        Some(View::Ready(index)) => mentions
            .iter()
            .map(|mention| resolve_own(mention, &page, Some(index.as_ref())))
            .collect(),
        _ => mentions
            .iter()
            .map(|mention| resolve_own(mention, &page, None))
            .collect(),
    };
    // Then the other projects that define a name left without a candidate.
    let mut needed: BTreeSet<usize> = BTreeSet::new();
    for (mention, outcome) in mentions.iter().zip(&own) {
        if outcome.is_some() {
            continue;
        }
        dependencies.elsewhere.extend(mention_names(mention));
        let Some(name) = mention.names.last().map(|name| strip(name)) else {
            continue;
        };
        for (index, other) in plan.projects.iter().enumerate() {
            let Some(episode) = other.episode.as_deref().filter(|_| other.id != project.id) else {
                continue;
            };
            // A project whose labels cannot be read is opened to find out.
            let defines = match labels_at(db, &other.id, episode) {
                Ok(labels) => labels.contains(name),
                Err(_) => true,
            };
            if defines {
                needed.insert(index);
            }
        }
    }
    for &index in &needed {
        open(db, &plan.projects[index], views);
    }
    let mut others: Vec<&Index> = Vec::with_capacity(needed.len());
    for &index in &needed {
        match views.get(&plan.projects[index].id) {
            Some(View::Ready(view)) => others.push(view.as_ref()),
            Some(View::Unavailable) => return Ok(None),
            _ => {}
        }
    }
    let mut found = Vec::with_capacity(mentions.len());
    let (mut linked, mut ambiguous, mut unresolved, mut elsewhere) = (0, 0, 0, 0);
    for (mention, outcome) in mentions.into_iter().zip(own) {
        let outcome = outcome.unwrap_or_else(|| resolve_elsewhere(&mention, &others));
        let window = match &outcome {
            Outcome::Linked(target, scope) => {
                linked += 1;
                if *scope == "workspace" {
                    elsewhere += 1;
                }
                let key = (target.repository.clone(), target.node_id.clone());
                let window = match windows.get(&key) {
                    Some(known) => known.clone(),
                    None => {
                        let computed = match views.get(&target.repository) {
                            Some(View::Ready(index)) => target_window(db, index, target, files)?,
                            _ => None,
                        };
                        windows.insert(key, computed.clone());
                        computed
                    }
                };
                if let Some((sha, ..)) = &window {
                    dependencies
                        .targets
                        .entry(target.repository.clone())
                        .or_default()
                        .insert(target.relpath.clone(), sha.clone());
                }
                window
            }
            Outcome::Ambiguous(..) => {
                ambiguous += 1;
                None
            }
            Outcome::Unresolved => {
                unresolved += 1;
                None
            }
        };
        found.push(Row {
            text: content[mention.byte_start..mention.byte_end].to_string(),
            mention,
            outcome,
            window,
        });
    }
    Ok(Some(Computed {
        document: document.clone(),
        counts: json!({"mentions":found.len(),"linked":linked,"ambiguous":ambiguous,
            "unresolved":unresolved,"cross_project":elsewhere,"omitted":omitted}),
        rows: found,
        dependencies,
    }))
}

/// A target's file sha256 in its episode and its bytes and lines there: from
/// its start line to the next definition's (the code arm's window).
fn target_window(
    db: &Connection,
    index: &Index,
    target: &Target,
    files: &mut Lines,
) -> Result<Option<Window>> {
    let Some(sha) = index
        .manifest(db)?
        .and_then(|heads| heads.get(&target.relpath))
        .cloned()
    else {
        return Ok(None);
    };
    let key = (index.repository.clone(), sha.clone());
    if !files.contains_key(&key) {
        let lines = object(db, &index.repository, &sha)?
            .map(|content| (content.len(), span::line_starts(&content)));
        files.insert(key.clone(), lines);
    }
    let Some((length, offsets)) = &files[&key] else {
        return Ok(None);
    };
    let graph: &Graph = &index.graph;
    let Some(node) = graph.node_index(&json!(target.node_id)) else {
        return Ok(None);
    };
    let attributes = &graph.nodes()[node].attributes;
    let Some(start) = span::start_line(attributes) else {
        return Ok(None);
    };
    let start = start.max(1);
    let next = span::end_line(attributes).map(|end| end + 1).or_else(|| {
        index
            .starts
            .get(&target.relpath)
            .and_then(|list| span::next_start(list, start))
    });
    let end_line = next.map_or(offsets.len(), |n| (n - 1).max(start));
    let bytes = span::byte_range(offsets, *length, Some(start), next);
    if bytes.end <= bytes.start {
        return Ok(None);
    }
    Ok(Some((sha, bytes.start, bytes.end, start, end_line)))
}

/// The document's current sha256: its head, or its present connector copy.
fn current_sha(db: &Connection, document: &Document) -> Result<Option<String>> {
    let row = match (&document.path, &document.connector) {
        (Some(path), _) => {
            let graph = graph_of(db)?;
            crate::store_content::head_sha(&graph, &document.project, path)?
                .map(|sha256| json!({"sha256":sha256}))
        }
        (None, Some((connector, id))) => one(
            db,
            "SELECT d.sha256 FROM am_connector_docs d JOIN am_connectors c ON c.id=d.connector_id \
             WHERE d.connector_id=? AND d.id=? AND d.present=1 AND c.state='active'",
            params![connector, id],
        )?,
        _ => None,
    };
    Ok(row.map(|row| s(&row, "sha256").to_string()))
}

/// Replace one document's rows (this program's only: an extension's rows
/// are its own) and record what they were computed from. The document's
/// mentions are written as one batched `UNWIND` per outcome kind: edges to
/// the resolved symbol, and edges back to the document for the rest.
fn write(db: &mut cypherlite::Connection, computed: &Computed) -> Result<usize> {
    let document = &computed.document;
    let key = [("repo", json!(document.project)), ("doc", json!(document.id))];
    store_graph::rows(
        db,
        "MATCH (d:ConnectorDoc {repository_id: $repo, id: $doc})-[l:LINKS_TO]->() \
         WHERE l.producer = $producer DELETE l",
        gparams(key.iter().cloned().chain([("producer", json!(PRODUCER))])),
    )?;
    let stamp = now();
    // `counts` is stored as fixed-key dotted properties (`counts.mentions`,
    // …: the engine stores no map values); `dependencies` is dynamic-keyed
    // and stays stringified JSON.
    let mut node = json!({"source_kind":document.kind,"document_sha256":document.sha,
        "input":document.input,"updated_at":stamp,
        "dependencies":serde_json::to_string(&computed.dependencies)?});
    if let Some(counts) = computed.counts.as_object() {
        for (key, value) in counts {
            node[format!("counts.{key}")] = value.clone();
        }
    }
    if let Some((connector, _)) = &document.connector {
        node["connector"] = json!(connector);
    }
    store_graph::rows(
        db,
        "MERGE (d:ConnectorDoc {repository_id: $repo, id: $doc}) SET d += $props",
        gparams(key.iter().cloned().chain([("props", node)])),
    )?;
    let mut symbols = Vec::new();
    let mut plain = Vec::new();
    for (seq, row) in computed.rows.iter().enumerate() {
        let (state, scope, candidates, target) = match &row.outcome {
            Outcome::Linked(target, scope) => {
                ("linked", Some(*scope), 1usize, Some(target.as_ref()))
            }
            Outcome::Ambiguous(n, scope) => ("ambiguous", Some(*scope), *n, None),
            Outcome::Unresolved => ("unresolved", None, 0, None),
        };
        let facts = target.map_or(json!({}), |t| {
            let mut facts = json!({"node_id":t.node_id,"revision":t.revision,"qualified_name":t.qualified_name,
                "kind":t.kind,"label":t.label,"episode":t.episode,"confidence":t.confidence,
                "names":row.mention.names});
            if let Some((_, _, _, line, end_line)) = &row.window {
                facts["line"] = json!(line);
                facts["end_line"] = json!(end_line);
            }
            facts
        });
        let mut props = json!({"seq":seq,"document_sha256":document.sha,"source_kind":document.kind,
            "byte_start":row.mention.byte_start,"byte_end":row.mention.byte_end,
            "mention":row.text,"mark":row.mention.mark,"state":state,"candidates":candidates,
            "rung":RUNG,"producer":PRODUCER,"created_at":stamp});
        // The target's facts as fixed-key dotted properties (map values are
        // not storable; `names` is a list of scalars, which is).
        if let Some(facts) = facts.as_object() {
            for (key, value) in facts {
                if !value.is_null() {
                    props[format!("target.{key}")] = value.clone();
                }
            }
        }
        if let Some(scope) = scope {
            props["scope"] = json!(scope);
        }
        if let Some(t) = target {
            props["target_repository_id"] = json!(t.repository);
            props["target_relpath"] = json!(t.relpath);
            if let Some(stable) = &t.stable_id {
                props["stable_id"] = json!(stable);
            }
            if let Some((sha, a, b, _, _)) = &row.window {
                props["target_sha256"] = json!(sha);
                props["target_start"] = json!(a);
                props["target_end"] = json!(b);
            }
        }
        if let Some(t) = target
            && let Some(stable) = &t.stable_id
        {
            symbols.push(json!({"target_repo":t.repository,"stable_id":stable,"props":props}));
        } else {
            plain.push(json!({"props":props}));
        }
    }
    if !symbols.is_empty() {
        store_graph::rows(
            db,
            "UNWIND $rows AS row \
             MATCH (d:ConnectorDoc {repository_id: $repo, id: $doc}) \
             MERGE (t:Symbol {repository_id: row.target_repo, stable_id: row.stable_id}) \
             CREATE (d)-[l:LINKS_TO]->(t) SET l += row.props",
            gparams(key.iter().cloned().chain([("rows", json!(symbols))])),
        )?;
    }
    if !plain.is_empty() {
        store_graph::rows(
            db,
            "UNWIND $rows AS row \
             MATCH (d:ConnectorDoc {repository_id: $repo, id: $doc}) \
             CREATE (d)-[l:LINKS_TO]->(d) SET l += row.props",
            gparams(key.iter().cloned().chain([("rows", json!(plain))])),
        )?;
    }
    Ok(computed.rows.len())
}

/// Queue the workspace's `link` job after a connector sync or import
/// changed `connector`'s documents in `repository`, as a projection queues
/// it ([`crate::jobs::enqueue_link`]): a job still waiting covers both, and
/// one already running is followed by a new one, since it may have read the
/// documents before this change. Its document step re-links the changed
/// documents; its import step finds the episodes unchanged. A connector
/// whose documents are never read for links (captured agent sessions, the
/// SDK's conversation sessions) queues nothing, unless links of its
/// documents remain to be removed.
pub fn after_sync(
    db: &Connection,
    repository: &str,
    connector: &str,
    p: &Principal,
) -> Result<Value> {
    let Some(row) = one(
        db,
        "SELECT kind,name FROM am_connectors WHERE id=? AND repository_id=?",
        params![connector, repository],
    )?
    else {
        return Ok(Value::Null);
    };
    let read = !agent_sessions(s(&row, "kind"), s(&row, "name"))
        && one(
            db,
            "SELECT 1 AS yes FROM am_connector_docs WHERE connector_id=? AND present=1 \
             AND coalesce(json_extract(metadata,'$.provider_metadata.schema'),'')<>? LIMIT 1",
            params![connector, SDK_SESSION_SCHEMA],
        )?
        .is_some();
    let linked = graph_of(db)?
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (d:ConnectorDoc {repository_id: $repo, connector: $connector}) \
                 RETURN 1 AS yes LIMIT 1",
                gparams([("repo", json!(repository)), ("connector", json!(connector))]),
            )
        })?
        .is_some();
    if !read && !linked {
        return Ok(json!({"skipped":"no documents read for links"}));
    }
    let episode = graph_projection::current_episode(db, repository)?;
    crate::jobs::enqueue_link(db, repository, p, &episode)
}

/// What a `link` job's document step did, counted for the projects its
/// principal may read (connector documents only for an owner).
#[derive(Default)]
struct Tally {
    relinked: usize,
    mentions: usize,
    removed: usize,
    changed: usize,
    failed: usize,
    unavailable: BTreeSet<String>,
}

/// The `link` job's document step, also run by
/// `graph.link`: recompute the mention links of the workspace's documents
/// whose rows no longer hold, and remove those of documents that are gone.
///
/// A document is recomputed when its bytes, its inline-code spans or the
/// rules changed, or when what its rows depend on changed in a project
/// since the episode the workspace's links were last checked against (the
/// project's checkpoint, [`Dependencies`]): a name its mentions look up in
/// its own project, a name that found no candidate there and was looked up
/// in the others, or the bytes of a file a link's window was read from. A
/// projection elsewhere, or a new project, recomputes only the documents it
/// can change.
///
/// Every project of the workspace takes part except PR scratch projects,
/// whoever triggered the run: rows are filtered when they are read, never
/// when they are written. Mentions are resolved on a read-only snapshot.
/// The rows are then written in short transactions of at most
/// [`WRITE_BATCH`] documents. Each re-checks `leased` (the job's lease) and
/// that every project still resolves against the episode the plan read;
/// if one moved, nothing more is written and another `link` job is queued
/// to read it again. A document whose bytes changed meanwhile is skipped:
/// the change queued its own job. A project whose episode cannot be read is
/// left out: its documents, and those that would have looked a name up in
/// it, keep their rows and are read again by the next job. The checkpoints
/// move to the plan's episodes once every batch is written.
///
/// Returns counts for the projects `principal` may read, connector
/// documents only for an owner.
pub fn relink(
    store: &Store,
    workspace: &str,
    principal: &str,
    leased: impl Fn(&Connection) -> Result<()>,
) -> Result<Value> {
    let (plan, resolved) = store.read(|db| {
        let plan = plan(db, workspace)?;
        let resolved = compute(db, &plan);
        Ok((plan, resolved))
    })?;
    write_all(store, workspace, principal, &leased, &plan, &resolved)
}

/// Whether every project still resolves against the episode `plan` read.
/// `expected` is the last [`fingerprint`] seen to hold.
fn holds(db: &Connection, workspace: &str, plan: &Plan, expected: &mut Value) -> Result<bool> {
    let now = fingerprint(db, workspace)?;
    if now == *expected {
        return Ok(true);
    }
    // A sync or a projection landed; only a moved reading matters.
    let planned: BTreeMap<String, String> = plan
        .projects
        .iter()
        .filter_map(|project| Some((project.id.clone(), project.episode.clone()?)))
        .collect();
    if readings(db, workspace)? != planned {
        return Ok(false);
    }
    *expected = now;
    Ok(true)
}

/// Flag the stored rows of documents `(repo, doc)` for recomputation
/// (`input='retry'`), one batched statement.
fn mark_retry_in(db: &mut cypherlite::Connection, documents: &[(&str, &str)]) -> Result<()> {
    if documents.is_empty() {
        return Ok(());
    }
    let listed: Vec<Value> = documents
        .iter()
        .map(|(repo, doc)| json!({"repo":repo,"doc":doc}))
        .collect();
    store_graph::rows(
        db,
        "UNWIND $rows AS row \
         MATCH (d:ConnectorDoc {repository_id: row.repo, id: row.doc}) SET d.input = 'retry'",
        gparams([("rows", json!(listed))]),
    )?;
    Ok(())
}

/// [`mark_retry_in`] as its own transaction.
fn mark_retry(graph: &GraphStore, documents: &[(&str, &str)]) -> Result<()> {
    if documents.is_empty() {
        return Ok(());
    }
    graph.transaction(|db| mark_retry_in(db, documents))
}

fn write_all(
    store: &Store,
    workspace: &str,
    principal: &str,
    leased: &dyn Fn(&Connection) -> Result<()>,
    plan: &Plan,
    resolved: &Resolved,
) -> Result<Value> {
    let graph = store.graph();
    let advance: Vec<(&str, Option<&str>)> = plan
        .projects
        .iter()
        .filter(|project| {
            plan.checkpoints.get(&project.id).map(Option::as_deref)
                != Some(project.episode.as_deref())
        })
        .map(|project| (project.id.as_str(), project.episode.as_deref()))
        .collect();
    let mut tally = Tally::default();
    if resolved.computed.is_empty()
        && resolved.failed.is_empty()
        && resolved.unavailable.is_empty()
        && plan.gone.is_empty()
        && plan.retired.is_empty()
        && advance.is_empty()
        && plan.rules_current
    {
        // Every document's links hold: nothing to write.
        return Ok(summary(&tally, false));
    }
    let batches: Vec<&[Computed]> = if resolved.computed.is_empty() {
        vec![&[]]
    } else {
        resolved.computed.chunks(WRITE_BATCH).collect()
    };
    let mut expected = plan.fingerprint.clone();
    let mut moved = false;
    // Documents written by earlier batches: computed against the plan's
    // episodes, which the checkpoints only reach once every batch is in.
    let mut written: Vec<(&str, &str)> = Vec::new();
    for (number, batch) in batches.iter().enumerate() {
        let last = number + 1 == batches.len();
        let wrote = store.transaction(|db| {
            leased(db)?;
            let p = crate::store::authorize_hash(db, workspace, principal, "read")?;
            if !holds(db, workspace, plan, &mut expected)? {
                // The plan read what has changed since: what this run wrote
                // is read again whatever the checkpoints say, and so is the
                // rest, by the job queued here.
                mark_retry(&graph, &written)?;
                if let Some(first) = one(
                    db,
                    "SELECT id FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id LIMIT 1",
                    [workspace],
                )? {
                    let first = s(&first, "id");
                    let episode = graph_projection::current_episode(db, first)?;
                    crate::jobs::enqueue_link(db, first, &p, &episode)?;
                }
                return Ok(false);
            }
            let mut readable = HashMap::new();
            let mut visible = |db: &Connection, project: &str, kind: &str| -> Result<bool> {
                if kind != "repository_file" && p.role != "owner" {
                    return Ok(false);
                }
                if let Some(known) = readable.get(project) {
                    return Ok(*known);
                }
                let known = graph_projection::repo_readable(db, &p, project)?;
                readable.insert(project.to_string(), known);
                Ok(known)
            };
            let mut to_write: Vec<&Computed> = Vec::new();
            for item in *batch {
                let kind = item.document.kind;
                if current_sha(db, &item.document)?.as_deref() != Some(item.document.sha.as_str()) {
                    tally.changed += usize::from(visible(db, &item.document.project, kind)?);
                    continue;
                }
                written.push((&item.document.project, &item.document.id));
                if visible(db, &item.document.project, kind)? {
                    tally.relinked += 1;
                    tally.mentions += item.rows.len();
                }
                to_write.push(item);
            }
            if last {
                for (project, document) in &plan.gone {
                    let kind = if document.starts_with("file:") {
                        "repository_file"
                    } else {
                        "connector_source"
                    };
                    // A document in `gone` has a sources row by construction
                    // (the plan read it), so its node goes: one row removed.
                    if visible(db, project, kind)? {
                        tally.removed += 1;
                    }
                }
                // Documents that could not be resolved keep their rows and
                // are read again by the next job, whatever the checkpoints
                // say.
                for (project, document) in &resolved.failed {
                    let kind = if document.starts_with("file:") {
                        "repository_file"
                    } else {
                        "connector_source"
                    };
                    tally.failed += usize::from(visible(db, project, kind)?);
                }
                for project in &resolved.unavailable {
                    if visible(db, project, "repository_file")? {
                        tally.unavailable.insert(project.clone());
                    }
                }
            }
            // The batch's graph writes commit as one transaction, gated by
            // the job's lease and the readings check above. A SQL commit
            // failure after the graph commit leaves rows the next run
            // recomputes from their recorded inputs.
            let stamp = now();
            graph.transaction(|db| {
                for item in &to_write {
                    write(db, item)?;
                }
                if !last {
                    return Ok(());
                }
                if !plan.gone.is_empty() {
                    let gone: Vec<Value> = plan
                        .gone
                        .iter()
                        .map(|(project, document)| json!({"repo":project,"doc":document}))
                        .collect();
                    store_graph::rows(
                        db,
                        "UNWIND $rows AS row \
                         MATCH (d:ConnectorDoc {repository_id: row.repo, id: row.doc})-[l:LINKS_TO]->() \
                         WHERE l.producer = $producer DELETE l",
                        gparams([("rows", json!(gone)), ("producer", json!(PRODUCER))]),
                    )?;
                    // The document's node holds its sources row; a document
                    // an extension still has rows for keeps its node.
                    store_graph::rows(
                        db,
                        "UNWIND $rows AS row \
                         MATCH (d:ConnectorDoc {repository_id: row.repo, id: row.doc}) \
                         OPTIONAL MATCH (d)-[l:LINKS_TO]->() \
                         WITH d, count(l) AS links WHERE links = 0 DELETE d",
                        gparams([("rows", json!(gone))]),
                    )?;
                }
                let failed: Vec<(&str, &str)> = resolved
                    .failed
                    .iter()
                    .map(|(project, document)| (project.as_str(), document.as_str()))
                    .collect();
                mark_retry_in(db, &failed)?;
                if !advance.is_empty() {
                    let rows: Vec<Value> = advance
                        .iter()
                        .map(|(project, episode)| {
                            let mut props =
                                json!({"repository_id":project,"updated_at":stamp});
                            if let Some(episode) = episode {
                                props["episode"] = json!(episode);
                            }
                            json!({"project":project,"props":props})
                        })
                        .collect();
                    store_graph::rows(
                        db,
                        "UNWIND $rows AS row \
                         MATCH (c:DocLinkCheckpoint {repository_id: row.project}) DELETE c",
                        gparams([("rows", json!(rows))]),
                    )?;
                    store_graph::rows(
                        db,
                        "UNWIND $rows AS row CREATE (c:DocLinkCheckpoint) SET c += row.props",
                        gparams([("rows", json!(rows))]),
                    )?;
                }
                if !plan.retired.is_empty() {
                    store_graph::rows(
                        db,
                        "UNWIND $rows AS repo \
                         MATCH (c:DocLinkCheckpoint {repository_id: repo}) DELETE c",
                        gparams([("rows", json!(plan.retired))]),
                    )?;
                }
                store_graph::rows(
                    db,
                    "MERGE (t:DocLinkState {workspace_id: $workspace}) SET t += $props",
                    gparams([
                        ("workspace", json!(workspace)),
                        ("props", json!({"rules":RULES,"updated_at":stamp})),
                    ]),
                )?;
                Ok(())
            })?;
            Ok(true)
        })?;
        if !wrote {
            moved = true;
            break;
        }
    }
    Ok(summary(&tally, moved))
}

fn summary(tally: &Tally, requeued: bool) -> Value {
    json!({"rules":RULES,"relinked":tally.relinked,"mentions":tally.mentions,"removed":tally.removed,
        "changed_during_run":tally.changed,"failed":tally.failed,
        "unavailable_projects":tally.unavailable,"requeued":requeued})
}

// ---------------------------------------------------------------------------
// Reading the links

/// Which links a query follows (`doc_links`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Links of repository documents: only with `doc_links: true`, until a
    /// measurement shows that what they lift helps answers.
    pub repository: bool,
    /// Links of connector documents: only with `doc_links: true` until their
    /// precision gate has passed.
    pub connector: bool,
    /// Whether documents take part at all: off only with `doc_links: false`,
    /// which reads no link and keeps document nodes and documents' edges to
    /// code out of the code arm, `rank` and `graph_local`. Left out, the
    /// links of the selected windows are counted, not followed.
    pub documents: bool,
}

impl Options {
    pub fn parse(args: &Value) -> Result<Self> {
        match args.get("doc_links") {
            None | Some(Value::Null) => Ok(Self {
                repository: false,
                connector: false,
                documents: true,
            }),
            Some(Value::Bool(on)) => Ok(Self {
                repository: *on,
                connector: *on,
                documents: *on,
            }),
            Some(_) => Err(Error::bad("doc_links must be true or false")),
        }
    }
    /// Whether a link is followed (lifted into the packet).
    pub fn enabled(&self) -> bool {
        self.repository || self.connector
    }
    /// Whether documents take part in the query: the links are read
    /// (followed or counted), and document nodes and documents' edges to
    /// code stay in the code arm, `rank` and `graph_local`.
    pub fn documents(&self) -> bool {
        self.documents
    }
    fn follows(&self, kind: &str) -> bool {
        if kind == "repository_file" {
            self.repository
        } else {
            self.connector
        }
    }
    /// The source kinds whose links are followed, for `mechanisms`.
    pub fn sources(&self) -> Vec<&'static str> {
        [
            ("repository_file", self.repository),
            ("connector_source", self.connector),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(kind, _)| kind)
        .collect()
    }
}

/// A file edge of the static relationships from a document to code
/// (`references` or `document_link` from a Markdown or reStructuredText
/// file to a file that is neither): `doc_links: false` leaves it out of
/// `rank`'s relationship lift and of `graph_local`'s property graph (the
/// switch removes every document-to-code path). Links between pages
/// stay.
pub fn document_edge(edge: &Value) -> bool {
    matches!(
        edge["relation"].as_str().or(edge["kind"].as_str()),
        Some("references" | "document_link")
    ) && format_of(s(edge, "source")).is_some()
        && format_of(s(edge, "target")).is_none()
}

/// Whether a document is one of the SDK's conversation sessions (its
/// metadata, as a snapshot lists it, carries [`SDK_SESSION_SCHEMA`]).
pub fn sdk_session(document: &Value) -> bool {
    document["metadata"]["schema"] == SDK_SESSION_SCHEMA
}

/// Whether the `link` job reads the document `id` (a source id): a Markdown
/// or reStructuredText file, or a document of a connector other than the
/// captured agent sessions that is not an SDK conversation `session`. (A
/// Sphinx `.txt` page counts once it has rows.)
fn linkable(db: &Connection, id: &str, session: bool) -> Result<bool> {
    if let Some(path) = id.strip_prefix("file:") {
        return Ok(format_of(path).is_some());
    }
    let Some((connector, _)) = id
        .strip_prefix("connector:")
        .and_then(|rest| rest.split_once(':'))
    else {
        return Ok(false);
    };
    if session {
        return Ok(false);
    }
    Ok(one(
        db,
        "SELECT kind,name FROM am_connectors WHERE id=?",
        [connector],
    )?
    .is_some_and(|row| !agent_sessions(s(&row, "kind"), s(&row, "name"))))
}

/// The producers whose rows this program reads: its own, and those of the
/// installed extensions, which name themselves as producers (an
/// extension's rows count only while it is installed).
pub fn producers() -> Vec<&'static str> {
    std::iter::once(PRODUCER)
        .chain(crate::extension::installed().names())
        .collect()
}

/// The link rows of one project, read during a query: they live in the
/// graph store (found from the SQL connection's store), the connectors it
/// checks a document against are still SQL.
pub struct Lookup<'a> {
    db: &'a Connection,
    graph: std::sync::Arc<GraphStore>,
    repository: String,
}

/// One link row as a query reads it.
#[derive(Clone, Debug)]
struct Link {
    project: String,
    document_id: String,
    document_sha256: String,
    source_kind: String,
    byte_start: usize,
    byte_end: usize,
    mention: String,
    mark: String,
    target_repository: String,
    stable_id: Value,
    relpath: String,
    target_sha256: String,
    target_start: Option<usize>,
    target_end: Option<usize>,
    target: Value,
    rung: String,
    producer: String,
}

impl<'a> Lookup<'a> {
    pub fn new(db: &'a Connection, repository: &str) -> Result<Self> {
        Ok(Self {
            db,
            graph: graph_of(db)?,
            repository: repository.to_string(),
        })
    }
    fn links(&self, cypher: &str, params: Params) -> Result<Vec<Link>> {
        let found = self
            .graph
            .read(|db| store_graph::rows(db, cypher, params))?;
        Ok(found
            .into_iter()
            .map(|row| {
                let number = |key: &str| row[key].as_u64().and_then(|n| usize::try_from(n).ok());
                Link {
                    project: s(&row, "repository_id").to_string(),
                    document_id: s(&row, "document_id").to_string(),
                    document_sha256: s(&row, "document_sha256").to_string(),
                    source_kind: s(&row, "source_kind").to_string(),
                    byte_start: number("byte_start").unwrap_or(0),
                    byte_end: number("byte_end").unwrap_or(0),
                    mention: s(&row, "mention").to_string(),
                    mark: s(&row, "mark").to_string(),
                    target_repository: s(&row, "target_repository_id").to_string(),
                    stable_id: row["stable_id"].clone(),
                    relpath: s(&row, "target_relpath").to_string(),
                    target_sha256: s(&row, "target_sha256").to_string(),
                    target_start: number("target_start"),
                    target_end: number("target_end"),
                    target: target_facts(&row),
                    rung: s(&row, "rung").to_string(),
                    producer: s(&row, "producer").to_string(),
                }
            })
            .collect())
    }
    /// Linked mentions inside `[a, b)` of one of the project's documents.
    fn mentions_in(&self, document: &str, a: usize, b: usize) -> Result<Vec<Link>> {
        self.links(
            &format!(
                "MATCH (d:ConnectorDoc {{repository_id: $repo, id: $doc}})-[l:LINKS_TO]->() \
                 WHERE l.state = 'linked' AND l.byte_start >= $a AND l.byte_end <= $b \
                 AND l.producer IN $producers \
                 RETURN {LINK_COLUMNS} ORDER BY l.byte_start, l.seq"
            ),
            gparams([
                ("repo", json!(self.repository)),
                ("doc", json!(document)),
                ("a", json!(a)),
                ("b", json!(b)),
                ("producers", json!(producers())),
            ]),
        )
    }
    /// The project's own documents' linked mentions of definitions whose
    /// window in `relpath` overlaps `[a, b)`.
    fn mentions_of(&self, relpath: &str, a: usize, b: usize) -> Result<Vec<Link>> {
        self.links(
            &format!(
                "MATCH (d:ConnectorDoc {{repository_id: $repo}})-[l:LINKS_TO]->() \
                 WHERE l.target_repository_id = $repo AND l.target_relpath = $path \
                 AND l.state = 'linked' AND l.target_start < $b AND l.target_end > $a \
                 AND l.producer IN $producers \
                 RETURN {LINK_COLUMNS} ORDER BY d.id, l.byte_start, l.seq"
            ),
            gparams([
                ("repo", json!(self.repository)),
                ("path", json!(relpath)),
                ("a", json!(a)),
                ("b", json!(b)),
                ("producers", json!(producers())),
            ]),
        )
    }
    /// Whether the links of `document` were computed for its current bytes.
    fn computed_for(&self, document: &str, sha: &str) -> Result<bool> {
        Ok(self
            .graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (d:ConnectorDoc {repository_id: $repo, id: $doc, document_sha256: $sha}) \
                     RETURN 1 AS yes",
                    gparams([
                        ("repo", json!(self.repository)),
                        ("doc", json!(document)),
                        ("sha", json!(sha)),
                    ]),
                )
            })?
            .is_some())
    }
}

/// What the `doc_links` step added to a query's candidates.
pub struct Lifted {
    pub candidates: Vec<Value>,
    pub report: Value,
}

fn line_of(content: &str, at: usize) -> usize {
    content.as_bytes()[..at.min(content.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// A window `[a, b)` of `document` as a candidate no passage covers (as the
/// code arm synthesizes its anchors).
fn window(document: &Value, a: usize, b: usize, identity: String, facts: Value) -> Option<Value> {
    let content = s(document, "content");
    let text = content.get(a..b)?;
    if text.trim().is_empty() {
        return None;
    }
    let mut source = document.as_object().cloned().unwrap_or_default();
    source.remove("content");
    source.insert("byte_start".into(), json!(a));
    source.insert("byte_end".into(), json!(b));
    source.insert("line".into(), json!(line_of(content, a)));
    source.insert(
        "end_line".into(),
        json!(line_of(content, b.saturating_sub(1).max(a))),
    );
    source.insert("total_bytes".into(), json!(content.len()));
    source.insert("complete".into(), json!(a == 0 && b == content.len()));
    source.insert("truncated".into(), json!(b < content.len()));
    source.insert(ARM.into(), facts);
    Some(
        json!({"identity":identity,"kind":"source","origin":document["id"],"title":document["title"],
        "text":text,"source_sha":document["sha256"],"source":source}),
    )
}

/// The paragraph around `[a, b)` (between blank lines), at most
/// [`PARAGRAPH_BYTES`] on each side of the mention.
fn paragraph(content: &str, a: usize, b: usize) -> (usize, usize) {
    let mut start = content[..a].rfind("\n\n").map_or(0, |n| n + 2);
    if a - start > PARAGRAPH_BYTES {
        start = a - PARAGRAPH_BYTES;
        while !content.is_char_boundary(start) {
            start += 1;
        }
    }
    let mut end = content[b..].find("\n\n").map_or(content.len(), |n| b + n);
    if end - b > PARAGRAPH_BYTES {
        end = b + PARAGRAPH_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
    }
    (start, end)
}

/// The `doc_links` step of a query: after the fusion,
/// each selected window, in rank order, lifts what its links reach, one hop:
/// a document window lifts the definitions its mentions name (`doc_to_code`),
/// and a code window lifts the passages of the project's documents that
/// mention a definition inside it (`code_to_doc`). A link counts only while
/// both its document and its target file still have the bytes it was
/// computed from (otherwise it is `pending`, in links), and only for
/// documents in the reader's snapshot, which is already filtered by access
/// and connector freshness; a selected document the `link` job has not read
/// at its bytes yet counts in `pending_documents`. Lifted windows follow the
/// fused results and the code arm's anchors, at most [`LIFT_LIMIT`], never
/// one that a candidate already covers. Links into another project are not
/// followed here (workspace scope reads that project). A link whose kind of
/// document `options` does not follow is only counted, in
/// `withheld_repository` or `withheld_connector` (status `counted` when no
/// kind is followed).
pub fn lift(
    lookup: &Lookup<'_>,
    options: &Options,
    snapshot: &Snapshot,
    passages: &[Value],
    selected: &[Value],
    candidates: &[Value],
) -> Result<Lifted> {
    let documents: HashMap<&str, &Value> =
        snapshot.documents.iter().map(|d| (s(d, "id"), d)).collect();
    let mut chunks: HashMap<&str, Vec<(usize, usize, usize)>> = HashMap::new();
    for (index, row) in passages.iter().enumerate() {
        if row["kind"] != "source" {
            continue;
        }
        let source = &row["source"];
        if let (Some(a), Some(b)) = (source["byte_start"].as_u64(), source["byte_end"].as_u64()) {
            chunks
                .entry(s(source, "id"))
                .or_default()
                .push((a as usize, b as usize, index));
        }
    }
    let mut taken = Taken {
        ranges: candidates
            .iter()
            .filter_map(|c| {
                Some((
                    s(&c["source"], "id").to_string(),
                    c["source"]["byte_start"].as_u64()? as usize,
                    c["source"]["byte_end"].as_u64()? as usize,
                ))
            })
            .collect(),
        identities: candidates
            .iter()
            .map(|c| s(c, "identity").to_string())
            .collect(),
        lifted: Vec::new(),
    };
    let mut considered = 0usize;
    let mut pending = 0usize;
    // Links of a kind of document `options` does not follow: counted only.
    let mut withheld_repository = 0usize;
    let mut withheld_connector = 0usize;
    let mut covered = 0usize;
    let mut unlinked = BTreeSet::new();
    'hits: for hit in selected {
        let source = &hit["source"];
        let id = s(source, "id");
        let (Some(a), Some(b)) = (source["byte_start"].as_u64(), source["byte_end"].as_u64())
        else {
            continue;
        };
        let (a, b) = (a as usize, b as usize);
        let Some(document) = documents.get(id) else {
            continue;
        };
        let sha = s(document, "sha256");
        let from = s(hit, "identity");
        let relpath = id.strip_prefix("file:");
        // A document window whose links would be followed but are not
        // computed for these bytes yet: the `link` job has not run since it
        // changed.
        if options.follows(s(document, "source_kind"))
            && linkable(lookup.db, id, sdk_session(document))?
            && !lookup.computed_for(id, sha)?
        {
            unlinked.insert(id.to_string());
        }
        // A document window: the definitions its mentions name.
        for link in lookup.mentions_in(id, a, b)? {
            // A link into another project is neither followed nor counted
            // here: whether the reader may read that project is the
            // workspace scope's question.
            if link.target_repository != lookup.repository {
                continue;
            }
            considered += 1;
            if !options.follows(&link.source_kind) {
                if link.source_kind == "repository_file" {
                    withheld_repository += 1;
                } else {
                    withheld_connector += 1;
                }
                continue;
            }
            if link.document_sha256 != sha {
                pending += 1;
                continue;
            }
            let target_id = format!("file:{}", link.relpath);
            let (Some(target), Some(start), Some(end)) = (
                documents.get(target_id.as_str()),
                link.target_start,
                link.target_end,
            ) else {
                continue;
            };
            if s(target, "sha256") != link.target_sha256 {
                pending += 1;
                continue;
            }
            if taken.lifted.len() >= LIFT_LIMIT {
                break 'hits;
            }
            let mut facts = link_facts(&link, "doc_to_code", from);
            facts["line"] = link.target["line"].clone();
            facts["end_line"] = link.target["end_line"].clone();
            let identity = format!("{target_id}:{start}#{ARM}");
            if let Some(candidate) = window(target, start, end, identity, facts)
                && !taken.add(candidate)
            {
                covered += 1;
            }
        }
        // A code window: the project's document passages naming a
        // definition inside it.
        let Some(relpath) = relpath.filter(|path| format_of(path).is_none()) else {
            continue;
        };
        for link in lookup.mentions_of(relpath, a, b)? {
            // Only documents in the reader's snapshot count, so a link from
            // a document the reader may not read leaves no trace.
            let Some(mentioning) = documents.get(link.document_id.as_str()) else {
                continue;
            };
            considered += 1;
            if !options.follows(&link.source_kind) {
                if link.source_kind == "repository_file" {
                    withheld_repository += 1;
                } else {
                    withheld_connector += 1;
                }
                continue;
            }
            if link.target_sha256 != sha || s(mentioning, "sha256") != link.document_sha256 {
                pending += 1;
                continue;
            }
            if taken.lifted.len() >= LIFT_LIMIT {
                break 'hits;
            }
            let facts = link_facts(&link, "code_to_doc", from);
            let covering = chunks
                .get(link.document_id.as_str())
                .into_iter()
                .flatten()
                .find(|(x, y, _)| *x <= link.byte_start && link.byte_end <= *y);
            let candidate = match covering {
                Some((_, _, index)) => {
                    let mut candidate = passages[*index].clone();
                    candidate["source"][ARM] = facts;
                    Some(candidate)
                }
                None => {
                    let content = s(mentioning, "content");
                    let (x, y) = paragraph(content, link.byte_start, link.byte_end);
                    let identity = format!("{}:{x}#{ARM}", link.document_id);
                    window(mentioning, x, y, identity, facts)
                }
            };
            if let Some(candidate) = candidate
                && !taken.add(candidate)
            {
                covered += 1;
            }
        }
    }
    let status = if considered == 0 {
        "no_links"
    } else if options.enabled() {
        "followed"
    } else {
        "counted"
    };
    let report = json!({"status":status,"repository":options.repository,"connector":options.connector,
        "links":considered,"lifted":taken.lifted.len(),"deduplicated":covered,
        "pending":pending,"pending_documents":unlinked.len(),
        "withheld_repository":withheld_repository,"withheld_connector":withheld_connector,
        "limit":LIFT_LIMIT});
    Ok(Lifted {
        candidates: taken.lifted,
        report,
    })
}

/// What a query's candidates already hold, and what links lifted.
struct Taken {
    ranges: Vec<(String, usize, usize)>,
    identities: BTreeSet<String>,
    lifted: Vec<Value>,
}

impl Taken {
    /// Add a lifted window unless a candidate covers part of it already.
    fn add(&mut self, candidate: Value) -> bool {
        let id = s(&candidate["source"], "id").to_string();
        let a = candidate["source"]["byte_start"].as_u64().unwrap_or(0) as usize;
        let b = candidate["source"]["byte_end"].as_u64().unwrap_or(0) as usize;
        if self
            .ranges
            .iter()
            .any(|(x, p, q)| *x == id && *p < b && a < *q)
            || !self
                .identities
                .insert(s(&candidate, "identity").to_string())
        {
            return false;
        }
        self.ranges.push((id, a, b));
        self.lifted.push(candidate);
        true
    }
}

fn link_facts(link: &Link, direction: &str, from: &str) -> Value {
    json!({"via":"link","rung":link.rung,"producer":link.producer,"direction":direction,"from":from,
        "document_id":link.document_id,"mention":link.mention,"mark":link.mark,
        "mention_byte_start":link.byte_start,"mention_byte_end":link.byte_end,
        "stable_id":link.stable_id,"revision":link.target["revision"],"qualified_name":link.target["qualified_name"],
        "relpath":link.relpath,"episode":link.target["episode"],"project":link.project})
}

// ---------------------------------------------------------------------------
// Reverse reads and coverage (`graph.doc_links`)

/// A document a reader may read now: its sha256, and whether it is one of
/// the SDK's conversation sessions.
#[derive(Clone)]
struct Listed {
    sha: String,
    session: bool,
}

/// What a reader may read, per project (the snapshot's filter: access,
/// connector sharing and freshness).
struct Readable<'a> {
    db: &'a Connection,
    p: &'a Principal,
    documents: HashMap<String, Option<HashMap<String, Listed>>>,
    projects: HashMap<String, bool>,
    every: Option<bool>,
}

impl<'a> Readable<'a> {
    fn new(db: &'a Connection, p: &'a Principal) -> Self {
        Self {
            db,
            p,
            documents: HashMap::new(),
            projects: HashMap::new(),
            every: None,
        }
    }
    fn project(&mut self, project: &str) -> Result<bool> {
        if let Some(known) = self.projects.get(project) {
            return Ok(*known);
        }
        let known = one(
            self.db,
            "SELECT 1 AS yes FROM repo_projects WHERE id=? AND workspace_id=?",
            params![project, self.p.workspace],
        )?
        .is_some()
            && graph_projection::repo_readable(self.db, self.p, project)?;
        self.projects.insert(project.to_string(), known);
        Ok(known)
    }
    /// Whether the reader may read every project a mention may have been
    /// looked up in: all of the workspace's projects but PR scratch ones.
    /// Only such a reader sees how a mention resolved across projects. For
    /// any other reader the outcome would tell something about a project
    /// they may not read (that it defines the name, or that it does not), so
    /// it reads as unresolved.
    fn every(&mut self) -> Result<bool> {
        if let Some(known) = self.every {
            return Ok(known);
        }
        let scratch = graph_projection::pr_scratch_projects(self.db, &self.p.workspace)?;
        let mut every = true;
        for row in rows(
            self.db,
            "SELECT id FROM repo_projects WHERE workspace_id=?",
            [&self.p.workspace],
        )? {
            let id = s(&row, "id");
            if !scratch.contains(id) && !self.project(id)? {
                every = false;
                break;
            }
        }
        self.every = Some(every);
        Ok(every)
    }
    /// The documents of `project` the reader may read now, by id (`None`
    /// when the reader may not read the project).
    fn listed(&mut self, project: &str) -> Result<Option<&HashMap<String, Listed>>> {
        if !self.documents.contains_key(project) {
            let listed = if self.project(project)? {
                let repository = crate::store::repo(self.db, self.p, project)?;
                Some(
                    crate::memory::documents(self.db, &repository, self.p)?
                        .iter()
                        .map(|d| {
                            (
                                s(d, "id").to_string(),
                                Listed {
                                    sha: s(d, "sha256").to_string(),
                                    session: sdk_session(d),
                                },
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };
            self.documents.insert(project.to_string(), listed);
        }
        Ok(self.documents[project].as_ref())
    }
    /// The document when the reader may read it now.
    fn document(&mut self, project: &str, document: &str) -> Result<Option<Listed>> {
        Ok(self
            .listed(project)?
            .and_then(|listed| listed.get(document).cloned()))
    }
    /// How a stored row reads for this reader: a link into a project they
    /// may not read, or any outcome across projects when they may not read
    /// them all ([`Self::every`]), shows as no link, so neither a row nor a
    /// count reveals another project.
    fn state(&mut self, link: &Value) -> Result<&'static str> {
        if s(link, "scope") == "workspace" && !self.every()? {
            return Ok("unresolved");
        }
        Ok(match s(link, "state") {
            "linked" => {
                let target = s(link, "target_repository_id");
                if !target.is_empty() && self.project(target)? {
                    "linked"
                } else {
                    "unresolved"
                }
            }
            "ambiguous" => "ambiguous",
            _ => "unresolved",
        })
    }
}

/// `graph.doc_links` (read): the documents that mention a definition
/// (`stable_id`, over the workspace's projects), the mentions of one of the
/// project's documents (`document_id`), or, with neither, the project's link
/// coverage: per source kind, the documents read, their mentions and how
/// they resolved, and the linked mentions by rung. Only what the reader may
/// read now counts: documents of projects and connectors they may read,
/// links into projects they may read, outcomes across projects only for a
/// reader of every project; any other link reads as no link. Rows computed
/// for other bytes count as pending. Rows of producers this program does not
/// run are not read.
pub fn read(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let graph = store.graph();
    store.authorized_read(workspace, token, "read", |db, p| {
        let repository = crate::store::repo(db, p, crate::string(args, "repository_id", 100)?)?;
        let project = s(&repository, "id").to_string();
        let mut readable = Readable::new(db, p);
        let producers = json!(producers());
        let limit = crate::integer(args, "limit", 200, 2000)?.max(1);
        if let Some(stable) = args.get("stable_id").filter(|v| !v.is_null()) {
            let stable = stable
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 200)
                .ok_or_else(|| Error::bad("stable_id must be 1–200 bytes"))?;
            let mut out = Vec::new();
            for link in graph.read(|db| {
                store_graph::rows(
                    db,
                    &format!(
                        "MATCH (d:ConnectorDoc)-[l:LINKS_TO]->() \
                         WHERE l.target_repository_id = $repo AND l.stable_id = $stable \
                         AND l.state = 'linked' AND l.producer IN $producers \
                         RETURN {LINK_COLUMNS} \
                         ORDER BY d.repository_id, d.id, l.byte_start, l.seq"
                    ),
                    gparams([
                        ("repo", json!(project)),
                        ("stable", json!(stable)),
                        ("producers", producers.clone()),
                    ]),
                )
            })? {
                let document = s(&link, "document_id");
                if readable
                    .document(s(&link, "repository_id"), document)?
                    .is_none_or(|listed| listed.sha != s(&link, "document_sha256"))
                    || readable.state(&link)? != "linked"
                {
                    continue;
                }
                if out.len() < limit {
                    out.push(json!({"repository_id":link["repository_id"],"document_id":document,
                        "source_kind":link["source_kind"],"byte_start":link["byte_start"],"byte_end":link["byte_end"],
                        "mention":link["mention"],"mark":link["mark"],"rung":link["rung"],"producer":link["producer"],
                        "scope":link["scope"],"target_relpath":link["target_relpath"]}));
                }
            }
            return Ok(json!({"repository_id":project,"stable_id":stable,"documents":out,"rules":RULES}));
        }
        if let Some(document) = args.get("document_id").filter(|v| !v.is_null()) {
            let document = document
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 1500)
                .ok_or_else(|| Error::bad("document_id must be 1–1,500 bytes"))?;
            let listed = readable
                .document(&project, document)?
                .ok_or_else(|| Error::bad("Source unavailable"))?;
            let mut out = Vec::new();
            let mut pending = false;
            for link in graph.read(|db| {
                store_graph::rows(
                    db,
                    &format!(
                        "MATCH (d:ConnectorDoc {{repository_id: $repo, id: $doc}})-[l:LINKS_TO]->() \
                         WHERE l.producer IN $producers \
                         RETURN {LINK_COLUMNS} ORDER BY l.byte_start, l.seq"
                    ),
                    gparams([
                        ("repo", json!(project)),
                        ("doc", json!(document)),
                        ("producers", producers.clone()),
                    ]),
                )
            })? {
                if s(&link, "document_sha256") != listed.sha {
                    pending = true;
                    continue;
                }
                let state = readable.state(&link)?;
                let mut item = json!({"byte_start":link["byte_start"],"byte_end":link["byte_end"],
                    "mention":link["mention"],"mark":link["mark"],"state":state,"rung":link["rung"],"producer":link["producer"]});
                if state == "linked" {
                    let facts = target_facts(&link);
                    item["target"] = json!({"repository_id":link["target_repository_id"],"stable_id":link["stable_id"],
                        "relpath":link["target_relpath"],"qualified_name":facts["qualified_name"],
                        "revision":facts["revision"],"kind":facts["kind"],"line":facts["line"],
                        "end_line":facts["end_line"],"episode":facts["episode"],"scope":link["scope"]});
                } else if state == "ambiguous" {
                    item["candidates"] = link["candidates"].clone();
                }
                if out.len() < limit {
                    out.push(item);
                }
            }
            // Pending: rows for other bytes, or a document the job reads that
            // it has not read at these bytes yet.
            let computed = graph
                .read(|db| {
                    store_graph::one(
                        db,
                        "MATCH (d:ConnectorDoc {repository_id: $repo, id: $doc, document_sha256: $sha}) \
                         RETURN 1 AS yes",
                        gparams([
                            ("repo", json!(project)),
                            ("doc", json!(document)),
                            ("sha", json!(listed.sha)),
                        ]),
                    )
                })?
                .is_some();
            let pending = pending || (!computed && linkable(db, document, listed.session)?);
            return Ok(json!({"repository_id":project,"document_id":document,"mentions":out,
                "pending":pending,"rules":RULES}));
        }
        coverage(&graph, db, &project, &mut readable)
    })
}

fn coverage(
    graph: &GraphStore,
    db: &Connection,
    project: &str,
    readable: &mut Readable<'_>,
) -> Result<Value> {
    let mut kinds: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let bump = |kinds: &mut BTreeMap<String, Map<String, Value>>, kind: &str, key: &str, n: u64| {
        let entry = kinds.entry(kind.to_string()).or_default();
        let value = entry.get(key).and_then(Value::as_u64).unwrap_or(0) + n;
        entry.insert(key.into(), json!(value));
    };
    // The documents the reader may read now; nothing else counts.
    let listed = readable.listed(project)?.cloned().unwrap_or_default();
    let mut pending = 0u64;
    let mut read: HashSet<String> = HashSet::new();
    let mut computed: HashMap<String, String> = HashMap::new();
    for source in graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo}) \
             RETURN d.id AS document_id, d.source_kind AS source_kind, \
             d.document_sha256 AS document_sha256, d.`counts.omitted` AS omitted",
            gparams([("repo", json!(project))]),
        )
    })? {
        let document = s(&source, "document_id").to_string();
        read.insert(document.clone());
        let Some(found) = listed.get(&document) else {
            continue;
        };
        if found.sha != s(&source, "document_sha256") {
            pending += 1;
            continue;
        }
        let kind = s(&source, "source_kind").to_string();
        bump(&mut kinds, &kind, "documents", 1);
        bump(
            &mut kinds,
            &kind,
            "omitted",
            source["omitted"].as_u64().unwrap_or(0),
        );
        computed.insert(document, kind);
    }
    // Documents the job reads that it has not read yet ([`linkable`], with
    // the project's agent-session connectors read once).
    let sessions: HashSet<String> = rows(
        db,
        "SELECT id FROM am_connectors WHERE repository_id=? AND kind='jsonl' AND name=?",
        params![project, crate::capture::CONNECTOR_NAME],
    )?
    .iter()
    .map(|row| s(row, "id").to_string())
    .collect();
    for (document, found) in listed.iter().filter(|(id, _)| !read.contains(*id)) {
        let unread = match document.strip_prefix("file:") {
            Some(path) => format_of(path).is_some(),
            None => {
                !found.session
                    && document
                        .strip_prefix("connector:")
                        .and_then(|rest| rest.split_once(':'))
                        .is_some_and(|(connector, _)| !sessions.contains(connector))
            }
        };
        pending += u64::from(unread);
    }
    let mut mentions: HashMap<String, u64> = HashMap::new();
    let mut rungs: BTreeMap<String, u64> = BTreeMap::new();
    for link in graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo})-[l:LINKS_TO]->() \
             WHERE l.producer IN $producers \
             RETURN d.id AS document_id, l.document_sha256 AS document_sha256, l.state AS state, \
             l.scope AS scope, l.target_repository_id AS target_repository_id, l.rung AS rung",
            gparams([("repo", json!(project)), ("producers", json!(producers()))]),
        )
    })? {
        let document = s(&link, "document_id");
        let Some(kind) = computed.get(document) else {
            continue;
        };
        if listed.get(document).map(|found| found.sha.as_str()) != Some(s(&link, "document_sha256"))
        {
            continue;
        }
        *mentions.entry(document.to_string()).or_default() += 1;
        let state = readable.state(&link)?;
        bump(&mut kinds, kind, state, 1);
        if state == "linked" {
            *rungs.entry(s(&link, "rung").to_string()).or_default() += 1;
            if s(&link, "target_repository_id") != project {
                bump(&mut kinds, kind, "linked_other_projects", 1);
            }
        }
    }
    for (document, kind) in &computed {
        let found = mentions.get(document).copied().unwrap_or(0);
        bump(&mut kinds, kind, "mentions", found);
        if found > 0 {
            bump(&mut kinds, kind, "documents_with_mentions", 1);
        }
    }
    let mut total = Map::new();
    for counts in kinds.values() {
        for (key, value) in counts {
            let sum =
                total.get(key).and_then(Value::as_u64).unwrap_or(0) + value.as_u64().unwrap_or(0);
            total.insert(key.clone(), json!(sum));
        }
    }
    Ok(
        json!({"repository_id":project,"rules":RULES,"coverage":{"by_source_kind":kinds,"total":total,
        "by_rung":rungs,"pending_documents":pending}}),
    )
}

// ---------------------------------------------------------------------------
// Forgetting and project deletion (the link rows live in the graph store, so
// `crate::deletion` reaches them through these, not through SQL tables).

/// The documents of `repository` with stored rows that `scope` covers.
fn scoped_documents(
    graph: &GraphStore,
    repository: &str,
    scope: &crate::deletion::Scope,
) -> Result<Vec<String>> {
    Ok(graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (d:ConnectorDoc {repository_id: $repo}) RETURN d.id AS id",
                gparams([("repo", json!(repository))]),
            )
        })?
        .into_iter()
        .map(|row| s(&row, "id").to_string())
        .filter(|id| scope.source(id))
        .collect())
}

/// The link rows of `repository`'s documents in `scope`, as the SQL rows
/// counted: `(links, sources)`.
pub fn forget_counts(
    db: &Connection,
    repository: &str,
    scope: &crate::deletion::Scope,
) -> Result<(usize, usize)> {
    let graph = graph_of(db)?;
    let documents = scoped_documents(&graph, repository, scope)?;
    if documents.is_empty() {
        return Ok((0, 0));
    }
    let links = graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (d:ConnectorDoc {repository_id: $repo})-[l:LINKS_TO]->() \
                 WHERE d.id IN $docs RETURN count(l) AS n",
                gparams([("repo", json!(repository)), ("docs", json!(documents))]),
            )
        })?
        .and_then(|row| row["n"].as_u64())
        .unwrap_or(0) as usize;
    Ok((links, documents.len()))
}

/// Delete the link rows and sources of `repository`'s documents in `scope`
/// (every producer's: forgotten text leaves no trace); returns how many
/// rows (links + sources) went.
pub fn forget(db: &Connection, repository: &str, scope: &crate::deletion::Scope) -> Result<usize> {
    let graph = graph_of(db)?;
    let documents = scoped_documents(&graph, repository, scope)?;
    if documents.is_empty() {
        return Ok(0);
    }
    let (links, sources) = forget_counts(db, repository, scope)?;
    graph.transaction(|db| {
        let docs = json!(documents);
        store_graph::rows(
            db,
            "UNWIND $docs AS doc \
             MATCH (d:ConnectorDoc {repository_id: $repo, id: doc})-[l:LINKS_TO]->() DELETE l",
            gparams([("repo", json!(repository)), ("docs", docs.clone())]),
        )?;
        store_graph::rows(
            db,
            "UNWIND $docs AS doc \
             MATCH (d:ConnectorDoc {repository_id: $repo, id: doc}) DELETE d",
            gparams([("repo", json!(repository)), ("docs", docs)]),
        )?;
        Ok(())
    })?;
    Ok(links + sources)
}

/// Delete a project's link rows, sources and checkpoint (the SQL store's
/// `ON DELETE CASCADE` of the tables these replace), when the project is
/// deleted.
pub fn drop_project(db: &Connection, repository: &str) -> Result<()> {
    graph_of(db)?.transaction(|db| {
        store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo})-[l:LINKS_TO]->() DELETE l",
            gparams([("repo", json!(repository))]),
        )?;
        store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo}) DELETE d",
            gparams([("repo", json!(repository))]),
        )?;
        store_graph::rows(
            db,
            "MATCH (c:DocLinkCheckpoint {repository_id: $repo}) DELETE c",
            gparams([("repo", json!(repository))]),
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(found: &[Mention]) -> Vec<(Option<String>, Vec<String>, &'static str)> {
        found
            .iter()
            .map(|m| (m.path.clone(), m.names.clone(), m.mark))
            .collect()
    }

    #[test]
    fn markdown_code_spans_follow_the_engines_pattern() {
        assert_eq!(code_spans("a `b` c"), vec![(3, 4)]);
        assert_eq!(code_spans("``a`b`` and `c`"), vec![(2, 5), (13, 14)]);
        // An unclosed run is text; the next run opens.
        assert_eq!(code_spans("`a ``b``"), vec![(5, 6)]);
        assert_eq!(code_spans("``"), Vec::<(usize, usize)>::new());
        assert_eq!(code_spans("`a``"), Vec::<(usize, usize)>::new());
        assert_eq!(code_spans("x `é` y"), vec![(3, 5)]);
    }

    #[test]
    fn markdown_mentions_skip_fences_and_front_matter_and_keep_byte_ranges() {
        let text = "---\ntitle: `skipped`\n---\n# Use `parse_header`\n\n```\n`fenced`\n```\nSee `wire.py::Parser::read` and ` Store.get() `, not `a.py`, `two words` or `print`.\r\n";
        let found = markdown(text);
        assert_eq!(
            names(&found),
            vec![
                (None, vec!["parse_header".to_string()], "code_span"),
                (
                    Some("wire.py".to_string()),
                    vec!["Parser".to_string(), "read".to_string()],
                    "code_span"
                ),
                (
                    None,
                    vec!["Store".to_string(), "get".to_string()],
                    "code_span"
                ),
                (None, vec!["print".to_string()], "code_span"),
            ]
        );
        for mention in &found {
            let text = &text[mention.byte_start..mention.byte_end];
            assert!(!text.starts_with(' ') && !text.ends_with(' '), "{text:?}");
        }
        assert_eq!(&text[found[2].byte_start..found[2].byte_end], "Store.get()");
    }

    #[test]
    fn line_splitting_matches_the_engines() {
        let text = "a\r\nb\rc\u{2028}d\n\ne";
        let split: Vec<&str> = lines(text).into_iter().map(|(_, l)| l).collect();
        assert_eq!(split, ["a", "b", "c", "d", "", "e"]);
        for (offset, line) in lines(text) {
            assert_eq!(&text[offset..offset + line.len()], line);
        }
        assert!(lines("").is_empty());
        assert_eq!(lines("a\n").len(), 1);
    }

    /// The engine's own reStructuredText extraction (`--docs`) and this
    /// module's find the same mentions on a page exercising every rule.
    #[test]
    fn rst_mentions_match_the_engines_extraction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("example.py"), "print(1)\n").unwrap();
        std::fs::create_dir_all(dir.path().join("click")).unwrap();
        std::fs::write(dir.path().join("click/core.py"), "").unwrap();
        let text = "=========\nAPI guide\n=========\n\nUse :func:`click.echo` and ``Group.command``.\n\n\
             Commands with ``parse_header()``\n--------------------------------\n\n.. autoclass:: click.core.Command\n   :members:\n\n\
             See :meth:`~click.core.Command.invoke` and :class:`Context <click.Context>`.\n\n\
             Example::\n\n    parse_args(ignored)\n\nBack to ``make_context()``, not ``ctx``,\n\
             ``Host``, ``DEBUG`` or the ``click.core`` module; ``MultiCommand`` is code.\n\n\
             .. code-block:: python\n\n   ``Hidden.inside``\n\nAfter ``ui.Render`` and ``a :func:`b` c``.\n\n\
             .. literalinclude:: example.py\n";
        let page = dir.path().join("api.rst");
        std::fs::write(&page, text).unwrap();
        let engine = graphicle::extractors::rst::extract_rst(&page).unwrap();
        let mut expected: Vec<(Option<String>, Vec<String>)> = engine["raw_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let mut names: Vec<String> = c["qualifiers"]
                    .as_array()
                    .map(|q| q.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                names.push(c["callee"].as_str().unwrap().to_string());
                (c["path"].as_str().map(str::to_string), names)
            })
            .collect();
        expected.sort();
        expected.dedup();
        let root = dir.path().to_path_buf();
        let module = move |names: &[String]| {
            let relative = names.join("/");
            names.len() >= 2
                && root.ancestors().take(8).any(|d| {
                    let path = d.join(&relative);
                    path.is_dir() || path.with_extension("py").is_file()
                })
        };
        let found = rst(text, &module);
        let mut got: Vec<(Option<String>, Vec<String>)> = found
            .iter()
            .map(|m| (m.path.clone(), m.names.clone()))
            .collect();
        got.sort();
        got.dedup();
        assert_eq!(got, expected);
        assert!(got.iter().any(|(_, n)| n == &["parse_header"]), "{got:?}");
        for mention in &found {
            let span = &text[mention.byte_start..mention.byte_end];
            assert!(
                span.contains(mention.names.last().unwrap().as_str()),
                "{span}"
            );
        }
    }

    #[test]
    fn module_names_and_sphinx_folders_read_the_head_set() {
        let heads: BTreeMap<String, String> = [
            ("src/click/core.py", "x"),
            ("src/pkg/__init__.py", "y"),
            ("docs/conf.py", "z"),
        ]
        .into_iter()
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect();
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(names_module(&heads, "src/api.rst", &n(&["click", "core"])));
        assert!(names_module(
            &heads,
            "src/deep/a/b/api.rst",
            &n(&["pkg", "__init__"])
        ));
        assert!(names_module(&heads, "api.rst", &n(&["src", "pkg"])));
        assert!(!names_module(&heads, "api.rst", &n(&["click", "core"])));
        assert!(!names_module(&heads, "src/api.rst", &n(&["core"])));
    }

    #[test]
    fn connector_mentions_join_recorded_spans_and_backticks() {
        let text = "Fix parse_header now; also `Store.get` and parse_header.";
        let found = connector(text, &[(4, 16), (4, 16), (43, 55)]);
        assert_eq!(
            found
                .iter()
                .map(|m| (&text[m.byte_start..m.byte_end], m.mark))
                .collect::<Vec<_>>(),
            [
                ("parse_header", "code_mark"),
                ("Store.get", "code_span"),
                ("parse_header", "code_mark")
            ]
        );
        // Plain words are never mentions.
        assert!(connector("call parse_header please", &[]).is_empty());
        assert!(parse_code_spans(&json!([[0, 4]]), "abcd").is_ok());
        for bad in [
            json!([[2, 2]]),
            json!([[0, 9]]),
            json!([[0]]),
            json!({}),
            json!([[0, 1]]),
        ] {
            assert!(
                parse_code_spans(&bad, if bad == json!([[0, 1]]) { "é" } else { "abcd" }).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn options_count_links_by_default_and_follow_them_when_asked() {
        let parse = |v: Value| Options::parse(&v).unwrap();
        // Left out: no link is followed, documents still take part.
        let default = parse(json!({}));
        assert_eq!(
            default,
            Options {
                repository: false,
                connector: false,
                documents: true
            }
        );
        assert!(!default.enabled() && default.documents());
        assert!(default.sources().is_empty());
        assert_eq!(parse(json!({"doc_links":null})), default);
        let on = parse(json!({"doc_links":true}));
        assert!(on.enabled() && on.documents());
        assert_eq!(on.sources(), ["repository_file", "connector_source"]);
        let off = parse(json!({"doc_links":false}));
        assert!(!off.enabled() && !off.documents());
        assert!(Options::parse(&json!({"doc_links":"yes"})).is_err());
        assert!(document_edge(
            &json!({"source":"docs/a.md","target":"a.py","relation":"references"})
        ));
        assert!(!document_edge(
            &json!({"source":"b.py","target":"a.py","relation":"references"})
        ));
        assert!(!document_edge(
            &json!({"source":"docs/a.md","target":"a.py","relation":"imports"})
        ));
        // A link between pages is not a path to code: the switch keeps it.
        for target in ["docs/b.md", "guide/index.rst"] {
            assert!(!document_edge(
                &json!({"source":"docs/a.md","target":target,"kind":"document_link"})
            ));
        }
        assert!(document_edge(
            &json!({"source":"docs/a.md","target":"config.yaml","kind":"document_link"})
        ));
    }

    #[test]
    fn mention_names_hold_qualifiers_names_and_cited_file_names() {
        let names = |text: &str| {
            let found = markdown(text);
            found.iter().flat_map(mention_names).collect::<Vec<_>>()
        };
        assert_eq!(names("`Store.get()`"), ["Store", "get"]);
        assert_eq!(
            names("`src/wire.py::Parser::read`"),
            ["Parser", "read", "wire.py"]
        );
        let changed = Changed::Names(Arc::new(HashSet::from(["read".to_string()])));
        let held: BTreeSet<String> = ["Parser".to_string(), "read".to_string()].into();
        assert!(changed.touches(&held));
        assert!(!changed.touches(&["write".to_string()].into()));
        assert!(!Changed::Nothing.touches(&held));
        assert!(Changed::All.touches(&BTreeSet::new()));
    }

    /// A store with one project, for the job's own steps.
    struct Unit {
        _dir: tempfile::TempDir,
        store: Store,
        workspace: String,
        key: String,
        repo: String,
    }

    impl Unit {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let ws = store.create_workspace("Doc links").unwrap();
            let workspace = s(&ws, "id").to_string();
            let key = s(&ws, "capability").to_string();
            let registered = store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"unit","name":"unit"}),
                )
                .unwrap();
            Self {
                _dir: dir,
                store,
                workspace,
                key,
                repo: s(&registered, "id").to_string(),
            }
        }
        fn sync(&self, files: &[(&str, &str)]) {
            let args = |mut extra: Value| {
                extra["repository_id"] = json!(self.repo);
                extra
            };
            let state = self
                .store
                .project_status(&self.workspace, &self.key, &args(json!({})))
                .unwrap();
            let manifest: Vec<Value> = files
                .iter()
                .map(|(path, text)| {
                    json!({"path":path,"bytes":text.len(),
                    "sha256":graphicle_memory::text::sha256(text.as_bytes())})
                })
                .collect();
            let begin = self
                .store
                .sync_prepare(
                    &self.workspace,
                    &self.key,
                    &args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest})),
                )
                .unwrap();
            let upload: Vec<Value> = files
                .iter()
                .map(|(path, text)| json!({"path":path,"content":text}))
                .collect();
            self.store
                .sync_upload(
                    &self.workspace,
                    &self.key,
                    &args(json!({"sync_id":begin["sync_id"],"files":upload})),
                )
                .unwrap();
            self.store
                .sync_commit(
                    &self.workspace,
                    &self.key,
                    &args(json!({"sync_id":begin["sync_id"]})),
                )
                .unwrap();
        }
        /// Run the waiting projection only; the `link` job it queues waits.
        fn project(&self) {
            let job = self
                .store
                .lease_job_of_kind(&self.repo, "project")
                .unwrap()
                .unwrap();
            let result = crate::jobs::run_projection(&self.store, &job);
            self.store.finish_job(&job, result).unwrap();
        }
        fn count(&self, sql: &str) -> i64 {
            self.store
                .read(|db| Ok(db.query_row(sql, [], |row| row.get(0))?))
                .unwrap()
        }
        fn graph_count(&self, cypher: &str) -> i64 {
            self.store
                .graph()
                .read(|db| {
                    Ok(store_graph::one(db, cypher, Params::new())?.unwrap()["n"]
                        .as_i64()
                        .unwrap())
                })
                .unwrap()
        }
        fn owner(&self) -> String {
            self.store
                .read(|db| {
                    Ok(db.query_row(
                        "SELECT capability_hash FROM workspaces WHERE id=?",
                        [&self.workspace],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
        }
    }

    /// Rows planned against an episode
    /// that a projection has replaced by the time they would be written are
    /// not written; another `link` job is queued, which reads the new one.
    #[test]
    fn rows_planned_before_a_projection_are_not_written_and_the_job_is_queued_again() {
        let unit = Unit::new();
        let page = "# A\n\nSee `parse_header`.\n";
        let second = "# B\n\n`parse_header` again.\n";
        unit.sync(&[
            ("wire.py", "def parse_header(line):\n    return line\n"),
            ("docs/a.md", page),
        ]);
        crate::jobs::drain_projection(&unit.store, &unit.repo).unwrap();
        assert_eq!(unit.graph_count("MATCH (d:ConnectorDoc) RETURN count(d) AS n"), 1);
        unit.sync(&[
            ("wire.py", "def parse_header(line):\n    return line\n"),
            ("docs/a.md", page),
            ("docs/b.md", second),
        ]);
        unit.project();
        let (planned, resolved) = unit
            .store
            .read(|db| {
                let planned = plan(db, &unit.workspace)?;
                let resolved = compute(db, &planned);
                Ok((planned, resolved))
            })
            .unwrap();
        let ids: Vec<&str> = planned.documents.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["file:docs/b.md"]);
        // The code moves on, and is projected, before the rows are written;
        // no `link` job is left waiting.
        let edited = "def parse_header(line):\n    return line.strip()\n";
        unit.sync(&[
            ("wire.py", edited),
            ("docs/a.md", page),
            ("docs/b.md", second),
        ]);
        unit.project();
        unit.store
            .transaction(|db| {
                db.execute("UPDATE am_jobs SET state='complete' WHERE kind='link'", [])?;
                Ok(())
            })
            .unwrap();
        let summary = write_all(
            &unit.store,
            &unit.workspace,
            &unit.owner(),
            &|_| Ok(()),
            &planned,
            &resolved,
        )
        .unwrap();
        assert_eq!(summary["requeued"], true, "{summary}");
        assert_eq!(summary["relinked"], 0);
        assert_eq!(
            unit.graph_count(
                "MATCH (d:ConnectorDoc {id: 'file:docs/b.md'}) RETURN count(d) AS n"
            ),
            0
        );
        assert_eq!(
            unit.count("SELECT count(*) FROM am_jobs WHERE kind='link' AND state='queued'"),
            1
        );
        // That job reads the new episode: both pages link to the edited code.
        crate::jobs::drain_projection(&unit.store, &unit.repo).unwrap();
        assert_eq!(unit.graph_count("MATCH (d:ConnectorDoc) RETURN count(d) AS n"), 2);
        let sha = graphicle_memory::text::sha256(edited.as_bytes());
        assert_eq!(
            unit.graph_count(&format!(
                "MATCH ()-[l:LINKS_TO]->() WHERE l.state='linked' AND l.target_sha256='{sha}' \
                 RETURN count(l) AS n"
            )),
            2
        );
    }

    #[test]
    fn paragraphs_are_bounded() {
        let text = "one\n\ntwo `x` three\nfour\n\nfive";
        let at = text.find('x').unwrap();
        assert_eq!(&text[5..23], "two `x` three\nfour");
        assert_eq!(paragraph(text, at, at + 1), (5, 23));
        let long = format!("{}`x`{}", "a".repeat(5000), "b".repeat(5000));
        let at = long.find('x').unwrap();
        let (a, b) = paragraph(&long, at, at + 1);
        assert_eq!((at - a, b - at - 1), (PARAGRAPH_BYTES, PARAGRAPH_BYTES));
    }
}
