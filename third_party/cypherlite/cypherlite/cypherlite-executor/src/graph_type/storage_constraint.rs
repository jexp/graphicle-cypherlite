//! Rendering one stored [`Constraint`] as a `CONSTRAINT ...` statement, shared
//! by the two graph-type renderers: CIP-180's canonical text
//! ([`canonical_constraint_from_storage`](super::canonical_constraint_from_storage))
//! and CIP-257's display text
//! (`graph_type_visualize::vis_constraint_text_from_storage`).
//!
//! The verb and target shape are driven by the typed [`ConstraintType`]
//! vocabulary, never by re-splitting the stored tag: one exhaustive match per
//! fact, no wildcard fallback. The two renderers' deliberate spelling deltas
//! are explicit parameters on [`StorageConstraintStyle`], so neither
//! re-implements the other's policy.

use cypherlite_model::{Constraint, ConstraintType};
use cypherlite_parser::token::quote_identifier_shaped;

use cypherlite_parser::token::quote_identifier_always;

/// How a renderer spells the constraint parts that legitimately differ
/// between CIP-180's canonical form and CIP-257's display form.
pub(crate) struct StorageConstraintStyle {
    /// Renders one identifier (constraint name, label, property key):
    /// canonical always backticks (its always-quoted policy), display
    /// applies the parser's shape-only display policy.
    identifier: fn(&str) -> String,
    /// Renders the constraint pattern's variable (`n`/`r`): canonical
    /// always backticks it, display leaves it bare.
    variable: fn(&str) -> String,
    /// Whether a single-property `REQUIRE` list is parenthesized: canonical
    /// always parenthesizes, display writes one bare `n.prop`.
    parenthesize_single_property: bool,
}

impl StorageConstraintStyle {
    /// CIP-180's canonical form: every name position backticked, every
    /// property list parenthesized.
    pub(crate) const CANONICAL: Self = Self {
        identifier: quote_identifier_always,
        variable: quote_identifier_always,
        parenthesize_single_property: true,
    };

    /// CIP-257's display form: the parser's shape-only display identifier
    /// policy (keywords stay bare -- display text is not re-parsed), a bare
    /// `n`/`r` variable, and a single property left unparenthesized.
    pub(crate) const DISPLAY: Self = Self {
        identifier: quote_identifier_shaped,
        variable: bare,
        parenthesize_single_property: false,
    };
}

/// A bare identifier — the display form's variable spelling.
fn bare(identifier: &str) -> String {
    identifier.to_string()
}

/// Renders `constraint` in a `CONSTRAINT <name> FOR <target> REQUIRE
/// <properties> <verb>` statement under `style`.
pub(crate) fn render_storage_constraint(
    constraint: &Constraint,
    identified: bool,
    style: &StorageConstraintStyle,
) -> String {
    let is_edge = constraint.entity_type == "RELATIONSHIP";
    let target = storage_constraint_target(style, is_edge, &constraint.label, identified);
    let properties = storage_constraint_properties(style, is_edge, &constraint.properties);
    let verb = storage_constraint_verb(
        constraint.constraint_type,
        constraint.property_type.as_deref(),
    );
    format!(
        "CONSTRAINT {} FOR {target} REQUIRE {properties} {verb}",
        (style.identifier)(&constraint.name)
    )
}

/// The `FOR` target pattern: node vs relationship, identifying (`=>`) vs
/// plain, with the variable and label rendered by `style`.
fn storage_constraint_target(
    style: &StorageConstraintStyle,
    is_edge: bool,
    label: &str,
    identified: bool,
) -> String {
    let variable = (style.variable)(if is_edge { "r" } else { "n" });
    let label = (style.identifier)(label);
    match (is_edge, identified) {
        (true, true) => format!("()-[{variable}:{label} =>]->()"),
        (true, false) => format!("()-[{variable}:{label}]->()"),
        (false, true) => format!("({variable}:{label} =>)"),
        (false, false) => format!("({variable}:{label})"),
    }
}

/// The `REQUIRE` property list, in the style's parenthesization policy.
fn storage_constraint_properties(
    style: &StorageConstraintStyle,
    is_edge: bool,
    properties: &[String],
) -> String {
    let variable = (style.variable)(if is_edge { "r" } else { "n" });
    let items: Vec<String> = properties
        .iter()
        .map(|property| format!("{variable}.{}", (style.identifier)(property)))
        .collect();
    if items.len() == 1 && !style.parenthesize_single_property {
        items[0].clone()
    } else {
        format!("({})", items.join(", "))
    }
}

/// The `REQUIRE` verb of a stored constraint's kind. Exhaustive over
/// [`ConstraintType`]: the stored tag is never re-split, and no wildcard
/// invents a spelling. The label/endpoint-label existence kinds have no
/// standalone `REQUIRE ... IS` form; their previous fallback rendered the
/// tag, which stays byte-identical here.
fn storage_constraint_verb(kind: ConstraintType, property_type: Option<&str>) -> String {
    match kind {
        ConstraintType::Key => "IS KEY".to_string(),
        ConstraintType::Unique => "IS UNIQUE".to_string(),
        ConstraintType::Exists => "IS NOT NULL".to_string(),
        ConstraintType::PropertyType => format!("IS :: {}", property_type.unwrap_or_default()),
        ConstraintType::LabelExistence => "IS labelExistence".to_string(),
        ConstraintType::NodeLabelExistence => "IS nodeLabelExistence".to_string(),
        ConstraintType::SourceLabel => "IS sourceLabel".to_string(),
        ConstraintType::SourceLabelExistence => "IS sourceLabelExistence".to_string(),
        ConstraintType::TargetLabel => "IS targetLabel".to_string(),
        ConstraintType::TargetLabelExistence => "IS targetLabelExistence".to_string(),
    }
}

/// The constraint-type rank both renderers' sort keys use (CIP-180's
/// KEY-before-UNIQUE tie-break): the four property kinds in their canonical
/// order, every label/endpoint-label kind after them. Typed, so the rank
/// reads the same vocabulary as the verb.
pub(crate) fn storage_constraint_rank(kind: ConstraintType) -> u8 {
    match kind {
        ConstraintType::Key => 1,
        ConstraintType::Unique => 2,
        ConstraintType::Exists => 3,
        ConstraintType::PropertyType => 4,
        ConstraintType::LabelExistence
        | ConstraintType::NodeLabelExistence
        | ConstraintType::SourceLabel
        | ConstraintType::SourceLabelExistence
        | ConstraintType::TargetLabel
        | ConstraintType::TargetLabelExistence => 5,
    }
}
