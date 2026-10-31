//! Fixtures shared by the crate's test modules: the CypherLite-authored
//! main-shape example document and its parsed form (ADR-0364 §6).

use crate::model::OntologyDocument;

/// The CypherLite-authored, main-shape, validation-only example document.
///
/// Main ships no root-level `example.json` (the pinned README's curl URLs
/// are stale), and upstream's Go testdata example does not validate
/// against main's own schema. This fixture covers every `Neo4jType` token,
/// all four constraint types, the nullable/empty forms and every typed
/// extension member; it deliberately contains main-valid constructs that
/// `apply` rejects, so it pins the validator, not convertibility
/// (ADR-0364 §6).
pub(crate) const EXAMPLE_JSON: &str = include_str!("../schema/example.json");

/// The parsed and validated example document.
pub(crate) fn example_doc() -> OntologyDocument {
    crate::parse_ontology_document(EXAMPLE_JSON.as_bytes()).expect("example validates")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Constraint, ConstraintType, Neo4jType, Property};

    #[test]
    fn the_example_is_main_shaped_and_validates() {
        let doc = example_doc();
        assert_eq!(doc.version(), "1");
        assert_eq!(doc.id(), "cypherlite-ontology-example");
        assert!(!doc.nodes().is_empty());
        assert!(!doc.relationships().is_empty());
    }

    /// Every node and relationship entry's declarations, in document
    /// order — the one walk the coverage tests below consult, so neither
    /// re-hand-rolls the nodes-then-relationships pass (and a second copy
    /// cannot drift).
    fn declarations(doc: &OntologyDocument) -> impl Iterator<Item = (&[Property], &[Constraint])> {
        doc.nodes()
            .iter()
            .map(|node| (&node.properties[..], &node.constraints[..]))
            .chain(
                doc.relationships()
                    .iter()
                    .map(|rel| (&rel.properties[..], &rel.constraints[..])),
            )
    }

    /// Every property type declared on the example's nodes and
    /// relationships.
    fn property_types(doc: &OntologyDocument) -> Vec<Neo4jType> {
        declarations(doc)
            .flat_map(|(properties, _)| properties)
            .filter_map(|property| property.property_type)
            .collect()
    }

    /// Every constraint kind declared on the example's nodes and
    /// relationships.
    fn constraint_types(doc: &OntologyDocument) -> Vec<ConstraintType> {
        declarations(doc)
            .flat_map(|(_, constraints)| constraints)
            .map(|constraint| constraint.constraint_type)
            .collect()
    }

    #[test]
    fn the_example_covers_every_neo4j_type_token() {
        let seen = property_types(&example_doc());
        // The fixture must exercise every token of the closed enum. Walk
        // the canonical vocabulary table so a newly added token cannot be
        // forgotten by the fixture (and a second literal list cannot
        // drift from it).
        for (token, ty) in crate::model::NEO4J_TYPES {
            assert!(seen.contains(ty), "example does not cover {token}");
        }
    }

    #[test]
    fn the_example_covers_all_four_constraint_types() {
        let seen = constraint_types(&example_doc());
        // Same rule as the type-token test: the closed constraint
        // vocabulary is walked from its one canonical table.
        for (token, kind) in crate::model::CONSTRAINTS {
            assert!(seen.contains(kind), "example does not cover {token}");
        }
    }
}
