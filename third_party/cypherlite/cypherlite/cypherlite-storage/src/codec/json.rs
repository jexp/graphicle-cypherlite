//! CypherLite GraphData snapshot (`graph.json` full rewrite). ADR-0050.

use cypherlite_catalog::{ConstraintDef, IndexDef};
use cypherlite_core::{Error, Node, Relationship, Result};
use cypherlite_executor::{CheckpointTheater, Graph};
use cypherlite_model::ConstraintType;
use serde::{Deserialize, Serialize};

use super::convert::{props_from_cypher_json, props_to_cypher_json};
use super::{Envelope, SnapshotCodecKind};
use crate::snapshot::{object_exists, put_object, LoadedSnapshot};
use crate::{storage_context, StoreTarget};

pub const GRAPH_JSON: &str = "graph.json";

#[derive(Serialize, Deserialize)]
struct GraphData {
    #[serde(
        default,
        deserialize_with = "deserialize_value_encoding",
        skip_serializing_if = "Option::is_none"
    )]
    value_encoding: Option<u64>,
    nodes: Vec<JsonNode>,
    relationships: Vec<JsonRelationship>,
    #[serde(default)]
    indexes: Vec<JsonIndex>,
    #[serde(default)]
    constraints: Vec<JsonConstraint>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::manifest::deserialize_checkpoint_theater"
    )]
    theater: Option<CheckpointTheater>,
    next_node_id: i64,
    next_rel_id: i64,
    #[serde(default)]
    graph_type: Option<String>,
    #[serde(default)]
    read_only: bool,
    /// The inline ontology document (ADR-0120 §6). Written only at
    /// `value_encoding` 3; absent means the store holds no ontology.
    /// Opaque to storage: kept verbatim, validated elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ontology: Option<String>,
}

fn deserialize_value_encoding<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u64::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
struct JsonNode {
    id: i64,
    labels: Vec<String>,
    properties: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct JsonRelationship {
    id: i64,
    #[serde(rename = "type")]
    rel_type: String,
    start_node: i64,
    end_node: i64,
    properties: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct JsonIndex {
    name: String,
    #[serde(default, alias = "label", deserialize_with = "deserialize_labels")]
    labels: Vec<String>,
    #[serde(default)]
    properties: Vec<String>,
    #[serde(rename = "type", default = "default_range")]
    index_type: String,
    #[serde(default)]
    index_config: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_node_entity")]
    entity_type: String,
}

#[derive(Serialize, Deserialize)]
struct JsonConstraint {
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    properties: Vec<String>,
    #[serde(rename = "type", default)]
    constraint_type: String,
    #[serde(default)]
    property_type: Option<String>,
    #[serde(default = "default_node_entity")]
    entity_type: String,
    #[serde(default)]
    enforced_label: Option<String>,
}

fn default_range() -> String {
    "range".into()
}

fn default_node_entity() -> String {
    "NODE".into()
}

fn deserialize_labels<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum LabelsOrLabel {
        Many(Vec<String>),
        Single(String),
    }
    Ok(match LabelsOrLabel::deserialize(deserializer)? {
        LabelsOrLabel::Many(v) => v,
        LabelsOrLabel::Single(s) => vec![s],
    })
}

pub fn dump(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    ontology: Option<&str>,
) -> Result<()> {
    if snapshot_ts == u64::MAX {
        return Err(Error::storage("JSON snapshot timestamp exhausted"));
    }
    if graph.next_node_id() < 0 || graph.next_rel_id() < 0 {
        return Err(Error::storage("JSON snapshot has unsupported next ID"));
    }
    let theater = graph.checkpoint_theater_for_write()?;
    let body = dump_to_vec(graph, snapshot_ts, theater, ontology)?;
    put_object(target, GRAPH_JSON, &body)?;
    let envelope = Envelope {
        codec: SnapshotCodecKind::Json,
        checkpoint_lsn,
        commit_ts: snapshot_ts,
    };
    let env_bytes =
        serde_json::to_vec_pretty(&envelope).map_err(|e| Error::storage(e.to_string()))?;
    put_object(target, "manifest.json", &env_bytes)?;
    graph.clear_checkpoint_dirty();
    Ok(())
}

fn dump_to_vec(
    graph: &Graph,
    snapshot_ts: u64,
    theater: CheckpointTheater,
    ontology: Option<&str>,
) -> Result<Vec<u8>> {
    let data = graph_to_data(graph, snapshot_ts, theater, ontology)?;
    serde_json::to_vec(&data).map_err(|e| Error::storage(e.to_string()))
}

fn graph_to_data(
    graph: &Graph,
    snapshot_ts: u64,
    theater: CheckpointTheater,
    ontology: Option<&str>,
) -> Result<GraphData> {
    let contents = super::model::SnapshotContents::from_graph(graph, snapshot_ts)?;
    let mut nodes = Vec::new();
    for n in contents.nodes {
        let id = n.id;
        nodes.push(JsonNode {
            id,
            labels: std::sync::Arc::unwrap_or_clone(n.labels),
            properties: props_to_cypher_json(&n.properties)
                .map_err(|e| storage_context(e, format_args!("node {id}")))?,
        });
    }
    let mut relationships = Vec::new();
    for r in contents.relationships {
        let id = r.id;
        relationships.push(JsonRelationship {
            id,
            rel_type: r.rel_type,
            start_node: r.start,
            end_node: r.end,
            properties: props_to_cypher_json(&r.properties)
                .map_err(|e| storage_context(e, format_args!("relationship {id}")))?,
        });
    }
    let indexes = contents
        .indexes
        .into_iter()
        .map(|d| {
            // The durable record tells the catalog truth when the theater
            // knows it; indexes that predate the theater keep the
            // historical defaults.
            let detail = theater.index_detail(&d.name);
            JsonIndex {
                name: d.name,
                labels: vec![d.label],
                properties: vec![d.property],
                index_type: detail
                    .map(|detail| detail.kind.display_name().to_string())
                    .unwrap_or_else(default_range),
                index_config: serde_json::Map::new(),
                entity_type: match detail {
                    Some(detail) if detail.relationship => "RELATIONSHIP".into(),
                    _ => default_node_entity(),
                },
            }
        })
        .collect();
    let constraints = contents
        .constraints
        .into_iter()
        .map(|d| JsonConstraint {
            enforced_label: theater.enforced_label(&d.name).map(str::to_owned),
            name: d.name,
            label: d.label,
            properties: d.properties,
            constraint_type: d.kind,
            property_type: d.property_type,
            entity_type: if d.relationship {
                "RELATIONSHIP".into()
            } else {
                default_node_entity()
            },
        })
        .collect();
    Ok(GraphData {
        // The format follows the content (ADR-0120 §8): encoding 3 when the
        // inline ontology is present, 2 otherwise.
        value_encoding: Some(crate::version::value_encoding::version_for_write(
            ontology.is_some(),
        )),
        nodes,
        relationships,
        indexes,
        constraints,
        theater: Some(theater),
        next_node_id: contents.next_node_id,
        next_rel_id: contents.next_rel_id,
        graph_type: contents.graph_type,
        read_only: false,
        ontology: ontology.map(str::to_owned),
    })
}

#[cfg(test)]
pub fn load(target: &StoreTarget) -> Result<Graph> {
    Ok(load_with_checkpoint_lsn(target)?.graph)
}

/// Load the snapshot and the checkpoint LSN from the one envelope read,
/// reporting the decoded `value_encoding` marker (ADR-0127 Decision 2) and
/// the inline ontology document (ADR-0120 §6; `None` when the store holds
/// none). `graph.json` is read exactly once. The LSN is 0 when no manifest
/// exists yet.
pub(crate) fn load_with_checkpoint_lsn(target: &StoreTarget) -> Result<LoadedSnapshot> {
    use crate::version::{value_encoding, SnapshotMarker, Subsystem};
    let (envelope_commit_ts, checkpoint_lsn) = envelope_meta(target)?;
    if !object_exists(target, GRAPH_JSON)? {
        return Ok(LoadedSnapshot::empty(checkpoint_lsn));
    }
    let bytes = crate::snapshot::get_object(target, GRAPH_JSON)?;
    let text = String::from_utf8(bytes).map_err(|e| Error::storage(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(LoadedSnapshot::empty(checkpoint_lsn));
    }
    let data: GraphData = serde_json::from_str(&text).map_err(|e| {
        let error = Error::storage(format!("failed to parse graph JSON: {e}"));
        // A legacy-store failure names its version, probed on the error
        // path only from bytes already in hand (ADR-0127 Decision 7).
        match probe_value_encoding(&text) {
            Some(version) => crate::version::annotate_legacy_failure(
                error,
                SnapshotMarker::Legacy(Subsystem::ValueEncoding, version),
            ),
            None => error,
        }
    })?;
    // The version marker gates everything else (ADR-0114 Decision 2); the
    // registry's per-subsystem gate normalizes the absent marker to
    // encoding 1, once, and rejects an unreadable store with the one
    // unsupported-version error (ADR-0127 Decision 2). The gate classifies
    // against the content the body carries (ADR-0120 §8), and the legacy
    // subsystem is named at the gate.
    let has_ontology = data.ontology.is_some();
    let (encoding, marker) = value_encoding::gate(data.value_encoding, has_ontology)?;
    // A legacy-store failure names the version where the loader knows it
    // (ADR-0127 Decision 4); a current store keeps its error verbatim.
    let (graph, ontology) = graph_from_data(data, encoding, envelope_commit_ts)
        .map_err(|error| crate::version::annotate_legacy_failure(error, marker))?;
    Ok(LoadedSnapshot {
        graph,
        checkpoint_lsn,
        ontology,
        marker,
    })
}

/// Error-path-only marker probe (ADR-0127 Decision 7): when the full
/// `GraphData` decode fails, the store's version is still named from the
/// bytes already in hand. Never called on the success path, so a
/// current-format open never parses `graph.json` twice.
#[derive(Deserialize)]
struct ValueEncodingProbe {
    #[serde(default)]
    value_encoding: Option<u64>,
}

fn probe_value_encoding(text: &str) -> Option<u64> {
    let probe: ValueEncodingProbe = serde_json::from_str(text).ok()?;
    // The probe classifies without the content flag: an unreadable marker
    // is unreadable either way, and a content-mismatched readable marker is
    // still worth naming on this error path.
    match crate::version::value_encoding::classify(probe.value_encoding, false) {
        crate::version::Classification::Legacy(version) => Some(version),
        _ => None,
    }
}

fn envelope_meta(target: &StoreTarget) -> Result<(Option<u64>, u64)> {
    let Some(env) = crate::codec::read_envelope(target)? else {
        return Ok((None, 0));
    };
    if env.commit_ts == u64::MAX {
        return Err(Error::storage("JSON manifest timestamp exhausted"));
    }
    Ok((Some(env.commit_ts).filter(|ts| *ts > 0), env.checkpoint_lsn))
}

/// Decode into the in-memory graph, returning the inline ontology document
/// with it (ADR-0120 §6). `value_encoding` is the marker the caller
/// already classified and normalized (absent → 1) via the registry.
fn graph_from_data(
    data: GraphData,
    value_encoding: u64,
    envelope_commit_ts: Option<u64>,
) -> Result<(Graph, Option<String>)> {
    let ontology = data.ontology;
    if data.read_only {
        return Err(Error::storage("read-only snapshot not supported"));
    }
    if data.next_node_id < 0 || data.next_rel_id < 0 {
        return Err(Error::storage("JSON snapshot has unsupported next ID"));
    }
    for n in &data.nodes {
        cypherlite_core::validate_persisted_id(n.id, "JSON snapshot node")?;
    }
    for r in &data.relationships {
        cypherlite_core::validate_persisted_id(r.id, "JSON snapshot relationship")?;
        cypherlite_core::validate_persisted_id(r.start_node, "JSON snapshot relationship start")?;
        cypherlite_core::validate_persisted_id(r.end_node, "JSON snapshot relationship end")?;
    }
    let graph = Graph::default();
    graph.set_next_node_id(data.next_node_id);
    graph.set_next_rel_id(data.next_rel_id);
    let mut loaded = 0usize;
    for n in data.nodes {
        let properties = props_from_cypher_json(&n.properties, value_encoding)
            .map_err(|e| storage_context(e, format_args!("node {}", n.id)))?;
        graph.load_checkpoint_node(Node {
            id: n.id,
            labels: std::sync::Arc::new(n.labels),
            properties: cypherlite_core::PropertyMap::share(properties),
        })?;
        loaded += 1;
    }
    for r in data.relationships {
        let properties = props_from_cypher_json(&r.properties, value_encoding)
            .map_err(|e| storage_context(e, format_args!("relationship {}", r.id)))?;
        graph.load_checkpoint_rel(Relationship {
            id: r.id,
            start: r.start_node,
            end: r.end_node,
            rel_type: r.rel_type,
            properties: cypherlite_core::PropertyMap::share(properties),
        })?;
        loaded += 1;
    }
    if let Some(ts) = envelope_commit_ts {
        graph.set_commit_ts(ts);
    }
    if graph.commit_ts() == 0 && loaded > 0 {
        graph.set_commit_ts(1);
    }
    let mut legacy_enforced = Vec::new();
    for c in data.constraints {
        if let Some(label) = &c.enforced_label {
            let constraint_type = c
                .constraint_type
                .parse::<ConstraintType>()
                .map_err(|e| Error::storage(format!("invalid JSON enforced constraint: {e}")))?;
            legacy_enforced.push(cypherlite_model::Constraint {
                name: c.name.clone(),
                label: c.label.clone(),
                properties: c.properties.clone(),
                constraint_type,
                property_type: c.property_type.clone(),
                entity_type: c.entity_type.clone(),
                enforced_label: Some(label.clone()),
            });
        }
        graph.install_named_constraint(ConstraintDef {
            name: c.name,
            relationship: c.entity_type.eq_ignore_ascii_case("RELATIONSHIP"),
            label: c.label,
            properties: c.properties,
            kind: c.constraint_type,
            property_type: c.property_type,
        })?;
    }
    for idx in data.indexes {
        // An index record without labels/properties has no catalog keys to
        // install under; skip it rather than fabricate an empty-string
        // label/property (mirrors the empty-name guard).
        let (Some(label), Some(property)) = (idx.labels.first(), idx.properties.first()) else {
            continue;
        };
        if !idx.name.is_empty() {
            graph.install_named_index(IndexDef {
                name: idx.name,
                label: label.clone(),
                property: property.clone(),
            })?;
        }
    }
    if let Some(theater) = data.theater {
        if legacy_enforced
            .iter()
            .any(|c| theater.enforced_label(&c.name) != c.enforced_label.as_deref())
        {
            return Err(Error::storage(
                "JSON constraint enforced_label disagrees with checkpoint theater",
            ));
        }
        graph.restore_checkpoint_theater(theater)?;
    } else {
        graph.restore_graph_type_spec(data.graph_type.as_deref())?;
        graph.restore_legacy_enforced_constraints(legacy_enforced)?;
        graph.mark_legacy_unenforced();
    }
    graph.finish_restore();
    Ok((graph, ontology))
}

pub fn snapshot_data_bytes(target: &StoreTarget) -> Result<u64> {
    let mut sum = 0u64;
    for name in [crate::snapshot::MANIFEST_JSON, GRAPH_JSON] {
        if !target.vfs.exists(name)? {
            continue;
        }
        // Bounded JSON metadata: prove the size on the metadata and on the
        // opened handle without reading the object. `graph.json` is the
        // large payload and is summed from metadata alone.
        let len = match crate::snapshot::bounded_read_for(name) {
            Some(kind) => crate::snapshot::checked_object_len(target, name, kind)?.0,
            None => target.vfs.metadata(name)?.len,
        };
        sum = sum.saturating_add(len);
    }
    Ok(sum)
}

#[cfg(test)]
#[path = "json/tests.rs"]
mod decoding_tests;
