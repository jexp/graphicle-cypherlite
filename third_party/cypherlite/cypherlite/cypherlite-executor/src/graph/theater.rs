//! Committed SHOW metadata captured with a checkpoint, plus the read-admission
//! schema snapshot derived from it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use cypherlite_catalog::{ConstraintDef, IndexDef};
use cypherlite_core::{Error, Namespace, Result, Value};
use serde::{Deserialize, Serialize};

use crate::redo::{self, RedoOp};

use super::{matching_enforced_label, requires_enforced_label, Graph};

/// The five committed per-graph SHOW-metadata maps that make up a checkpoint
/// theater. Composed by the live graph state (a plain `TheaterData`), the
/// serialized checkpoint image (`CheckpointTheater`'s flattened `data`) and
/// the read-admission snapshot (`ShowSchema`'s `data`). The databases table
/// is NOT here: it is store-level ([`DatabasesCatalog`], ADR-0276).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TheaterData {
    pub index_details: BTreeMap<String, redo::IndexDetailSpec>,
    pub index_options: BTreeMap<String, BTreeMap<String, Value>>,
    pub graph_types: BTreeMap<String, crate::graph_type::GraphTypeSpec>,
    pub current_graph_type: crate::graph_type::GraphTypeSpec,
    pub derived_constraints: BTreeMap<String, cypherlite_model::Constraint>,
}

/// One namespace's entry in the store-level databases catalog (ADR-0116
/// §8, ADR-0276): the settings `SHOW DATABASES` reports and `ALTER
/// DATABASE ... SET DEFAULT LANGUAGE` rewrites. Serde keeps the
/// historical two-element array shape (`[language, composite]`) so
/// existing checkpoint images keep loading.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "(u32, bool)", into = "(u32, bool)")]
pub struct DatabaseEntry {
    /// Default Cypher language version for the namespace (`5` or `25`).
    pub default_language: u32,
    /// Whether the namespace is a composite database.
    pub composite: bool,
}

impl From<(u32, bool)> for DatabaseEntry {
    fn from((default_language, composite): (u32, bool)) -> Self {
        Self {
            default_language,
            composite,
        }
    }
}

impl From<DatabaseEntry> for (u32, bool) {
    fn from(entry: DatabaseEntry) -> Self {
        (entry.default_language, entry.composite)
    }
}

/// ADR-0116 §8: record a namespace's default language in `map`. The redo
/// name is the serde/replay boundary: it matches the fixed namespaces
/// case-insensitively and is stored under the typed key; entries for any
/// other name (written by earlier `CREATE DATABASE` calls or an older
/// binary) are dropped here.
fn map_put_database(
    map: &mut BTreeMap<Namespace, DatabaseEntry>,
    name: &str,
    language: u32,
    composite: bool,
) {
    if let Some(ns) = Namespace::parse(name) {
        map.insert(
            ns,
            DatabaseEntry {
                default_language: language,
                composite,
            },
        );
    }
}

/// Store-level databases catalog (ADR-0276; keeps the ADR-0116 §8
/// invariants): the three fixed
/// namespaces' settings (default language, composite flag) live in ONE
/// table shared by every namespace graph of the store through the
/// `StoreServices` bundle. `ALTER DATABASE` writes it and `SHOW DATABASES`
/// reads it from any namespace session, so the two can never diverge the
/// way per-graph theater copies did. The table is keyed by `Namespace`, so
/// it holds exactly the three fixed entries by construction and every
/// lookup is total; the on-disk checkpoint theater image keeps serializing
/// it as strings under its historical `databases` key, with the boundary
/// conversion in [`Self::sanitize`] and [`Graph::checkpoint_theater`].
#[derive(Debug)]
pub(crate) struct DatabasesCatalog {
    entries: Mutex<BTreeMap<Namespace, DatabaseEntry>>,
}

impl DatabasesCatalog {
    /// ADR-0116 §1/§8: a fixed namespace's default entry — `CYPHER 25`,
    /// non-composite — until an `ALTER DATABASE ... SET DEFAULT LANGUAGE`
    /// redo says otherwise. `Default` and [`Self::sanitize`] seed every
    /// fixed namespace with it, and the typed key plus filtered inserts
    /// keep exactly the three entries present.
    pub(crate) const DEFAULT_DATABASE_ENTRY: DatabaseEntry = DatabaseEntry {
        default_language: 25,
        composite: false,
    };

    /// The seeded table: exactly the three fixed namespaces, each at its
    /// default entry.
    fn seeded() -> BTreeMap<Namespace, DatabaseEntry> {
        Namespace::ALL
            .iter()
            .map(|ns| (*ns, Self::DEFAULT_DATABASE_ENTRY))
            .collect()
    }

    /// The committed table at one capture point (read admission or
    /// checkpoint cut).
    pub(crate) fn snapshot(&self) -> BTreeMap<Namespace, DatabaseEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// ADR-0116 §8: record a namespace's default language (committed redo
    /// path; see `map_put_database` for the matching rule).
    pub(crate) fn put_database(&self, name: &str, language: u32, composite: bool) {
        map_put_database(&mut self.entries.lock().unwrap(), name, language, composite);
    }

    /// ADR-0116 §8: the boundary conversion for a checkpoint image's
    /// string-keyed table. Entries for non-fixed names (from an older
    /// store's `CREATE DATABASE` theater) are dropped so the next
    /// checkpoint no longer carries them; names match case-insensitively,
    /// and a fixed namespace missing from the image gets its default
    /// (`CYPHER 25`).
    pub(crate) fn sanitize(
        map: &BTreeMap<String, DatabaseEntry>,
    ) -> BTreeMap<Namespace, DatabaseEntry> {
        // One matching rule (`Namespace::parse`, as in `map_put_database`):
        // fold the image's entries over the seeded defaults, keeping only
        // the fixed namespaces.
        let mut out = Self::seeded();
        for (name, entry) in map {
            if let Some(ns) = Namespace::parse(name) {
                out.insert(ns, *entry);
            }
        }
        out
    }

    /// Install a checkpoint image's (already sanitized) table as the
    /// store-level catalog.
    pub(crate) fn install(&self, map: BTreeMap<Namespace, DatabaseEntry>) {
        *self.entries.lock().unwrap() = map;
    }
}

impl Default for DatabasesCatalog {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Self::seeded()),
        }
    }
}

/// Typed committed SHOW metadata stored with a checkpoint's named catalog.
/// Missing images in older snapshots use the historical empty theater.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointTheater {
    /// Store-level databases table at the checkpoint cut (ADR-0276; ADR-0116
    /// §8 invariants).
    /// Declared first so the serialized key order matches the historical
    /// flattened layout, where `databases` led `TheaterData`.
    pub databases: BTreeMap<String, DatabaseEntry>,
    #[serde(flatten)]
    data: TheaterData,
    /// Exact catalog definitions imported without known label enforcement.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    legacy_unknown_constraints: BTreeMap<String, ConstraintDef>,
}

impl CheckpointTheater {
    /// Enforced label of a named constraint, if present.
    #[doc(hidden)]
    pub fn enforced_label(&self, name: &str) -> Option<&str> {
        self.data
            .derived_constraints
            .get(name)
            .and_then(|constraint| constraint.enforced_label.as_deref())
    }

    /// Catalog detail of a named index, if present.
    #[doc(hidden)]
    pub fn index_detail(&self, name: &str) -> Option<&redo::IndexDetailSpec> {
        self.data.index_details.get(name)
    }
}

/// Catalog facts captured alongside an entity pin at read admission. The
/// caller serializes capture with commit publication; execution needs no
/// publication lock and never consults newer named schema.
pub struct SchemaSnapshot {
    pub(crate) indexes: Vec<IndexDef>,
    pub(crate) constraints: Vec<ConstraintDef>,
    pub(crate) show: ShowSchema,
}

/// The read-admission SHOW metadata: the store-level databases table plus
/// the graph's committed theater maps, captured at one admission point.
/// Composes [`TheaterData`] the way [`CheckpointTheater`] does, so the five
/// maps and the "set graph type ⇒ re-derive constraints" invariant
/// ([`TheaterData::set_current_graph_type`]) exist once. `apply_overlay`
/// replays the statement's speculative catalog redo on this private copy;
/// committed state changes only through `Graph::apply_redo`.
#[derive(Debug, Clone)]
pub(crate) struct ShowSchema {
    pub databases: BTreeMap<Namespace, DatabaseEntry>,
    pub data: TheaterData,
}

impl ShowSchema {
    /// Capture the committed SHOW metadata at one admission point: the
    /// store-level databases table plus this graph's theater maps.
    pub(crate) fn capture(databases: &DatabasesCatalog, theater: &TheaterData) -> Self {
        Self {
            databases: databases.snapshot(),
            data: theater.clone(),
        }
    }

    /// Apply speculative catalog operations to this private admitted copy.
    /// The committed state is changed only by `Graph::apply_redo` at
    /// publication. Returns the decode error for a corrupt op exactly as the
    /// committed interpreter does, so the speculative and committed paths
    /// agree.
    pub(crate) fn apply_overlay<'o>(
        &mut self,
        ops: impl IntoIterator<Item = &'o RedoOp>,
    ) -> Result<()> {
        for op in ops {
            match op {
                RedoOp::CatalogIndexDetail {
                    name,
                    detail,
                    options,
                } => {
                    // Decode before mutating, matching `Graph::apply_redo`.
                    // DDL produced these options with `props_json`; a corrupt
                    // external redo is an error, never a silently dropped entry.
                    let options = redo::props_from_json(options)?;
                    self.data.index_details.insert(name.clone(), detail.clone());
                    self.data.index_options.insert(name.clone(), options);
                }
                RedoOp::DropIndex { name } => {
                    self.data.index_details.remove(name);
                    self.data.index_options.remove(name);
                }
                RedoOp::PutDatabase {
                    name,
                    language,
                    composite,
                } => {
                    map_put_database(&mut self.databases, name, *language, *composite);
                }
                RedoOp::DropDatabase { .. } => {
                    // ADR-0116 §1/§8: the fixed namespaces cannot be
                    // dropped, and the `Namespace`-keyed table can only
                    // ever hold them, so a legacy `DROP DATABASE` redo is
                    // a no-op.
                }
                RedoOp::PutGraphType { name, body } => {
                    self.data.graph_types.insert(name.clone(), body.clone());
                }
                RedoOp::SetCurrentGraphType { body } => {
                    self.data.set_current_graph_type(body)?;
                }
                RedoOp::CatalogDerivedConstraint { constraint } => {
                    self.data
                        .derived_constraints
                        .insert(constraint.name.clone(), constraint.clone());
                }
                RedoOp::DropConstraint { name } => {
                    self.data.derived_constraints.remove(name);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl TheaterData {
    /// Install `spec` as the current graph type and re-derive its constraint
    /// index in one step. The single "set graph type => re-derive constraints"
    /// path shared by committed replay and snapshot restore. Total over
    /// persisted bytes: a deserialized spec never ran the body validator,
    /// so a poisoned default value is the typed storage-corruption error
    /// (`persisted_derive_error`), never a panic.
    pub(crate) fn set_current_graph_type(
        &mut self,
        spec: &crate::graph_type::GraphTypeSpec,
    ) -> Result<()> {
        // Derive first, install second: a failed derivation leaves the
        // theater untouched (never the poisoned spec with its stale
        // constraint index) -- the caller's error path sees the old state.
        let derived = crate::graph_type::derived_constraints_map(&spec.to_body())
            .map_err(crate::graph_type::persisted_derive_error)?;
        self.current_graph_type = spec.clone();
        self.derived_constraints = derived;
        Ok(())
    }
}

impl Graph {
    /// Capture the committed checkpoint metadata at the caller's checkpoint cut.
    #[doc(hidden)]
    pub fn checkpoint_theater(&self) -> CheckpointTheater {
        CheckpointTheater {
            // The on-disk image keeps the historical string keys; the
            // in-memory table is `Namespace`-keyed.
            databases: self
                .databases()
                .snapshot()
                .into_iter()
                .map(|(ns, entry)| (ns.as_str().to_string(), entry))
                .collect(),
            data: self.theater.lock().unwrap().clone(),
            legacy_unknown_constraints: self
                .legacy_unenforced
                .lock()
                .unwrap()
                .clone()
                .into_iter()
                .collect(),
        }
    }

    /// Validate a typed checkpoint image against its named catalog before
    /// installing it; a missing enforced label must never relax a constraint.
    #[doc(hidden)]
    pub fn restore_checkpoint_theater(&self, image: CheckpointTheater) -> Result<()> {
        self.restore_checkpoint_theater_image(image, true)
    }

    /// CIP-176 spec string for the JSON snapshot codec (ADR-0050). A
    /// deserialized default value the canonical renderer cannot render is
    /// the storage-corruption error family the checkpoint path uses for
    /// the same persisted bytes (typed, never a panic).
    pub fn graph_type_spec(&self) -> Result<Option<String>> {
        let model = self.theater.lock().unwrap().current_graph_type.clone();
        if model == Default::default() {
            return Ok(None);
        }
        crate::graph_type::canonicalize_graph_type_body(&model.to_body())
            .map_err(crate::graph_type::persisted_derive_error)
            .map(Some)
    }

    /// Restore a CypherLite GraphData `graph_type` spec after snapshot load.
    pub fn restore_graph_type_spec(&self, spec: Option<&str>) -> Result<()> {
        let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        let inner = raw
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(raw);
        let body = cypherlite_parser::parse_graph_type_body(inner)
            .map_err(|e| Error::storage(format!("invalid graph_type spec: {e}")))?;
        // Install without re-validation, exactly as WAL replay and
        // db.ontology.apply do (ADR-0388): the store's preview set is not
        // resolved yet at load time (configure_preview runs after), so a
        // gated construct a legitimately gated-opened store wrote must
        // restore unchanged. Poisoned bytes still fail typed here: the
        // derivation below renders every default it stores (the family's
        // persisted message, round 8's rust Medium fix).
        let spec = crate::graph_type::GraphTypeSpec::from_body(&body)
            .map_err(crate::graph_type::persisted_validation_error)?;
        let mut theater = self.theater.lock().unwrap();
        theater.set_current_graph_type(&spec)?;
        Ok(())
    }

    /// Produce a validated typed image, preserving exact legacy-unknown
    /// definitions alongside all known committed theater metadata.
    #[doc(hidden)]
    pub fn checkpoint_theater_for_write(&self) -> Result<CheckpointTheater> {
        let image = self.checkpoint_theater();
        self.validate_checkpoint_theater_image(&image, true)?;
        Ok(image)
    }

    /// Record historical missing enforcement only after loading an untyped source.
    #[doc(hidden)]
    pub fn mark_legacy_unenforced(&self) {
        let image = self.checkpoint_theater();
        let catalog = self.catalog.lock().unwrap();
        let mut legacy = self.legacy_unenforced.lock().unwrap();
        for def in catalog
            .constraints
            .values()
            .filter(|d| requires_enforced_label(d))
        {
            if !image
                .data
                .derived_constraints
                .get(&def.name)
                .is_some_and(|c| matching_enforced_label(def, c))
            {
                legacy.insert(def.name.clone(), def.clone());
            }
        }
    }

    fn restore_checkpoint_theater_image(
        &self,
        image: CheckpointTheater,
        require_complete: bool,
    ) -> Result<()> {
        self.validate_checkpoint_theater_image(&image, require_complete)?;
        *self.legacy_unenforced.lock().unwrap() = image
            .legacy_unknown_constraints
            .iter()
            .map(|(name, def)| (name.clone(), def.clone()))
            .collect();
        // ADR-0116 §8: an older store's image may carry `CREATE
        // DATABASE` theater rows; the boundary conversion keeps only the
        // fixed namespaces so the next checkpoint image no longer
        // contains them.
        let databases = DatabasesCatalog::sanitize(&image.databases);
        // ADR-0276: the databases table is store-level — install it
        // into the shared catalog every namespace graph reads.
        self.databases().install(databases);
        // Checkpoint rows feed only data labels and types; the DDL names live
        // in this theater image. Record them through the same catalog sets so
        // a name that existed only in an index or constraint survives a
        // restart exactly as it did before one (ADR-0363 decision 5).
        for detail in image.data.index_details.values() {
            self.record_schema_names(detail.relationship, detail.labels.iter().cloned());
        }
        for constraint in image.data.derived_constraints.values() {
            if let Some(enforced) = &constraint.enforced_label {
                self.record_schema_names(false, std::iter::once(enforced.clone()));
            }
        }
        *self.theater.lock().unwrap() = image.data;
        self.refresh_index_charge();
        Ok(())
    }

    fn validate_checkpoint_theater_image(
        &self,
        image: &CheckpointTheater,
        require_complete: bool,
    ) -> Result<()> {
        let catalog = self.catalog.lock().unwrap();
        if !image.legacy_unknown_constraints.is_empty() && !require_complete {
            return Err(Error::storage(
                "legacy checkpoint theater cannot carry unknown markers",
            ));
        }
        for (name, def) in &image.legacy_unknown_constraints {
            if name != &def.name
                || !requires_enforced_label(def)
                || catalog.constraints.get(name) != Some(def)
                || image.data.derived_constraints.contains_key(name)
            {
                return Err(Error::storage(
                    "checkpoint legacy-unknown constraint does not match its named definition",
                ));
            }
        }
        for def in catalog
            .constraints
            .values()
            .filter(|d| requires_enforced_label(d))
        {
            if !image
                .data
                .derived_constraints
                .get(&def.name)
                .is_some_and(|c| matching_enforced_label(def, c))
            {
                if image.legacy_unknown_constraints.get(&def.name) == Some(def)
                    && !image.data.derived_constraints.contains_key(&def.name)
                {
                    continue;
                }
                if require_complete {
                    return Err(Error::storage(
                        "checkpoint named label constraint is missing matching enforced label",
                    ));
                }
            }
        }
        for expected in
            crate::graph_type::derive_constraints(&image.data.current_graph_type.to_body())
                .map_err(crate::graph_type::persisted_derive_error)?
        {
            if image.data.derived_constraints.get(&expected.name) != Some(&expected) {
                return Err(Error::storage(
                    "checkpoint graph type is missing a derived constraint",
                ));
            }
        }
        for (name, constraint) in &image.data.derived_constraints {
            if name != &constraint.name
                || (constraint.entity_type != "NODE" && constraint.entity_type != "RELATIONSHIP")
            {
                return Err(Error::storage(
                    "invalid checkpoint derived constraint identity",
                ));
            }
            let label_kind = constraint.constraint_type.is_enforced_label_kind();
            let relationship = constraint.entity_type == "RELATIONSHIP";
            let wrong_entity = constraint
                .constraint_type
                .enforced_label_site()
                .is_some_and(|site| site.is_relationship() != relationship);
            if wrong_entity {
                return Err(Error::storage(
                    "checkpoint enforced-label entity type mismatch",
                ));
            }
            if label_kind
                && constraint
                    .enforced_label
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                return Err(Error::storage(
                    "checkpoint constraint is missing its enforced label",
                ));
            }
            if constraint.enforced_label.is_some() {
                let matches = catalog.constraints.values().any(|def| {
                    (def.name == *name || !catalog.constraints.contains_key(name))
                        && def.label == constraint.label
                        && def.relationship == relationship
                        && def.properties == constraint.properties
                        && def.kind == constraint.constraint_type.as_str()
                        && def.property_type == constraint.property_type
                });
                if !matches || !label_kind {
                    return Err(Error::storage(
                        "invalid checkpoint enforced-label constraint",
                    ));
                }
            }
        }
        drop(catalog);
        for name in image
            .data
            .index_details
            .keys()
            .chain(image.data.index_options.keys())
        {
            if self.named_index(name).is_none() {
                return Err(Error::storage("checkpoint index detail has no named index"));
            }
        }
        Ok(())
    }

    /// Recover the legacy GraphData enforced-label field when no typed theater
    /// image exists. Base definitions must already be installed.
    #[doc(hidden)]
    pub fn restore_legacy_enforced_constraints(
        &self,
        constraints: Vec<cypherlite_model::Constraint>,
    ) -> Result<()> {
        let mut image = self.checkpoint_theater();
        for constraint in constraints {
            image
                .data
                .derived_constraints
                .insert(constraint.name.clone(), constraint);
        }
        // Legacy GraphData may omit theater entirely; validate supplied entries
        // without imposing the completeness guarantee of a typed image.
        self.restore_checkpoint_theater_image(image, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redo::{IndexDetailSpec, IndexKindSpec};

    fn range_index_detail() -> IndexDetailSpec {
        IndexDetailSpec {
            kind: IndexKindSpec::Range,
            relationship: false,
            labels: vec!["N".into()],
            properties: vec!["p".into()],
            with_properties: Vec::new(),
        }
    }

    fn admitted_show() -> ShowSchema {
        ShowSchema::capture(&DatabasesCatalog::default(), &TheaterData::default())
    }

    #[test]
    fn apply_overlay_propagates_corrupt_index_options() {
        let mut show = admitted_show();
        let ops = vec![RedoOp::CatalogIndexDetail {
            name: "idx".into(),
            detail: range_index_detail(),
            options: serde_json::json!("corrupted"),
        }];
        // A decode failure is an error, never a silently dropped index entry
        // (matching `Graph::apply_redo`).
        assert!(show.apply_overlay(&ops).is_err());
        assert!(show.data.index_details.is_empty());
        assert!(show.data.index_options.is_empty());
    }

    #[test]
    fn apply_overlay_installs_valid_index_options() {
        let mut show = admitted_show();
        let ops = vec![RedoOp::CatalogIndexDetail {
            name: "idx".into(),
            detail: range_index_detail(),
            options: serde_json::json!({}),
        }];
        show.apply_overlay(&ops).unwrap();
        assert!(show.data.index_details.contains_key("idx"));
        assert!(show.data.index_options.contains_key("idx"));
    }

    /// ADR-0116 §1/§8/§10: the default store-level catalog carries exactly
    /// the three fixed namespaces.
    #[test]
    fn default_catalog_has_exactly_the_fixed_namespaces() {
        let catalog = DatabasesCatalog::default();
        let snapshot = catalog.snapshot();
        let names: Vec<Namespace> = snapshot.keys().copied().collect();
        assert_eq!(names, Namespace::ALL);
    }

    /// ADR-0116 §8: `PutDatabase` redo for a non-fixed name (an older
    /// store's `CREATE DATABASE` theater) is dropped at the replay
    /// boundary; fixed names match case-insensitively and store under the
    /// typed key, and a `DropDatabase` redo is a no-op because the fixed
    /// namespaces can never be dropped. The speculative overlay and the
    /// committed store-level catalog apply the same rule.
    #[test]
    fn database_ops_keep_only_the_fixed_namespaces() {
        let ops = [
            RedoOp::PutDatabase {
                name: "analytics".into(),
                language: 5,
                composite: false,
            },
            RedoOp::PutDatabase {
                name: "Neo4j".into(),
                language: 5,
                composite: false,
            },
            RedoOp::DropDatabase {
                name: "system".into(),
            },
        ];
        let mut show = admitted_show();
        show.apply_overlay(&ops).unwrap();
        let names: Vec<Namespace> = show.databases.keys().copied().collect();
        assert_eq!(names, Namespace::ALL);
        assert_eq!(
            show.databases[&Namespace::Neo4j],
            DatabaseEntry {
                default_language: 5,
                composite: false,
            }
        );

        let catalog = DatabasesCatalog::default();
        for op in &ops {
            match op {
                RedoOp::PutDatabase {
                    name,
                    language,
                    composite,
                } => catalog.put_database(name, *language, *composite),
                RedoOp::DropDatabase { .. } => {}
                _ => unreachable!(),
            }
        }
        assert_eq!(catalog.snapshot(), show.databases);
    }

    /// ADR-0116 §8: the boundary conversion for a checkpoint image drops
    /// non-fixed entries and fills any missing fixed namespace with its
    /// default.
    #[test]
    fn sanitize_databases_drops_non_fixed_and_fills_defaults() {
        let mut map = BTreeMap::new();
        map.insert(
            "analytics".to_string(),
            DatabaseEntry {
                default_language: 5,
                composite: false,
            },
        );
        map.insert(
            "NEO4J".to_string(),
            DatabaseEntry {
                default_language: 5,
                composite: true,
            },
        );
        let fixed = DatabasesCatalog::sanitize(&map);
        let names: Vec<Namespace> = fixed.keys().copied().collect();
        assert_eq!(names, Namespace::ALL);
        assert_eq!(
            fixed[&Namespace::Neo4j],
            DatabaseEntry {
                default_language: 5,
                composite: true,
            }
        );
        assert_eq!(
            fixed[&Namespace::System],
            DatabasesCatalog::DEFAULT_DATABASE_ENTRY
        );
        assert_eq!(
            fixed[&Namespace::SystemOntologies],
            DatabasesCatalog::DEFAULT_DATABASE_ENTRY
        );
    }
}
