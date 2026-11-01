//! The load path's identity resolution (ADR-0406 §4/§5): the stable-id
//! restoration from the sidecar manifest, and the link pass that
//! resolves targets — materializing stub nodes — while it restores or
//! assigns relationship ids.
//!
//! Extracted from `load.rs` (the no-1k-line rule, LABS-311), mirroring
//! how `load/tests.rs` sits beside it: the orchestrator keeps the
//! frontmatter classification, the parse/canonicalize passes, the
//! embeddings, and the expectation recording; this module owns the two
//! halves of the stable-id contract (nodes and relationships) and the
//! target resolution between them.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, Node, Relationship, Result, Value};
use cypherlite_executor::Graph;
use cypherlite_markdown as md;

use super::super::super::convert::props_from_cypher_json;
use super::super::manifest::VaultManifest;
use super::super::vault::file_name;
use super::super::{codec_error, rel_identity, vault_key, DEFAULT_KEY_PROPERTY};
use super::ParsedFile;
use cypherlite_markdown::fold_case;

/// The stable-id assignment's own state: every canonical key's id, the
/// next fresh id, and the highest id this load ever hands out or
/// restores. [`restore_ids`] reconciles the two counters before
/// returning, so `next_fresh_id` is the one "next free id" fact every
/// consumer reads.
pub(super) struct IdAssignment {
    pub(super) ids_by_key: BTreeMap<String, i64>,
    pub(super) next_fresh_id: i64,
    pub(super) max_assigned_id: i64,
}

/// The rel side's assignment state, mirroring [`IdAssignment`]: the
/// assigned identity map (keyed by the canonical relationship
/// identity) and the allocator's final next-free value.
pub(super) struct RelAssignment {
    pub(super) ids: BTreeMap<String, i64>,
    pub(super) next_rel_id: i64,
}

/// Restore the stable node ids from the sidecar
/// manifest's map so id-based WAL replay after a checkpoint addresses
/// the same nodes the writes named; a first (bootstrap) load assigns
/// them in deterministic path order.
pub(super) fn restore_ids(parsed: &[ParsedFile], manifest: &VaultManifest) -> Result<IdAssignment> {
    let mut ids = IdAssignment {
        ids_by_key: BTreeMap::new(),
        next_fresh_id: manifest.next_node_id.max(1),
        max_assigned_id: 0,
    };
    for file in parsed {
        let id = match manifest.node_ids.get(&file.key) {
            // The manifest invariant (ADR-0406 §5) keeps the seed above
            // every restored id — a sidecar that violates it would hand
            // a fresh file an id a restored node already owns (the
            // later row silently replacing it), so it fails closed like
            // every other corrupt-sidecar shape instead.
            Some(id) if *id >= ids.next_fresh_id => {
                return Err(codec_error(format!(
                    "sidecar node id {id} for '{}' is not below the recorded next id {}",
                    file.key, ids.next_fresh_id
                )))
            }
            Some(id) => *id,
            None => {
                let id = ids.next_fresh_id;
                ids.next_fresh_id += 1;
                id
            }
        };
        ids.max_assigned_id = ids.max_assigned_id.max(id);
        ids.ids_by_key.insert(file.key.clone(), id);
    }
    // One spelling of "the allocator's final value is the next free
    // id": the manifest invariant (ADR-0406 §5) keeps the seed above
    // every restored id, and this clamp is the one place that
    // reconciles a sparse restored map which beat it — the stub
    // materializer's every assignment then threads it upward. The
    // graph's allocator and the recorded expectation both read this
    // one field, never a second derivation.
    ids.next_fresh_id = ids.next_fresh_id.max(ids.max_assigned_id + 1);
    Ok(ids)
}

/// Secondary label → primary label mapping (ADR-0406 §2): a target
/// naming a secondary label resolves to that node's primary path.
fn primary_label_map(parsed: &[ParsedFile]) -> BTreeMap<String, String> {
    let mut primary_of: BTreeMap<String, String> = BTreeMap::new();
    for file in parsed {
        for label in std::iter::once(&file.label).chain(&file.doc.extra_labels) {
            primary_of
                .entry(fold_case(label))
                .or_insert_with(|| file.label.clone());
        }
    }
    primary_of
}

/// Resolve a full-form link target (ADR-0406 §4): the named label
/// substitutes to the target's primary label (secondary labels map to
/// primaries), and a not-yet-materialized target is materialized as a
/// stub right here — a real node with the link's label and key value,
/// ready to be filled in (Obsidian's click-to-create).
fn resolve_full_target(
    graph: &Graph,
    keys: &BTreeMap<String, String>,
    ids: &mut IdAssignment,
    primary_of: &BTreeMap<String, String>,
    label: &str,
    key: &str,
) -> Result<(String, i64)> {
    let primary = primary_of
        .get(&fold_case(label))
        .cloned()
        .unwrap_or_else(|| label.to_string());
    let target_key = vault_key(&primary, key);
    let end_id = match ids.ids_by_key.get(&target_key) {
        Some(&id) => id,
        None => {
            let key_name = keys
                .get(&primary)
                .cloned()
                .unwrap_or_else(|| DEFAULT_KEY_PROPERTY.to_string());
            let id = ids.next_fresh_id;
            ids.next_fresh_id += 1;
            ids.max_assigned_id = ids.max_assigned_id.max(id);
            ids.ids_by_key.insert(target_key.clone(), id);
            let mut stub_props = BTreeMap::new();
            stub_props.insert(key_name, Value::String(key.into()));
            graph
                .load_checkpoint_node(Node {
                    id,
                    labels: Arc::new(vec![primary.clone()]),
                    properties: cypherlite_core::PropertyMap::share(stub_props),
                })
                .map_err(|e| codec_error(format!("materializing stub {target_key}: {e}")))?;
            id
        }
    };
    Ok((target_key, end_id))
}

/// Resolve a short-form link target (ADR-0406 §4): the unique
/// case-folded name match over the loaded keys — by construction
/// already a key in `ids.ids_by_key`. No match, or several, fails
/// closed with the authored errors naming the file.
fn resolve_short_target(ids: &IdAssignment, path: &str, name: &str) -> Result<(String, i64)> {
    // The candidate filter is the walk's own basename extraction
    // (`file_name`), case-folded — the identity fold's basename half.
    let candidates: Vec<&String> = ids
        .ids_by_key
        .keys()
        .filter(|candidate| fold_case(file_name(candidate)) == fold_case(name))
        .collect();
    match candidates.as_slice() {
        [only] => Ok((
            (*only).clone(),
            *ids.ids_by_key
                .get(*only)
                .expect("the id pass keyed every parsed file"),
        )),
        [] => Err(codec_error(format!(
            "{path}: short-form link [[{name}]] has no candidate; use the \
             full form [[Label/<key>]] or create the note"
        ))),
        _ => {
            let listed = candidates
                .iter()
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(codec_error(format!(
                "{path}: short-form link [[{name}]] is ambiguous between {listed}; \
                 use the full form"
            )))
        }
    }
}

/// Load phase 4 (ADR-0406 §4): relationships from outgoing links, with
/// unresolved full-form targets materialized as stub nodes; stable
/// relationship ids restore from the manifest's identity map. Returns
/// the rel side's assignment state — the assigned identity map and
/// the allocator's final next-free value.
pub(super) fn resolve_links_and_stubs(
    graph: &Graph,
    parsed: &[ParsedFile],
    keys: &BTreeMap<String, String>,
    ids: &mut IdAssignment,
    rel_ids: &BTreeMap<String, i64>,
    mut rel_id: i64,
    value_encoding: u64,
) -> Result<RelAssignment> {
    // The seeded next free id, captured before the allocator hands any
    // fresh id out: the bound every restored rel id is judged against
    // (the manifest invariant's rel half, below).
    let seeded_next_rel_id = rel_id;
    let primary_of = primary_label_map(parsed);

    // Relationships from outgoing links; unresolved targets materialize
    // stub nodes (ADR-0406 §4). Set semantics: the crate already
    // deduplicated exact entries within each list.
    let mut assigned_rel_ids: BTreeMap<String, i64> = BTreeMap::new();
    for file in parsed {
        for (rel_type, entries) in &file.doc.links {
            for entry in entries {
                // Resolve the target, each arm yielding the resolved
                // vault key and its id.
                let (target_key, end_id) = match &entry.target {
                    md::LinkTarget::Full { label, key } => {
                        resolve_full_target(graph, keys, ids, &primary_of, label, key)?
                    }
                    md::LinkTarget::Short(name) => resolve_short_target(ids, &file.path, name)?,
                };
                // An empty property map decodes to the empty map (the
                // codec's converter iterates nothing) — no special
                // case, same as the checkpoint side.
                let properties = props_from_cypher_json(
                    &entry.properties.clone().into_iter().collect(),
                    value_encoding,
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "{path}: relationship {rel_type}: {e}",
                        path = file.path
                    ))
                })?;
                let identity = rel_identity(&file.key, rel_type, &target_key, &entry.properties);
                // The manifest invariant's rel half (ADR-0406 §5),
                // restore_ids' discipline: the seed stays above every
                // restored id — a sidecar that violates it would hand a
                // fresh relationship an id a restored one already owns,
                // so it fails closed with the same corrupt-sidecar
                // error shape instead of silently colliding.
                let id = match rel_ids.get(&identity).copied() {
                    Some(id) if id >= seeded_next_rel_id => {
                        return Err(codec_error(format!(
                            "sidecar relationship id {id} for '{identity}' is not below \
                             the recorded next id {seeded_next_rel_id}"
                        )))
                    }
                    Some(id) => id,
                    None => {
                        let id = rel_id;
                        rel_id += 1;
                        id
                    }
                };
                assigned_rel_ids.insert(identity, id);
                graph
                    .load_checkpoint_rel(Relationship {
                        id,
                        start: *ids
                            .ids_by_key
                            .get(&file.key)
                            .expect("the id pass keyed every parsed file"),
                        end: end_id,
                        rel_type: rel_type.clone(),
                        properties: cypherlite_core::PropertyMap::share(properties),
                    })
                    .map_err(|e| {
                        Error::storage(format!(
                            "{path}: relationship {rel_type}: {e}",
                            path = file.path
                        ))
                    })?;
            }
        }
    }
    Ok(RelAssignment {
        ids: assigned_rel_ids,
        next_rel_id: rel_id,
    })
}
