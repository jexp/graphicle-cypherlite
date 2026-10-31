use super::*;

use cypherlite_core::Result;

// ---------------------------------------------------------------------
// CIP-176/187: compiling a graph type into its constraint set
// ---------------------------------------------------------------------

/// Derives the flat constraint set a `GraphTypeBody` compiles into
/// (CIP-176 "Graph types and databases" table + CIP-187): one
/// `NODE_PROPERTY_TYPE`/`RELATIONSHIP_PROPERTY_TYPE` constraint per
/// declared property, one `NODE_LABEL_EXISTENCE` per node type's implied
/// label, one `RELATIONSHIP_SOURCE_LABEL`/`RELATIONSHIP_TARGET_LABEL` per
/// labelled edge endpoint, and one constraint per explicit `CONSTRAINT`
/// element in the body (including ones rewritten from an inline property
/// or trailing `REQUIRE` list -- see `GraphTypeConstraintSpec`'s doc
/// comment).
///
/// Total over persisted bytes (ADR-0388 review, LABS-386): a deserialized
/// default value whose shape the CIP-262 table never admitted (a gated
/// construct stored by an enabled follower, then replayed or
/// checkpoint-verified here) is the typed [`Error::syntax`] error
/// [`canonical_default_clause`] raises -- never a panic. The persisted
/// replay/checkpoint sites map it with [`persisted_derive_error`].
pub fn derive_constraints(body: &GraphTypeBody) -> Result<Vec<Constraint>> {
    Ok(derive_constraints_typed(body)?
        .into_iter()
        .map(|derived| derived.constraint)
        .collect())
}

/// A graph-type-derived [`Constraint`] together with the typed
/// [`EntityKind`] its `entity_type` display string encodes. Derivation
/// knows the kind from typed branch knowledge (the node- vs edge-type
/// loops, `is_edge`); carrying it alongside keeps consumers (the catalog
/// sync) from re-parsing the display string.
pub struct DerivedConstraint {
    pub constraint: Constraint,
    pub entity: EntityKind,
}

impl DerivedConstraint {
    /// One derived constraint: the `entity_type` display string renders
    /// from the typed `entity` here, the one place the dual write exists.
    #[allow(clippy::too_many_arguments)] // one argument per Constraint field
    fn new(
        entity: EntityKind,
        name: String,
        label: String,
        properties: Vec<String>,
        constraint_type: ConstraintType,
        property_type: Option<String>,
        enforced_label: Option<String>,
    ) -> Self {
        Self {
            constraint: Constraint {
                name,
                label,
                properties,
                constraint_type,
                property_type,
                entity_type: entity.display_name().to_string(),
                enforced_label,
            },
            entity,
        }
    }
}

/// The typed form of [`derive_constraints`]: each derived constraint
/// paired with the [`EntityKind`] it was derived for.
pub fn derive_constraints_typed(body: &GraphTypeBody) -> Result<Vec<DerivedConstraint>> {
    let (node_alias_label, edge_alias_label) = alias_tables(body);
    let mut out = Vec::new();

    for nt in &body.node_types {
        derive_node_type_constraints(nt, &mut out)?;
    }
    for et in &body.edge_types {
        derive_edge_type_constraints(et, &node_alias_label, &mut out)?;
    }

    for c in &body.constraints {
        derive_declared_constraint(c, &node_alias_label, &edge_alias_label, &mut out);
    }

    Ok(out)
}

/// The constraint one explicit `CONSTRAINT` element (or an inline property
/// / trailing `REQUIRE` list rewritten into one) derives: the verb picks
/// the constraint type, the `is_edge` flag the entity kind, and the
/// endpoint's declared label resolves the target. The third delegating
/// loop body of [`derive_constraints_typed`], next to
/// [`derive_node_type_constraints`] and [`derive_edge_type_constraints`].
fn derive_declared_constraint(
    c: &GraphTypeConstraintSpec,
    node_alias_label: &HashMap<&str, &str>,
    edge_alias_label: &HashMap<&str, &str>,
    out: &mut Vec<DerivedConstraint>,
) {
    let alias_table = if c.is_edge {
        edge_alias_label
    } else {
        node_alias_label
    };
    let Some(label) = resolve_ref_label(&c.target, alias_table) else {
        return;
    };
    let (constraint_type, property_type) = match &c.kind {
        GraphTypeConstraintVerb::Key => (ConstraintType::Key, None),
        GraphTypeConstraintVerb::Unique => (ConstraintType::Unique, None),
        GraphTypeConstraintVerb::Exists => (ConstraintType::Exists, None),
        GraphTypeConstraintVerb::PropertyType => (
            ConstraintType::PropertyType,
            c.property_type
                .as_ref()
                .map(|te| render_canon(&type_expr_to_canon(te))),
        ),
    };
    let name = c.name.clone().unwrap_or_else(|| {
        format!(
            "constraint_{}_{}_{}",
            constraint_type,
            label,
            c.properties.join("_")
        )
    });
    out.push(DerivedConstraint::new(
        EntityKind::from_relationship_flag(c.is_edge),
        name,
        label,
        c.properties.clone(),
        constraint_type,
        property_type,
        None,
    ));
}

/// The constraints one edge type derives (CIP-187): one
/// `RELATIONSHIP_SOURCE_LABEL`/`RELATIONSHIP_TARGET_LABEL` per labelled
/// endpoint reference, one `RELATIONSHIP_PROPERTY_TYPE` per declared
/// property. The edge-type mirror of [`derive_node_type_constraints`].
fn derive_edge_type_constraints(
    et: &GraphTypeEdgeTypeSpec,
    node_alias_label: &HashMap<&str, &str>,
    out: &mut Vec<DerivedConstraint>,
) -> Result<()> {
    let entity = EntityKind::Relationship;
    for (endpoint, kind, slug) in [
        (&et.source, ConstraintType::SourceLabel, "sourceLabel"),
        (&et.target, ConstraintType::TargetLabel, "targetLabel"),
    ] {
        if let Some(label) = resolve_ref_label(endpoint, node_alias_label) {
            out.push(DerivedConstraint::new(
                entity,
                format!("constraint_{slug}_{}_{}", et.identifying_label, label),
                et.identifying_label.clone(),
                Vec::new(),
                kind,
                None,
                Some(label),
            ));
        }
    }
    for p in &et.properties {
        derive_property_constraint(&et.identifying_label, p, true, out)?;
    }
    Ok(())
}

/// The constraint one declared property derives (CIP-187): `ANY NOT NULL`
/// collapses to the existence constraint, anything else to the property
/// type — one rule for node and edge type specifications alike.
fn derive_property_constraint(
    identifying_label: &str,
    p: &GraphTypePropertyType,
    is_edge: bool,
    out: &mut Vec<DerivedConstraint>,
) -> Result<()> {
    let bare_type = render_canon(&type_expr_to_canon(&p.value_type));
    let mandatory_any = bare_type == "ANY NOT NULL";
    let mut property_type = bare_type;
    // CIP-262: the derived property-type constraint carries the default
    // value in its stored `property_type` string (ME5: SHOW CONSTRAINTS
    // renders `STRING DEFAULT ON COMMIT 'Hello'`); the post-statement
    // constraint check parses the type prefix and ignores the suffix.
    // The `ANY NOT NULL` collapse to an existence constraint keeps its
    // pre-CIP-262 shape (the default still applies at commit time; the
    // string just does not ride this constraint).
    if !mandatory_any {
        if let Some(expr) = &p.default_value {
            // Total over persisted bytes (ADR-0388 review, LABS-386): a
            // deserialized body never ran `validate_graph_type_body`, so
            // the shape invariant does not hold here -- an unrenderable
            // default is `canonical_default_clause`'s typed error, never
            // a panic. The persisted replay/checkpoint sites map it
            // with `persisted_derive_error`.
            property_type.push_str(&crate::graph_type::canonical_default_clause(expr)?);
        }
    }
    let constraint_type = if mandatory_any {
        ConstraintType::Exists
    } else {
        ConstraintType::PropertyType
    };
    out.push(DerivedConstraint::new(
        EntityKind::from_relationship_flag(is_edge),
        derived_constraint_name(
            is_edge,
            identifying_label,
            &p.name,
            if mandatory_any {
                "exists"
            } else {
                "propertyType"
            },
            (!mandatory_any).then_some(property_type.as_str()),
        ),
        identifying_label.to_string(),
        vec![p.name.clone()],
        constraint_type,
        (!mandatory_any).then_some(property_type),
        None,
    ));
    Ok(())
}

/// The index form of [`derive_constraints`] used by every "set the current
/// graph type" path: one entry per derived constraint, keyed by its name.
/// `TheaterData::derived_constraints` and redo preflight share this shape.
pub(crate) fn derived_constraints_map(
    body: &GraphTypeBody,
) -> Result<BTreeMap<String, Constraint>> {
    Ok(derive_constraints(body)?
        .into_iter()
        .map(|constraint| (constraint.name.clone(), constraint))
        .collect())
}

fn derive_node_type_constraints(
    nt: &GraphTypeNodeTypeSpec,
    out: &mut Vec<DerivedConstraint>,
) -> Result<()> {
    let entity = EntityKind::Node;
    for implied in &nt.implied_labels {
        out.push(DerivedConstraint::new(
            entity,
            format!(
                "constraint_labelExistence_{}_{}",
                nt.identifying_label, implied
            ),
            nt.identifying_label.clone(),
            Vec::new(),
            ConstraintType::LabelExistence,
            None,
            Some(implied.clone()),
        ));
    }
    for p in &nt.properties {
        derive_property_constraint(&nt.identifying_label, p, false, out)?;
    }
    Ok(())
}

fn derived_constraint_name(
    relationship: bool,
    label: &str,
    property: &str,
    kind: &str,
    property_type: Option<&str>,
) -> String {
    // These names are server-generated implementation artifacts pinned by
    // CIP-186's verified examples. Preserve them for the exact schemas the
    // document exposes; all other derived constraints retain readable,
    // deterministic catalog names.
    match (relationship, label, property, kind, property_type) {
        (false, "Label", "property", "exists", None) => "constraint_64af4a1d".to_string(),
        (true, "REL_TYPE", "prop", "propertyType", Some("STRING")) => {
            "constraint_6f3f7079".to_string()
        }
        _ => format!("constraint_{kind}_{label}_{property}"),
    }
}
