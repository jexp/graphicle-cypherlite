//! The rule model both frontends produce: what a rule document declares,
//! whichever text it was written in. A frontend reads its text into
//! [`RawBlock`]s, one per rule block, and [`build`] checks them against the
//! rule grammar (keys, ids, severities, patterns, bodies) and assembles the
//! [`RuleDocument`]. Rule semantics live here and nowhere else.
use crate::digest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Metadata keys each block kind accepts; any other key is an error.
const CONCEPT_KEYS: &[&str] = &["id", "description", "requires", "severity"];
const CONSTRAINT_KEYS: &[&str] = &["id", "description", "requires", "severity"];
const GROUP_KEYS: &[&str] = &["id", "description", "include", "overrides"];

/// The severity ladder of the architecture layer, lowest first. Serializes
/// as the ladder's names (`"INFO"`…`"BLOCKER"`), the spelling the frontends
/// read and the rule store keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
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

    /// A severity as [`Severity::as_str`] writes it: `INFO`…`BLOCKER`, the
    /// spelling the frontends read and the rule store keeps.
    pub fn parse(text: &str) -> Option<Self> {
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
    /// The 1-based line of the block's opening.
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

/// The kind of a rule block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Concept,
    Constraint,
    Group,
}

impl Kind {
    /// The kind a Markdown info string or an AsciiDoc `role` names.
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "concept" => Some(Kind::Concept),
            "constraint" => Some(Kind::Constraint),
            "group" => Some(Kind::Group),
            _ => None,
        }
    }

    pub(super) fn name(self) -> &'static str {
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
}

/// Why a block is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The block is never closed.
    Unclosed,
    /// A required metadata key is absent.
    MissingKey(&'static str),
    /// The block's id is already declared by an earlier block.
    DuplicateId,
    /// A concept or constraint has no cypher body.
    MissingBody,
    /// Any other problem, stated in full.
    Invalid(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Unclosed => write!(f, "block is never closed"),
            Reason::MissingKey(key) => write!(f, "missing required key '{key}'"),
            Reason::DuplicateId => write!(f, "id is already declared by an earlier block"),
            Reason::MissingBody => write!(f, "missing cypher body"),
            Reason::Invalid(message) => write!(f, "{message}"),
        }
    }
}

/// A malformed rule block: its kind, id (when known), line, the file it sits
/// in when that is not the document given to the parser, and the reason.
/// An error outside any block (an include that cannot be read, say) has an
/// empty kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError {
    pub kind: &'static str,
    pub id: Option<String>,
    pub line: usize,
    pub file: Option<String>,
    pub reason: Reason,
}

impl RuleError {
    pub(super) fn new(kind: &'static str, id: Option<&str>, line: usize, reason: Reason) -> Self {
        Self {
            kind,
            id: id.map(str::to_string),
            line,
            file: None,
            reason,
        }
    }

    pub(super) fn in_file(mut self, file: Option<&str>) -> Self {
        self.file = file.map(str::to_string);
        self
    }

    /// Name `file` as the source of the error, unless a nested include already
    /// named the file it came from.
    pub(super) fn in_file_if_unset(mut self, file: Option<&str>) -> Self {
        if self.file.is_none() {
            self.file = file.map(str::to_string);
        }
        self
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = self
            .file
            .as_deref()
            .map(|file| format!(" of {file}"))
            .unwrap_or_default();
        if self.kind.is_empty() {
            return write!(f, "line {}{place}: {}", self.line, self.reason);
        }
        let name = self
            .id
            .as_deref()
            .map(|id| format!(" '{id}'"))
            .unwrap_or_default();
        write!(
            f,
            "{} block{name} at line {}{place}: {}",
            self.kind, self.line, self.reason
        )
    }
}

impl std::error::Error for RuleError {}

impl From<RuleError> for crate::Error {
    fn from(error: RuleError) -> Self {
        crate::Error::bad(error)
    }
}

/// One metadata entry of a block, keyed by its canonical name for the kind
/// (`requires`, `include`, `severity`, ...). `line` is where it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Field {
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// Whether `key` is a metadata key as either frontend reads it: ASCII
/// letters, digits and underscores, not empty.
pub(super) fn is_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A rule block as a frontend read it: its kind, the line it starts on, the
/// file it is in (`None` for the document itself), its metadata and its
/// cypher body (`None` when the block has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RawBlock {
    pub kind: Kind,
    pub line: usize,
    pub file: Option<String>,
    pub fields: Vec<Field>,
    pub body: Option<String>,
}

/// Check the blocks of one document and assemble the rules. The first
/// malformed block stops the build.
pub(super) fn build(blocks: Vec<RawBlock>) -> Result<RuleDocument, RuleError> {
    let mut document = RuleDocument::default();
    let mut ids = BTreeSet::new();
    for block in &blocks {
        match block.kind {
            Kind::Group => {
                let group = build_group(block)?;
                claim(&mut ids, block, &group.id)?;
                document.groups.push(group);
            }
            Kind::Concept => {
                let rule = build_rule(block)?;
                claim(&mut ids, block, &rule.id)?;
                document.concepts.push(rule);
            }
            Kind::Constraint => {
                let rule = build_rule(block)?;
                claim(&mut ids, block, &rule.id)?;
                document.constraints.push(rule);
            }
        }
    }
    Ok(document)
}

fn claim(ids: &mut BTreeSet<String>, block: &RawBlock, id: &str) -> Result<(), RuleError> {
    if ids.insert(id.to_string()) {
        Ok(())
    } else {
        Err(fail(block, Some(id), Reason::DuplicateId))
    }
}

type Fields<'a> = BTreeMap<&'a str, &'a Field>;

fn build_rule(block: &RawBlock) -> Result<Rule, RuleError> {
    let fields = fields(block)?;
    let id = required_id(block, &fields)?;
    let name = Some(id.as_str());
    let description = text_of(&fields, "description");
    let requires = list_of(block, name, &fields, "requires")?;
    let severity = match fields.get("severity") {
        Some(field) => severity_of(block, name, field)?,
        None if block.kind == Kind::Concept => Severity::Info,
        None => return Err(missing(block, name, "severity")),
    };
    let cypher = match &block.body {
        Some(cypher) if !cypher.is_empty() => cypher.clone(),
        _ => return Err(fail(block, name, Reason::MissingBody)),
    };
    let rule_sha = digest(&json!({
        "kind": block.kind.name(),
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
        line: block.line,
        rule_sha,
    })
}

fn build_group(block: &RawBlock) -> Result<Group, RuleError> {
    if block.body.is_some() {
        return Err(fail(
            block,
            None,
            Reason::Invalid("a group has no cypher body".into()),
        ));
    }
    let fields = fields(block)?;
    let id = required_id(block, &fields)?;
    let name = Some(id.as_str());
    let description = text_of(&fields, "description");
    let include = list_of(block, name, &fields, "include")?;
    if include.is_empty() {
        return Err(fail(
            block,
            name,
            Reason::Invalid("include must name at least one rule or pattern".into()),
        ));
    }
    let overrides = overrides_of(block, name, &fields)?;
    let rule_sha = digest(&json!({
        "kind": block.kind.name(),
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
        line: block.line,
        rule_sha,
    })
}

fn fail(block: &RawBlock, id: Option<&str>, reason: Reason) -> RuleError {
    RuleError::new(block.kind.name(), id, block.line, reason).in_file(block.file.as_deref())
}

fn missing(block: &RawBlock, name: Option<&str>, key: &'static str) -> RuleError {
    fail(block, name, Reason::MissingKey(key))
}

/// The block's metadata by key. A key the kind does not accept, or one given
/// twice, is an error.
fn fields(block: &RawBlock) -> Result<Fields<'_>, RuleError> {
    let id = block
        .fields
        .iter()
        .find(|field| field.key == "id")
        .map(|field| field.value.as_str());
    let mut fields = BTreeMap::new();
    for field in &block.fields {
        if !block.kind.keys().contains(&field.key.as_str()) {
            return Err(fail(
                block,
                id,
                Reason::Invalid(format!(
                    "unknown key '{}' on line {}",
                    field.key, field.line
                )),
            ));
        }
        if fields.insert(field.key.as_str(), field).is_some() {
            return Err(fail(
                block,
                id,
                Reason::Invalid(format!(
                    "key '{}' repeated on line {}",
                    field.key, field.line
                )),
            ));
        }
    }
    Ok(fields)
}

fn required_id(block: &RawBlock, fields: &Fields) -> Result<String, RuleError> {
    let Some(field) = fields.get("id") else {
        return Err(missing(block, None, "id"));
    };
    let id = &field.value;
    let invalid =
        id.is_empty() || id.contains(char::is_whitespace) || id.contains(['*', ',', '[', ']', '=']);
    if invalid {
        return Err(fail(
            block,
            Some(id.as_str()),
            Reason::Invalid(format!(
                "id '{id}' on line {} is not a plain identifier",
                field.line
            )),
        ));
    }
    Ok(id.clone())
}

fn text_of(fields: &Fields, key: &str) -> String {
    fields
        .get(key)
        .map(|field| field.value.clone())
        .unwrap_or_default()
}

/// A `[a, b]` list of rule ids or patterns; absent means empty.
fn list_of(
    block: &RawBlock,
    name: Option<&str>,
    fields: &Fields,
    key: &str,
) -> Result<Vec<String>, RuleError> {
    let Some(field) = fields.get(key) else {
        return Ok(Vec::new());
    };
    let items = list(&field.value).ok_or_else(|| {
        fail(
            block,
            name,
            Reason::Invalid(format!(
                "'{key}' on line {} must be a list like [a, b]",
                field.line
            )),
        )
    })?;
    for item in &items {
        pattern(item).map_err(|problem| {
            fail(
                block,
                name,
                Reason::Invalid(format!("'{key}' on line {}: {problem}", field.line)),
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
fn pattern(item: &str) -> Result<(), String> {
    if item.contains(char::is_whitespace) {
        return Err(format!("'{item}' contains whitespace"));
    }
    let stars = item.matches('*').count();
    if stars > 1 || (stars == 1 && !item.ends_with('*')) {
        return Err(format!(
            "'{item}': '*' is allowed only as the last character"
        ));
    }
    Ok(())
}

fn severity_of(block: &RawBlock, name: Option<&str>, field: &Field) -> Result<Severity, RuleError> {
    Severity::parse(&field.value).ok_or_else(|| {
        fail(
            block,
            name,
            Reason::Invalid(format!(
                "severity '{}' on line {} is not one of INFO, MINOR, MAJOR, CRITICAL, BLOCKER",
                field.value, field.line
            )),
        )
    })
}

/// `[id=SEVERITY, ...]` severity overrides of a group.
fn overrides_of(
    block: &RawBlock,
    name: Option<&str>,
    fields: &Fields,
) -> Result<Vec<(String, Severity)>, RuleError> {
    let Some(field) = fields.get("overrides") else {
        return Ok(Vec::new());
    };
    let invalid = |detail: String| fail(block, name, Reason::Invalid(detail));
    let items = list(&field.value).ok_or_else(|| {
        invalid(format!(
            "'overrides' on line {} must be a list like [id=MAJOR]",
            field.line
        ))
    })?;
    items
        .iter()
        .map(|item| {
            let (id, severity_text) = item
                .split_once('=')
                .ok_or_else(|| invalid(format!("override '{item}' is not 'id=SEVERITY'")))?;
            pattern(id.trim()).map_err(&invalid)?;
            let severity = Severity::parse(severity_text.trim())
                .ok_or_else(|| invalid(format!("override '{item}' has an unknown severity")))?;
            Ok((id.trim().to_string(), severity))
        })
        .collect()
}
