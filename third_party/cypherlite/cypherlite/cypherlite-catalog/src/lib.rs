use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;

use cypherlite_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Named `(label, property)` catalog entry (ADR-0040).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub label: String,
    pub property: String,
}

impl IndexDef {
    /// The index's `:Label(property)` schema text — the one spelling the
    /// schema-naming reads render (the plan tree's `AllNodesScan`
    /// suggestion, the warnings that name an existing index, the store
    /// manifest's index lines), never a per-site `format!`. The property renders its bare name:
    /// `IndexDef.property` is dual-encoded in the catalog (bare for
    /// `CREATE INDEX` rows, legacy `{property}#{KIND}#{name}` for
    /// constraint-backed index rows), and this text names the schema,
    /// not the encoding — the head of the encoding, which is the whole
    /// property when it is bare. The executor's `redo` module owns the
    /// full typed decoder (`split_legacy_property_encoding`); this is
    /// its head half, the one fact the catalog's own render reads: the
    /// schema text never shows the legacy storage encoding.
    pub fn index_text(&self) -> String {
        let property = self
            .property
            .split('#')
            .next()
            .expect("a split yields a head");
        format!(":{}({})", self.label, property)
    }
}

/// Named uniqueness / existence constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintDef {
    pub name: String,
    #[serde(default)]
    pub relationship: bool,
    pub label: String,
    pub properties: Vec<String>,
    pub kind: String,
    #[serde(default)]
    pub property_type: Option<String>,
}

/// Whether a constraint kind tag is backed by a range index — the single
/// `&str`-boundary predicate for the stringly-typed kind: the tag is
/// parsed via [`cypherlite_model::ConstraintType`] and mapped to
/// [`cypherlite_model::ConstraintType::is_index_backed`]. An unparseable
/// tag is not index-backed (fail closed). The free-function form of
/// [`ConstraintDef::is_index_backed`], for callers holding a bare kind
/// tag rather than a stored definition.
pub fn constraint_kind_is_index_backed(kind: &str) -> bool {
    cypherlite_model::ConstraintType::from_str(kind).is_ok_and(|kind| kind.is_index_backed())
}

impl ConstraintDef {
    /// Whether this constraint's kind is backed by a range index —
    /// delegates to [`constraint_kind_is_index_backed`], the single
    /// `&str`-boundary predicate for the stringly-typed `kind`.
    pub fn is_index_backed(&self) -> bool {
        constraint_kind_is_index_backed(&self.kind)
    }

    /// The constraint-equivalence predicate: same entity kind
    /// (node/relationship), label, property set, constraint kind, and
    /// property type. The one place this five-field rule lives.
    pub fn schema_equivalent(
        &self,
        relationship: bool,
        label: &str,
        properties: &[String],
        kind: &str,
        property_type: Option<&str>,
    ) -> bool {
        self.relationship == relationship
            && self.label == label
            && self.properties == properties
            && self.kind == kind
            && self.property_type.as_deref() == property_type
    }
}

/// In-memory catalog of labels, relationship types, and named indexes.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub labels: BTreeSet<String>,
    pub rel_types: BTreeSet<String>,
    pub indexes: BTreeMap<String, IndexDef>,
    pub constraints: BTreeMap<String, ConstraintDef>,
}

impl Catalog {
    pub fn record_labels(&mut self, labels: impl IntoIterator<Item = String>) {
        self.labels.extend(labels);
    }

    pub fn record_rel_type(&mut self, t: impl Into<String>) {
        self.rel_types.insert(t.into());
    }

    /// Record one DDL-named graph catalog name in the namespace its entity
    /// kind selects (ADR-0363 decision 5): `relationship` names a
    /// relationship type, otherwise a node label. The one entity-kind split
    /// every schema-object insertion and redo replay shares.
    pub fn record_schema_name(&mut self, relationship: bool, name: String) {
        if relationship {
            self.record_rel_type(name);
        } else {
            self.record_labels([name]);
        }
    }

    pub fn index_by_schema(&self, label: &str, property: &str) -> Option<&IndexDef> {
        self.indexes
            .values()
            .find(|i| i.label == label && i.property == property)
    }

    pub fn insert_index(&mut self, def: IndexDef) -> Result<()> {
        // A named index's subject is a graph-catalog fact (ADR-0363
        // decision 5): a replaying insert records it, and the catalog's name
        // sets are monotone — a later `DROP INDEX` keeps the name, like a
        // label outlives its last node. `IndexDef` carries no entity kind, so
        // its subject is recorded as a node label here; the typed
        // `CatalogIndexDetail` additionally records a relationship index's
        // type in the relationship-type set. A rejected insert records
        // nothing.
        if let Some(existing) = self.indexes.get(&def.name) {
            if existing.label == def.label && existing.property == def.property {
                self.record_schema_name(false, def.label.clone());
                return Ok(());
            }
            return Err(Error::semantic(format!(
                "IndexAlreadyExists: `{}` already exists",
                def.name
            )));
        }
        if let Some(other) = self.index_by_schema(&def.label, &def.property) {
            return Err(Error::semantic(format!(
                "EquivalentSchemaRuleAlreadyExists: (:{} {{{}}}) already indexed as `{}`",
                def.label, def.property, other.name
            )));
        }
        self.record_schema_name(false, def.label.clone());
        self.indexes.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str) -> bool {
        self.indexes.remove(name).is_some()
    }

    pub fn insert_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        // A constraint's subject is a graph-catalog fact in the namespace its
        // entity kind selects (ADR-0363 decision 5), recorded by a replaying
        // identical insert too so an older store still learns it. Monotone: a
        // later `DROP CONSTRAINT` keeps the name. A rejected insert records
        // nothing.
        if let Some(existing) = self.constraints.get(&def.name) {
            if existing == &def {
                self.record_schema_name(def.relationship, def.label.clone());
                return Ok(());
            }
            return Err(Error::semantic(format!(
                "ConstraintAlreadyExists: `{}` already exists",
                def.name
            )));
        }
        // A label/endpoint-label existence kind's identity includes its
        // enforced label, which the catalog def does not carry (it lives
        // in the theater's derived-constraint index): two same-label
        // existence constraints with different enforced labels are two
        // rules, not a duplicate (CIP-266's multi-parent
        // `(:A => :B&C)` derives two). Every other kind compares as
        // before.
        let identity_includes_enforced_label =
            cypherlite_model::ConstraintType::from_str(&def.kind)
                .is_ok_and(|kind| kind.enforced_label_site().is_some());
        if !identity_includes_enforced_label
            && self.constraints.values().any(|existing| {
                existing.relationship == def.relationship
                    && existing.label == def.label
                    && existing.properties == def.properties
                    && existing.kind == def.kind
            })
        {
            return Err(Error::semantic(
                "EquivalentSchemaRuleAlreadyExists: equivalent constraint exists",
            ));
        }
        self.record_schema_name(def.relationship, def.label.clone());
        self.constraints.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn drop_constraint(&mut self, name: &str) -> bool {
        self.constraints.remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let mut c = Catalog::default();
        c.record_labels(["Person".into()]);
        c.record_rel_type("KNOWS");
        assert!(c.labels.contains("Person"));
        assert!(c.rel_types.contains("KNOWS"));
    }

    #[test]
    fn constraint_def_is_index_backed_parses_kind_at_str_boundary() {
        let def = |kind: &str| ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: kind.into(),
            property_type: None,
        };
        assert!(def("unique").is_index_backed());
        assert!(def("key").is_index_backed());
        assert!(!def("exists").is_index_backed());
        assert!(!def("propertyType").is_index_backed());
        assert!(!def("labelExistence").is_index_backed());
        assert!(!def("nodeLabelExistence").is_index_backed());
        assert!(!def("sourceLabel").is_index_backed());
        assert!(!def("sourceLabelExistence").is_index_backed());
        assert!(!def("targetLabel").is_index_backed());
        assert!(!def("targetLabelExistence").is_index_backed());
        // An unparseable tag fails closed: not index-backed.
        assert!(!def("not-a-kind").is_index_backed());
        assert!(!def("").is_index_backed());
    }

    #[test]
    fn constraint_def_is_index_backed_delegates_to_the_free_fn() {
        let def = |kind: &str| ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: kind.into(),
            property_type: None,
        };
        for kind in [
            "unique",
            "key",
            "exists",
            "propertyType",
            "labelExistence",
            "nodeLabelExistence",
            "sourceLabel",
            "sourceLabelExistence",
            "targetLabel",
            "targetLabelExistence",
            "not-a-kind",
            "",
        ] {
            assert_eq!(
                def(kind).is_index_backed(),
                constraint_kind_is_index_backed(kind),
                "{kind}"
            );
        }
    }

    #[test]
    fn constraint_def_schema_equivalent_compares_all_five_fields() {
        let def = ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: "unique".into(),
            property_type: Some("STRING".into()),
        };
        let props = vec!["p".to_string()];
        assert!(def.schema_equivalent(false, "L", &props, "unique", Some("STRING")));
        // The constraint's own name is not part of the equivalence rule.
        assert!(ConstraintDef {
            name: "other".into(),
            ..def.clone()
        }
        .schema_equivalent(false, "L", &props, "unique", Some("STRING")));
        // Flip each of the five fields in turn: no longer equivalent.
        assert!(!def.schema_equivalent(true, "L", &props, "unique", Some("STRING")));
        assert!(!def.schema_equivalent(false, "M", &props, "unique", Some("STRING")));
        assert!(!def.schema_equivalent(false, "L", &["q".to_string()], "unique", Some("STRING")));
        assert!(!def.schema_equivalent(false, "L", &props, "key", Some("STRING")));
        assert!(!def.schema_equivalent(false, "L", &props, "unique", None));
        assert!(!def.schema_equivalent(false, "L", &props, "unique", Some("INTEGER")));
        // `None` on the stored definition matches only a `None` query.
        let untyped = ConstraintDef {
            property_type: None,
            ..def
        };
        assert!(untyped.schema_equivalent(false, "L", &props, "unique", None));
        assert!(!untyped.schema_equivalent(false, "L", &props, "unique", Some("STRING")));
    }

    #[test]
    fn named_index_unique_name_and_schema() {
        let mut c = Catalog::default();
        c.insert_index(IndexDef {
            name: "person_name".into(),
            label: "Person".into(),
            property: "name".into(),
        })
        .unwrap();
        c.insert_index(IndexDef {
            name: "person_name".into(),
            label: "Person".into(),
            property: "name".into(),
        })
        .unwrap();
        assert!(c
            .insert_index(IndexDef {
                name: "person_name".into(),
                label: "Person".into(),
                property: "age".into(),
            })
            .is_err());
        assert!(c
            .insert_index(IndexDef {
                name: "other".into(),
                label: "Person".into(),
                property: "name".into(),
            })
            .is_err());
        assert!(c.drop_index("person_name"));
        assert!(!c.drop_index("person_name"));
    }

    /// ADR-0363 decision 5: inserting a named schema object records its
    /// subject in the graph-catalog namespace the object's model selects — a
    /// node constraint's label or a relationship constraint's type — and a
    /// rejected insert records nothing.
    #[test]
    fn insert_records_schema_subject_names() {
        let mut c = Catalog::default();
        let constraint = |name: &str, relationship: bool, label: &str| ConstraintDef {
            name: name.into(),
            relationship,
            label: label.into(),
            properties: vec!["p".into()],
            kind: "unique".into(),
            property_type: None,
        };

        c.insert_index(IndexDef {
            name: "i".into(),
            label: "Indexed".into(),
            property: "k".into(),
        })
        .unwrap();
        c.insert_constraint(constraint("nc", false, "NodeC"))
            .unwrap();
        c.insert_constraint(constraint("rc", true, "RelC")).unwrap();

        assert!(c.labels.contains("Indexed"));
        assert!(c.labels.contains("NodeC"));
        assert!(c.rel_types.contains("RelC"));
        assert!(!c.labels.contains("RelC"));

        c.insert_index(IndexDef {
            name: "i".into(),
            label: "Other".into(),
            property: "k".into(),
        })
        .unwrap_err();
        c.insert_constraint(constraint("nc", false, "Other"))
            .unwrap_err();
        assert!(!c.labels.contains("Other"));
    }
}
