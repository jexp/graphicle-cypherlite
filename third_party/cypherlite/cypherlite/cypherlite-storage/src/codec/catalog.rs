//! The checkpoint catalog encoding shared by the JSON and Markdown
//! codecs (ADR-0406): one home for the index/constraint wire structs and
//! both directions of the conversion, so the sidecar manifests cannot
//! drift apart.

use cypherlite_catalog::{ConstraintDef, IndexDef};
use cypherlite_core::{Error, Result};
use cypherlite_executor::{CheckpointTheater, Graph};
use cypherlite_model::{ConstraintType, EntityKind};
use serde::{Deserialize, Serialize};

use super::model::SnapshotContents;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct JsonIndex {
    pub name: String,
    #[serde(default, alias = "label", deserialize_with = "deserialize_labels")]
    pub labels: Vec<String>,
    #[serde(default)]
    pub properties: Vec<String>,
    #[serde(rename = "type", default = "default_range")]
    pub index_type: String,
    #[serde(default)]
    pub index_config: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_node_entity")]
    pub entity_type: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct JsonConstraint {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub properties: Vec<String>,
    #[serde(rename = "type", default)]
    pub constraint_type: String,
    #[serde(default)]
    pub property_type: Option<String>,
    #[serde(default = "default_node_entity")]
    pub entity_type: String,
    #[serde(default)]
    pub enforced_label: Option<String>,
}

impl JsonIndex {
    /// The catalog keys this wire row installs under, after the one
    /// malformed-row check: exactly one label and one property (the
    /// engine's `IndexDef` invariant; the wire keeps the historical
    /// list spellings for compatibility) and a non-empty name — the
    /// exact shape the writer emits. A row violating any of the three
    /// is corruption, never a row to silently skip: the error names
    /// the row (mirroring `install_constraint_rows`' typed boundary).
    fn catalog_keys(&self) -> Result<(&str, &str)> {
        let invalid =
            |problem: &str| Error::storage(format!("invalid index row '{}': {problem}", self.name));
        if self.name.is_empty() {
            return Err(invalid("the name is empty"));
        }
        if self.labels.len() != 1 {
            return Err(invalid("expected exactly one label"));
        }
        if self.properties.len() != 1 {
            return Err(invalid("expected exactly one property"));
        }
        Ok((&self.labels[0], &self.properties[0]))
    }
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

/// The snapshot contents' catalog, in the wire form (the theater knows
/// the durable display truth; indexes predating it keep the historical
/// defaults).
pub(crate) fn to_json(contents: &SnapshotContents, theater: &CheckpointTheater) -> CatalogWire {
    let indexes = contents
        .indexes
        .iter()
        .map(|d| {
            let detail = theater.index_detail(&d.name);
            // The typed entity-kind vocabulary emits the display name;
            // `default_node_entity` stays the serde deserialization
            // default only.
            let entity_kind = EntityKind::from_relationship_flag(
                detail.is_some_and(|detail| detail.relationship),
            );
            JsonIndex {
                name: d.name.clone(),
                labels: vec![d.label.clone()],
                properties: vec![d.property.clone()],
                index_type: detail
                    .map(|detail| detail.kind.display_name().to_string())
                    .unwrap_or_else(default_range),
                index_config: serde_json::Map::new(),
                entity_type: entity_kind.display_name().to_string(),
            }
        })
        .collect();
    let constraints = contents
        .constraints
        .iter()
        .map(|d| JsonConstraint {
            enforced_label: theater.enforced_label(&d.name).map(str::to_owned),
            name: d.name.clone(),
            label: d.label.clone(),
            properties: d.properties.clone(),
            constraint_type: d.kind.clone(),
            property_type: d.property_type.clone(),
            entity_type: EntityKind::from_relationship_flag(d.relationship)
                .display_name()
                .to_string(),
        })
        .collect();
    CatalogWire {
        indexes,
        constraints,
        graph_type: contents.graph_type.clone(),
    }
}

/// The catalog (and graph type) fields a codec's manifest carries —
/// the one wire carrier both codecs embed (the JSON codec spreads it
/// into `graph.json`, the markdown sidecar `#[serde(flatten)]`s it), so
/// a manifest never re-spells the field set.
#[derive(Default, Serialize, Deserialize, Clone, Debug)]
pub(crate) struct CatalogWire {
    #[serde(default)]
    pub indexes: Vec<JsonIndex>,
    #[serde(default)]
    pub constraints: Vec<JsonConstraint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_type: Option<String>,
}

/// Decode the constraint rows and install their catalog definitions —
/// the one boundary between the stored row shape and the model/def
/// crossings. Returns the typed model records of the rows that name an
/// enforced label, for the theater cross-check.
fn install_constraint_rows(
    graph: &Graph,
    rows: Vec<JsonConstraint>,
) -> Result<Vec<cypherlite_model::Constraint>> {
    let mut legacy_enforced = Vec::new();
    for row in rows {
        // The one typed entity-kind boundary (never a case-insensitive
        // string compare): a value naming neither kind is the same typed
        // storage error the preflight and theater raise.
        let kind = cypherlite_model::EntityKind::parse(&row.name, &row.entity_type)
            .map_err(Error::storage)?;
        let def = ConstraintDef {
            name: row.name,
            relationship: kind.is_relationship(),
            label: row.label,
            properties: row.properties,
            kind: row.constraint_type,
            property_type: row.property_type,
            // The row's label is sourced from the theater on write and
            // cross-checked here; install it into the catalog row so the
            // identity survives reload (ADR-0450).
            enforced_label: row.enforced_label,
        };
        // The model record derives from the same def (one constraint shape
        // per row); only rows naming an enforced label reach the theater.
        if def.enforced_label.is_some() {
            let constraint_type = def
                .kind
                .parse::<ConstraintType>()
                .map_err(|e| Error::storage(ConstraintType::unknown_kind_message(&def.name, &e)))?;
            legacy_enforced.push(def.to_model(constraint_type));
        }
        graph.install_named_constraint(def)?;
    }
    Ok(legacy_enforced)
}

/// The decoded constraint rows must agree with the restored theater's
/// enforced labels.
fn ensure_constraint_rows_match_theater(
    rows: &[cypherlite_model::Constraint],
    theater: &CheckpointTheater,
) -> Result<()> {
    if rows.iter().any(|constraint| {
        theater.enforced_label(&constraint.name) != constraint.enforced_label.as_deref()
    }) {
        return Err(Error::storage(
            "JSON constraint enforced_label disagrees with checkpoint theater",
        ));
    }
    Ok(())
}

/// Restore a loaded catalog into a freshly loaded graph — the tail of
/// every codec's load path (install constraints and indexes, then the
/// theater or the legacy pre-theater markers).
pub(crate) fn restore(
    graph: &Graph,
    wire: CatalogWire,
    theater: Option<CheckpointTheater>,
) -> Result<()> {
    let CatalogWire {
        indexes,
        constraints,
        graph_type,
    } = wire;
    let legacy_enforced = install_constraint_rows(graph, constraints)?;
    for idx in indexes {
        let (label, property) = idx.catalog_keys()?;
        graph.install_named_index(IndexDef {
            name: idx.name.clone(),
            label: label.to_string(),
            property: property.to_string(),
        })?;
    }
    if let Some(theater) = theater {
        ensure_constraint_rows_match_theater(&legacy_enforced, &theater)?;
        graph.restore_checkpoint_theater(theater)?;
    } else {
        graph.restore_graph_type_spec(graph_type.as_deref())?;
        graph.restore_legacy_enforced_constraints(legacy_enforced)?;
        graph.mark_legacy_unenforced();
    }
    graph.finish_restore();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, labels: &[&str], properties: &[&str]) -> JsonIndex {
        JsonIndex {
            name: name.to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            properties: properties.iter().map(|s| s.to_string()).collect(),
            index_type: default_range(),
            index_config: serde_json::Map::new(),
            entity_type: default_node_entity(),
        }
    }

    /// The writer's exact shape (one label, one property, a name)
    /// installs; every other shape is a malformed row that fails
    /// closed naming the row instead of being silently skipped
    /// (F-1-47/F-3-11/F-7-15).
    #[test]
    fn index_rows_outside_the_writers_shape_fail_closed() {
        let well_formed = row("person_name", &["Person"], &["name"]);
        assert_eq!(well_formed.catalog_keys().unwrap(), ("Person", "name"));
        let malformed = [
            row("", &["Person"], &["name"]),
            row("person_name", &[], &["name"]),
            row("person_name", &["Person"], &[]),
            row("person_name", &["Person", "Thing"], &["name"]),
            row("person_name", &["Person"], &["name", "title"]),
        ];
        for row in malformed {
            let error = row.catalog_keys().expect_err("malformed row fails");
            assert!(error.to_string().contains("invalid index row"), "{error}");
        }
    }
}
