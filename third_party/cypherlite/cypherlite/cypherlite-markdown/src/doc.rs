//! The vault node-file grammar (ADR-0406 §2): one markdown file per
//! node, frontmatter keys classified by the **schema**, not by shape.
//!
//! A frontmatter key under a non-reserved name is a relationship link
//! list iff it names a relationship type in the caller-supplied
//! vocabulary (the persisted catalog or the bootstrap's inferred set) —
//! the value's shape then *validates* the entries; every other key is a
//! property, whatever its value looks like. This is the LABS-311
//! no-shape-sniffing rule applied to the frontmatter itself: a
//! link-shaped property (`records: [{target: …}]` with `records` not a
//! rel type) stays a property.
//!
//! A link list entry is either a bare wikilink string (a prop-less
//! relationship) or a map whose `target` key holds the wikilink plus
//! the relationship's properties. Exact duplicate entries — same
//! target, same properties — deduplicate in both directions
//! (relationships carry set semantics: an edge is identified by
//! `(start, type, end, properties)`).
//!
//! Reserved-key rules, one home for the whole vocabulary:
//!
//! - `type` carries the primary label (the OKF-required key). A `type`
//!   value equal to the directory label is structural; any other value
//!   is a node property named `type` (Obsidian vaults use `type` as
//!   free metadata) and is *written back* demoted to `type_property`,
//!   so both survive the round trip.
//! - `labels` carries labels beyond the primary; a node property
//!   literally named `labels` always demotes to `labels_property`.
//! - A property with a reserved mirror name (`type_property`,
//!   `labels_property`) fails closed at render — the mirror would
//!   swallow it on reload. A property named `target` on a relationship
//!   entry fails closed at render for the same reason.
//! - The rel-type vocabulary must stay disjoint from the reserved
//!   names and the body properties: a relationship type spelled `type`,
//!   `labels`, one of their mirrors, or any of the label's body
//!   property names fails closed at parse and render — it would
//!   swallow the structural key and emit a file the next open
//!   misreads.
//! - The link-shaped key set must equal the declared vocabulary
//!   exactly: at render, a property key named like a relationship type
//!   and a link key absent from the vocabulary both fail closed — the
//!   next open classifies by the vocabulary alone, so any other key
//!   set would be misread.
//!
//! The body is handled by [`crate::body`] (ADR-0460): the label's
//! ordered body-property list — `["body"]` when nothing is configured,
//! ADR-0406's rule — decides which properties render as the file body.
//! The first present one renders bare, each subsequent one as a
//! `## <property>` section; a listed property that is absent,
//! non-string, or not representable (blank or outer whitespace) stays
//! a frontmatter property.

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::Result;
use serde_json::Value;

use crate::body::{parse_body_properties, render_body_properties, representable_as_body};
use crate::link::{parse_link, render_wikilink, LinkTarget};
use crate::vault_error;
use crate::yaml::{emit_yaml_entries, join_frontmatter, parse_yaml_value, split_frontmatter};

/// Frontmatter key carrying the primary label (OKF's required `type`).
pub(crate) const KEY_TYPE: &str = "type";
/// Demotion mirror for a node property named `type`.
pub(crate) const KEY_TYPE_PROPERTY: &str = "type_property";
/// Frontmatter key carrying labels beyond the primary.
pub(crate) const KEY_LABELS: &str = "labels";
/// Demotion mirror for a node property named `labels`.
pub(crate) const KEY_LABELS_PROPERTY: &str = "labels_property";
/// The map key holding a link entry's wikilink target.
pub const KEY_TARGET: &str = "target";
/// The reserved per-label index-view filename (ADR-0406 §2).
pub const INDEX_VIEW: &str = "index.md";
/// The reserved changelog-view filename (ADR-0406 §2).
pub const LOG_VIEW: &str = "log.md";
/// The reserved generated-view filenames at any hierarchy level
/// (ADR-0406 §2): never concept nodes. The one table a third reserved
/// view edits alone.
pub(crate) const RESERVED_VIEWS: [&str; 2] = [INDEX_VIEW, LOG_VIEW];

/// Is this file name (with its `.md` extension, at any hierarchy level)
/// one of the reserved generated views? Judged through the codec's one
/// path-identity relation — the case fold (ADR-0406 §2) — so a case
/// variant of a view name (`Index.md`, `LOG.MD`) is reserved exactly
/// like the canonical spelling: on a case-insensitive filesystem the
/// generated view and the variant are the same file, and the vault walk
/// (which consults this predicate) would otherwise load a node the next
/// checkpoint's view render overwrites. The one predicate every
/// reserved-view consumer consults — the storage codec's vault walk,
/// checkpoint planner, and node-name refusal.
pub fn is_reserved_view_file_name(name: &str) -> bool {
    RESERVED_VIEWS
        .iter()
        .any(|view| view.eq_ignore_ascii_case(name))
}

/// One relationship link entry: a target plus optional properties
/// (empty properties = the bare wikilink form).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkEntry {
    pub target: LinkTarget,
    pub properties: BTreeMap<String, Value>,
}

impl LinkEntry {
    /// The deterministic sort key: target, then the properties'
    /// canonical JSON (`serde_json::Map` is key-sorted, so the string
    /// ordering is stable) — the one spelling of entry identity that
    /// the sort and the duplicate removal below both consult.
    fn sort_key(entry: &LinkEntry) -> (&LinkTarget, String) {
        (
            &entry.target,
            serde_json::to_string(&entry.properties)
                .expect("serializing a JSON value is infallible"),
        )
    }

    /// The canonical link-list form: sorted by the `sort_key` entry
    /// identity above, with exact duplicates removed (set semantics,
    /// ADR-0406 §2) — the
    /// one ordering every writer of a link list consults: the parser,
    /// the renderer, and the storage codec's checkpoint planner.
    pub fn sorted_dedup(mut entries: Vec<LinkEntry>) -> Vec<LinkEntry> {
        entries.sort_by(|a, b| LinkEntry::sort_key(a).cmp(&LinkEntry::sort_key(b)));
        entries.dedup_by(|a, b| LinkEntry::sort_key(a) == LinkEntry::sort_key(b));
        entries
    }
}

/// A node as the vault grammar sees it. Properties use the engine's
/// canonical tagged JSON values; the label's body-property list decides
/// which of them render as the file body (ADR-0460).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeDoc {
    pub primary_label: String,
    /// Labels beyond the primary, sorted, deduplicated.
    pub extra_labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
    /// Outgoing links by relationship type — the one direction the
    /// vault stores; incoming edges are derived, never stored.
    pub links: BTreeMap<String, Vec<LinkEntry>>,
}

impl NodeDoc {
    /// The node's body-property values, in `props` order: each listed
    /// property carried as representable text (non-empty, no outer
    /// whitespace) — the one canonical body rule every reader of a
    /// [`NodeDoc`] consults (ADR-0460).
    fn body_values<'s, 'p>(&'s self, props: &'p [String]) -> Vec<(&'p str, &'s str)> {
        props
            .iter()
            .filter_map(|name| match self.properties.get(name) {
                Some(Value::String(text)) if representable_as_body(text) => {
                    Some((name.as_str(), text.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    /// Set one body property (a builder for the body-only load path —
    /// the raw-file tolerance names the label's first body property).
    /// [`body_property`]'s rule is the contract: text that cannot be
    /// the body (blank, outer whitespace) inserts no property — the
    /// tolerance's own semantics for a raw file with no usable body,
    /// not a silent drop a caller could miss.
    pub fn with_body_property(mut self, name: &str, body: String) -> Self {
        if let Some(value) = body_property(body) {
            self.properties.insert(name.to_string(), value);
        }
        self
    }
}

/// The body property's value: text that can be the bare body (non-empty,
/// no outer whitespace) is the file body; any other text is no property
/// at all — the construction half of the body rule, shared by the parser
/// and the builder.
fn body_property(body: String) -> Option<Value> {
    representable_as_body(&body).then_some(Value::String(body))
}

/// The reserved frontmatter vocabulary, classified once: the parser's
/// key dispatch and the renderer's reserved-name rules both consult this
/// one table — `type`/`labels` carry structure and their `_property`
/// mirrors are the demotion targets.
enum ReservedKey {
    Type,
    TypeProperty,
    Labels,
    LabelsProperty,
}

fn reserved_key(key: &str) -> Option<ReservedKey> {
    match key {
        KEY_TYPE => Some(ReservedKey::Type),
        KEY_TYPE_PROPERTY => Some(ReservedKey::TypeProperty),
        KEY_LABELS => Some(ReservedKey::Labels),
        KEY_LABELS_PROPERTY => Some(ReservedKey::LabelsProperty),
        _ => None,
    }
}

/// The reserved-key, rel-type, and body-property vocabularies must stay
/// disjoint: a relationship type spelled like a reserved frontmatter key
/// or a body property would be classified as links at parse (swallowing
/// the structural key or the section) and double-emitted at render — a
/// file the next open misreads. One guard wherever the vocabularies
/// enter: both entry points below consult it.
fn reject_vocabulary_collisions(rel_types: &BTreeSet<String>, body_props: &[String]) -> Result<()> {
    for name in rel_types {
        if reserved_key(name).is_some() {
            return Err(vault_error(format!(
                "relationship type '{name}' is reserved by the markdown vault frontmatter"
            )));
        }
        if body_props.iter().any(|prop| prop == name) {
            return Err(vault_error(format!(
                "relationship type '{name}' is a body property of this label; the vault \
                 would misread its section heading as links"
            )));
        }
    }
    Ok(())
}

/// The grammar's one entry guard, both halves together: the
/// body-property list's own validation and the vocabulary disjointness.
/// Each grammar entry point (parse, render) calls this — never one
/// validator without the other.
fn check_body_grammar(rel_types: &BTreeSet<String>, body_props: &[String]) -> Result<()> {
    validate_body_properties(body_props)?;
    reject_vocabulary_collisions(rel_types, body_props)
}

/// The canonical string-set form: sorted and deduplicated — the vault's
/// set semantics for label lists and every other string collection.
fn sorted_dedup_strings(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = items.into_iter().collect();
    set.into_iter().collect()
}

/// The one validation a body-property list passes wherever it enters —
/// the storage codec's config parse and both grammar entries below:
/// names are non-empty, none is a reserved frontmatter key, and no name
/// appears twice (a duplicate would render two sections the parse
/// cannot tell apart).
pub fn validate_body_properties(props: &[String]) -> Result<()> {
    // Duplicates are judged through the case fold (body's `fold_case`,
    // the same fold marker recognition consults): `Notes` and `notes`
    // in one list are the same section twice.
    let mut seen = BTreeSet::new();
    for name in props {
        if name.is_empty() {
            return Err(vault_error("a body property name is empty"));
        }
        // A name the section grammar can never round-trip is refused
        // here: the marker match is the heading's **first word**, so a
        // name containing any whitespace (interior included) renders a
        // `## ` boundary line the next open cannot recognize — the
        // property's text would silently migrate into the preceding
        // segment, the exact re-attribution ADR-0460 rules out. Names
        // are single words, and outer whitespace never survives the
        // flag grammar's trim.
        if name.chars().any(char::is_whitespace) {
            return Err(vault_error(format!(
                "body property name {name:?} is not a single word"
            )));
        }
        if !seen.insert(crate::body::fold_case(name)) {
            return Err(vault_error(format!(
                "body property '{name}' appears twice in the list (case-insensitively)"
            )));
        }
        if reserved_key(name).is_some() {
            return Err(vault_error(format!(
                "body property '{name}' is reserved by the markdown vault frontmatter"
            )));
        }
    }
    Ok(())
}

/// Parse one node file. `label` is the label directory; `file` names
/// the file in error messages; `rel_types` is the relationship-type
/// vocabulary the classification consults (ADR-0406 §4); `body_props`
/// is the label's ordered body-property list (ADR-0460).
pub fn parse_node_doc(
    label: &str,
    file: &str,
    text: &str,
    rel_types: &BTreeSet<String>,
    body_props: &[String],
) -> Result<NodeDoc> {
    let (frontmatter, body_text) = split_frontmatter(text);
    let root = frontmatter.map(parse_yaml_value).transpose()?;
    parse_node_doc_root(label, file, root, body_text, rel_types, body_props)
}

/// The already-parsed form of [`parse_node_doc`]: the caller that has
/// classified the frontmatter tri-state (absent, parsed,
/// present-but-unparseable) hands the parsed root over, so a file's
/// yaml is parsed exactly once per open — the classification pass and
/// the node parse share the one parse. `root: None` is absent
/// frontmatter.
pub fn parse_node_doc_root(
    label: &str,
    file: &str,
    root: Option<Value>,
    body_text: &str,
    rel_types: &BTreeSet<String>,
    body_props: &[String],
) -> Result<NodeDoc> {
    check_body_grammar(rel_types, body_props)?;
    // An empty block (`---\n---`) is absent frontmatter in disguise:
    // `parse_yaml_value`'s tail normalizes empty and whitespace-only
    // input to the empty mapping — the one home of that rule. Every
    // other non-mapping root, an explicit `~`/`null` document included,
    // fails closed here.
    let entries = match root.unwrap_or_else(|| Value::Object(Default::default())) {
        Value::Object(map) => map,
        other => {
            return Err(vault_error(format!(
                "{file}: frontmatter must be a mapping, found {other}"
            )))
        }
    };

    let mut doc = NodeDoc {
        primary_label: label.to_string(),
        ..NodeDoc::default()
    };
    for (key, value) in entries {
        if rel_types.contains(&key) {
            let entries = parse_link_list(&key, &value, file)?;
            doc.links.insert(key, entries);
            continue;
        }
        match reserved_key(&key) {
            Some(ReservedKey::Type) => {
                if value != Value::String(label.to_string()) {
                    // A non-structural `type` (Obsidian metadata) stays a
                    // node property; the directory names the label.
                    insert_property(&mut doc, KEY_TYPE, value, file)?;
                }
            }
            Some(ReservedKey::TypeProperty) => {
                insert_property(&mut doc, KEY_TYPE, value, file)?;
            }
            Some(ReservedKey::Labels) => {
                doc.extra_labels = parse_extra_labels(&value, label, file)?
            }
            Some(ReservedKey::LabelsProperty) => {
                insert_property(&mut doc, KEY_LABELS, value, file)?;
            }
            // A body-property key in frontmatter is a plain property
            // here; the file body below the fence is inserted after the
            // loop, and a collision between the two fails closed there.
            None => {
                insert_property(&mut doc, &key, value, file)?;
            }
        }
    }

    // The body properties (ADR-0460): the preamble is the first list
    // entry's segment, each `## <name>`-headed section a later entry's
    // — a frontmatter key of the same name collides and fails closed
    // in insert_property below.
    for (name, text) in parse_body_properties(body_text, body_props, file)? {
        insert_property(&mut doc, &name, Value::String(text), file)?;
    }
    Ok(doc)
}

/// Insert a property, failing closed when the same name arrives twice
/// (frontmatter and body colliding, or a mirror-key collision) instead
/// of letting one win silently.
fn insert_property(doc: &mut NodeDoc, key: &str, value: Value, file: &str) -> Result<()> {
    if doc.properties.contains_key(key) {
        return Err(vault_error(format!(
            "{file}: property '{key}' arrives twice (frontmatter and body, or a mirror-key collision)"
        )));
    }
    doc.properties.insert(key.to_string(), value);
    Ok(())
}

fn parse_extra_labels(value: &Value, primary: &str, file: &str) -> Result<Vec<String>> {
    // Both halves of the shape check — the value is a list, and each
    // entry is a string — share the one message.
    let labels_shape_error = || vault_error(format!("{file}: labels must be a list of strings"));
    let list = value.as_array().ok_or_else(labels_shape_error)?;
    let mut labels: Vec<String> = Vec::new();
    for entry in list {
        let label = entry.as_str().ok_or_else(labels_shape_error)?;
        if label.is_empty() {
            return Err(vault_error(format!(
                "{file}: the labels list holds an empty label entry"
            )));
        }
        if label != primary {
            labels.push(label.to_string());
        }
    }
    Ok(sorted_dedup_strings(labels))
}

/// Validate and parse one relationship-type key's entry list (shape is
/// a validation of the entry, never the discriminator — the key was
/// already classified by the vocabulary). Exact duplicates deduplicate
/// (set semantics).
fn parse_link_list(key: &str, value: &Value, file: &str) -> Result<Vec<LinkEntry>> {
    let items = value
        .as_array()
        .ok_or_else(|| vault_error(format!("{file}: relationship key '{key}' must be a list")))?;
    let mut entries: Vec<LinkEntry> = Vec::new();
    for item in items {
        let entry = match item {
            Value::String(raw) => LinkEntry {
                target: parse_link(raw).ok_or_else(|| {
                    vault_error(format!(
                        "{file}: relationship key '{key}' has a malformed target '{raw}'"
                    ))
                })?,
                properties: BTreeMap::new(),
            },
            Value::Object(map) => {
                let target = map
                    .get(KEY_TARGET)
                    .and_then(Value::as_str)
                    .and_then(parse_link)
                    .ok_or_else(|| {
                        vault_error(format!(
                            "{file}: relationship key '{key}' has a map entry without a \
                             parseable '{KEY_TARGET}' wikilink"
                        ))
                    })?;
                let mut properties = BTreeMap::new();
                for (name, value) in map {
                    if name == KEY_TARGET {
                        continue;
                    }
                    properties.insert(name.clone(), value.clone());
                }
                LinkEntry { target, properties }
            }
            other => {
                return Err(vault_error(format!(
                    "{file}: relationship key '{key}' has a non-link entry {other}"
                )))
            }
        };
        entries.push(entry);
    }
    Ok(LinkEntry::sorted_dedup(entries))
}

/// Render one node file (deterministic byte output, ADR-0406 §2).
///
/// Frontmatter key order: `type`, `type_property` (demotion),
/// `labels`/`labels_property`, remaining properties sorted, then
/// relationship keys sorted with sorted, deduplicated entry lists — a
/// bare quoted wikilink for a prop-less relationship, a map with
/// `target` plus properties for the rest. The body properties render as
/// the file body (ADR-0460): the first present one bare, each
/// subsequent one as a `## <property>` section; a listed property that
/// is absent, non-string, or not representable stays in frontmatter.
///
/// Fails closed on a property key that collides with a relationship
/// type and on a link key absent from `rel_types` — either way the
/// emitted link-shaped key set would not equal the declared vocabulary,
/// and the next open would misclassify the key.
pub fn render_node_doc(
    doc: &NodeDoc,
    rel_types: &BTreeSet<String>,
    body_props: &[String],
) -> Result<String> {
    check_body_grammar(rel_types, body_props)?;
    let mut entries: Vec<(String, Value)> = Vec::new();
    entries.push((
        KEY_TYPE.to_string(),
        Value::String(doc.primary_label.clone()),
    ));

    push_mirror(&mut entries, doc, KEY_TYPE, KEY_TYPE_PROPERTY)?;
    push_mirror(&mut entries, doc, KEY_LABELS, KEY_LABELS_PROPERTY)?;
    if !doc.extra_labels.is_empty() {
        entries.push((
            KEY_LABELS.to_string(),
            Value::Array(
                doc.extra_labels
                    .iter()
                    .map(|label| Value::String(label.clone()))
                    .collect(),
            ),
        ));
    }

    let body = doc.body_values(body_props);
    // The rendered body names, one set built once — the property loop's
    // membership question is a lookup, not a scan.
    let body_names: BTreeSet<&str> = body.iter().map(|(name, _)| *name).collect();

    for (key, value) in &doc.properties {
        if rel_types.contains(key) {
            return Err(vault_error(format!(
                "property '{key}' collides with a relationship type of the same name; \
                 the vault would misread it as links"
            )));
        }
        match reserved_key(key) {
            // `type` and `labels` were emitted above from their
            // properties; a bare `_property` mirror name is reserved.
            Some(ReservedKey::Type) | Some(ReservedKey::Labels) => continue,
            Some(ReservedKey::TypeProperty) | Some(ReservedKey::LabelsProperty) => {
                return Err(vault_error(format!(
                    "property '{key}' is reserved by the markdown vault frontmatter"
                )));
            }
            None => {}
        }
        // A listed body property with representable text renders as the
        // body (bare or sectioned) below; a non-string, blank, or
        // outer-whitespace one stays in frontmatter like any property.
        if body_names.contains(key.as_str()) {
            continue;
        }
        entries.push((key.clone(), value.clone()));
    }

    for (rel_type, link_entries) in &doc.links {
        // The dual of the property-collision refusal above: parse
        // classifies a key as a link list iff it names a vocabulary
        // rel type, so a link key outside the vocabulary would emit a
        // link-shaped key the next open misreads as a property.
        if !rel_types.contains(rel_type) {
            return Err(vault_error(format!(
                "link key '{rel_type}' is not a relationship type in the vocabulary; \
                 the vault would misread it as a property"
            )));
        }
        let sorted = LinkEntry::sorted_dedup(link_entries.clone());
        // `sorted` is owned: consuming it removes a per-relationship
        // property-map clone from every checkpoint render.
        let items = sorted
            .into_iter()
            .map(|mut entry| -> Result<Value> {
                if entry.properties.contains_key(KEY_TARGET) {
                    return Err(vault_error(format!(
                        "relationship property '{KEY_TARGET}' collides with the link entry's \
                         target key"
                    )));
                }
                let wikilink = render_wikilink(&entry.target)?;
                if entry.properties.is_empty() {
                    Ok(Value::String(wikilink))
                } else {
                    entry
                        .properties
                        .insert(KEY_TARGET.to_string(), Value::String(wikilink));
                    Ok(Value::Object(entry.properties.into_iter().collect()))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        entries.push((rel_type.clone(), Value::Array(items)));
    }

    let yaml = emit_yaml_entries(&entries);
    let body_text = render_body_properties(body_props, &body)?;
    Ok(join_frontmatter(&yaml, &body_text))
}

/// Emit a source property under its demotion mirror key, refusing the
/// double carry (both the source property and its mirror present) —
/// the one spelling of the mirror-emission policy both pairs consult.
fn push_mirror(
    entries: &mut Vec<(String, Value)>,
    doc: &NodeDoc,
    source: &str,
    mirror: &str,
) -> Result<()> {
    if let Some(value) = doc.properties.get(source) {
        reject_mirror(source, mirror, doc)?;
        entries.push((mirror.to_string(), value.clone()));
    }
    Ok(())
}

fn reject_mirror(source: &str, mirror: &str, doc: &NodeDoc) -> Result<()> {
    if doc.properties.contains_key(mirror) {
        return Err(vault_error(format!(
            "properties '{source}' and '{mirror}' both exist; the '{mirror}' mirror key \
             cannot carry both"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
