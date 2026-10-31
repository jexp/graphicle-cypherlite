//! The AsciiDoc frontend: jQAssistant's rule-document convention, as in
//! jqa-dashboard's `dashboard.adoc`. A rule is a delimited `----` listing
//! whose block attributes name its kind (`[source,cypher,role=concept]`); a
//! group is an attribute line with no listing. The id is the block's anchor
//! (`[[id]]`) or its `id` attribute, the description its `.Title` or
//! `description` attribute.
//!
//! The text is read into the shared model's blocks; the rule grammar is the
//! model's. Before scanning, `include::target[]` lines are spliced in from
//! files relative to the document's folder, recursively. Conditional
//! directives (`ifdef::` and friends) are refused rather than guessed at.
//! Everything outside a rule (sections, prose, tables, comments and listings
//! that are not rules) is skipped.
use super::model::{self, Field, Kind, RawBlock, Reason, RuleDocument, RuleError};
use std::path::Path;

const MAX_INCLUDE_DEPTH: usize = 8;

/// Block attributes a rule block may carry. `role` and `source` select the
/// block; `verify` is jQAssistant's verification mode, read and not applied;
/// `style` is the block's first positional item (`sidebar` in `[sidebar]`).
const KNOWN: &[&str] = &[
    "id",
    "description",
    "requires",
    "requiresConcept",
    "requiresConcepts",
    "include",
    "includesConcepts",
    "includesConstraints",
    "severity",
    "overrides",
    "role",
    "source",
    "verify",
    "style",
];

const CONDITIONALS: &[&str] = &["ifdef::", "ifndef::", "ifeval::", "endif::"];

/// A physical line after include splicing: its text, the included file it
/// came from (`None` for the document itself) and its line number in that file.
struct Line {
    text: String,
    file: Option<String>,
    no: usize,
}

/// One block attribute. `line` is where it is written.
struct Attr {
    key: String,
    value: String,
    line: usize,
}

/// The attribute lines, anchor and title that wait for the next block.
#[derive(Default)]
struct Pending {
    first: Option<(usize, Option<String>)>,
    anchor: Option<(String, usize)>,
    title: Option<(String, usize)>,
    attrs: Vec<Attr>,
}

impl Pending {
    fn start(&mut self, no: usize, file: Option<&str>) {
        if self.first.is_none() {
            self.first = Some((no, file.map(str::to_string)));
        }
    }
}

/// Parse an AsciiDoc rule document. `base` is the folder `include::` targets
/// are read from; without one, a document that includes a file is refused.
pub fn parse(text: &str, base: Option<&Path>) -> Result<RuleDocument, RuleError> {
    let mut lines = Vec::new();
    splice(text, base, None, 0, &mut lines)?;
    model::build(scan(&lines)?)
}

fn problem(file: Option<&str>, line: usize, message: impl Into<String>) -> RuleError {
    RuleError::new("", None, line, Reason::Invalid(message.into())).in_file(file)
}

/// Read the document into lines, replacing each `include::` with the file's
/// lines.
fn splice(
    text: &str,
    base: Option<&Path>,
    file: Option<&str>,
    depth: usize,
    out: &mut Vec<Line>,
) -> Result<(), RuleError> {
    for (index, text) in text.lines().enumerate() {
        let no = index + 1;
        let trimmed = text.trim();
        if let Some(target) = include_target(trimmed) {
            if depth >= MAX_INCLUDE_DEPTH {
                return Err(problem(file, no, "include:: files nest deeper than 8 levels"));
            }
            if target.contains('{') {
                return Err(problem(
                    file,
                    no,
                    format!("include target '{target}' uses an attribute reference, which is not supported"),
                ));
            }
            let Some(dir) = base else {
                return Err(problem(
                    file,
                    no,
                    format!("include '{target}' needs the document's folder to resolve it"),
                ));
            };
            let path = dir.join(target);
            let shown = path.display().to_string();
            let body = std::fs::read_to_string(&path).map_err(|error| {
                problem(file, no, format!("cannot read include '{shown}': {error}"))
            })?;
            splice(&body, path.parent(), Some(shown.as_str()), depth + 1, out)?;
            continue;
        }
        if CONDITIONALS.iter().any(|name| trimmed.starts_with(name)) {
            return Err(problem(
                file,
                no,
                "conditional directives (ifdef, ifndef, ifeval, endif) are not supported",
            ));
        }
        out.push(Line {
            text: text.to_string(),
            file: file.map(str::to_string),
            no,
        });
    }
    Ok(())
}

/// The target of an `include::target[attributes]` line.
fn include_target(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("include::")?.strip_suffix(']')?;
    let (target, _) = rest.split_once('[')?;
    Some(target)
}

/// Walk the lines and collect the rule blocks, in document order.
fn scan(lines: &[Line]) -> Result<Vec<RawBlock>, RuleError> {
    let mut blocks = Vec::new();
    let mut pending = Pending::default();
    let mut at = 0;
    while at < lines.len() {
        let line = &lines[at];
        let text = line.text.trim();
        let no = line.no;
        let file = line.file.as_deref();
        at += 1;
        if text.is_empty() || (text.starts_with("//") && delimiter(text).is_none()) {
            continue;
        }
        if let Some(id) = anchor(text) {
            // A new anchor starts a new block: a pending group or rule that
            // had no listing ends here.
            if pending.anchor.is_some() || !pending.attrs.is_empty() {
                flush(&mut pending, &mut blocks)?;
            }
            pending.start(no, file);
            pending.anchor.get_or_insert((id.to_string(), no));
            continue;
        }
        if let Some(inner) = text.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            pending.start(no, file);
            pending.attrs.extend(attributes(inner, no));
            continue;
        }
        if let Some(title) = title(text) {
            pending.start(no, file);
            pending.title.get_or_insert((title, no));
            continue;
        }
        match delimiter(text) {
            Some((ch, n)) if verbatim(ch, n) => {
                let Some(close) = (at..lines.len()).find(|&j| lines[j].text.trim() == text) else {
                    let kind = role(&pending).map_or("", Kind::name);
                    return Err(RuleError::new(kind, None, no, Reason::Unclosed).in_file(file));
                };
                let body = lines[at..close]
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                match role(&pending) {
                    Some(kind) if ch == '-' => {
                        blocks.push(to_block(kind, &pending, Some(body.trim().to_string()))?);
                        pending = Pending::default();
                    }
                    _ => flush(&mut pending, &mut blocks)?,
                }
                at = close + 1;
            }
            // Compound delimiters (open block, example, sidebar, ...) end the
            // pending attributes; their content is scanned as prose.
            Some(_) => flush(&mut pending, &mut blocks)?,
            // Prose and section headings end pending attributes too: a group
            // has no listing and ends here; a rule without one is rejected.
            None => flush(&mut pending, &mut blocks)?,
        }
    }
    flush(&mut pending, &mut blocks)?;
    Ok(blocks)
}

/// Emit the pending attributes as a block when they name a rule kind (a
/// group with no listing, or a rule the model will reject as bodiless), and
/// clear them.
fn flush(pending: &mut Pending, blocks: &mut Vec<RawBlock>) -> Result<(), RuleError> {
    if let Some(kind) = role(pending) {
        blocks.push(to_block(kind, pending, None)?);
    }
    *pending = Pending::default();
    Ok(())
}

/// The rule kind the pending `role` attribute names.
fn role(pending: &Pending) -> Option<Kind> {
    pending
        .attrs
        .iter()
        .filter(|attr| attr.key == "role")
        .flat_map(|attr| attr.value.split_whitespace())
        .find_map(Kind::from_name)
}

/// Build the raw block of a rule kind from its pending attributes. Keys the
/// kind does not take are left for the model to reject, by name.
fn to_block(kind: Kind, pending: &Pending, body: Option<String>) -> Result<RawBlock, RuleError> {
    let (line, file) = pending.first.clone().unwrap_or((0, None));
    let ctx = Ctx {
        kind,
        line,
        file: file.as_deref(),
    };
    for attr in &pending.attrs {
        if !KNOWN.contains(&attr.key.as_str()) {
            return Err(ctx.invalid(format!(
                "unknown attribute '{}' on line {}",
                attr.key, attr.line
            )));
        }
        if attr.value.contains('{') {
            return Err(ctx.invalid(format!(
                "attribute reference in '{}' on line {} is not supported",
                attr.key, attr.line
            )));
        }
    }
    if body.is_some() {
        match single(&ctx, pending, "source")? {
            Some(source) if source.value == "cypher" => {}
            _ => return Err(ctx.invalid("a rule body must be a [source,cypher] listing".into())),
        }
    }

    let mut fields = Vec::new();
    match (&pending.anchor, single(&ctx, pending, "id")?) {
        (Some((anchor, at)), Some(attr)) if *anchor != attr.value => {
            return Err(ctx.invalid(format!(
                "anchor '{anchor}' on line {at} and id '{}' on line {} disagree",
                attr.value, attr.line
            )));
        }
        (Some((anchor, at)), _) => fields.push(field("id", anchor, *at)),
        (None, Some(attr)) => fields.push(field("id", &attr.value, attr.line)),
        (None, None) => {}
    }
    if let Some(attr) = single(&ctx, pending, "description")? {
        fields.push(field("description", &attr.value, attr.line));
    } else if let Some((title, at)) = &pending.title {
        fields.push(field("description", title, *at));
    }
    if let Some((items, at)) = list_attrs(pending, &["requires", "requiresConcept", "requiresConcepts"]) {
        fields.push(field("requires", &bracketed(&items), at));
    }
    if let Some((items, at)) = list_attrs(pending, &["include", "includesConcepts", "includesConstraints"]) {
        fields.push(field("include", &bracketed(&items), at));
    }
    if let Some(attr) = single(&ctx, pending, "severity")? {
        fields.push(field("severity", &attr.value, attr.line));
    }
    if let Some(attr) = single(&ctx, pending, "overrides")? {
        let value = if attr.value.starts_with('[') {
            attr.value.clone()
        } else {
            bracketed(std::slice::from_ref(&attr.value))
        };
        fields.push(field("overrides", &value, attr.line));
    }
    Ok(RawBlock {
        kind,
        line,
        file,
        fields,
        body,
    })
}

struct Ctx<'a> {
    kind: Kind,
    line: usize,
    file: Option<&'a str>,
}

impl Ctx<'_> {
    fn invalid(&self, message: String) -> RuleError {
        RuleError::new(self.kind.name(), None, self.line, Reason::Invalid(message)).in_file(self.file)
    }
}

/// The one attribute of a key; a key given twice is an error.
fn single<'a>(ctx: &Ctx, pending: &'a Pending, key: &str) -> Result<Option<&'a Attr>, RuleError> {
    let mut found = None;
    for attr in pending.attrs.iter().filter(|attr| attr.key == key) {
        if found.is_some() {
            return Err(ctx.invalid(format!("attribute '{key}' repeated on line {}", attr.line)));
        }
        found = Some(attr);
    }
    Ok(found)
}

/// The items of the list-valued attributes named in `keys`, merged in the
/// order written, with the first line that gives one. jQAssistant spells a
/// single reference `requiresConcept` and a list `requiresConcepts`; both
/// feed the same list.
fn list_attrs(pending: &Pending, keys: &[&str]) -> Option<(Vec<String>, usize)> {
    let mut items = Vec::new();
    let mut first = None;
    for attr in pending.attrs.iter().filter(|attr| keys.contains(&attr.key.as_str())) {
        first.get_or_insert(attr.line);
        items.extend(
            attr.value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty()),
        );
    }
    first.filter(|_| !items.is_empty()).map(|at| (items, at))
}

fn bracketed(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

fn field(key: &str, value: &str, line: usize) -> Field {
    Field {
        key: key.to_string(),
        value: value.to_string(),
        line,
    }
}

/// The attributes of an attribute line: `key=value` pairs, and positional
/// items. The first positional is the style; in `[source,cypher]` the second
/// is the listing's language.
fn attributes(inner: &str, line: usize) -> Vec<Attr> {
    let items = split_items(inner);
    let is_source = items.first().map(|item| item.trim()) == Some("source");
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let item = item.trim();
        if is_source && index == 0 {
            // The `source` keyword itself; the language is the next item.
            continue;
        }
        if let Some((key, value)) = item.split_once('=') {
            if model::is_key(key.trim()) {
                out.push(Attr {
                    key: key.trim().to_string(),
                    value: unquote(value),
                    line,
                });
                continue;
            }
        }
        let key = match index {
            _ if is_source => "source",
            0 => "style",
            _ => "positional",
        };
        out.push(Attr {
            key: key.to_string(),
            value: unquote(item),
            line,
        });
    }
    out
}

/// Split on commas outside quotes.
fn split_items(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                current.push(c);
            }
            (Some(open), c) if c == open => {
                quote = None;
                current.push(c);
            }
            (None, ',') => items.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    items.push(current);
    items
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if quoted {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// `[[id]]` or `[[id,reftext]]`: the anchor's id.
fn anchor(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("[[")?.strip_suffix("]]")?;
    Some(inner.split(',').next().unwrap_or("").trim())
}

/// `.Title`: a block title, not a list item or an ellipsis.
fn title(text: &str) -> Option<String> {
    let rest = text.strip_prefix('.')?;
    (!rest.is_empty() && !rest.starts_with(['.', ' '])).then(|| rest.trim().to_string())
}

/// A delimiter line: `--` (open block) or at least four of one of `-.=*_+/`.
fn delimiter(text: &str) -> Option<(char, usize)> {
    let first = text.chars().next()?;
    let n = text.chars().take_while(|c| *c == first).count();
    let delimits = (first == '-' && n == 2) || (n >= 4 && "-.=*_+/".contains(first));
    (delimits && text.len() == n).then_some((first, n))
}

/// The delimiters whose content is verbatim (a listing, literal, passthrough
/// or comment): their content is not scanned as prose.
fn verbatim(ch: char, n: usize) -> bool {
    n >= 4 && matches!(ch, '-' | '.' | '+' | '/')
}
