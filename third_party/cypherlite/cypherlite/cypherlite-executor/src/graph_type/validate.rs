use super::*;
use cypherlite_core::{preview_feature_disabled_text, PreviewFeature, PreviewFeatures};
use cypherlite_model::{ConstraintClassification, EntityKind};
use cypherlite_parser::CanonType;

/// Looks up a node/edge type alias's identifying label. Used both by
/// validation (reference resolution) and by derivation/canonicalization
/// (resolving an edge endpoint / constraint target to its label).
pub fn alias_tables(body: &GraphTypeBody) -> (HashMap<&str, &str>, HashMap<&str, &str>) {
    let node_alias_label: HashMap<&str, &str> = body
        .node_types
        .iter()
        .filter_map(|nt| {
            nt.alias
                .as_deref()
                .map(|a| (a, nt.identifying_label.as_str()))
        })
        .collect();
    let edge_alias_label: HashMap<&str, &str> = body
        .edge_types
        .iter()
        .filter_map(|et| {
            et.alias
                .as_deref()
                .map(|a| (a, et.identifying_label.as_str()))
        })
        .collect();
    (node_alias_label, edge_alias_label)
}

/// Resolves an `<node/edge type reference>` to the label it names, `None`
/// for the any type. An `AliasRef` is resolved through `alias_table`; an
/// `InSitu` reference already carries its label directly.
pub fn resolve_ref_label(
    r: &GraphTypeElementRef,
    alias_table: &HashMap<&str, &str>,
) -> Option<String> {
    match r {
        GraphTypeElementRef::Any => None,
        GraphTypeElementRef::AliasRef(a) => alias_table.get(a.as_str()).map(|s| s.to_string()),
        GraphTypeElementRef::InSitu { label, .. } => Some(label.clone()),
    }
}

/// Registers `a` as denoting `(is_edge, label)` in the shared element-type-
/// alias namespace (Rule 3/4), or confirms it already does. Only errors
/// when `a` is already bound to a genuinely different `(is_edge, label)`
/// pair -- a repeated in-situ reference restating its own identity (CS[8]:
/// `(st:Site => ...)` then `FOR (st:Site =>)`) is not a conflict.
pub(super) fn declare_alias<'a>(
    aliases: &mut HashMap<&'a str, (bool, &'a str)>,
    a: &'a str,
    is_edge: bool,
    label: &'a str,
) -> Result<(), EngineError> {
    match aliases.get(a) {
        Some(&(existing_is_edge, existing_label))
            if existing_is_edge != is_edge || existing_label != label =>
        {
            Err(syn("duplicate element type alias"))
        }
        _ => {
            aliases.insert(a, (is_edge, label));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------
// CIP-178: validation (CREATE GRAPH TYPE only)
// ---------------------------------------------------------------------

/// The executor-side ADR-0371 gate: the core's canonical
/// `PreviewFeatures::require`, carried through the `EngineError`
/// boundary with the same authored text -- the one gate the graph-type
/// layer's stored-state half consults (its in-body half lives at
/// statement admission). The text is the core's own (the `Semantic`
/// detail `require`'s only failure carries); the `EngineError` wrapper
/// supplies the class, so the core stays the single spelling.
fn gate(preview: PreviewFeatures, feature: PreviewFeature) -> Result<(), EngineError> {
    preview
        .require(feature)
        .map_err(|_| syn(preview_feature_disabled_text(feature)))
}

/// Validates a `GraphTypeBody` against the CIP-178 "shall" rules that need
/// whole-body context (alias/label uniqueness, reference resolution, the
/// constraint-only rules 1/2/13-16/22/23), plus the CIP graph-type trio's
/// semantic rules: CIP-283's optional (`prop :: ANY`) properties,
/// CIP-262's `DEFAULT ON COMMIT` shape/storable/conformance and
/// inheritance-interaction rules, and CIP-266's inheritance rules (cycle
/// freedom, restriction-only re-specification, instantiable diamond
/// intersections). Called by `CREATE GRAPH TYPE`'s clause handler and the
/// `ALTER CURRENT GRAPH TYPE` verb algebra -- see `GraphTypeBody`'s doc
/// comment.
///
/// `preview` is the store's `CYPHERLITE_PREVIEW`-resolved set (ADR-0371):
/// the stored-state half of the trio's gates fails closed here with the
/// same authored error statement admission produces for the in-body
/// half (an `ADD` whose identifying label a stored node element type
/// already implies).
pub fn validate_graph_type_body(
    body: &GraphTypeBody,
    preview: PreviewFeatures,
) -> Result<(), EngineError> {
    // Rule 5/6: duplicate identifying label (node/edge types each have
    // their own label namespace).
    let mut node_labels = HashSet::new();
    for nt in &body.node_types {
        if !node_labels.insert(nt.identifying_label.as_str()) {
            return Err(syn("duplicate identifying label"));
        }
    }
    let mut edge_labels = HashSet::new();
    for et in &body.edge_types {
        if !edge_labels.insert(et.identifying_label.as_str()) {
            return Err(syn("duplicate identifying label"));
        }
    }

    // Rule 3a/3b/4a: node type aliases and edge type aliases share one
    // element-type-alias namespace -- two node/edge type *specifications*
    // may not declare the same alias.
    let mut aliases: HashMap<&str, (bool, &str)> = HashMap::new();
    for nt in &body.node_types {
        if let Some(a) = &nt.alias {
            declare_alias(&mut aliases, a, false, &nt.identifying_label)?;
        }
    }
    for et in &body.edge_types {
        if let Some(a) = &et.alias {
            declare_alias(&mut aliases, a, true, &et.identifying_label)?;
        }
    }

    // Rule 3c/4c: a *bare* in-situ constraint reference's own alias (e.g.
    // `CONSTRAINT FOR (p:City) REQUIRE ...` with no `=>`, an ad-hoc
    // reference that need not resolve to a declared type at all) must not
    // collide with an unrelated node/edge type specification's own alias
    // (CIP-178 CS[9]/[11]: `(p:Person => ...)` elsewhere, then a `p`-aliased
    // constraint on an unrelated label, is rejected). An `=>`-qualified
    // in-situ reference's alias is, by contrast, a pure local rename of an
    // already-fully-identified target and does not introduce this
    // ambiguity -- CIP-181's own "alias shadowing" worked examples
    // (undesignated-constraints.feature UDC:11 and friends) explicitly
    // accept a `(c:Person =>)`-style constraint reusing an unrelated node
    // type's own alias, so `implies` references (and plain `AliasRef`s,
    // which always resolve to a declared type) are intentionally excluded
    // here. Constraint-to-constraint alias reuse is never checked -- Rule
    // 3c/4c only pits a constraint's alias against a *node/edge type
    // specification's* alias, not against other constraints (confirmed by
    // CIP-181's "Independent constraints for different non-implied labels").
    for c in &body.constraints {
        if let GraphTypeElementRef::InSitu {
            alias: Some(a),
            label,
            implies: false,
        } = &c.target
        {
            if let Some(&(existing_is_edge, existing_label)) = aliases.get(a.as_str()) {
                if existing_is_edge != c.is_edge || existing_label != label {
                    return Err(syn("duplicate element type alias"));
                }
            }
        }
    }

    // Rule 7 (own implied set): a node type's identifying label must not
    // recur in its own implied label set.
    for nt in &body.node_types {
        if nt.implied_labels.contains(&nt.identifying_label) {
            return Err(syn("identifying label repeated in implied label set"));
        }
    }
    // CIP-266 (replacing CIP-178 Rule 7's GTSB half): an implied label
    // may name another node element type's identifying label --
    // inheritance -- behind the `graph-type-implied-labels` preview gate
    // (ADR-0371). The in-body half of the gate lives at statement
    // admission; the stored-state half (an `ADD` whose identifying label
    // a stored node element type already implies) fails closed here with
    // the same authored error. Within the gate, inheritance must be
    // acyclic (NAGTE3).
    let inheritance = Inheritance::of(body);
    if !inheritance.is_empty() {
        gate(preview, PreviewFeature::GraphTypeImpliedLabels)?;
        if let Some((x, y)) = inheritance.find_cycle() {
            return Err(syn(format!(
                "The label `{x}` is not allowed as an implied label on the node element \
                 type identified by the label `{y}`: node element types cannot have \
                 circular inheritance"
            )));
        }
    }

    // Rule 8: a single-label (no implied labels) node type with an empty
    // property set is ineffectual.
    for nt in &body.node_types {
        if nt.implied_labels.is_empty() && nt.properties.is_empty() {
            return Err(syn("single-label node type with empty property set"));
        }
    }

    // PT Rule 3a/3b: duplicate property names; CIP-283's optional
    // (bare `ANY`) properties and CIP-262's default values, behind their
    // preview gates.
    for nt in &body.node_types {
        validate_property_set(&nt.properties, preview)?;
    }
    for et in &body.edge_types {
        validate_property_set(&et.properties, preview)?;
    }

    // CIP-266: the inheritance property-definition rules -- a re-specified
    // property must repeat or further restrict the intersection of its
    // parents' effective definitions (diamond: parents not defining the
    // property contribute `ANY`), and an implicitly inherited property
    // must be instantiable (a `NULL`/`NOTHING` intersection is not).
    if !inheritance.is_empty() {
        validate_inheritance_properties(body, &inheritance)?;
    }

    // CIP-262's interaction with CIP-266: the effective default map must
    // resolve (a property inheriting defaults from more than one implied
    // node element type is the FE8 conflict), and each effective default
    // must conform to the type the child element type declares for its
    // property (FE9: restricting the property type must not invalidate an
    // implicitly inherited default).
    // No gate here: `validate_property_set` already failed closed on
    // every default value the feature does not enable (the per-property
    // gate covers this whole block).
    if body_has_default_values(body) {
        let defaults = effective_default_map(body, &inheritance)?;
        for nt in &body.node_types {
            if let Some(map) = defaults.get(&nt.identifying_label) {
                for (property, default) in map {
                    let Some(declared) = nt.properties.iter().find(|p| &p.name == property) else {
                        continue;
                    };
                    if !default.value.conforms_to(&declared.value_type)? {
                        return Err(syn(format!(
                            "Invalid input {} for default property value. Expected to be \
                             {}",
                            default.value.render(),
                            render_canon(&type_expr_to_canon(&declared.value_type))
                        )));
                    }
                }
            }
        }
    }

    // Rule 2: a node type must not contain REL/RELATIONSHIP; an edge type
    // must not contain NODE (checked via each inline property constraint's
    // recorded qualifier).
    for nt in &body.node_types {
        for p in &nt.properties {
            if let Some(ic) = &p.inline_constraint {
                if ic.qualifier == Some(GraphTypeEntityQualifier::Rel) {
                    return Err(syn("REL/RELATIONSHIP keyword not allowed in node type"));
                }
            }
        }
    }
    for et in &body.edge_types {
        for p in &et.properties {
            if let Some(ic) = &p.inline_constraint {
                if ic.qualifier == Some(GraphTypeEntityQualifier::Node) {
                    return Err(syn("NODE keyword not allowed in edge type"));
                }
            }
        }
    }

    let (node_alias_label, edge_alias_label) = alias_tables(body);
    let node_id_labels: HashSet<&str> = body
        .node_types
        .iter()
        .map(|n| n.identifying_label.as_str())
        .collect();
    let edge_id_labels: HashSet<&str> = body
        .edge_types
        .iter()
        .map(|n| n.identifying_label.as_str())
        .collect();

    // Rule 3/4 (edge endpoints): an in-situ endpoint reference may not
    // carry an element type alias. Rule 1.2.3/REF[6]: an implies (`=>`)
    // in-situ reference must name a declared node type. REF[5]: an alias
    // reference must resolve to a declared node type alias.
    for et in &body.edge_types {
        for endpoint in [&et.source, &et.target] {
            match endpoint {
                GraphTypeElementRef::InSitu { alias: Some(_), .. } => {
                    return Err(syn(
                        "in-situ endpoint reference may not carry an element type alias",
                    ));
                }
                GraphTypeElementRef::InSitu {
                    alias: None,
                    label,
                    implies: true,
                } => {
                    if !node_id_labels.contains(label.as_str()) {
                        return Err(syn(
                            "implies in-situ reference must name a declared node type",
                        ));
                    }
                }
                // Kept as a nested `if` rather than a match guard: collapsing it
                // produces a guard line that different rustfmt versions wrap
                // inconsistently.
                #[allow(clippy::collapsible_match)]
                GraphTypeElementRef::AliasRef(a) => {
                    if !node_alias_label.contains_key(a.as_str()) {
                        return Err(syn("node type alias reference to an undeclared alias"));
                    }
                }
                _ => {}
            }
        }
    }

    // Rule 9: an ineffectual single-label edge type (empty props, any
    // source, any destination).
    for et in &body.edge_types {
        if et.properties.is_empty()
            && matches!(et.source, GraphTypeElementRef::Any)
            && matches!(et.target, GraphTypeElementRef::Any)
        {
            return Err(syn("single-label edge type is ineffectual"));
        }
    }

    // Constraint target resolution + Rules 1/15/16 (Rule 1/2/13/16-of-CS/
    // 22/23 are already enforced at parse time; see
    // `Parser::parse_graph_type_constraint_spec`).
    for c in &body.constraints {
        let (alias_table, id_labels) = if c.is_edge {
            (&edge_alias_label, &edge_id_labels)
        } else {
            (&node_alias_label, &node_id_labels)
        };
        match &c.target {
            GraphTypeElementRef::AliasRef(a) => {
                if !alias_table.contains_key(a.as_str()) {
                    return Err(syn(format!(
                        "{} type alias reference to an undeclared alias",
                        if c.is_edge { "edge" } else { "node" }
                    )));
                }
            }
            GraphTypeElementRef::InSitu {
                implies: true,
                label,
                ..
            } => {
                if !id_labels.contains(label.as_str()) {
                    return Err(syn("implies in-situ reference must name a declared type"));
                }
            }
            GraphTypeElementRef::InSitu {
                implies: false,
                label,
                ..
            } => {
                if matches!(
                    c.kind,
                    GraphTypeConstraintVerb::Exists | GraphTypeConstraintVerb::PropertyType
                ) && id_labels.contains(label.as_str())
                {
                    return Err(syn(
                        "independent constraint conflicts with an identifying label",
                    ));
                }
            }
            GraphTypeElementRef::Any => {}
        }
    }

    // Rule 13: duplicate constraint name.
    let mut names = HashSet::new();
    for c in &body.constraints {
        if let Some(n) = &c.name {
            if !names.insert(n.as_str()) {
                return Err(syn("duplicate constraint name"));
            }
        }
    }

    // Rule 14: duplicate constraint key -- same (entity, resolved label,
    // properties, kind).
    let resolve = |c: &GraphTypeConstraintSpec| -> Option<String> {
        let table = if c.is_edge {
            &edge_alias_label
        } else {
            &node_alias_label
        };
        resolve_ref_label(&c.target, table)
    };
    let mut seen_keys: Vec<(bool, String, Vec<String>, u8)> = Vec::new();
    for c in &body.constraints {
        if let Some(label) = resolve(c) {
            let key = (
                c.is_edge,
                label,
                c.properties.clone(),
                constraint_rank(&c.kind),
            );
            if seen_keys.contains(&key) {
                return Err(syn("duplicate constraint key"));
            }
            seen_keys.push(key);
        }
    }

    // CIP-181 Independent constraints [18-22]: an independent property
    // type constraint's value type is subject to the same restriction as a
    // standalone property type constraint (CIP-85's `validate_property_type`
    // -- no top-level NOT NULL, no ANY/NODE/RELATIONSHIP/PATH/MAP, and the
    // usual LIST<...> inner-type restrictions), unlike a node/edge type's
    // own declared properties (`validate_property_set`), which allow a
    // top-level NOT NULL.
    for c in &body.constraints {
        if c.kind == GraphTypeConstraintVerb::PropertyType {
            if let Some(te) = &c.property_type {
                let canon = type_expr_to_canon(te);
                cypherlite_parser::validate_property_type(&canon).map_err(|_| {
                    syn(format!(
                        "{} type constraint has a value type that is not supported{}",
                        if c.is_edge { "Edge" } else { "Node" },
                        if canon.not_null {
                            " (non-nullable type)"
                        } else {
                            ""
                        }
                    ))
                })?;
            }
        }
    }

    Ok(())
}

/// Whether any element type's properties carry a `DEFAULT ON COMMIT`
/// clause (CIP-262).
pub(crate) fn body_has_default_values(body: &GraphTypeBody) -> bool {
    body.node_types
        .iter()
        .flat_map(|element| element.properties.iter())
        .chain(
            body.edge_types
                .iter()
                .flat_map(|element| element.properties.iter()),
        )
        .any(|p| p.default_value.is_some())
}

/// The CIP-266 inheritance property-definition rules over the resolved
/// effective properties: a re-specified (`declared`) property must be
/// repeated exactly or further restrict the intersection of its parents'
/// effective definitions, and an implicitly inherited property must be
/// instantiable (not `NULL`/`NOTHING`).
fn validate_inheritance_properties(
    body: &GraphTypeBody,
    inheritance: &Inheritance,
) -> Result<(), EngineError> {
    let effective = EffectiveProperties::resolve(body, inheritance)?;
    let by_label: HashMap<&str, &GraphTypeNodeTypeSpec> = body
        .node_types
        .iter()
        .map(|n| (n.identifying_label.as_str(), n))
        .collect();
    for (label, node) in &by_label {
        for p in &node.properties {
            // The intersection of the parents' effective definitions,
            // a parent not defining the property contributing `ANY`
            // (CIP-266): the declared type must restrict it.
            let intersection =
                parents_effective_intersection(inheritance, &effective, label, &p.name);
            let declared = type_expr_to_canon(&p.value_type);
            if !canon_allows(&intersection, &declared) {
                return Err(syn(format!(
                    "The property definition `{} :: {}` is not allowed as it is either in \
                     conflict with or more permissive than the property definition `{} :: {}` \
                     coming from the intersection of property definitions of the implied \
                     node element types",
                    p.name,
                    render_canon(&declared),
                    p.name,
                    render_canon(&intersection)
                )));
            }
        }
        // An implicitly inherited (not re-specified) property must be
        // instantiable (NAGTE5: a `NULL` intersection is not an allowed
        // property type).
        for (property, definition) in effective.of(label) {
            if !definition.declared
                && matches!(
                    definition.canon.kind,
                    CanonKind::Predefined(PredefinedKind::Null | PredefinedKind::Nothing)
                )
            {
                return Err(syn(format!(
                    "The property definition `{} :: {}` coming from the intersection of \
                     property definitions from multiple implied node element types is not \
                     an allowed property type",
                    property,
                    render_canon(&definition.canon)
                )));
            }
        }
    }
    Ok(())
}

/// One child's parents' effective intersection for one property (CIP-266
/// diamond rule): every parent's effective definition -- `ANY` when the
/// parent's chain does not define the property -- intersected pointwise,
/// starting from `ANY`.
fn parents_effective_intersection(
    inheritance: &Inheritance,
    effective: &EffectiveProperties,
    label: &str,
    property: &str,
) -> CanonType {
    let mut intersection = CanonType::predefined(PredefinedKind::Any, false);
    for parent in inheritance.parents_of(label) {
        let parent_definition = effective
            .property(parent, property)
            .map(|d| d.canon.clone())
            .unwrap_or_else(|| CanonType::predefined(PredefinedKind::Any, false));
        intersection = canon_intersection(&intersection, &parent_definition);
    }
    intersection
}

/// PT Rule 3a (duplicate property names) / 3b (a bare `ANY` value type,
/// without `NOT NULL`, is not storable) / the `LIST<...>` inner-type
/// restrictions (CIP-85, shared with property type constraints -- no
/// union, no nested list, no nullable inner type; unlike a standalone
/// property type constraint, a top-level `NOT NULL` on the property itself
/// is fine here, so this doesn't call `validate_property_type` wholesale).
/// CIP-283: a bare `ANY` value type is allowed behind the
/// `graph-type-opt-props` preview gate. CIP-262: a `DEFAULT ON COMMIT`
/// value must be a literal or an allowed type constructor, storable on a
/// property, and conforming to the property's declared value type.
pub(super) fn validate_property_set(
    props: &[cypherlite_parser::GraphTypePropertyType],
    preview: PreviewFeatures,
) -> Result<(), EngineError> {
    let mut seen = HashSet::new();
    for p in props {
        if !seen.insert(p.name.as_str()) {
            return Err(syn("duplicate property name"));
        }
        if let Some(expr) = &p.default_value {
            gate(preview, PreviewFeature::GraphTypeDefaultValues)?;
            let default = DefaultValue::from_expr(expr)?;
            if !default.is_storable() {
                return Err(syn(format!(
                    "Invalid input {} for default property value: the value cannot be \
                     stored on a property",
                    default.render()
                )));
            }
            if !default.conforms_to(&p.value_type)? {
                return Err(syn(format!(
                    "Invalid input {} for default property value. Expected to be {}",
                    default.render(),
                    render_canon(&type_expr_to_canon(&p.value_type))
                )));
            }
        }
        let canon = type_expr_to_canon(&p.value_type);
        if matches!(canon.kind, CanonKind::Predefined(PredefinedKind::Any)) && !canon.not_null {
            // CIP-283: the optional property, behind its preview gate;
            // the historical rejection stays for a body that reached here
            // ungated (an ontology-converted body -- never a statement,
            // which statement admission already gated).
            if !preview.enabled(PreviewFeature::GraphTypeOptProps) {
                return Err(syn("non-storable value type"));
            }
        }
        if let CanonKind::List(elem) = &canon.kind {
            cypherlite_parser::validate_list_element(&canon, elem)
                .map_err(|e| syn(e.to_string()))?;
        }
    }
    Ok(())
}

pub(super) fn constraint_rank(kind: &GraphTypeConstraintVerb) -> u8 {
    match kind {
        GraphTypeConstraintVerb::Key => 1,
        GraphTypeConstraintVerb::Unique => 2,
        GraphTypeConstraintVerb::Exists => 3,
        GraphTypeConstraintVerb::PropertyType => 4,
    }
}

// ---------------------------------------------------------------------
// CIP-182/183: `ALTER CURRENT GRAPH TYPE DROP` element matching -- a drop
// element must specify either just the identifying label/type (matches
// regardless of the stored definition's other fields) or the *exact*
// stored definition (implied label set + property set, order-insensitive).
// ---------------------------------------------------------------------

/// Order-insensitive equality of two property-type sets: same names, and
/// for each name the same rendered canonical property type (including
/// `NOT NULL` and, per CIP-262, the `DEFAULT ON COMMIT` value -- a DROP
/// must restate a default it stored, FE4).
pub(crate) fn property_sets_equal(
    a: &[cypherlite_parser::GraphTypePropertyType],
    b: &[cypherlite_parser::GraphTypePropertyType],
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // An unrenderable restated default cannot match the stored one: the
    // drop is not the exact restatement FE4 demands (the typed
    // "node element type specified incorrectly" error the exact-match
    // check raises), never a renderer panic.
    let render = |p: &cypherlite_parser::GraphTypePropertyType| -> Option<(String, String)> {
        Some((p.name.clone(), super::canonical_property_type(p).ok()?))
    };
    let av: Option<Vec<(String, String)>> = a.iter().map(render).collect();
    let bv: Option<Vec<(String, String)>> = b.iter().map(render).collect();
    let (Some(mut av), Some(mut bv)) = (av, bv) else {
        // An unrenderable restatement cannot match the stored one.
        return false;
    };
    av.sort();
    bv.sort();
    av == bv
}

/// Whether a DROP node type spec is the minimal "single-label form"
/// (CIP-182: `(:Label =>)`, no implied labels, no properties) -- which
/// matches an existing node type by identifying label alone, regardless of
/// its other fields.
pub fn is_bare_node_type_ref(nt: &GraphTypeNodeTypeSpec) -> bool {
    nt.implied_labels.is_empty() && nt.properties.is_empty()
}

/// Whether a DROP node type spec exactly matches an existing node type's
/// full definition (implied label set + property set).
pub fn node_type_matches_exactly(
    spec: &GraphTypeNodeTypeSpec,
    existing: &GraphTypeNodeTypeSpec,
) -> bool {
    let mut sl: Vec<&str> = spec.implied_labels.iter().map(|s| s.as_str()).collect();
    let mut el: Vec<&str> = existing.implied_labels.iter().map(|s| s.as_str()).collect();
    sl.sort();
    el.sort();
    sl == el && property_sets_equal(&spec.properties, &existing.properties)
}

/// Whether a DROP edge type spec is the minimal "anonymous-endpoint form"
/// (CIP-182: `()-[:Label =>]->()`, both endpoints the any type, no
/// properties) -- which matches an existing edge type by identifying label
/// alone, regardless of its endpoints/properties.
pub fn is_bare_edge_type_ref(et: &cypherlite_parser::GraphTypeEdgeTypeSpec) -> bool {
    matches!(et.source, GraphTypeElementRef::Any)
        && matches!(et.target, GraphTypeElementRef::Any)
        && et.properties.is_empty()
}

/// Whether a DROP edge type spec exactly matches an existing edge type's
/// full definition (source/target resolved labels + property set).
/// `alias_table` resolves any `AliasRef` endpoints against the graph type
/// they were both declared/stored in.
pub fn edge_type_matches_exactly(
    spec: &cypherlite_parser::GraphTypeEdgeTypeSpec,
    existing: &cypherlite_parser::GraphTypeEdgeTypeSpec,
    alias_table: &HashMap<&str, &str>,
) -> bool {
    resolve_ref_label(&spec.source, alias_table) == resolve_ref_label(&existing.source, alias_table)
        && resolve_ref_label(&spec.target, alias_table)
            == resolve_ref_label(&existing.target, alias_table)
        && property_sets_equal(&spec.properties, &existing.properties)
}

// ---------------------------------------------------------------------
// CIP-186: constraint classification (undesignated/dependent/independent)
// and the CREATE/DROP CONSTRAINT integration built on it.
// ---------------------------------------------------------------------

/// Whether `label` is the identifying label of a node type (`is_edge`
/// `false`) or edge type (`is_edge` true) declared by `body` -- CIP-186's
/// "identifies a node/edge type" test, used both to classify a stored
/// constraint (dependent vs independent) and to reject a `CREATE
/// CONSTRAINT` that would create a dependent one.
pub fn label_identifies_type(body: &GraphTypeBody, is_edge: bool, label: &str) -> bool {
    if is_edge {
        body.edge_types
            .iter()
            .any(|et| et.identifying_label == label)
    } else {
        body.node_types
            .iter()
            .any(|nt| nt.identifying_label == label)
    }
}

/// CIP-186's three-way constraint classification:
/// [`ConstraintClassification::Undesignated`] for a uniqueness/key
/// constraint (always, regardless of graph type -- `IS UNIQUE`/`IS KEY`
/// never depend on the current graph type),
/// [`ConstraintClassification::Dependent`] for a graph-type-derived
/// label/endpoint-label constraint (CIP-187's
/// `labelExistence`/`sourceLabel`/`targetLabel` kinds) or for an
/// existence/property-type constraint whose label/relationship-type
/// currently identifies a node/edge type,
/// [`ConstraintClassification::Independent`] for every other constraint.
pub fn classify_constraint(
    body: &GraphTypeBody,
    c: &cypherlite_model::Constraint,
) -> ConstraintClassification {
    // A stored `entity_type` other than "RELATIONSHIP" classifies as a
    // node, matching the historical `== "RELATIONSHIP"` string test.
    let entity = EntityKind::from_display_name(&c.entity_type).unwrap_or(EntityKind::Node);
    classify_constraint_parts(body, Some(c.constraint_type), entity, &c.label)
}

/// The primitives form of [`classify_constraint`]: classifies from the
/// parsed kind (`None` for a stored catalog tag that no longer parses --
/// it keeps the historical label-identifies-type probe), the entity kind,
/// and the label, so a stored `ConstraintDef` row is classified without
/// building a [`cypherlite_model::Constraint`].
pub fn classify_constraint_parts(
    body: &GraphTypeBody,
    kind: Option<ConstraintType>,
    entity: EntityKind,
    label: &str,
) -> ConstraintClassification {
    match kind {
        // `IS UNIQUE`/`IS KEY` never depend on the current graph type.
        Some(ConstraintType::Unique | ConstraintType::Key) => {
            ConstraintClassification::Undesignated
        }
        // CIP-187's graph-type-derived label/endpoint-label kinds.
        Some(
            ConstraintType::LabelExistence
            | ConstraintType::SourceLabel
            | ConstraintType::TargetLabel,
        ) => ConstraintClassification::Dependent,
        Some(ConstraintType::Exists | ConstraintType::PropertyType) | None => {
            if label_identifies_type(body, entity.is_relationship(), label) {
                ConstraintClassification::Dependent
            } else {
                ConstraintClassification::Independent
            }
        }
        // The standalone CIP-177 label-existence forms are independent by
        // definition: no graph type manages them (parser/schema.rs).
        Some(
            ConstraintType::NodeLabelExistence
            | ConstraintType::SourceLabelExistence
            | ConstraintType::TargetLabelExistence,
        ) => ConstraintClassification::Independent,
    }
}
