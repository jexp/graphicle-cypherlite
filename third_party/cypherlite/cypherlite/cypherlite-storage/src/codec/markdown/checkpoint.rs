//! The checkpoint pipeline (ADR-0406 §5): render the graph back into
//! the vault — the learned-schema regeneration, the node-file plan,
//! the link/id plan, the render, and the copy-on-write journaled
//! apply, each one named pass below; [`checkpoint_at`] is the wiring.
//!
//! Extracted from `mod.rs` (the no-1k-line rule, LABS-311).

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::{Error, Node, Relationship, Result, Value};
use cypherlite_executor::{CheckpointTheater, Graph, IndexKindSpec};
use cypherlite_markdown as md;
use serde_json::Value as Json;

use super::super::catalog;
use super::super::convert::props_to_cypher_json;
use super::super::model::SnapshotContents;
use super::super::SnapshotCodecKind;
use super::journal;
use super::manifest::{read_manifest, write_manifest, FileStat, LearnedSchema, VaultManifest};
use super::vault::{
    file_name, prune_empty_dirs, vault_matches, vault_stats, walk_vault, walk_view_paths,
};
use super::views;
use super::{
    codec_error, infer_key_for_checkpoint, raw_node_doc, rel_identity, vault_key,
    MarkdownBodyProperties, VAULT_LABEL,
};
use crate::vfs::{atomic_write, read_to_vec, Vfs};
use crate::StoreTarget;

/// The checkpoint's wall clock, read once by the orchestrator — the
/// derived views stay pure over plan + changes (the changelog's date
/// heading is this time; the graph's commit counter is not a clock). A
/// clock before the epoch is reported, never silently dated as
/// 1970-01-01.
fn checkpoint_millis() -> Result<u64> {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .map_err(|e| codec_error(format!("the system clock is before the epoch: {e}")))
}

/// The node's primary label: the alphabetically first label — the one
/// spelling consulted by the learned-key retention, the key inference,
/// and the plan loop (never insertion order, which a rewrite can
/// change between the three). `None` for a label-less node: the
/// pre-passes skip it; the plan loop is the one that fails it closed
/// with the authored error.
fn primary_label(node: &Node) -> Option<&str> {
    node.labels.iter().min().map(String::as_str)
}

/// The vector-indexed (label, property) pairs the checkpoint lifts out
/// of node properties and into the sidecar's embeddings map
/// (ADR-0406 §2/§3).
fn vector_indexed_properties(
    contents: &SnapshotContents,
    theater: &CheckpointTheater,
) -> BTreeSet<(String, String)> {
    contents
        .indexes
        .iter()
        .filter(|index| {
            theater
                .index_detail(&index.name)
                .is_some_and(|detail| detail.kind == IndexKindSpec::Vector)
        })
        .map(|index| (index.label.clone(), index.property.clone()))
        .collect()
}

/// The learned schema, regenerated from the census (ADR-0406 §2): known
/// keys stay (dropped for labels the graph no longer holds), the
/// rel-type vocabulary is the union of what was learned and what the
/// graph now holds, and labels the schema does not know (bootstrap,
/// drift) get the same deterministic key inference load uses — `name`,
/// then `title`, then the alphabetically first string property every
/// node of the label carries.
fn regenerate_learned(
    previous: Option<&VaultManifest>,
    contents: &SnapshotContents,
) -> Result<LearnedSchema> {
    let mut learned = previous.map(|m| m.learned.clone()).unwrap_or_default();
    for rel in &contents.relationships {
        learned.rel_types.insert(rel.rel_type.clone());
    }
    // One census: the by-label map of the live labeled nodes is both
    // the retention verdict and the inference's input — the converted
    // property maps are built only for labels the schema does not know,
    // never for the whole graph on the checkpoint hot path.
    let mut by_label: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    for node in &contents.nodes {
        if let Some(primary) = primary_label(node) {
            by_label.entry(primary).or_default().push(node);
        }
    }
    learned
        .keys
        .retain(|label, _| by_label.contains_key(label.as_str()));
    for (label, nodes) in &by_label {
        if learned.keys.contains_key(*label) {
            continue;
        }
        // The engine's property map, converted to the vault's Json
        // shape once for the inference (the render pass's conversion).
        let entries: Vec<BTreeMap<String, Json>> = nodes
            .iter()
            .map(|node| {
                Ok(props_to_cypher_json(&node.properties)?
                    .into_iter()
                    .collect())
            })
            .collect::<Result<Vec<_>>>()?;
        let entries: Vec<&BTreeMap<String, Json>> = entries.iter().collect();
        learned
            .keys
            .insert(label.to_string(), infer_key_for_checkpoint(&entries));
    }
    Ok(learned)
}

/// One node's file plan — the plan loop's decision carried as data:
/// where the node lands (path, directory, name), its identity (the
/// vault key `label/name`), and its labels. The render pass, the view
/// render, and the manifest's stable-id maps all read the plan; none
/// re-derives the decision (the root-vs-directory branch is made
/// exactly once, in the plan loop).
pub(super) struct NodePlan {
    /// The node's primary label, unsanitized — the doc's
    /// `primary_label` and the vector-property filter key.
    primary: String,
    /// The sanitized label directory; the reserved `Vault` for a root
    /// file's round-trip. The view render reads it (a root file renders
    /// no label index).
    pub(super) label_dir: String,
    /// The sanitized key value — the filename stem.
    name: String,
    /// The vault key `label_dir/name` — the codec's identity
    /// convention (ADR-0406 §5's stable-id maps key on it). The view
    /// render reads it (the changelog's linkability set).
    pub(super) vault_key: String,
    /// Labels beyond the primary, sorted.
    extra_labels: Vec<String>,
    /// The planned file path: `{label_dir}/{name}.md`, or the bare
    /// `{name}.md` for a root file.
    path: String,
}

/// Whether a path's file name is a reserved generated view — the
/// grammar crate's one predicate over the vault walk's own
/// [`file_name`] extractor (the guarded-stats filter's class rule).
fn path_is_view(path: &str) -> bool {
    md::is_reserved_view_file_name(file_name(path))
}

/// Plan every node's file: the learned key property → filename, the
/// reserved-view refusal, the :Vault root round-trip, the raw
/// passthrough tolerance, and the collision check. Returns the
/// per-node plans plus the raw-frontmatter paths whose nodes are still
/// live (the passthrough carries only those). A short fold over
/// [`plan_node`]'s decisions — the load side's phase extraction
/// (parse → canonicalize) mirrored on the write side.
fn plan_node_files(
    nodes: &[Node],
    learned: &LearnedSchema,
    existing_by_path: &BTreeMap<String, Vec<u8>>,
    bodies: &MarkdownBodyProperties,
) -> Result<(BTreeMap<i64, NodePlan>, BTreeSet<String>)> {
    let mut plans: BTreeMap<i64, NodePlan> = BTreeMap::new();
    // The planned paths, case-folded with the load side's own fold
    // (ADR-0406 §2): load drops a case-only duplicate at canonicalization,
    // so the write side must refuse the same collision the read side
    // would silently resolve — an exact-string set would let `Ada` and
    // `ada` of one label both render (and overwrite each other on
    // case-insensitive filesystems).
    let mut planned_paths_folded: BTreeSet<String> = BTreeSet::new();
    let mut raw_live: BTreeSet<String> = BTreeSet::new();
    for node in nodes {
        let (plan, raw_path) = plan_node(
            node,
            learned,
            existing_by_path,
            &mut planned_paths_folded,
            bodies,
        )?;
        if let Some(path) = raw_path {
            raw_live.insert(path);
        }
        plans.insert(node.id, plan);
    }
    Ok((plans, raw_live))
}

/// One node's plan decision — the plan loop's per-node body as one
/// named pass, the eight policy gates in their fixed order: the
/// primary-label resolution, the key-property extraction and type
/// gate, the name/label sanitization, the :Vault root round-trip
/// decision, the reserved-view refusal, the fold-collision refusal,
/// the root-file existence refusal, and the raw-passthrough
/// unmodified check. Returns the node's [`NodePlan`] plus, for a
/// raw-frontmatter file whose node is still live and unmodified, the
/// path to pass through verbatim (the fold's raw-live accumulator).
fn plan_node(
    node: &Node,
    learned: &LearnedSchema,
    existing_by_path: &BTreeMap<String, Vec<u8>>,
    planned_paths_folded: &mut BTreeSet<String>,
    bodies: &MarkdownBodyProperties,
) -> Result<(NodePlan, Option<String>)> {
    let primary = primary_label(node).ok_or_else(|| {
        codec_error(format!(
            "node {} has no label; the markdown vault requires at least one label per node",
            node.id
        ))
    })?;
    // The by-label inference pass above keyed every labeled node,
    // so the missing-key case is the authored error below, never a
    // silent default.
    let key_name = learned
        .keys
        .get(primary)
        .expect("the by-label inference pass keyed every labeled node")
        .clone();
    let Value::String(key_value) = node.properties.get(&key_name).ok_or_else(|| {
        codec_error(format!(
            "node {} of label {primary} has no '{key_name}' property; the markdown \
             vault requires a non-null unique '{key_name}' on :{primary}",
            node.id
        ))
    })?
    else {
        return Err(codec_error(format!(
            "node {} of label {primary}: key property '{key_name}' must be a string",
            node.id
        )));
    };
    let name = md::sanitize_name_path(key_value)
        .map_err(|e| Error::storage(format!("node {}: {e}", node.id)))?;
    // The engine never writes to the vault root (ADR-0406 §2): a
    // :Vault node round-trips only into the exact root path it
    // loaded from — decided by provenance (the walked root file),
    // never by the ambient directory shape.
    let root_roundtrip = primary == VAULT_LABEL;
    // The label's directory segment is sanitized like the name
    // segment: a backticked label can carry characters that are
    // illegal in a path segment (`/`, `:`, …). Plain identifier
    // labels sanitize to themselves.
    let label_dir = if root_roundtrip {
        primary.to_string()
    } else {
        md::sanitize_label(primary).map_err(|e| Error::storage(format!("node {}: {e}", node.id)))?
    };
    let path = if root_roundtrip {
        format!("{name}.md")
    } else {
        format!("{label_dir}/{name}.md")
    };
    // A node whose file would land on a reserved view name is
    // refused here — judged on the planned path's final segment,
    // through the same case fold the walk applies, so a nested key
    // like `sub/index` (→ `Person/sub/index.md`) and a case variant
    // like a `Index` key are caught exactly like a top-level one: the
    // regenerated view would overwrite them at the same path and the
    // node would silently disappear at the next open (ADR-0406 §4 —
    // the walk skips the reserved names at every level).
    if path_is_view(&path) {
        return Err(codec_error(format!(
            "node {} of label {primary} would render to the reserved view file \
             '{path}'; rename its '{key_name}' value",
            node.id
        )));
    }
    // The path is a one-to-one encoding of the vault key, so the
    // folded planned-path set is the single sufficient collision
    // check (O(log n), not a scan of every planned key) — for root
    // files and label files alike, exact and case-only collisions
    // both included.
    if planned_paths_folded.contains(&md::fold_case(&path)) {
        return Err(codec_error(format!(
            "two nodes of label {primary} share the key value '{key_value}'; the vault \
             filename would collide"
        )));
    }
    if root_roundtrip && !existing_by_path.contains_key(&path) {
        return Err(codec_error(format!(
            "node {} has the reserved root label :{VAULT_LABEL} but no existing root file; \
             the engine never writes to the vault root — give it a real label directory",
            node.id
        )));
    }
    // A raw-frontmatter file passes through verbatim (ADR-0406 §6)
    // — the engine never writes through raw text. A node whose
    // body, key property, or labels were modified in-session (or
    // whose raw file is gone) fails closed instead of having the
    // modification silently discarded by the passthrough.
    let mut raw_path: Option<String> = None;
    if learned.raw_files.contains(&path) {
        let unmodified = match existing_by_path.get(&path) {
            Some(bytes) => raw_file_unmodified(
                node,
                primary,
                &key_name,
                &name,
                &String::from_utf8_lossy(bytes),
                bodies.resolve(primary),
            )?,
            None => false,
        };
        if !unmodified {
            return Err(codec_error(format!(
                "node {} is a raw-frontmatter file the engine modified in-session; the \
                 vault's raw text never carries engine writes — edit the file by hand \
                 and reopen",
                node.id
            )));
        }
        raw_path = Some(path.clone());
    }
    let vault_key = vault_key(&label_dir, &name);
    let mut labels = node.labels.as_ref().clone();
    labels.sort();
    let plan = NodePlan {
        primary: primary.to_string(),
        label_dir,
        name,
        vault_key,
        extra_labels: labels[1..].to_vec(),
        path,
    };
    planned_paths_folded.insert(md::fold_case(&plan.path));
    Ok((plan, raw_path))
}

/// The raw-passthrough shape contract (ADR-0406 §6, ADR-0460), one
/// definition: a raw-frontmatter file's node is exactly what the load's
/// tolerance built — one label, the label's first body property of
/// [`raw_node_doc`]'s construction, plus the key property mirroring the
/// filename (the generic insert every loaded file's key property goes
/// through). The node is compared against that construction wholesale.
fn raw_file_unmodified(
    node: &Node,
    primary: &str,
    key_name: &str,
    name: &str,
    raw: &str,
    body_list: &[String],
) -> Result<bool> {
    let (_, raw_body) = md::split_frontmatter(raw);
    let mut expected = raw_node_doc(primary, raw_body, body_list)?;
    expected
        .properties
        .insert(key_name.to_string(), Json::String(name.to_string()));
    let actual: BTreeMap<String, Json> = props_to_cypher_json(&node.properties)?
        .into_iter()
        .collect();
    Ok(node.labels.len() == 1 && actual == expected.properties)
}

/// Outgoing link lists by source node id, then relationship type.
type LinksByStart = BTreeMap<i64, BTreeMap<String, Vec<md::LinkEntry>>>;

/// The relationship plan, built in one pass over the relationships:
/// each source node's outgoing link lists (sorted, deduplicated — set
/// semantics) and the stable-id map (ADR-0406 §5), whose identity is
/// computed from the same converted properties as the entry — one
/// property serialization per relationship.
fn build_links(
    relationships: &[Relationship],
    plans: &BTreeMap<i64, NodePlan>,
) -> Result<(LinksByStart, BTreeMap<String, i64>)> {
    let mut links_by_start: LinksByStart = BTreeMap::new();
    let mut rel_ids: BTreeMap<String, i64> = BTreeMap::new();
    for rel in relationships {
        let start = plans
            .get(&rel.start)
            .expect("the plan loop keyed every node before its relationships");
        let end = plans
            .get(&rel.end)
            .expect("the plan loop keyed every node before its relationships");
        // An empty property map converts to the empty map (the codec's
        // converter iterates nothing) — no special case on either side
        // of the codec.
        let entry = md::LinkEntry {
            target: md::LinkTarget::Full {
                label: end.label_dir.clone(),
                key: end.name.clone(),
            },
            properties: props_to_cypher_json(&rel.properties)
                .map_err(|e| Error::storage(format!("relationship {}: {e}", rel.id)))?
                .into_iter()
                .collect(),
        };
        rel_ids.insert(
            rel_identity(
                &start.vault_key,
                &rel.rel_type,
                &end.vault_key,
                &entry.properties,
            ),
            rel.id,
        );
        links_by_start
            .entry(rel.start)
            .or_default()
            .entry(rel.rel_type.clone())
            .or_default()
            .push(entry);
    }
    for by_type in links_by_start.values_mut() {
        for entries in by_type.values_mut() {
            *entries = md::LinkEntry::sorted_dedup(std::mem::take(entries));
        }
    }
    Ok((links_by_start, rel_ids))
}

/// What rendering a label's node files produced: the planned files'
/// bytes and the vector-indexed properties keyed for the sidecar's
/// embeddings map.
type RenderedNodes = (BTreeMap<String, Vec<u8>>, BTreeMap<String, Json>);

/// Render every node file into the planned map, lifting the
/// vector-indexed properties into the sidecar's embeddings map — the
/// render pass reads the plan; it never re-derives labels, keys, or
/// paths.
fn render_node_files(
    nodes: &[Node],
    plans: &BTreeMap<i64, NodePlan>,
    mut links_by_start: LinksByStart,
    vector_props: &BTreeSet<(String, String)>,
    rel_types: &BTreeSet<String>,
    bodies: &MarkdownBodyProperties,
) -> Result<RenderedNodes> {
    let mut planned: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut embeddings: BTreeMap<String, Json> = BTreeMap::new();
    for node in nodes {
        let plan = plans
            .get(&node.id)
            .expect("the plan loop planned every node");
        let mut properties = props_to_cypher_json(&node.properties)
            .map_err(|e| Error::storage(format!("node {}: {e}", node.id)))?;
        // The label's vector properties in the set's ascending order —
        // the BTreeSet's ordering is the documented determinism (no
        // second sort).
        for property in vector_props
            .iter()
            .filter(|(label, _)| label.as_str() == plan.primary)
            .map(|(_, property)| property)
        {
            if let Some(value) = properties.remove(property) {
                if !value.is_null() {
                    embeddings.insert(format!("{}#{property}", plan.vault_key), value);
                }
            }
        }
        let doc = md::NodeDoc {
            primary_label: plan.primary.clone(),
            extra_labels: plan.extra_labels.clone(),
            properties: properties.into_iter().collect(),
            links: links_by_start.remove(&node.id).unwrap_or_default(),
        };
        // The label's resolved body-property list (ADR-0460) — the
        // same resolution the load side consulted, from the same
        // configuration the open resolved.
        let rendered = md::render_node_doc(&doc, rel_types, bodies.resolve(&plan.primary))
            .map_err(|e| Error::storage(format!("{}: {e}", plan.path)))?;
        planned.insert(plan.path.clone(), rendered.into_bytes());
    }
    Ok((planned, embeddings))
}

pub(crate) fn checkpoint_at(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    ontology: Option<&str>,
) -> Result<()> {
    if snapshot_ts == u64::MAX {
        return Err(codec_error("markdown snapshot timestamp exhausted"));
    }
    journal::recover(target)?;
    let previous = read_manifest(target)?;
    let vfs = target.vfs();
    let existing = walk_vault(&*vfs)?;
    let existing_stats = vault_stats(&existing);
    let existing_by_path: BTreeMap<String, Vec<u8>> = existing
        .into_iter()
        .map(|f| (f.walked.path, f.bytes))
        .collect();
    // The external-write guard (ADR-0406 §5): with a manifest, the vault
    // must byte-match the last checkpoint; without one, everything the
    // walk finds was loaded as nodes at open, so all of it is
    // engine-owned for this first checkpoint.
    if let Some(previous) = &previous {
        if !vault_matches(&existing_stats, previous) {
            return Err(codec_error(
                "the vault changed on disk while the database was open; close the \
                 database, let your edits in, and reopen (concurrent writers are not \
                 merged)",
            ));
        }
    }

    let contents = SnapshotContents::from_graph(graph, snapshot_ts)?;
    let theater = graph.checkpoint_theater_for_write()?;
    let vector_props = vector_indexed_properties(&contents, &theater);
    let learned = regenerate_learned(previous.as_ref(), &contents)?;
    // The body-property configuration resolves through the one
    // resolution the load side also consults (ADR-0460); the plan and
    // the render below read the same value.
    // The previously recorded form, derived once for both consumers:
    // the materialized view the render parses with, and the raw form
    // the sidecar carries forward.
    let previous_bodies = super::bodies_of(previous.as_ref());
    let bodies = super::resolve_bodies(target, &previous_bodies)?;
    // The sidecar records the pre-materialization form (ADR-0460): the
    // explicit open-time config verbatim, else the previously recorded
    // one — unconfigured stores keep omitting the field, and an
    // unchanged configuration is carried forward byte-equal.
    let recorded = super::recorded_bodies(target, &previous_bodies);

    let (plans, raw_live) = plan_node_files(&contents.nodes, &learned, &existing_by_path, &bodies)?;
    let (links_by_start, rel_ids) = build_links(&contents.relationships, &plans)?;
    let (mut planned, embeddings) = render_node_files(
        &contents.nodes,
        &plans,
        links_by_start,
        &vector_props,
        &learned.rel_types,
        &bodies,
    )?;

    // Raw files (unparseable frontmatter, ADR-0406 §6) pass through
    // byte-identical — the raw text is the user's, never rewritten, and
    // only while their node is still live: an in-session delete stays
    // deleted.
    for path in &raw_live {
        if let Some(bytes) = existing_by_path.get(path) {
            planned.insert(path.clone(), bytes.clone());
        }
    }

    // The guarded (non-view) stats, computed once as a stats-only
    // projection — no second bytes map (a view is regenerated
    // wholesale, never guarded, never a changelog entry). It feeds the
    // changelog input and the manifest's files map, so the two
    // provably see the same file set.
    let guarded_stats: BTreeMap<String, FileStat> = planned
        .iter()
        .filter(|(path, _)| !path_is_view(path))
        .map(|(path, bytes)| (path.clone(), FileStat::of(bytes)))
        .collect();
    let changes = changes_since(previous.as_ref(), &guarded_stats);
    let rendered_views = views::render(&plans, &changes, checkpoint_millis()?);
    planned.extend(
        rendered_views
            .iter()
            .map(|(path, text)| (path.clone(), text.clone().into_bytes())),
    );

    // The new manifest, published last.
    let manifest = VaultManifest {
        codec: Some(SnapshotCodecKind::Markdown),
        checkpoint_lsn,
        commit_ts: snapshot_ts,
        value_encoding: Some(crate::version::value_encoding::version_for_write(
            ontology.is_some(),
        )),
        next_node_id: contents.next_node_id,
        next_rel_id: contents.next_rel_id,
        catalog: catalog::to_json(&contents, &theater),
        theater: Some(theater),
        read_only: false,
        ontology: ontology.map(str::to_owned),
        learned,
        bodies: recorded,
        embeddings,
        files: guarded_stats,
        // The stable ids this checkpoint assigns (ADR-0406 §5): the
        // next load restores them, so id-based WAL replay addresses
        // the same entities the writes named.
        node_ids: plans
            .iter()
            .map(|(id, plan)| (plan.vault_key.clone(), *id))
            .collect(),
        rel_ids,
    };

    apply_checkpoint(
        target,
        graph,
        &*vfs,
        &planned,
        &rendered_views,
        existing_by_path,
        manifest,
    )
}

/// The checkpoint's apply tail (ADR-0406 §5): the copy-on-write journal
/// of everything about to be overwritten or deleted, the writes and
/// deletions, the manifest published last, then journal removal — one
/// named phase of the checkpoint pipeline.
fn apply_checkpoint(
    target: &StoreTarget,
    graph: &Graph,
    vfs: &dyn Vfs,
    planned: &BTreeMap<String, Vec<u8>>,
    rendered_views: &[(String, String)],
    mut existing_by_path: BTreeMap<String, Vec<u8>>,
    manifest: VaultManifest,
) -> Result<()> {
    // Copy-on-write journal of everything about to be overwritten or
    // deleted, then the apply, then the manifest, then journal removal.
    // The journal's atomicity must cover the derived views too
    // (ADR-0406 §5): the walk skips the reserved view names, so no
    // rendered view path can already sit in `existing_by_path` — every
    // existing view still needs its old bytes staged for the journal,
    // the one spelling of which planned files are views.
    for path in rendered_views.iter().map(|(path, _)| path) {
        if vfs.exists(path)? {
            existing_by_path.insert(path.clone(), read_to_vec(vfs, path)?);
        }
    }
    journal::stage(target, &existing_by_path, &manifest)?;
    // Deletions BEFORE writes: a case-only key rename plans
    // `Person/Ada.md` over an existing `Person/ada.md` — on a
    // case-insensitive filesystem the two spellings are one physical
    // file, so deleting the stale spelling after the write would
    // unlink the node file the write just produced. Deleting first is
    // the one order that is a rename on every filesystem, and the
    // staged journal above restores the old bytes if a write fails
    // after a deletion.
    let deletable: Vec<String> = existing_by_path
        .keys()
        .filter(|path| !planned.contains_key(*path))
        .cloned()
        .collect();
    for path in deletable {
        vfs.remove_file(&path)
            .map_err(|e| codec_error(format!("removing {path}: {e}")))?;
    }
    for (path, bytes) in planned {
        // The byte-identical skip compares against the pre-deletion
        // census: an identical file was never stale, so it is still on
        // disk exactly as planned.
        match existing_by_path.get(path) {
            Some(current) if current == bytes => {}
            _ => atomic_write(vfs, path, bytes)?,
        }
    }
    // The view class is explicit: every view file on disk (the walk's
    // own class enumeration) minus the views this checkpoint rendered
    // is the stale set — a label directory whose nodes are all gone
    // loses its per-label view, so the prune below can fire for a
    // previously checkpointed label.
    let planned_views: BTreeSet<&str> = rendered_views
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    for view in walk_view_paths(vfs)? {
        if !planned_views.contains(view.as_str()) {
            vfs.remove_file(&view)
                .map_err(|e| codec_error(format!("removing stale view {view}: {e}")))?;
        }
    }
    prune_empty_dirs(vfs)?;
    let manifest = with_recorded_mtimes(vfs, manifest)?;
    write_manifest(vfs, &manifest)?;
    journal::clear(target)?;
    graph.clear_checkpoint_dirty();
    Ok(())
}

/// Record the just-written files' real mtimes on the manifest before
/// publication, so the next open's stat-first guard consults them
/// instead of re-reading every byte.
fn with_recorded_mtimes(vfs: &dyn Vfs, mut manifest: VaultManifest) -> Result<VaultManifest> {
    for (path, stat) in manifest.files.iter_mut() {
        stat.mtime = vfs.metadata(path)?.mtime;
    }
    Ok(manifest)
}

/// The changelog input: which guarded files are new, gone, or changed
/// relative to the previous checkpoint.
fn changes_since(
    previous: Option<&VaultManifest>,
    guarded_stats: &BTreeMap<String, FileStat>,
) -> Vec<views::Change> {
    let mut changes: Vec<views::Change> = Vec::new();
    let empty = BTreeMap::new();
    let old = previous.map(|m| &m.files).unwrap_or(&empty);
    for (path, stat) in guarded_stats {
        match old.get(path) {
            None => changes.push(views::Change::Added(path.clone())),
            Some(recorded) if recorded != stat => {
                changes.push(views::Change::Changed(path.clone()))
            }
            _ => {}
        }
    }
    for path in old.keys() {
        if !guarded_stats.contains_key(path) {
            changes.push(views::Change::Removed(path.clone()));
        }
    }
    changes
}
