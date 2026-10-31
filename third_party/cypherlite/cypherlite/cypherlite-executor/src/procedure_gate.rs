//! The one built-in-procedure → preview-feature mapping (ADR-0378).
//!
//! The `db.ontology.*` procedure family is gated behind ADR-0371's
//! `CYPHERLITE_PREVIEW` opt-in. Both consumers of that fact — the facade's
//! plan gate (`cypherlite::gate`) and the `SHOW PROCEDURES` catalog
//! (`show_catalog`) — read this mapping, and the mapping itself derives the
//! family from the canonical registry predicate
//! ([`is_ontology_procedure`]), so the gate never re-spells the family.

use cypherlite_core::PreviewFeature;

use crate::procedure_registry::is_ontology_procedure;

/// The preview feature gating procedure `name` (ADR-0378):
/// `Some(PreviewFeature::Ontology)` when `name` resolves to one of the
/// built-in `db.ontology.*` registry signatures, `None` for every other
/// name. The lookup is by name against the built-in registry only and never
/// consults the host procedure registry, so a user-registered procedure is
/// not gated — a host `db.ontology.custom`, for example, resolves to no
/// built-in signature and stays ungated.
pub fn procedure_preview_feature(name: &str) -> Option<PreviewFeature> {
    is_ontology_procedure(name).then_some(PreviewFeature::Ontology)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure_registry::BUILTIN_PROCEDURE_SIGNATURES;

    #[test]
    fn the_ontology_family_is_the_only_gated_procedure_family() {
        // The gated names, spelled literally: this is the independent fact
        // the test pins, not a re-run of the predicate the mapping is
        // defined from, so a mapping change that moved a name in or out of
        // the family fails here.
        let family = [
            ("db.ontology.set", Some(PreviewFeature::Ontology)),
            ("db.ontology.get", Some(PreviewFeature::Ontology)),
            ("db.ontology.drop", Some(PreviewFeature::Ontology)),
            ("db.ontology.apply", Some(PreviewFeature::Ontology)),
        ];
        for (name, expected) in family {
            assert_eq!(procedure_preview_feature(name), expected, "{name}");
        }
        // A non-ontology name is ungated, and so is a host-registered
        // `db.ontology.*` name: the mapping reads the built-in registry
        // only.
        assert_eq!(procedure_preview_feature("db.labels"), None);
        assert_eq!(procedure_preview_feature("db.ontology.custom"), None);
        // No other registry row is gated either: the gated set is exactly
        // the four literal names above, so a registry row added or dropped
        // without the matching mapping change fails here.
        let gated: Vec<&str> = BUILTIN_PROCEDURE_SIGNATURES
            .iter()
            .filter(|signature| procedure_preview_feature(signature.name).is_some())
            .map(|signature| signature.name)
            .collect();
        assert_eq!(
            gated,
            [
                "db.ontology.set",
                "db.ontology.get",
                "db.ontology.drop",
                "db.ontology.apply"
            ]
        );
    }
}
