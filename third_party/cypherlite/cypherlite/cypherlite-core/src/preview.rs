//! Preview-feature gating for the Cypher dialect (ADR-0371).
//!
//! A preview feature is a Cypher language feature accepted into a CIP but
//! not yet part of the pinned dialect floor (the openCypher TCK pin).
//! `CYPHERLITE_PREVIEW` is the global opt-in: a comma-separated list of
//! kebab-case feature names, read once at `Database::open`. Unset or empty
//! means no preview features; an unknown name fails open with a loud usage
//! error naming the known names — never a silent ignore.
//!
//! Only syntax-extending features gate: a purely additive function name
//! never needs the opt-in, so it never lands in this registry. The one
//! exception (ADR-0378) is a built-in procedure family whose contract
//! tracks a non-final external spec: the `db.ontology.*` procedures behind
//! the `ontology` row. The exception is not general — additive function
//! names and procedure families on our own pinned contract still never
//! gate. Each gated feature checks the gate where it would otherwise be
//! parsed or dispatched; while disabled the check fails with
//! [`preview_feature_disabled`]'s authored error.
//!
//! The registry macro is the single source: variant, doc, and name are
//! declared once, and the macro emits every derivation — the enum, the
//! exhaustive `kebab_name`/`from_kebab_name` matches (coverage is a
//! compile guarantee), and [`PREVIEW_FEATURE_TABLE`], the registry view
//! the fail-closed known-names list renders from. A later feature PR adds
//! its row here and gates its syntax; nothing else names the feature.

use cypherlite_error::TckErrorClass;

use crate::Error;

/// The one registry: each preview feature's variant, doc comment, and
/// kebab-case `CYPHERLITE_PREVIEW` name are declared from this single
/// list, so a variant cannot exist without its name and vice versa — a
/// forgotten row is a compile error, not a runtime `expect`. Every other
/// spelling of a feature name derives from [`PREVIEW_FEATURE_TABLE`].
macro_rules! preview_feature_registry {
    ($(($variant:ident, $doc:literal, $name:literal)),* $(,)?) => {
        /// A gated preview Cypher feature. The table order is the
        /// registry order; rows are never renumbered, only appended.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum PreviewFeature {
            $(
                #[doc = $doc]
                $variant,
            )*
        }

        /// The one registry: each preview feature with its kebab-case
        /// `CYPHERLITE_PREVIEW` name. The table order is the registry
        /// order; rows are never renumbered, only appended.
        pub const PREVIEW_FEATURE_TABLE: &[(PreviewFeature, &str)] =
            &[$((PreviewFeature::$variant, $name)),*];

        /// Every registered feature, registry order — the enumeration
        /// callers iterate instead of re-spelling the vocabulary.
        pub const PREVIEW_FEATURES: &[PreviewFeature] = &[$(PreviewFeature::$variant),*];

        /// The registry width bound: the bitset's `u32` holds every
        /// registered feature — a 33rd row is a compile error, not a
        /// runtime wrap.
        const _: () = assert!([$(stringify!($variant)),*].len() <= 32);

        impl PreviewFeature {
            /// The feature's kebab-case `CYPHERLITE_PREVIEW` name. The
            /// exhaustive match is the compile guarantee the registry
            /// exists for — coverage cannot drift from the table.
            pub fn kebab_name(self) -> &'static str {
                match self {
                    $(PreviewFeature::$variant => $name,)*
                }
            }

            /// The feature for a kebab-case `CYPHERLITE_PREVIEW` name.
            /// `None` when `name` is not a registered feature.
            pub fn from_kebab_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(PreviewFeature::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

preview_feature_registry! {
    (HybridSearch, "CIP-278 hybrid search `COMBINE` (reserved; lands later).", "hybrid-search"),
    (LocalCallables, "CIP-231 local callables: DEFINE PROCEDURE/FUNCTION, ADR-0382.", "local-callables"),
    (GraphTypeOptProps, "CIP-283 graph-type optional properties (`prop :: ANY` in element types).", "graph-type-opt-props"),
    (GraphTypeDefaultValues, "CIP-262 graph-type property default values (`DEFAULT ON COMMIT <value expression>`).", "graph-type-default-values"),
    (GraphTypeImpliedLabels, "CIP-266 graph-type implied labels that identify node element types (inheritance).", "graph-type-implied-labels"),
    (
        Ontology,
        "The `db.ontology.*` procedures (ADR-0378: the first non-syntax extension of ADR-0371's \
         scope policy — the family's contract tracks the non-final Ontology Graph Spec 1.0.0 and \
         CIP-287, and row plus gate are removed once both are final).",
        "ontology"
    ),
    (
        GqlFor,
        "CIP-250 (GQ10): the GQL `FOR <variable> IN <expression>` clause, the UNWIND-successor \
         spelling normalized to the existing UNWIND infrastructure.",
        "gql-for"
    ),
}

/// The registered feature names, comma-separated, for the fail-closed
/// unknown-name error. The table's order, the registry's order.
fn known_feature_names() -> String {
    PREVIEW_FEATURE_TABLE
        .iter()
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The set of preview features enabled by `CYPHERLITE_PREVIEW` for one
/// open store. Empty ([`PreviewFeatures::none`]) disables every gated
/// feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewFeatures(u32);

/// The fail-closed empty set: the value a store-level service bundle
/// carries until open installs the resolved `CYPHERLITE_PREVIEW` set
/// (ADR-0371, ADR-0378), so an executor-only `Graph::default()` gates every
/// preview feature closed.
impl Default for PreviewFeatures {
    fn default() -> Self {
        Self::none()
    }
}

impl PreviewFeatures {
    /// No preview features: every gate fails closed.
    pub fn none() -> Self {
        Self(0)
    }

    /// Parse one `CYPHERLITE_PREVIEW` value: a comma-separated list of
    /// kebab-case feature names, whitespace tolerated around each name.
    /// An empty or blank value enables nothing. An unknown name (including
    /// an empty list segment) is a loud usage error naming the known
    /// names — fail-closed, never silently ignored.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        if raw.trim().is_empty() {
            return Ok(Self::none());
        }
        let mut enabled = Self(0);
        for name in raw.split(',') {
            let name = name.trim();
            let feature = PreviewFeature::from_kebab_name(name).ok_or_else(|| {
                Error::semantic(format!(
                    "{}: invalid CYPHERLITE_PREVIEW value: unknown preview feature \
                     '{name}' (known features: {})",
                    TckErrorClass::SemanticError.as_str(),
                    known_feature_names(),
                ))
            })?;
            // A fieldless variant's discriminant is its registry index.
            enabled.0 |= 1 << (feature as u32);
        }
        Ok(enabled)
    }

    /// Whether `feature` is enabled for this store — the typed gate
    /// predicate a gated feature checks where it would otherwise be parsed
    /// or dispatched.
    pub fn enabled(self, feature: PreviewFeature) -> bool {
        self.0 & (1 << (feature as u32)) != 0
    }

    /// The gate check: `Ok(())` when `feature` is enabled, otherwise the
    /// authored fail-closed error of [`preview_feature_disabled`].
    pub fn require(self, feature: PreviewFeature) -> Result<(), Error> {
        if self.enabled(feature) {
            Ok(())
        } else {
            Err(preview_feature_disabled(feature))
        }
    }
}

/// The authored fail-closed gate text for a preview feature invoked while
/// disabled, already prefixed with its `SemanticError` class — the single
/// source of the gate message. [`PreviewFeatures::require`] wraps it in an
/// [`Error`]; layers outside this crate (the executor's graph-type
/// semantic layer, which fails closed on the same constructs its
/// statement-admission gate already checked when they only materialize
/// against stored graph-type state) render it through their own error
/// type without re-spelling it.
pub fn preview_feature_disabled_text(feature: PreviewFeature) -> String {
    let name = feature.kebab_name();
    format!(
        "{}: preview feature '{name}' is not enabled: set CYPHERLITE_PREVIEW={name}",
        TckErrorClass::SemanticError.as_str(),
    )
}

/// The authored fail-closed gate error for a preview feature invoked while
/// disabled (the house style of the ADR-0229 §2 authored text): the
/// engine renders `semantic error: SemanticError: preview feature
/// '<name>' is not enabled: set CYPHERLITE_PREVIEW=<name>`.
pub(crate) fn preview_feature_disabled(feature: PreviewFeature) -> Error {
    Error::semantic(preview_feature_disabled_text(feature))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_round_trip() {
        // The macro single-sources the variant list and the rows, so
        // exhaustiveness is a compile guarantee; these pin the rest: no
        // name spelled twice, and every row's name parses back to its
        // variant.
        let mut seen = std::collections::HashSet::new();
        for (feature, name) in PREVIEW_FEATURE_TABLE {
            assert_eq!(feature.kebab_name(), *name);
            assert!(seen.insert(*name), "duplicate name {name}");
            assert_eq!(PreviewFeature::from_kebab_name(name), Some(*feature));
        }
        assert_eq!(PreviewFeature::from_kebab_name("nope"), None);
    }

    #[test]
    fn parse_empty_and_unset_value_enable_nothing() {
        assert_eq!(PreviewFeatures::parse("").unwrap(), PreviewFeatures::none());
        assert_eq!(
            PreviewFeatures::parse("   ").unwrap(),
            PreviewFeatures::none()
        );
        for feature in PREVIEW_FEATURES {
            assert!(!PreviewFeatures::none().enabled(*feature));
        }
    }

    #[test]
    fn parse_comma_lists_and_whitespace() {
        let features =
            PreviewFeatures::parse(" hybrid-search,graph-type-implied-labels , local-callables ")
                .unwrap();
        assert!(features.enabled(PreviewFeature::HybridSearch));
        assert!(features.enabled(PreviewFeature::GraphTypeImpliedLabels));
        assert!(features.enabled(PreviewFeature::LocalCallables));
        assert!(!features.enabled(PreviewFeature::GraphTypeOptProps));
        assert!(!features.enabled(PreviewFeature::GraphTypeDefaultValues));
        // A repeated name stays a plain enable, not an error.
        assert_eq!(
            PreviewFeatures::parse("hybrid-search, hybrid-search").unwrap(),
            PreviewFeatures::parse("hybrid-search").unwrap(),
        );
    }

    #[test]
    fn parse_unknown_name_fails_closed_naming_known_names() {
        let error = PreviewFeatures::parse("hybrid-search,bogus").unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{error}");
        let display = error.to_string();
        assert!(
            display.contains("unknown preview feature 'bogus'"),
            "{display}"
        );
        for (_, name) in PREVIEW_FEATURE_TABLE {
            assert!(display.contains(name), "{display} misses {name}");
        }
        // An empty segment is an unknown name, not a silent skip.
        let error = PreviewFeatures::parse("hybrid-search,,local-callables").unwrap_err();
        assert!(
            error.to_string().contains("unknown preview feature ''"),
            "{}",
            error
        );
    }

    #[test]
    fn parse_each_registered_name_enables_exactly_that_feature() {
        // Table-driven over the one registry: every row's name parses to a
        // set with exactly that row enabled, so a new row is covered
        // without a new test. (The `ontology` row is ADR-0378's
        // procedure-family exception to ADR-0371's syntax-only scope
        // policy; it needs no special case.)
        for feature in PREVIEW_FEATURES {
            let enabled = PreviewFeatures::parse(feature.kebab_name()).unwrap();
            assert!(enabled.enabled(*feature), "{}", feature.kebab_name());
            for other in PREVIEW_FEATURES {
                if other != feature {
                    assert!(
                        !enabled.enabled(*other),
                        "{} also enabled {}",
                        feature.kebab_name(),
                        other.kebab_name()
                    );
                }
            }
        }
    }

    #[test]
    fn require_gates_with_the_authored_disabled_error() {
        let features = PreviewFeatures::parse("local-callables").unwrap();
        assert!(features.require(PreviewFeature::LocalCallables).is_ok());
        let error = features.require(PreviewFeature::HybridSearch).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic error: SemanticError: preview feature 'hybrid-search' is not enabled: \
             set CYPHERLITE_PREVIEW=hybrid-search"
        );
        // The disabled text is stable for every registered feature.
        for (feature, name) in PREVIEW_FEATURE_TABLE {
            let error = preview_feature_disabled(*feature);
            assert!(
                error.to_string().ends_with(&format!(
                    "preview feature '{name}' is not enabled: set CYPHERLITE_PREVIEW={name}"
                )),
                "{}",
                error
            );
        }
    }
}
