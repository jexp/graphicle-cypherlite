//! Schema-mutating catalog commands (indexes, constraints, databases,
//! graph types) extracted from `execute_catalog`. Each returns the
//! statement's final rows.

use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;
use std::str::FromStr;

use cypherlite_core::{Error, Result, Value};
use cypherlite_model::ConstraintType;

use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;
use crate::redo::RedoOp;

use super::catalog::schema_name_value;

/// The `IF NOT EXISTS` fork every existence probe shares: an existing
/// rule either returns the statement's rows (the clause asked to tolerate
/// it) or carries the probe's error.
/// The `IF NOT EXISTS` fork every probe shares: an existing rule either
/// returns the statement's rows (the clause asked to tolerate it) or
/// carries the probe's error. Takes the rows by value — every caller is
/// an early return.
fn exists_result(
    c: &cypherlite_parser::CreateIndexClause,
    rows: Vec<Row>,
    err: Error,
) -> Result<Vec<Row>> {
    if c.if_not_exists {
        Ok(rows)
    } else {
        Err(err)
    }
}

/// The duplicated-token policy: no token repeats within its vocabulary.
fn has_duplicates<'a, T: Hash + Eq + 'a>(mut items: impl Iterator<Item = &'a T>) -> bool {
    let mut seen = HashSet::new();
    items.any(|item| !seen.insert(item))
}

pub(crate) fn catalog_create_index(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::CreateIndexClause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    let name = match &c.name {
        Some(name) => schema_name_value(name, params)?,
        None => format!(
            "{}_index_{}_{}",
            c.kind.display_name().to_ascii_lowercase(),
            c.labels.join("_"),
            c.properties.join("_")
        ),
    };
    if graph.named_index(&name).is_some() {
        return exists_result(c, rows, Error::semantic("IndexAlreadyExists"));
    }
    if c.if_not_exists
        && matches!(c.kind, cypherlite_parser::IndexKind::Range)
        && c.labels.len() == 1
        && graph.named_indexes().iter().any(|index| {
            index.label == c.labels[0]
                && crate::redo::split_legacy_property_encoding(&index.property).0
                    == c.properties.join(",")
        })
    {
        return Ok(rows);
    }
    if matches!(
        c.kind,
        cypherlite_parser::IndexKind::Point | cypherlite_parser::IndexKind::Text
    ) && c.properties.len() != 1
    {
        return Err(Error::semantic(
            "InvalidArguments: this index type requires one property",
        ));
    }
    let mut options = BTreeMap::new();
    for (key, expression) in &c.options {
        options.insert(
            key.clone(),
            eval(
                graph,
                &Row::new(),
                &cypherlite_front::lower_expr(expression)?,
                params,
                cypher_version,
            )?,
        );
    }
    if let Some(parameter) = &c.options_param {
        match params.get(parameter) {
            Some(Value::Map(map)) => options = BTreeMap::clone(map),
            // ADR-0308: an absent parameter is ParameterMissing, not a type.
            None => return Err(Error::parameters_missing([parameter.as_str()])),
            Some(_) => {
                return Err(Error::ty(
                    "InvalidInputValue: OPTIONS parameter must be a MAP",
                ))
            }
        }
    }
    if options.keys().any(|key| {
        !matches!(
            key.as_str(),
            crate::vector_similarity::KEY_INDEX_PROVIDER
                | crate::vector_similarity::KEY_INDEX_CONFIG
        )
    }) {
        return Err(Error::semantic("InvalidIndexOption"));
    }
    // Total over shape, like indexConfig below: a non-string
    // indexProvider is rejected at the same boundary, never silently
    // dropped.
    match options.get(crate::vector_similarity::KEY_INDEX_PROVIDER) {
        None => {}
        Some(Value::String(provider)) => {
            if provider.contains("btree")
                || matches!(
                    c.kind,
                    cypherlite_parser::IndexKind::Lookup
                        | cypherlite_parser::IndexKind::Vector
                        | cypherlite_parser::IndexKind::Text
                )
            {
                return Err(Error::semantic("InvalidIndexOption"));
            }
        }
        Some(_) => return Err(Error::semantic("InvalidIndexOption")),
    }
    if matches!(
        c.kind,
        cypherlite_parser::IndexKind::Range | cypherlite_parser::IndexKind::Text
    ) {
        // Range/Text consume none of an indexConfig, so a non-empty one
        // is rejected (an empty Map rides through unchanged). The other
        // non-vector kinds' exemption is the CIP-11 contract: POINT and
        // LOOKUP indexes carry their config through verbatim.
        if crate::vector_similarity::index_config(&options)?.is_some_and(|map| !map.is_empty()) {
            return Err(Error::semantic("InvalidIndexConfig"));
        }
    }
    if matches!(c.kind, cypherlite_parser::IndexKind::Vector) {
        // The vector option's one shape policy and one adjacent block:
        // navigate (Map or absent — anything else is rejected here,
        // never silently dropped), validate through the one parse,
        // apply the DDL defaults, and re-insert — nothing shuttles.
        if c.properties.len() != 1 {
            return Err(Error::syntax("UnsupportedIndexOrConstraint"));
        }
        // One spelling of the duplicated-token policy: the label and the
        // property vocabularies are separate sets, each without
        // duplicates.
        if has_duplicates(c.labels.iter())
            || has_duplicates(c.properties.iter().chain(&c.with_properties))
        {
            return Err(Error::semantic("IndexContainsDuplicatedTokens"));
        }
        options.insert(
            crate::vector_similarity::KEY_INDEX_CONFIG.into(),
            crate::vector_similarity::create_config(&options)?,
        );
    }

    let details = graph.show_schema()?.data.index_details;
    let relationship_index = c.entity_type == cypherlite_parser::SchemaEntityType::Relationship;
    let equivalent = details.values().any(|old| {
        old.kind == crate::redo::IndexKindSpec::from(c.kind)
            && old.relationship == relationship_index
            && old.labels == c.labels
            && old.properties == c.properties
            && old.with_properties == c.with_properties
    });
    if equivalent {
        return exists_result(
            c,
            rows,
            Error::semantic("EquivalentSchemaRuleAlreadyExists"),
        );
    }
    // ADR-0372: the schema-rule conflict is narrowed to serving pairs —
    // only a RANGE index can back a constraint (ADR-0339), so only a RANGE
    // index conflicts with an index-backed constraint on the same
    // entity/label/properties. FULLTEXT/VECTOR/POINT indexes coexist with
    // every constraint: they serve search, not validation, and cannot
    // serve as a backing.
    let conflicts_with_constraint = graph.named_constraints().iter().any(|constraint| {
        crate::redo::serving_pair_conflicts(
            crate::redo::ServingPairIndex {
                kind: crate::redo::IndexKindSpec::from(c.kind),
                relationship: relationship_index,
                labels: &c.labels,
                properties: &c.properties,
            },
            crate::redo::ServingPairConstraint {
                backed: constraint.is_index_backed(),
                relationship: constraint.relationship,
                label: &constraint.label,
                properties: &constraint.properties,
            },
        )
    });
    if conflicts_with_constraint {
        return exists_result(c, rows, Error::semantic("IndexWithNameAlreadyExists"));
    }
    let label = c.labels.first().cloned().unwrap_or_default();
    let property = format!(
        "{}#{}#{}",
        c.properties.join(","),
        c.kind.display_name(),
        name
    );
    let detail = crate::redo::IndexDetailSpec {
        kind: c.kind.into(),
        relationship: c.entity_type == cypherlite_parser::SchemaEntityType::Relationship,
        labels: c.labels.clone(),
        properties: c.properties.clone(),
        with_properties: c.with_properties.clone(),
    };
    graph.push_catalog_redo(RedoOp::CatalogIndexDetail {
        name: name.clone(),
        detail,
        options: crate::redo::props_json(&options),
    });
    graph.create_named_index(name, label, property)?;
    Ok(rows)
}

pub(crate) fn catalog_drop_index(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::DropIndexClause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>> {
    let name = schema_name_value(&c.name, params)?;
    if graph.named_index(&name).is_none() {
        if c.if_exists {
            return Ok(rows);
        }
        return Err(Error::semantic("IndexNotFound"));
    }
    graph.drop_named_index(name)?;
    Ok(rows)
}

pub(crate) fn catalog_create_constraint(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::CreateConstraintClause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    if c.property_type.is_some() && c.properties.len() != 1 {
        return Err(Error::syntax(
            "InvalidConstraintSyntax: only single property IS TYPED constraints are supported",
        ));
    }
    // Parse the constraint kind tag once, up front: an unknown tag is a
    // semantic error (fail closed), so the `CatalogDerivedConstraint` redo
    // op can never be silently skipped after constraint creation.
    let constraint_type = ConstraintType::from_str(&c.constraint_type).map_err(Error::semantic)?;
    let property_type = match &c.property_type {
        Some(ty) => {
            let canon = cypherlite_parser::type_expr_to_canon(ty);
            cypherlite_parser::validate_property_type(&canon)
                .map_err(|error| Error::semantic(error.to_string()))?;
            Some(cypherlite_parser::render_canon(&canon))
        }
        None => None,
    };
    let name = match &c.name {
        Some(name) => schema_name_value(name, params)?,
        None => format!(
            "constraint_{}_{}_{}",
            c.constraint_type,
            c.label,
            c.properties.join("_")
        ),
    };
    if graph.named_constraints().iter().any(|def| def.name == name) {
        if c.if_not_exists {
            return Ok(rows);
        }
        return Err(Error::semantic("ConstraintAlreadyExists"));
    }
    let relationship = c.entity_type == cypherlite_parser::SchemaEntityType::Relationship;
    let current_graph_type = graph.show_schema()?.data.current_graph_type.to_body();
    if matches!(
        constraint_type,
        ConstraintType::Exists | ConstraintType::PropertyType
    ) && crate::graph_type::label_identifies_type(&current_graph_type, relationship, &c.label)
    {
        return Err(Error::execution(
            "IncompatibleGraphTypeDependence: dependent constraints are managed by the graph type",
        ));
    }
    let equivalent = graph.named_constraints().iter().any(|def| {
        def.schema_equivalent(
            relationship,
            &c.label,
            &c.properties,
            &c.constraint_type,
            property_type.as_deref(),
        )
    });
    if equivalent {
        if c.if_not_exists {
            return Ok(rows);
        }
        if constraint_type != ConstraintType::Exists {
            return Err(Error::semantic("EquivalentSchemaRuleAlreadyExists"));
        }
    }
    if constraint_type.is_index_backed()
        && graph.named_constraints().iter().any(|def| {
            def.is_index_backed()
                && def.relationship == relationship
                && def.label == c.label
                && def.properties == c.properties
        })
    {
        return Err(Error::semantic("ConstraintConflict"));
    }
    // ADR-0372: an index-backed constraint conflicts with an existing RANGE
    // index on the same label/property — the constraint's backing
    // (ADR-0339) would duplicate it — closing the index-first order's gap
    // (LABS-274) for the serving pair. Non-serving kinds
    // (FULLTEXT/VECTOR/POINT) coexist: they serve search, not validation.
    // `IF NOT EXISTS` treats the existing index as satisfying the rule,
    // mirroring the index direction's `if_not_exists` arm above.
    let range_index_on_property = graph
        .show_schema()?
        .data
        .index_details
        .values()
        .any(|detail| {
            crate::redo::serving_pair_conflicts(
                crate::redo::ServingPairIndex {
                    kind: detail.kind,
                    relationship: detail.relationship,
                    labels: &detail.labels,
                    properties: &detail.properties,
                },
                crate::redo::ServingPairConstraint {
                    backed: constraint_type.is_index_backed(),
                    relationship: c.entity_type
                        == cypherlite_parser::SchemaEntityType::Relationship,
                    label: &c.label,
                    properties: &c.properties,
                },
            )
        });
    if constraint_type.is_index_backed() && range_index_on_property {
        if c.if_not_exists {
            return Ok(rows);
        }
        return Err(Error::semantic("EquivalentSchemaRuleAlreadyExists"));
    }
    if graph.named_index(&name).is_some() {
        return Err(Error::semantic("DuplicatedIndexName"));
    }
    for (key, value) in &c.options {
        // ADR-0337: the lowering and eval errors propagate — an absent
        // parameter surfaces its typed ParameterMissing (ADR-0308) instead
        // of being swallowed into the generic shape error.
        let expr = cypherlite_front::lower_expr(value)?;
        let value = eval(graph, &Row::new(), &expr, params, cypher_version)?;
        if key != "indexConfig" || !matches!(value, Value::Map(ref map) if map.is_empty()) {
            return Err(Error::semantic("InvalidOptionsMap"));
        }
    }
    graph.create_named_constraint(
        name.clone(),
        relationship,
        c.label.clone(),
        c.properties.clone(),
        c.constraint_type.clone(),
        property_type.clone(),
    )?;
    if let Some(enforced_label) = &c.enforced_label {
        graph.push_catalog_redo(RedoOp::CatalogDerivedConstraint {
            constraint: cypherlite_model::Constraint {
                name: name.clone(),
                label: c.label.clone(),
                properties: c.properties.clone(),
                constraint_type,
                property_type: property_type.clone(),
                entity_type: cypherlite_model::EntityKind::from_relationship_flag(relationship)
                    .display_name()
                    .into(),
                enforced_label: Some(enforced_label.clone()),
            },
        });
    }
    if constraint_type.is_index_backed() {
        super::ddl::create_constraint_backing_index(
            graph,
            &name,
            relationship,
            &c.label,
            &c.properties,
        )?;
    }
    Ok(rows)
}

pub(crate) fn catalog_drop_constraint(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::DropConstraintClause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>> {
    let name = schema_name_value(&c.name, params)?;
    let state = graph.show_schema()?;
    let is_dependent = state
        .data
        .derived_constraints
        .get(&name)
        .is_some_and(|constraint| {
            crate::graph_type::classify_constraint(
                &state.data.current_graph_type.to_body(),
                constraint,
            ) == cypherlite_model::ConstraintClassification::Dependent
        });
    if is_dependent {
        return Err(Error::execution(
            "DependentConstraintManagedIndividually: dependent constraints cannot be dropped",
        ));
    }
    if !graph.named_constraints().iter().any(|def| def.name == name) {
        if c.if_exists {
            return Ok(rows);
        }
        return Err(Error::semantic("ConstraintNotFound"));
    }
    super::ddl::exec_drop_constraint(graph, &name, false)?;
    Ok(rows)
}

pub(crate) fn catalog_alter_database(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::AlterDatabaseClause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>> {
    let name = schema_name_value(&c.name, params)?;
    // ADR-0116 §1/§2: namespace names match case-insensitively; an
    // unknown name fails with CE's text unless `IF EXISTS` makes the
    // command a no-op.
    let Some(ns) = cypherlite_core::Namespace::parse(&name) else {
        if c.if_exists {
            return Ok(rows);
        }
        return Err(cypherlite_core::ce_messages::alter_unknown_database(&name));
    };
    let canonical = ns.as_str();
    let state = graph.show_schema()?;
    let composite = state.databases[&ns].composite;
    let language = c.default_language;
    if !matches!(language, 5 | 25) {
        return Err(Error::semantic(
            "InvalidArgumentValue: unsupported Cypher version",
        ));
    }
    graph.push_catalog_redo(RedoOp::PutDatabase {
        name: canonical.to_string(),
        language,
        composite,
    });
    Ok(rows)
}

pub(crate) fn catalog_create_graph_type(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::CreateGraphTypeClause,
    rows: Vec<Row>,
) -> Result<Vec<Row>> {
    if !c.via_as {
        crate::graph_type::validate_graph_type_body(&c.body, graph.preview())
            .map_err(|e| Error::semantic(e.to_string()))?;
    }
    // Parse-to-model happens exactly once, here at DDL-execution
    // time; the redo op carries the typed model, and replay stores
    // it verbatim (no format-then-parse). The capture is fail-closed
    // on a CIP-262 default value the (already-run) validator would
    // have rejected -- never lossy.
    let body = crate::graph_type::GraphTypeSpec::from_body(&c.body)
        .map_err(|e| Error::semantic(e.to_string()))?;
    graph.push_catalog_redo(RedoOp::PutGraphType {
        name: c.name.clone(),
        body,
    });
    Ok(rows)
}

pub(crate) fn alter_current_graph_type(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::AlterCurrentGraphTypeClause,
    rows: Vec<Row>,
) -> Result<Vec<Row>> {
    // The stored model converts to the parser model once, here at
    // DDL-execution time; the per-verb body algebra lives in
    // `crate::graph_type` and its result is what gets persisted. Both
    // target forms resolve to a body first, then take the single
    // `apply_verb` path (the parser only builds `Named` for `SET`).
    let body = match &c.target {
        cypherlite_parser::AlterCurrentGraphTypeTarget::Named(name) => {
            // ADR-0211: `ALTER CURRENT GRAPH TYPE SET GRAPH TYPE <name>`
            // promotes a declared named type to current -- the same pipeline
            // as `SET { <body> }` (validation, derived-constraint sync),
            // sourced from the catalog's stored body instead of the statement.
            graph
                .show_schema()?
                .data
                .graph_types
                .get(name)
                .ok_or_else(|| Error::semantic("GraphTypeNotFound"))?
                .to_body()
        }
        cypherlite_parser::AlterCurrentGraphTypeTarget::Body(verb_body) => verb_body.clone(),
    };
    set_current_graph_type(graph, c.verb, &body)?;
    Ok(rows)
}

/// How the shared `ALTER CURRENT GRAPH TYPE` pipeline tail validates
/// existing data before the first catalog write. The entry point encodes
/// the decision once, so no caller passes a bare mode flag across the
/// module boundary.
enum DataValidation {
    /// The DDL path ([`set_current_graph_type`]): its writes go through
    /// the ordinary mid-transaction constraint validation.
    MidTransaction,
    /// ADR-0226 §3's validate-first data scan (`db.ontology.apply`,
    /// [`set_current_graph_type_validate_first`]): the captured snapshot
    /// is checked against every constraint the resulting graph type
    /// derives before any catalog change commits.
    ValidateFirst,
}

/// The plain DDL `ALTER CURRENT GRAPH TYPE` pipeline tail: purge decision,
/// verb algebra against the current body, derived-constraint sync, and
/// the `SetCurrentGraphType` catalog redo — all computed from the same
/// captured schema snapshot, with no data scan before the first catalog
/// write (the DDL's writes go through the ordinary mid-transaction
/// validation).
pub(crate) fn set_current_graph_type(
    graph: &ExecGraph<'_>,
    verb: cypherlite_parser::AlterGraphTypeVerb,
    body: &cypherlite_parser::GraphTypeBody,
) -> Result<()> {
    set_current_graph_type_tail(graph, verb, body, DataValidation::MidTransaction)
}

/// `db.ontology.apply`'s entry into the same pipeline tail
/// ([`set_current_graph_type`]), with ADR-0226 §3's validate-first data
/// scan spliced between derivation and the first catalog write.
pub(crate) fn set_current_graph_type_validate_first(
    graph: &ExecGraph<'_>,
    verb: cypherlite_parser::AlterGraphTypeVerb,
    body: &cypherlite_parser::GraphTypeBody,
) -> Result<()> {
    set_current_graph_type_tail(graph, verb, body, DataValidation::ValidateFirst)
}

fn set_current_graph_type_tail(
    graph: &ExecGraph<'_>,
    verb: cypherlite_parser::AlterGraphTypeVerb,
    body: &cypherlite_parser::GraphTypeBody,
    validation: DataValidation,
) -> Result<()> {
    let state = graph.show_schema()?;
    let purge = super::ddl::constraints_to_purge(
        graph
            .named_constraints()
            .into_iter()
            .map(|constraint| constraint.name)
            .collect(),
        &state.data.derived_constraints,
        verb,
    );
    let current = state.data.current_graph_type.to_body();
    // The store's preview set threads into the verb algebra (ADR-0371):
    // the stored-state half of the graph-type trio's gates -- an `ADD`
    // whose identifying label a stored node element type already
    // implies, a bare `ANY` a merged body introduces -- fails closed
    // here with the same authored error statement admission produces.
    let body = crate::graph_type::apply_verb(verb, current, body, graph.preview())?;
    // The body came through the verb algebra's validation, so the
    // derivation cannot fail here -- but it is total anyway (persisted
    // replay sites need the typed failure), so propagate rather than
    // assume.
    let derived = crate::graph_type::derive_constraints_typed(&body)?;
    if matches!(validation, DataValidation::ValidateFirst) {
        super::constraint_validation::validate_derived_constraints(graph, &derived)?;
    }
    super::ddl::sync_derived_constraints(graph, purge, derived)?;
    graph.push_catalog_redo(RedoOp::SetCurrentGraphType {
        // Fail-closed on a default value the verb algebra's validation
        // already rejected -- never a lossy capture.
        body: crate::graph_type::GraphTypeSpec::from_body(&body)
            .map_err(|e| Error::semantic(e.to_string()))?,
    });
    Ok(())
}
