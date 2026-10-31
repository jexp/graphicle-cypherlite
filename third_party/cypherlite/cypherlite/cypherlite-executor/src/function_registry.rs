//! Canonical built-in function registry: the one table of the functions
//! `SHOW FUNCTIONS` describes and the typed deprecation lookup the query
//! notifications read (ADR-0363 decision 5).
//!
//! Mirrors [`crate::procedure_registry`]: the table and its typed lookups
//! live here, while `show_catalog` only renders the rows. A catalog entry is
//! deprecated exactly when its `deprecated_by` option is `Some` (01N01), so
//! there is no second flag to keep aligned.

use cypherlite_core::GRAPH_SELECTOR_FUNCTIONS;
use cypherlite_front::agg;

use crate::catalog_meta::{DeprecatedBy, ProcArgMeta};

/// Rich catalog metadata for the built-in functions CIP-84/CIP-113/CIP-157
/// scenarios pin -- `category: None` keeps the vector/predicate/
/// aggregating/scalar classification [`function_category`] owns; every other
/// function falls back to a generic, always-safe placeholder (empty
/// `argumentDescription`, `isDeprecated: false`, `deprecatedBy: null`) via
/// `function_catalog_rows`'s placeholder branch, matching CIP-84's own
/// "everything else isDeprecated=false / deprecatedBy=null" default.
/// `signature` (CIP-113) is *derived* from `args`/`return_type` via
/// `render_function_signature` rather than
/// stored literally, except for `special_signature`, used only by the
/// `all`/`any`/`none`/`single` list-comprehension-styled quantifiers, whose
/// grammar (`variable :: VARIABLE IN list :: LIST<ANY> WHERE predicate ::
/// ANY`) CIP-113 keeps as a special case rather than an ordinary parameter
/// list.
pub(crate) struct FunctionMeta {
    pub(crate) category: Option<&'static str>,
    pub(crate) description: &'static str,
    pub(crate) special_signature: Option<&'static str>,
    /// The formal parameters. Most arrays use the compact
    /// [`ProcArgMeta::described`] constructor inside an inline
    /// `const { &[...] }`: a runtime `&[...]` of constructor calls is a
    /// temporary, not a promoted `'static` slice. Entries carrying a
    /// `default` keep the explicit struct literal so the default stays
    /// visible at its entry.
    pub(crate) args: &'static [ProcArgMeta],
    pub(crate) return_type: &'static str,
    /// The replacement when the function is deprecated (CIP-157's
    /// `deprecatedBy`); `None` means not deprecated. One fact, so a bool and
    /// its replacement cannot disagree.
    pub(crate) deprecated_by: Option<DeprecatedBy>,
}

/// Looks up every catalog row a function name should produce: zero (falls
/// through to `function_catalog_rows`'s generic placeholder), one (the
/// common case), or several for a name whose overloads differ by *arity*
/// (CIP-113's `range()` -- a 2-arg and a 3-arg form -- kept as separate
/// signature rows since only same-arity, different-type overloads condense
/// into a closed dynamic union). One dispatcher owns the arity-overloaded
/// names; every other name reads [`function_meta`].
pub(crate) fn function_metas(name: &str) -> Vec<FunctionMeta> {
    match name {
        "range" => range_metas(),
        "normalize" => normalize_metas(),
        _ => function_meta(name).into_iter().collect(),
    }
}

/// Names that are `Predicate`-category functions rather than the generic
/// `Scalar` bucket. Small, curated list — only functions whose category is
/// actually asserted by a scenario need to be accurate here.
fn is_predicate(name: &str) -> bool {
    matches!(name, "property_exists" | "exists")
}

/// Names that are `Vector`-category functions (CIP-121 `namespace.feature`
/// asserts `category = 'Vector'` for `vector.similarity.*`). Covers the
/// whole `vector`/`vector.*`/`vector_*` family for consistency, not just
/// the two functions a scenario currently checks.
fn is_vector(name: &str) -> bool {
    name == "vector" || name.starts_with("vector.") || name.starts_with("vector_")
}

/// Whether the function's `aggregating` catalog column is true — the one
/// aggregate-name lookup `SHOW FUNCTIONS` and the category fallback share.
pub(crate) fn function_is_aggregating(name: &str) -> bool {
    agg::is_aggregate_name(name)
}

/// The `category` cell of the function's `SHOW FUNCTIONS` row (CIP-84):
/// the registry owns the classification, so the renderer never re-derives
/// it from the name. A metadata entry's explicit `category` wins; otherwise
/// a vector/predicate/aggregating family name maps to its category and
/// every remaining function is `Scalar` — including the `None` placeholder
/// for a name with no metadata entry.
pub(crate) fn function_category(name: &str, meta: Option<&FunctionMeta>) -> &'static str {
    if let Some(category) = meta.and_then(|meta| meta.category) {
        return category;
    }
    if is_vector(name) {
        "Vector"
    } else if is_predicate(name) {
        "Predicate"
    } else if function_is_aggregating(name) {
        "Aggregating"
    } else {
        "Scalar"
    }
}

/// CIP-113's two `range()` arity-overloads: the 2-arg form (default step 1)
/// and the 3-arg form.
fn range_metas() -> Vec<FunctionMeta> {
    vec![
        FunctionMeta {
            category: None,
            description: "Returns a `LIST` of `INTEGER` values within a specified range, with a default step of 1.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("start", "The first number in the range.", "INTEGER"),
                ProcArgMeta::described("end", "The last number in the range.", "INTEGER"),
            ] },
            return_type: "LIST<INTEGER>",
            deprecated_by: None,
        },
        FunctionMeta {
            category: None,
            description: "Returns a `LIST` of `INTEGER` values within a specified range, with a given step.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("start", "The first number in the range.", "INTEGER"),
                ProcArgMeta::described("end", "The last number in the range.", "INTEGER"),
                ProcArgMeta::described("step", "The step between each value in the range.", "INTEGER"),
            ] },
            return_type: "LIST<INTEGER>",
            deprecated_by: None,
        },
    ]
}

/// CIP-132: `normalize` has a one-argument form (implicit NFC) and a
/// two-argument form (explicit `normalForm` keyword), kept as separate
/// signature rows the same way `range`'s arity-overloads are, since
/// `normalForm` is a bare keyword rather than a typed parameter with a
/// uniform default across arities.
fn normalize_metas() -> Vec<FunctionMeta> {
    vec![
        FunctionMeta {
            category: Some("String"),
            description: "Normalizes a `STRING` using the NFC normalization form.",
            special_signature: None,
            args: const {
                &[ProcArgMeta::described(
                    "input",
                    "The `STRING` to normalize.",
                    "STRING",
                )]
            },
            return_type: "STRING",
            deprecated_by: None,
        },
        FunctionMeta {
            category: Some("String"),
            description: "Normalizes a `STRING` using the specified normalization form.",
            special_signature: None,
            args: const {
                &[
                    ProcArgMeta::described("input", "The `STRING` to normalize.", "STRING"),
                    ProcArgMeta {
                        name: "normalForm",
                        description: "The normalization form to use.",
                        type_str: "[NFC, NFD, NFKC, NFKD]",
                        deprecated: false,
                        default: Some("NFC"),
                    },
                ]
            },
            return_type: "STRING",
            deprecated_by: None,
        },
    ]
}

fn function_meta(name: &str) -> Option<FunctionMeta> {
    match name {
        "abs" => Some(FunctionMeta {
            category: Some("Numeric"),
            description: "Returns the absolute value of an `INTEGER` or `FLOAT`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A numeric value from which the absolute number will be returned.", "INTEGER | FLOAT")] },
            return_type: "INTEGER | FLOAT",
            deprecated_by: None,
        }),
        // CIP-84/CIP-157: `id` is deprecated under Cypher 25, in favor of
        // `elementId`/an application-generated id.
        "id" => Some(FunctionMeta {
            category: None,
            description: "Returns the id of a `NODE` or `RELATIONSHIP`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A node or a relationship.", "NODE | RELATIONSHIP")] },
            return_type: "INTEGER",
            deprecated_by: Some(DeprecatedBy::identified(
                "elementId",
                "elementId or consider using an application-generated id",
            )),
        }),
        "round" => Some(FunctionMeta {
            category: None,
            description: "Returns the value of a number rounded to the closest value of the specified precision, using the specified rounding mode.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("value", "The number to round.", "INTEGER | FLOAT"),
                ProcArgMeta {
                    name: "precision",
                    description: "The number of decimal places to round to.",
                    type_str: "INTEGER",
                    deprecated: false,
                    default: Some("0"),
                },
                ProcArgMeta {
                    name: "mode",
                    description: "The rounding mode to use.",
                    type_str: "STRING",
                    deprecated: false,
                    default: Some("'HALF_UP'"),
                },
            ] },
            return_type: "FLOAT",
            deprecated_by: None,
        }),
        // CIP-113 F2: a no-argument function, cataloged for the composite-
        // database `graph.*` family (CIP-61's `graph.names`/CIP-147's
        // signature) -- surfaced in `SHOW FUNCTIONS` per CIP-230's reserved-
        // namespace list even though calling it is out of scope for a
        // single-graph embedded engine (see `CATALOG_ONLY_FUNCTIONS`).
        "graph.names" => Some(FunctionMeta {
            category: None,
            description: "Returns a `LIST<STRING>` of the names of the constituent graphs of a composite database.",
            special_signature: None,
            args: const { &[] },
            return_type: "LIST<STRING>",
            deprecated_by: None,
        }),
        "graph.byname" => Some(FunctionMeta {
            category: None,
            description: "Returns a `GRAPH` reference to a constituent graph of a composite database, resolved by name.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "The name of a constituent graph.", "STRING")] },
            return_type: "GRAPH",
            deprecated_by: None,
        }),
        "graph.byelementid" => Some(FunctionMeta {
            category: None,
            description: "Returns a `GRAPH` reference to a constituent graph of a composite database, resolved from an element id.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "An element id belonging to a constituent graph.", "STRING")] },
            return_type: "GRAPH",
            deprecated_by: None,
        }),
        "graph.propertiesbyname" => Some(FunctionMeta {
            category: None,
            description: "Returns a `MAP` of the properties of a constituent graph of a composite database, resolved by name.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "The name of a constituent graph.", "STRING")] },
            return_type: "MAP",
            deprecated_by: None,
        }),
        // CIP-113 F5/F6/F7/F24: the temporal constructors all share the
        // same two-optional-parameter (`input`/`pattern`, both defaulting
        // to `DEFAULT_TEMPORAL_ARGUMENT`) shape.
        "localtime" => Some(temporal_constructor_meta("LOCAL TIME")),
        "time" => Some(temporal_constructor_meta("ZONED TIME")),
        "localdatetime" => Some(temporal_constructor_meta("LOCAL DATETIME")),
        "date" => Some(temporal_constructor_meta("DATE")),
        "datetime.fromepoch" => Some(FunctionMeta {
            category: None,
            description: "Returns the `ZONED DATETIME` obtained by adding the number of seconds and nanoseconds since the start of the epoch to midnight, January 1, 1970 UTC.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("seconds", "The number of seconds since the start of the epoch.", "INTEGER | FLOAT"),
                ProcArgMeta::described("nanoseconds", "The number of nanoseconds since the start of the epoch.", "INTEGER | FLOAT"),
            ] },
            return_type: "ZONED DATETIME",
            deprecated_by: None,
        }),
        // CIP-113 F14: like the other temporal constructors, but `input` is
        // required (no default) -- the doc's own example.
        "duration" => Some(FunctionMeta {
            category: None,
            description: "Constructs a `DURATION` value.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("input", "A map, string, or duration to construct the duration from.", "ANY"),
                ProcArgMeta {
                    name: "pattern",
                    description: "The format of the input string.",
                    type_str: "STRING",
                    deprecated: false,
                    default: Some("DEFAULT_TEMPORAL_ARGUMENT"),
                },
            ] },
            return_type: "DURATION",
            deprecated_by: None,
        }),
        "keys" => Some(FunctionMeta {
            category: None,
            description: "Returns a `LIST<STRING>` containing the `STRING` representations for all the property names of a `NODE`, `RELATIONSHIP`, or `MAP`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "An entity or map from which to return property keys.", "NODE | RELATIONSHIP | MAP")] },
            return_type: "LIST<STRING>",
            deprecated_by: None,
        }),
        "properties" => Some(FunctionMeta {
            category: None,
            description: "Returns a `MAP` containing all the properties of a `NODE`, `RELATIONSHIP`, or `MAP`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "An entity or map from which to return all properties.", "NODE | RELATIONSHIP | MAP")] },
            return_type: "MAP",
            deprecated_by: None,
        }),
        "point" => Some(FunctionMeta {
            category: None,
            description: "Returns a `POINT` value, given a `MAP` containing the point's coordinates.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A map describing the point to construct.", "MAP")] },
            return_type: "POINT",
            deprecated_by: None,
        }),
        "point.distance" | "distance" => Some(FunctionMeta {
            category: None,
            description: "Returns a `FLOAT` representing the distance between any two points in the same CRS.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("from", "The point to measure the distance from.", "POINT"),
                ProcArgMeta::described("to", "The point to measure the distance to.", "POINT"),
            ] },
            return_type: "FLOAT",
            deprecated_by: None,
        }),
        "startnode" => Some(FunctionMeta {
            category: None,
            description: "Returns the start `NODE` of a `RELATIONSHIP`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A relationship whose start node is returned.", "RELATIONSHIP")] },
            return_type: "NODE",
            deprecated_by: None,
        }),
        "endnode" => Some(FunctionMeta {
            category: None,
            description: "Returns the end `NODE` of a `RELATIONSHIP`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A relationship whose end node is returned.", "RELATIONSHIP")] },
            return_type: "NODE",
            deprecated_by: None,
        }),
        "type" => Some(FunctionMeta {
            category: None,
            description: "Returns a `STRING` representation of the `RELATIONSHIP` type.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A relationship whose type is returned.", "RELATIONSHIP")] },
            return_type: "STRING",
            deprecated_by: None,
        }),
        "nodes" => Some(FunctionMeta {
            category: None,
            description: "Returns a `LIST<NODE>` containing all the `NODE` values in a `PATH`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A path from which to return all nodes.", "PATH")] },
            return_type: "LIST<NODE>",
            deprecated_by: None,
        }),
        "relationships" | "rels" => Some(FunctionMeta {
            category: None,
            description: "Returns a `LIST<RELATIONSHIP>` containing all the `RELATIONSHIP` values in a `PATH`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A path from which to return all relationships.", "PATH")] },
            return_type: "LIST<RELATIONSHIP>",
            deprecated_by: None,
        }),
        "tostring" => Some(FunctionMeta {
            category: None,
            description: "Converts an `ANY` value to a `STRING` value.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A value to be converted to a string.", "ANY")] },
            return_type: "STRING",
            deprecated_by: None,
        }),
        "isnan" => Some(FunctionMeta {
            category: None,
            description: "Returns whether `input` is `NaN`.",
            special_signature: None,
            args: const { &[ProcArgMeta::described("input", "A numeric value to check for `NaN`.", "INTEGER | FLOAT")] },
            return_type: "BOOLEAN",
            deprecated_by: None,
        }),
        "left" => Some(FunctionMeta {
            category: None,
            description: "Returns a `STRING` containing the specified number of leftmost characters of the given `STRING`.",
            special_signature: None,
            args: const { &[
                ProcArgMeta::described("original", "The string from which to return characters.", "STRING"),
                ProcArgMeta::described("length", "The number of leftmost characters to return.", "INTEGER"),
            ] },
            return_type: "STRING",
            deprecated_by: None,
        }),
        // CIP-113 F18/F19: the quantifier functions keep their pre-existing
        // list-comprehension-styled signature (`variable :: VARIABLE IN
        // list :: LIST<ANY> WHERE predicate :: ANY`) rather than an
        // ordinary parameter list; only the type-name renaming rules apply
        // to it, already reflected in the literal text below.
        "all" => Some(quantifier_meta(
            "all(variable :: VARIABLE IN list :: LIST<ANY> WHERE predicate :: ANY) :: BOOLEAN",
        )),
        "any" => Some(quantifier_meta(
            "any(variable :: VARIABLE IN list :: LIST<ANY> WHERE predicate :: ANY) :: BOOLEAN",
        )),
        "none" => Some(quantifier_meta(
            "none(variable :: VARIABLE IN list :: LIST<ANY> WHERE predicate :: ANY) :: BOOLEAN",
        )),
        "single" => Some(quantifier_meta(
            "single(variable :: VARIABLE IN list :: LIST<ANY> WHERE predicate :: ANY) :: BOOLEAN",
        )),
        _ => None,
    }
}

/// Builds the list-comprehension-styled signature CIP-113 keeps for
/// `all`/`any`/`none`/`single` (F18/F19): `name(variable :: VARIABLE IN
/// list :: LIST<ANY> WHERE predicate :: ANY) :: BOOLEAN`. The signature is
/// the caller's, so the four names cannot share a fabricated fallback.
fn quantifier_meta(signature: &'static str) -> FunctionMeta {
    FunctionMeta {
        category: Some("Predicate"),
        description: "A list-comprehension-styled quantifier function.",
        special_signature: Some(signature),
        args: const { &[] },
        return_type: "BOOLEAN",
        deprecated_by: None,
    }
}

/// Shared shape of `date`/`localtime`/`time`/`localdatetime`: two optional
/// parameters (`input`, `pattern`), both defaulting to
/// `DEFAULT_TEMPORAL_ARGUMENT` -- verified live against Neo4j 2026.05 for
/// all four constructors (CIP-113 NOTES.md "Adversarial review fix pass").
fn temporal_constructor_meta(return_type: &'static str) -> FunctionMeta {
    FunctionMeta {
        category: None,
        description: "Constructs a temporal value from the given input parameters.",
        special_signature: None,
        args: const {
            &[
                ProcArgMeta {
                    name: "input",
                    description: "A map, string, or temporal value to construct the value from.",
                    type_str: "ANY",
                    deprecated: false,
                    default: Some("DEFAULT_TEMPORAL_ARGUMENT"),
                },
                ProcArgMeta {
                    name: "pattern",
                    description: "The format of the input string.",
                    type_str: "STRING",
                    deprecated: false,
                    default: Some("DEFAULT_TEMPORAL_ARGUMENT"),
                },
            ]
        },
        return_type,
        deprecated_by: None,
    }
}

/// CIP-230 reserved-namespace `graph.*` functions (`graph.names`,
/// `graph.byName`, `graph.byElementId`, `graph.propertiesByName`): real
/// Neo4j built-ins that enumerate/target constituent graphs of a *composite*
/// (Fabric) database. A single-graph embedded engine has no composite
/// database concept to execute these against (see MANIFEST.md CIP-61/107/
/// 147/193/212), so they're not in `validate::KNOWN_FUNCTIONS` and can't be
/// called -- but CIP-230's reserved-namespace list still expects them to be
/// *discoverable* via `SHOW FUNCTIONS` (01:[1]), so they get catalog-only
/// entries here, the same "listed but not executable" treatment the
/// `procedure_registry` table gives several procedures. The names come
/// from [`cypherlite_core::GRAPH_SELECTOR_FUNCTIONS`], shared with the
/// planner's `use_target` so the two lists cannot drift.
pub(crate) const CATALOG_ONLY_FUNCTIONS: &[&str] = &GRAPH_SELECTOR_FUNCTIONS;

/// The replacement when `name` resolves to a deprecated built-in function,
/// `None` otherwise. One typed lookup over [`function_metas`] — the same
/// table `SHOW FUNCTIONS` renders — so no caller keeps a second list of
/// deprecated names. Function calls match case-insensitively (CIP-230), so
/// the lookup lowercases like the validator does. The registry's
/// `deprecated_by` field is the whole deprecation fact — `None` means not
/// deprecated.
pub fn builtin_function_deprecation(name: &str) -> Option<DeprecatedBy> {
    let lower = name.to_ascii_lowercase();
    function_metas(&lower)
        .into_iter()
        .find_map(|meta| meta.deprecated_by)
}

#[cfg(test)]
mod tests {
    use crate::catalog_meta::DeprecationForm;

    use super::*;

    /// The lookup reads the one function catalog: `id` is deprecated with
    /// its `elementId` replacement (while its catalog display keeps the
    /// CIP-157 prose sentence), `elementId` itself is not, and an unknown
    /// name is not.
    #[test]
    fn function_deprecation_lookup_reads_the_function_catalog() {
        let replacement = builtin_function_deprecation("ID").expect("id is deprecated");
        assert_eq!(
            replacement.display(),
            "elementId or consider using an application-generated id"
        );
        assert_eq!(replacement.form(), DeprecationForm::Identifier("elementId"));
        assert!(builtin_function_deprecation("elementId").is_none());
        assert!(builtin_function_deprecation("no.such.function").is_none());
    }
}
