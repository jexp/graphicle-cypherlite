//! Architecture rule documents: the Markdown authoring surface of the
//! architecture layer (architecture-concepts.md §1.5).
//!
//! A rule document is ordinary Markdown. Each rule is one fenced block whose
//! info string names its kind: `concept`, `constraint` or `group`. The block
//! holds `key: value` metadata lines and, for concepts and constraints, a
//! nested ```cypher fence with the body. `docs/architecture-rules.md` shows
//! the layout with a complete example.
//!
//! [`parse`] is a pure function over the document text. [`store`] writes the
//! parsed rules as `(:ConceptRule)`, `(:ConstraintRule)` and `(:RuleGroup)`
//! nodes scoped to one project, keyed by `(repository_id, id)`. Each node
//! carries a `rule_sha`: a digest of the rule's content, so re-parsing
//! identical text yields the same value.
use crate::store_graph::{self, GraphStore, Params};
use crate::{Result, digest};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The info strings that open a rule block.
const KINDS: &[(&str, Kind)] = &[
    ("concept", Kind::Concept),
    ("constraint", Kind::Constraint),
    ("group", Kind::Group),
];

/// Metadata keys each block kind accepts; any other key is an error.
const CONCEPT_KEYS: &[&str] = &["id", "description", "requires", "severity"];
const CONSTRAINT_KEYS: &[&str] = &["id", "description", "requires", "severity"];
const GROUP_KEYS: &[&str] = &["id", "description", "include", "overrides"];

/// The severity ladder of the architecture layer, lowest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Minor,
    Major,
    Critical,
    Blocker,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Minor => "MINOR",
            Severity::Major => "MAJOR",
            Severity::Critical => "CRITICAL",
            Severity::Blocker => "BLOCKER",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "INFO" => Severity::Info,
            "MINOR" => Severity::Minor,
            "MAJOR" => Severity::Major,
            "CRITICAL" => Severity::Critical,
            "BLOCKER" => Severity::Blocker,
            _ => return None,
        })
    }
}

/// A concept or a constraint: the same metadata and body, different meaning
/// (a concept labels matches, a constraint reports them as violations).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub description: String,
    /// Rule ids or wildcard patterns (`arch:Module/*`) this rule runs after.
    pub requires: Vec<String>,
    pub severity: Severity,
    pub cypher: String,
    /// The 1-based line of the block's opening fence.
    pub line: usize,
    pub rule_sha: String,
}

/// A group: rules selected by include patterns, with severity overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub description: String,
    pub include: Vec<String>,
    pub overrides: Vec<(String, Severity)>,
    pub line: usize,
    pub rule_sha: String,
}

/// Every rule a document declares, in document order within each kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleDocument {
    pub concepts: Vec<Rule>,
    pub constraints: Vec<Rule>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Concept,
    Constraint,
    Group,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Concept => "concept",
            Kind::Constraint => "constraint",
            Kind::Group => "group",
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Kind::Concept => CONCEPT_KEYS,
            Kind::Constraint => CONSTRAINT_KEYS,
            Kind::Group => GROUP_KEYS,
        }
    }

    fn from_info(info: &str) -> Option<Self> {
        KINDS.iter().find(|(name, _)| *name == info).map(|(_, kind)| *kind)
    }
}

/// Why a block is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The block's fence is never closed.
    Unclosed,
    /// A required metadata key is absent.
    MissingKey(&'static str),
    /// The block's id is already declared by an earlier block.
    DuplicateId,
    /// A concept or constraint has no ```cypher body.
    MissingBody,
    /// Any other problem, stated in full.
    Invalid(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Unclosed => write!(f, "fence is never closed"),
            Reason::MissingKey(key) => write!(f, "missing required key '{key}'"),
            Reason::DuplicateId => write!(f, "id is already declared by an earlier block"),
            Reason::MissingBody => write!(f, "missing ```cypher body"),
            Reason::Invalid(message) => write!(f, "{message}"),
        }
    }
}

/// A malformed rule block: its kind, id (when known), line and the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError {
    pub kind: &'static str,
    pub id: Option<String>,
    pub line: usize,
    pub reason: Reason,
}

impl RuleError {
    fn new(kind: &'static str, id: Option<&str>, line: usize, reason: Reason) -> Self {
        Self {
            kind,
            id: id.map(str::to_string),
            line,
            reason,
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self
            .id
            .as_deref()
            .map(|id| format!(" '{id}'"))
            .unwrap_or_default();
        write!(
            f,
            "{} block{} at line {}: {}",
            self.kind, name, self.line, self.reason
        )
    }
}

impl std::error::Error for RuleError {}

impl From<RuleError> for crate::Error {
    fn from(error: RuleError) -> Self {
        crate::Error::bad(error)
    }
}

/// Parse a rule document. Fenced blocks whose info string is not a rule kind
/// are prose or examples and are skipped, including their contents.
pub fn parse(text: &str) -> std::result::Result<RuleDocument, RuleError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut document = RuleDocument::default();
    let mut ids = BTreeSet::new();
    let mut at = 0;
    while at < lines.len() {
        let Some((ticks, info)) = fence_open(lines[at]) else {
            at += 1;
            continue;
        };
        let line = at + 1;
        let Some(close) = fence_close(&lines, at + 1, ticks) else {
            if let Some(kind) = Kind::from_info(info) {
                return Err(RuleError::new(kind.name(), None, line, Reason::Unclosed));
            }
            at += 1;
            continue;
        };
        if let Some(kind) = Kind::from_info(info) {
            let body = &lines[at + 1..close];
            match kind {
                Kind::Group => {
                    let group = parse_group(body, line)?;
                    if !ids.insert(group.id.clone()) {
                        return Err(duplicate(kind, &group.id, line));
                    }
                    document.groups.push(group);
                }
                Kind::Concept | Kind::Constraint => {
                    let rule = parse_rule(kind, body, line)?;
                    if !ids.insert(rule.id.clone()) {
                        return Err(duplicate(kind, &rule.id, line));
                    }
                    match kind {
                        Kind::Concept => document.concepts.push(rule),
                        _ => document.constraints.push(rule),
                    }
                }
            }
        }
        at = close + 1;
    }
    Ok(document)
}

fn duplicate(kind: Kind, id: &str, line: usize) -> RuleError {
    RuleError::new(kind.name(), Some(id), line, Reason::DuplicateId)
}

/// A concept or constraint block: metadata lines, then a ```cypher fence.
fn parse_rule(kind: Kind, body: &[&str], line: usize) -> std::result::Result<Rule, RuleError> {
    let (meta, cypher) = split_body(kind, body, line)?;
    let fields = metadata(kind, &meta, line)?;
    let id = required_id(kind, &fields, line)?;
    let name = Some(id.as_str());
    let description = text_field(&fields, "description").unwrap_or_default();
    let requires = list_field(kind, name, &fields, "requires", line)?;
    let severity = match fields.get("severity") {
        Some((value, at)) => severity(kind, name, value, *at, line)?,
        None if kind == Kind::Concept => Severity::Info,
        None => return Err(missing(kind, name, line, "severity")),
    };
    let cypher = match cypher {
        Some(cypher) if !cypher.is_empty() => cypher,
        _ => return Err(RuleError::new(kind.name(), name, line, Reason::MissingBody)),
    };
    let rule_sha = digest(&json!({
        "kind": kind.name(),
        "id": id,
        "description": description,
        "requires": requires,
        "severity": severity.as_str(),
        "cypher": cypher,
    }));
    Ok(Rule {
        id,
        description,
        requires,
        severity,
        cypher,
        line,
        rule_sha,
    })
}

/// A group block: metadata lines only.
fn parse_group(body: &[&str], line: usize) -> std::result::Result<Group, RuleError> {
    let kind = Kind::Group;
    let (meta, cypher) = split_body(kind, body, line)?;
    if cypher.is_some() {
        return Err(RuleError::new(
            kind.name(),
            None,
            line,
            Reason::Invalid("a group has no cypher body".into()),
        ));
    }
    let fields = metadata(kind, &meta, line)?;
    let id = required_id(kind, &fields, line)?;
    let name = Some(id.as_str());
    let description = text_field(&fields, "description").unwrap_or_default();
    let include = list_field(kind, name, &fields, "include", line)?;
    if include.is_empty() {
        return Err(RuleError::new(
            kind.name(),
            name,
            line,
            Reason::Invalid("include must name at least one rule or pattern".into()),
        ));
    }
    let overrides = match fields.get("overrides") {
        Some((value, at)) => override_list(kind, name, value, *at, line)?,
        None => Vec::new(),
    };
    let rule_sha = digest(&json!({
        "kind": kind.name(),
        "id": id,
        "description": description,
        "include": include,
        "overrides": overrides
            .iter()
            .map(|(id, severity)| format!("{id}={}", severity.as_str()))
            .collect::<Vec<_>>(),
    }));
    Ok(Group {
        id,
        description,
        include,
        overrides,
        line,
        rule_sha,
    })
}

/// Split a block into its metadata lines and the body of its nested
/// ```cypher fence (`None` when the block has no such fence).
fn split_body<'a>(
    kind: Kind,
    body: &[&'a str],
    line: usize,
) -> std::result::Result<(Vec<&'a str>, Option<String>), RuleError> {
    let Some(open) = body
        .iter()
        .position(|text| matches!(fence_open(text), Some((_, "cypher"))))
    else {
        return Ok((body.to_vec(), None));
    };
    let (ticks, _) = fence_open(body[open]).unwrap_or((3, ""));
    let Some(close) = fence_close(body, open + 1, ticks) else {
        return Err(RuleError::new(kind.name(), None, line, Reason::Unclosed));
    };
    if body[close + 1..].iter().any(|text| !text.trim().is_empty()) {
        return Err(RuleError::new(
            kind.name(),
            None,
            line,
            Reason::Invalid("text after the cypher body; put prose outside the rule block".into()),
        ));
    }
    let cypher = body[open + 1..close].join("\n").trim().to_string();
    Ok((body[..open].to_vec(), Some(cypher)))
}

/// `key: value` lines, keyed and checked against the kind's allowed keys.
/// Blank lines are ignored; a repeated key is an error.
fn metadata(
    kind: Kind,
    meta: &[&str],
    line: usize,
) -> std::result::Result<BTreeMap<String, (String, usize)>, RuleError> {
    let mut fields: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut pending = Vec::new();
    for (offset, text) in meta.iter().enumerate() {
        let at = line + 1 + offset;
        if text.trim().is_empty() {
            continue;
        }
        match text.split_once(':') {
            Some((key, value)) if is_key(key.trim()) => {
                pending.push((key.trim().to_string(), value.trim().to_string(), at));
            }
            _ => {
                return Err(RuleError::new(
                    kind.name(),
                    None,
                    line,
                    Reason::Invalid(format!("line {at} is not a 'key: value' pair")),
                ));
            }
        }
    }
    let id = pending
        .iter()
        .find(|(key, _, _)| key == "id")
        .map(|(_, value, _)| value.clone());
    let name = id.as_deref();
    for (key, value, at) in pending {
        if !kind.keys().contains(&key.as_str()) {
            return Err(RuleError::new(
                kind.name(),
                name,
                line,
                Reason::Invalid(format!("unknown key '{key}' on line {at}")),
            ));
        }
        if fields.contains_key(&key) {
            return Err(RuleError::new(
                kind.name(),
                name,
                line,
                Reason::Invalid(format!("key '{key}' repeated on line {at}")),
            ));
        }
        fields.insert(key, (value, at));
    }
    Ok(fields)
}

fn is_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn required_id(
    kind: Kind,
    fields: &BTreeMap<String, (String, usize)>,
    line: usize,
) -> std::result::Result<String, RuleError> {
    let Some((id, at)) = fields.get("id") else {
        return Err(missing(kind, None, line, "id"));
    };
    let invalid = id.is_empty()
        || id.contains(char::is_whitespace)
        || id.contains(['*', ',', '[', ']', '=']);
    if invalid {
        return Err(RuleError::new(
            kind.name(),
            Some(id.as_str()),
            line,
            Reason::Invalid(format!("id '{id}' on line {at} is not a plain identifier")),
        ));
    }
    Ok(id.clone())
}

fn missing(kind: Kind, name: Option<&str>, line: usize, key: &'static str) -> RuleError {
    RuleError::new(kind.name(), name, line, Reason::MissingKey(key))
}

fn text_field(fields: &BTreeMap<String, (String, usize)>, key: &str) -> Option<String> {
    fields.get(key).map(|(value, _)| value.clone())
}

/// A `[a, b]` list of rule ids or patterns; absent means empty.
fn list_field(
    kind: Kind,
    name: Option<&str>,
    fields: &BTreeMap<String, (String, usize)>,
    key: &str,
    line: usize,
) -> std::result::Result<Vec<String>, RuleError> {
    let Some((value, at)) = fields.get(key) else {
        return Ok(Vec::new());
    };
    let items = list(value).ok_or_else(|| {
        RuleError::new(
            kind.name(),
            name,
            line,
            Reason::Invalid(format!("'{key}' on line {at} must be a list like [a, b]")),
        )
    })?;
    for item in &items {
        pattern(item).map_err(|problem| {
            RuleError::new(
                kind.name(),
                name,
                line,
                Reason::Invalid(format!("'{key}' on line {at}: {problem}")),
            )
        })?;
    }
    Ok(items)
}

/// `[a, b, "c"]` as items; `None` when the text is not a bracketed list or an
/// item is empty.
fn list(value: &str) -> Option<Vec<String>> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            (!item.is_empty()).then(|| item.to_string())
        })
        .collect()
}

/// A rule id, or a pattern whose only `*` is a trailing wildcard.
fn pattern(item: &str) -> std::result::Result<(), String> {
    if item.contains(char::is_whitespace) {
        return Err(format!("'{item}' contains whitespace"));
    }
    let stars = item.matches('*').count();
    if stars > 1 || (stars == 1 && !item.ends_with('*')) {
        return Err(format!("'{item}': '*' is allowed only as the last character"));
    }
    Ok(())
}

/// `line` is the block's line (what a [`RuleError`] reports); `at` is the
/// key's line, named in the message.
fn severity(
    kind: Kind,
    name: Option<&str>,
    value: &str,
    at: usize,
    line: usize,
) -> std::result::Result<Severity, RuleError> {
    Severity::parse(value).ok_or_else(|| {
        RuleError::new(
            kind.name(),
            name,
            line,
            Reason::Invalid(format!(
                "severity '{value}' on line {at} is not one of INFO, MINOR, MAJOR, CRITICAL, BLOCKER"
            )),
        )
    })
}

/// `[id=SEVERITY, ...]` severity overrides of a group. `at` and `line` as in
/// [`severity`].
fn override_list(
    kind: Kind,
    name: Option<&str>,
    value: &str,
    at: usize,
    line: usize,
) -> std::result::Result<Vec<(String, Severity)>, RuleError> {
    let invalid = |detail: String| RuleError::new(kind.name(), name, line, Reason::Invalid(detail));
    let items = list(value)
        .ok_or_else(|| invalid(format!("'overrides' on line {at} must be a list like [id=MAJOR]")))?;
    items
        .iter()
        .map(|item| {
            let (id, severity_text) = item
                .split_once('=')
                .ok_or_else(|| invalid(format!("override '{item}' is not 'id=SEVERITY'")))?;
            pattern(id.trim()).map_err(|problem| invalid(problem))?;
            let severity = Severity::parse(severity_text.trim())
                .ok_or_else(|| invalid(format!("override '{item}' has an unknown severity")))?;
            Ok((id.trim().to_string(), severity))
        })
        .collect()
}

/// A line that opens a fence: the backtick count and the info string.
fn fence_open(line: &str) -> Option<(usize, &str)> {
    let text = line.trim_start();
    let ticks = text.bytes().take_while(|b| *b == b'`').count();
    (ticks >= 3).then(|| (ticks, text[ticks..].trim()))
}

/// The line that closes a fence opened with `ticks` backticks: at least that
/// many backticks and nothing else.
fn fence_close(lines: &[&str], from: usize, ticks: usize) -> Option<usize> {
    (from..lines.len()).find(|&at| {
        let text = lines[at].trim();
        text.len() >= ticks && text.bytes().all(|b| b == b'`')
    })
}

/// Write the document's rules as nodes scoped to `repository_id`, merged on
/// `(repository_id, id)` so a re-parse updates in place. `source` is the
/// document's path, kept as evidence of where a rule was declared.
pub fn store(
    graph: &GraphStore,
    repository_id: &str,
    source: &str,
    document: &RuleDocument,
) -> Result<()> {
    graph.transaction(|db| {
        for rule in &document.concepts {
            merge_rule(db, "ConceptRule", repository_id, source, rule)?;
        }
        for rule in &document.constraints {
            merge_rule(db, "ConstraintRule", repository_id, source, rule)?;
        }
        for group in &document.groups {
            let overrides: Vec<String> = group
                .overrides
                .iter()
                .map(|(id, severity)| format!("{id}={}", severity.as_str()))
                .collect();
            let props = json!({
                "description": group.description,
                "include": list_text(&group.include),
                "overrides": list_text(&overrides),
                "rule_sha": group.rule_sha,
                "source": source,
                "line": group.line,
            });
            merge_node(db, "RuleGroup", repository_id, &group.id, &props)?;
        }
        Ok(())
    })
}

fn merge_rule(
    db: &mut cypherlite::Connection,
    label: &str,
    repository_id: &str,
    source: &str,
    rule: &Rule,
) -> Result<()> {
    let props = json!({
        "description": rule.description,
        "requires": list_text(&rule.requires),
        "severity": rule.severity.as_str(),
        "cypher": rule.cypher,
        "rule_sha": rule.rule_sha,
        "source": source,
        "line": rule.line,
    });
    merge_node(db, label, repository_id, &rule.id, &props)
}

/// A list property as JSON text: the store keeps lists of strings in String
/// properties, parsed at the boundary (see `deletion.rs`, JSON payloads).
fn list_text(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

fn merge_node(
    db: &mut cypherlite::Connection,
    label: &str,
    repository_id: &str,
    id: &str,
    props: &Value,
) -> Result<()> {
    let cypher = format!(
        "MERGE (r:{label} {{repository_id: $repo, id: $id}}) SET r += $props"
    );
    let params: Params = store_graph::params(&json!({
        "repo": repository_id,
        "id": id,
        "props": props,
    }));
    store_graph::write(db, &cypher, params)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Layers

Prose is ignored, and so is this example:

`````markdown
````concept
id: arch:Example
````
`````

````concept
id: arch:Layer/Persistence
requires: [arch:Module/*]
severity: MINOR
description: Persistence modules
```cypher
MATCH (m:Module) WHERE m.name =~ '.*\\.db'
MERGE (l:Layer {name: 'Persistence'})
```
````

````constraint
id: arch:NoCycles
severity: BLOCKER
description: No module cycles
```cypher
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1) RETURN p
```
````

````group
id: arch:default
include: [arch:*]
overrides: [arch:NoCycles=MAJOR]
description: Default pack
````
";

    fn error(text: &str) -> RuleError {
        parse(text).expect_err("document should be rejected")
    }

    #[test]
    fn parses_concepts_constraints_and_groups() {
        let document = parse(DOC).unwrap();
        assert_eq!(document.concepts.len(), 1);
        assert_eq!(document.constraints.len(), 1);
        assert_eq!(document.groups.len(), 1);

        let concept = &document.concepts[0];
        assert_eq!(concept.id, "arch:Layer/Persistence");
        assert_eq!(concept.requires, vec!["arch:Module/*"]);
        assert_eq!(concept.severity, Severity::Minor);
        assert_eq!(concept.description, "Persistence modules");
        assert!(concept.cypher.starts_with("MATCH (m:Module)"));
        assert!(concept.cypher.ends_with("MERGE (l:Layer {name: 'Persistence'})"));
        assert_eq!(concept.line, 11);

        let constraint = &document.constraints[0];
        assert_eq!(constraint.severity, Severity::Blocker);
        assert!(constraint.requires.is_empty());

        let group = &document.groups[0];
        assert_eq!(group.include, vec!["arch:*"]);
        assert_eq!(group.overrides, vec![("arch:NoCycles".to_string(), Severity::Major)]);
    }

    #[test]
    fn concept_severity_defaults_to_info() {
        let document = parse(
            "````concept\nid: arch:Plain\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        )
        .unwrap();
        assert_eq!(document.concepts[0].severity, Severity::Info);
    }

    #[test]
    fn rule_sha_is_stable_across_reparses_and_moves_with_content() {
        let first = parse(DOC).unwrap();
        let again = parse(DOC).unwrap();
        assert_eq!(first.concepts[0].rule_sha, again.concepts[0].rule_sha);
        assert_eq!(first.groups[0].rule_sha, again.groups[0].rule_sha);

        // Moving a block down the document keeps its content digest.
        let shifted = parse(&format!("\n\n{DOC}")).unwrap();
        assert_eq!(first.constraints[0].rule_sha, shifted.constraints[0].rule_sha);
        assert_ne!(first.constraints[0].line, shifted.constraints[0].line);

        let edited = parse(&DOC.replace("MAJOR", "MINOR")).unwrap();
        assert_ne!(first.groups[0].rule_sha, edited.groups[0].rule_sha);
        let reworded = parse(&DOC.replace("MATCH p", "MATCH q")).unwrap();
        assert_ne!(first.constraints[0].rule_sha, reworded.constraints[0].rule_sha);
    }

    #[test]
    fn constraint_without_severity_names_block_and_line() {
        let text = "# Doc\n\n````constraint\nid: arch:NoSeverity\n```cypher\nMATCH (n) RETURN n\n```\n````\n";
        let error = error(text);
        assert_eq!(error.line, 3);
        assert_eq!(error.id.as_deref(), Some("arch:NoSeverity"));
        assert_eq!(error.reason, Reason::MissingKey("severity"));
        assert_eq!(
            error.to_string(),
            "constraint block 'arch:NoSeverity' at line 3: missing required key 'severity'"
        );
    }

    #[test]
    fn unknown_key_and_bad_severity_name_the_block() {
        let unknown = error(
            "````concept\nid: arch:Typo\nseverty: INFO\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        );
        assert_eq!(unknown.line, 1);
        assert!(unknown.to_string().contains("concept block 'arch:Typo' at line 1"));
        assert!(unknown.to_string().contains("unknown key 'severty'"));

        let severity = error(
            "````constraint\nid: arch:Loud\nseverity: LOUD\n```cypher\nMATCH (n) RETURN n\n```\n````\n",
        );
        assert!(severity.to_string().contains("severity 'LOUD'"));
        assert_eq!(severity.line, 1);
    }

    #[test]
    fn structural_faults_are_typed() {
        let duplicate = error(&format!("{DOC}\n````concept\nid: arch:NoCycles\n```cypher\nRETURN 1\n```\n````\n"));
        assert_eq!(duplicate.reason, Reason::DuplicateId);
        assert_eq!(duplicate.id.as_deref(), Some("arch:NoCycles"));

        let body = error("````concept\nid: arch:NoBody\nseverity: INFO\n````\n");
        assert_eq!(body.reason, Reason::MissingBody);

        let unclosed = error("# Doc\n\n````concept\nid: arch:Open\n");
        assert_eq!(unclosed.line, 3);
        assert_eq!(unclosed.reason, Reason::Unclosed);

        let wildcard = error("````group\nid: arch:Bad\ninclude: [arch:*Foo]\n````\n");
        assert!(wildcard.to_string().contains("'*' is allowed only as the last character"));

        let empty_group = error("````group\nid: arch:Empty\ninclude: []\n````\n");
        assert!(empty_group.to_string().contains("at least one rule"));
    }

    #[test]
    fn format_doc_example_parses() {
        // The example in docs/architecture-rules.md is the documented layout;
        // it must parse, and the outer fence must hide it from the document.
        let doc = include_str!("../../../docs/architecture-rules.md");
        let start = doc.find("`````markdown\n# Architecture rules").unwrap() + "`````markdown\n".len();
        let end = start + doc[start..].find("\n`````\n").unwrap();
        let example = parse(&doc[start..end]).unwrap();
        assert_eq!(example.concepts.len(), 2);
        assert_eq!(example.constraints.len(), 2);
        assert_eq!(example.groups.len(), 1);
        assert!(parse(doc).unwrap().concepts.is_empty());
    }

    #[test]
    fn foreign_fences_are_skipped_whole() {
        // A rule block shown inside a five-backtick Markdown example is prose,
        // and a later real rule still parses.
        let text = "```rust\nfn main() {}\n```\n\n`````markdown\n````concept\nid: arch:Shown\n```cypher\nRETURN 1\n```\n````\n`````\n\n````concept\nid: arch:Real\n```cypher\nRETURN 2\n```\n````\n";
        let document = parse(text).unwrap();
        assert_eq!(document.concepts.len(), 1);
        assert_eq!(document.concepts[0].id, "arch:Real");
    }

    #[test]
    fn store_merges_rules_per_project() {
        let graph = GraphStore::open_in_memory().unwrap();
        let document = parse(DOC).unwrap();
        store(&graph, "repo-a", "docs/architecture/layers.md", &document).unwrap();
        store(&graph, "repo-a", "docs/architecture/layers.md", &document).unwrap();
        store(&graph, "repo-b", "docs/architecture/layers.md", &document).unwrap();

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
        assert_eq!(counts[0]["n"], json!(1));
        assert_eq!(counts[1]["n"], json!(1));

        let sha = graph
            .read(|db| {
                store_graph::one(
                    db,
                    "MATCH (r:ConstraintRule {repository_id: 'repo-a', id: 'arch:NoCycles'}) RETURN r.rule_sha AS sha, r.severity AS severity",
                    Params::new(),
                )
            })
            .unwrap()
            .expect("constraint node");
        assert_eq!(sha["sha"], json!(document.constraints[0].rule_sha));
        assert_eq!(sha["severity"], json!("BLOCKER"));

        let groups = graph
            .read(|db| {
                store_graph::rows(db, "MATCH (g:RuleGroup) RETURN g.id AS id", Params::new())
            })
            .unwrap();
        assert_eq!(groups.len(), 2);
    }
}
