//! The markdown vault snapshot codec (ADR-0406): the durable snapshot
//! **is** a folder of markdown files.
//!
//! ## Layout
//!
//! This module is the family's wiring and shared-policy home: the
//! open-time guards and the identity policies both pipelines consult
//! (the case fold, the vault-key join, the key-property inference, the
//! raw node document, the relationship identity). The pipelines live
//! in child modules — [`load`] (parse the walked vault into the
//! graph), [`checkpoint`] (render the graph back into the vault),
//! [`journal`], [`manifest`], [`vault`], and [`views`].
//!
//! ## Load
//!
//! Read `.cypherlite/manifest.json` (or bootstrap when absent), recover
//! a journaled checkpoint if one is pending, walk the vault
//! (`<Label>/<key>.md`; reserved view files skipped), infer or consult
//! the learned schema (per-label key properties, the rel-type
//! vocabulary), build the graph — node ids in deterministic path order —
//! materialize stub nodes for unresolved wikilink targets (Obsidian's
//! click-to-create semantics), restore sidecar embeddings, and hand the
//! WAL the checkpoint LSN exactly like the other codecs.
//!
//! ## Checkpoint (copy-on-write journaled)
//!
//! Render every node file plus the derived `index.md`/`log.md` views,
//! guard against external writes (stat-first: length + crc), copy each
//! to-be-overwritten file's current content into
//! `.cypherlite/checkpoint-journal/` with an intent manifest, then write
//! only changed files atomically, delete files whose node is gone,
//! prune empty label directories, publish the sidecar manifest last,
//! and remove the journal. The pipeline is phased — the learned schema,
//! the node-file plan, the link/id plan, and the render are each one
//! named pass in [`checkpoint`]; [`checkpoint_at`] is the wiring.
//!
//! ## Classification
//!
//! A frontmatter key is a link list iff it names a relationship type in
//! the vocabulary (the learned schema's set, extended by this open's
//! bare-wikilink evidence — drift regenerates the schema, ADR-0406 §2).
//! Shape validates entries; it never classifies them.

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::{Error, Result};
use cypherlite_markdown as md;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::snapshot::DeferredOpenAction;
use crate::StoreTarget;

pub(crate) use checkpoint::checkpoint_at;
pub(crate) use load::load_with_checkpoint_lsn;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use manifest::{probe_path, refuse_reset};
pub(crate) use manifest::{read_manifest, VaultManifest, ENGINE_DIR, MANIFEST, WAL_DIR};
use vault::*;

/// The markdown vault's body-property configuration (ADR-0460): an
/// ordered default list plus per-label overrides, the one carrier the
/// open-time flags, `OpenOptions`, and the sidecar manifest's `bodies`
/// field all share. Store configuration, not learned schema — an
/// explicit config at open replaces the recorded one wholesale, and
/// every checkpoint records the resolved configuration back so the
/// vault is self-describing (later opens need no flag).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct MarkdownBodyProperties {
    /// The ordered default list for labels without their own entry.
    /// Empty is the wire form of the unconfigured legacy vault — the
    /// single-property `["body"]` list that
    /// [`materialize_legacy_default`](Self::materialize_legacy_default)
    /// supplies at the construction and resolution boundaries, never
    /// [`resolve`](Self::resolve) (the plain lookup).
    #[serde(default)]
    pub default: Vec<String>,
    /// Label → its ordered body-property list, replacing the default
    /// for that label.
    #[serde(default)]
    pub per_label: BTreeMap<String, Vec<String>>,
}

impl MarkdownBodyProperties {
    /// Parse the one configuration grammar both the CLI flag and
    /// `CYPHERLITE_MARKDOWN_BODY_PROPERTIES` use (ADR-0460):
    /// `;`-separated segments; a bare segment is one or more
    /// comma-separated property names accumulating as the default in
    /// order (`summary,notes`), a `Label=summary,notes` segment sets
    /// that label's list. Names may not be empty; every list passes the
    /// grammar crate's validation (reserved names, duplicates).
    pub fn parse(spec: &str) -> Result<Self> {
        let mut config = MarkdownBodyProperties::default();
        for segment in spec.split(';') {
            let segment = segment.trim();
            let names_error = || {
                Error::semantic(format!(
                    "invalid markdown body properties '{spec}': an entry names no property"
                ))
            };
            if let Some((label, names)) = segment.split_once('=') {
                let label = label.trim();
                if label.is_empty() {
                    return Err(names_error());
                }
                let list = parse_body_name_list(names).ok_or_else(names_error)?;
                // A `=` inside a name (`Book=summary=x`) would silently
                // configure a property the grammar can never express
                // back — the entry separator is reserved (ADR-0460).
                if list.iter().any(|name| name.contains('=')) {
                    return Err(Error::semantic(format!(
                        "invalid markdown body properties '{spec}': '=' is the entry \
                         separator and cannot appear in a property name"
                    )));
                }
                if config.per_label.insert(label.to_string(), list).is_some() {
                    return Err(Error::semantic(format!(
                        "invalid markdown body properties '{spec}': label '{label}' is named \
                         twice"
                    )));
                }
            } else {
                let list = parse_body_name_list(segment).ok_or_else(names_error)?;
                config.default.extend(list);
            }
        }
        // Every segment of the split either fails above or contributes
        // names, and the per-label-only form materializes its explicit
        // default — so a parsed configuration is always resolve-ready.
        config.materialize_legacy_default();
        config.validate()?;
        Ok(config)
    }

    /// The one validation every list in the configuration passes
    /// (the grammar crate's own — reserved names, duplicates, names the
    /// section grammar can round-trip), run at every entry: the spec
    /// parse above, the codec's sidecar resolution, and the
    /// [`StoreTarget`](crate::StoreTarget) builder that installs an
    /// open-time config. Single-crate surface: the engine constructs
    /// configurations through [`Self::parse`].
    pub(crate) fn validate(&self) -> Result<()> {
        md::validate_body_properties(&self.default)
            .map_err(|e| Error::semantic(format!("invalid markdown body properties: {e}")))?;
        for (label, list) in &self.per_label {
            // An empty per-label list is refused loudly: `resolve`
            // would return it and the grammar would read no body for
            // the label at all — the file's text silently dropped at
            // the next checkpoint. The empty *default* is the one
            // legitimate empty list (the unconfigured legacy form).
            if list.is_empty() {
                return Err(Error::semantic(format!(
                    "invalid markdown body properties for label '{label}': the list names no \
                     property"
                )));
            }
            md::validate_body_properties(list).map_err(|e| {
                Error::semantic(format!(
                    "invalid markdown body properties for label '{label}': {e}"
                ))
            })?;
        }
        Ok(())
    }

    /// The label's ordered body-property list (ADR-0460): its per-label
    /// entry, else the default — the plain lookup, materialized at the
    /// boundaries ([`Self::parse`] for constructed configurations,
    /// [`Self::materialize_legacy_default`] for the sidecar's wire
    /// form), never here.
    pub(crate) fn resolve(&self, label: &str) -> &[String] {
        self.per_label
            .get(label)
            .map(Vec::as_slice)
            .unwrap_or(&self.default)
    }

    /// The wire-minimal default: no default list and no per-label
    /// entries — the field a sidecar manifest omits.
    pub(crate) fn is_default(&self) -> bool {
        self.default.is_empty() && self.per_label.is_empty()
    }

    /// The legacy form's one materialization: an empty default list —
    /// the wire form of an unconfigured sidecar — becomes the explicit
    /// single-property `["body"]` list, so a configuration that passed
    /// a construction or resolution boundary is always resolve-ready.
    /// The two boundaries that call this are [`Self::parse`] (the spec
    /// grammar's output) and the codec's `resolve_bodies` (a sidecar's
    /// recorded form); no reader re-spells the rule.
    fn materialize_legacy_default(&mut self) {
        if self.default.is_empty() {
            self.default = vec![md::BODY_KEY.to_string()];
        }
    }
}

/// One segment's comma-separated, trimmed name list — the grammar's
/// name half, `None` when any entry is empty or the segment names
/// nothing (a `summary,,notes` typo fails closed, never silently
/// drops).
fn parse_body_name_list(names: &str) -> Option<Vec<String>> {
    names
        .split(',')
        .map(|name| {
            let name = name.trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// The recorded body-property configuration of an existing markdown
/// store (ADR-0460): the sidecar manifest's `bodies` field, the default
/// form when the store has none (or is not a markdown store). The
/// clone/publish path consults it so a markdown→markdown copy keeps the
/// source's layout when the caller passes no explicit configuration.
pub fn recorded_body_properties(target: &StoreTarget) -> Result<MarkdownBodyProperties> {
    Ok(read_manifest(target)?
        .map(|manifest| manifest.bodies)
        .unwrap_or_default())
}

/// The sidecar manifest's recorded body-property configuration — the
/// one mapping from a manifest to its `bodies`, wrapped by
/// [`recorded_body_properties`] (which reads it from disk) and called
/// directly by the checkpoint side, which already holds the manifest it
/// recovered.
pub(super) fn bodies_of(manifest: Option<&VaultManifest>) -> MarkdownBodyProperties {
    manifest
        .map(|manifest| &manifest.bodies)
        .cloned()
        .unwrap_or_default()
}

/// The configuration the sidecar records (ADR-0460): the explicit
/// open-time configuration verbatim, else the previously recorded form —
/// the **pre-materialization** form, so an unchanged vault's sidecar
/// compares equal (a pre-0460 sidecar stays untouched) and unconfigured
/// stores keep omitting the field. The pipelines parse and render with
/// the materialized view [`resolve_bodies`] returns.
pub(super) fn recorded_bodies(
    target: &StoreTarget,
    previous: &MarkdownBodyProperties,
) -> MarkdownBodyProperties {
    target
        .markdown_bodies()
        .cloned()
        .unwrap_or_else(|| previous.clone())
}

/// The one resolution of the body-property configuration both sides of
/// the codec consult (ADR-0460): the explicit open-time config when this
/// open carried one — replacing the sidecar's recorded one wholesale —
/// else the recorded one, else the default (ADR-0406's single
/// `body`). The resolved configuration is validated here, so a
/// hand-edited sidecar's `bodies` fails the open loudly instead of
/// silently dropping bodies at the next checkpoint.
pub(super) fn resolve_bodies(
    target: &StoreTarget,
    recorded: &MarkdownBodyProperties,
) -> Result<MarkdownBodyProperties> {
    // The selection is [`recorded_bodies`]'s one body; this function is
    // its documented materializing wrapper.
    let mut bodies = recorded_bodies(target, recorded);
    bodies.validate()?;
    // The wire form's empty default (an old or absent sidecar) is the
    // legacy single-property `body` — materialized once, here, so every
    // reader below is a plain lookup.
    bodies.materialize_legacy_default();
    Ok(bodies)
}

/// The reserved implicit label root-level `.md` files load under
/// (ADR-0406 §2). The engine never writes a node to the vault root.
const VAULT_LABEL: &str = "Vault";

/// The fallback key property when nothing else identifies a node
/// (ADR-0406 §2): the checkpoint-side inference's floor. The load-side
/// inference and the stub materializer consult the same fact.
const DEFAULT_KEY_PROPERTY: &str = "name";

/// The one vault-key join — `Label/name` (ADR-0406 §2). Every site that
/// constructs a node's vault key (the checkpoint plan, the load-side
/// canonicalization, the root-file prefixing) consults this, never a
/// bare `format!`.
fn vault_key(label: &str, name: &str) -> String {
    format!("{label}/{name}")
}

/// The one key-property inference core for labels the learned schema
/// does not know (ADR-0406 §2): `name`, then `title`, then the
/// alphabetically first property every entry carries as a string — so
/// a new label arriving via a query and one arriving via a new vault
/// file get the same deterministic key. The two policies over that
/// shared candidate iteration are the named wrappers below, so a
/// caller's signature states which policy it selects — never a
/// nullable mode switch. None fits → [`DEFAULT_KEY_PROPERTY`] (the
/// plan loop's authored missing-key error fires for labels with no
/// usable key).
fn infer_key(
    properties: &[&BTreeMap<String, Json>],
    value_fits: impl Fn(&str, usize) -> bool,
) -> String {
    let mut candidates: Vec<&str> = vec![DEFAULT_KEY_PROPERTY, "title"];
    let mut declared: BTreeSet<&str> = BTreeSet::new();
    for props in properties {
        declared.extend(props.keys().map(String::as_str));
    }
    candidates.extend(declared);
    for candidate in candidates {
        let fits = properties.iter().enumerate().all(|(index, props)| {
            props
                .get(candidate)
                .and_then(Json::as_str)
                .is_some_and(|value| value_fits(value, index))
        });
        if fits {
            return candidate.to_string();
        }
    }
    DEFAULT_KEY_PROPERTY.to_string()
}

/// The load side's policy over the shared core (ADR-0406 §2): the file
/// is the identity, so a candidate's value must sanitize to its own
/// file name — `file_names` is one parallel slice, matched once per
/// call, never per entry.
fn infer_key_for_load(properties: &[&BTreeMap<String, Json>], file_names: &[&str]) -> String {
    infer_key(properties, |value, index| {
        md::sanitize_name_path(value).is_ok_and(|sanitized| sanitized == file_names[index])
    })
}

/// The checkpoint side's policy over the shared core: any string-valued
/// property fits.
fn infer_key_for_checkpoint(properties: &[&BTreeMap<String, Json>]) -> String {
    infer_key(properties, |_, _| true)
}

/// The raw-file tolerance's one construction (ADR-0406 §6, ADR-0460):
/// an unparseable-frontmatter file loads body-only — the label's
/// **first** body property holds `parse_body`'s normalization of the
/// raw text (nothing at all when the body is not representable, the
/// non-empty no-outer-whitespace rule). Load's raw arm builds the
/// node's document with it; the checkpoint's unmodified check derives
/// its expectation from the same construction, never a second
/// hand-spelled fingerprint of the shape.
fn raw_node_doc(label: &str, raw_body: &str, body_list: &[String]) -> Result<md::NodeDoc> {
    // The caller passes a list a boundary materialized — `parse` or
    // `resolve_bodies`, both of which leave the default non-empty and
    // validate every per-label list non-empty — so an empty list here
    // is a caller bug, never a silent default.
    let first = body_list
        .first()
        .expect("body lists from parse()/resolve_bodies() are never empty");
    Ok(md::NodeDoc {
        primary_label: label.to_string(),
        ..md::NodeDoc::default()
    }
    .with_body_property(first, md::parse_body(raw_body)?))
}

/// A relationship's canonical identity (ADR-0406 §5's stable-id
/// contract): source vault key, type, target vault key, and the sorted
/// properties JSON — identical on the load side and the checkpoint
/// side, so the persisted id map addresses the same relationship both
/// directions (parallel relationships of one triple differ by
/// properties).
fn rel_identity(
    source_key: &str,
    rel_type: &str,
    target_key: &str,
    properties: &BTreeMap<String, Json>,
) -> String {
    let props = serde_json::to_string(properties).expect("serializing a JSON value is infallible");
    format!("{source_key}\u{1}{rel_type}\u{1}{target_key}\u{1}{props}")
}

fn codec_error(message: impl Into<String>) -> Error {
    // The grammar crate owns the `markdown vault: ` prefix policy; this
    // codec reports through it instead of re-spelling the prefix.
    md::vault_error(message)
}

/// Whether the vault changed on disk since the last checkpoint — the
/// dirty-WAL open guard (ADR-0406 §5), through the one `vault_matches`
/// predicate. `false` when there is no manifest yet.
fn vault_changed_since_checkpoint(target: &StoreTarget) -> Result<bool> {
    let Some(manifest) = read_manifest(target)? else {
        return Ok(false);
    };
    Ok(!vault_matches_stat_first(&*target.vfs(), &manifest)?)
}

/// The markdown store's dirty-WAL open guard (ADR-0406 §5): with WAL
/// records past the checkpoint LSN, replaying deltas over a vault that
/// changed underneath is refused instead of corrupting the base. The
/// codec owns the policy; `open` dispatches per codec.
fn check_open_guard(target: &StoreTarget, wal_lsn: u64, checkpoint_lsn: u64) -> Result<()> {
    if wal_lsn > checkpoint_lsn && vault_changed_since_checkpoint(target)? {
        return Err(codec_error(
            "the markdown vault changed on disk while uncheckpointed writes were pending; \
             reopen without the edits or recover from a checkpointed state",
        ));
    }
    Ok(())
}

/// The markdown store's writable-open tail (ADR-0406 §5): the guard
/// above first — replaying deltas over an externally-edited vault is
/// refused — then, the base proven safe, the load's deferred
/// re-baseline publishes, so the edits the load picked up become the
/// guard's new expectation without ever bypassing the refusal. The
/// codec owns the policy; `open` dispatches per codec.
pub(crate) fn finish_open(
    target: &StoreTarget,
    wal_lsn: u64,
    checkpoint_lsn: u64,
    deferred: Option<DeferredOpenAction>,
) -> Result<()> {
    check_open_guard(target, wal_lsn, checkpoint_lsn)?;
    match deferred {
        Some(action) => action.run(target),
        None => Ok(()),
    }
}

pub(crate) fn snapshot_data_bytes(target: &StoreTarget) -> Result<u64> {
    // Summed from metadata alone — the JSON codec's twin sums without
    // reading store bytes, and so does the vault (the paths-only walk
    // never fetches content).
    let mut sum = 0u64;
    for walked in walk_paths(&*target.vfs())? {
        sum = sum.saturating_add(target.vfs.metadata(&walked.path)?.len);
    }
    if target.vfs.exists(MANIFEST)? {
        sum = sum.saturating_add(target.vfs.metadata(MANIFEST)?.len);
    }
    Ok(sum)
}

mod checkpoint;
mod journal;
mod load;
mod manifest;
mod vault;
mod views;

#[cfg(test)]
mod tests;
