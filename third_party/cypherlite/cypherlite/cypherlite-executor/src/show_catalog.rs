//! SHOW FUNCTIONS / SHOW PROCEDURES static catalogs (CIP discoverability).
use crate::catalog_meta::ProcArgMeta;
use crate::function_registry::{
    function_category, function_is_aggregating, function_metas, FunctionMeta,
    CATALOG_ONLY_FUNCTIONS,
};
use crate::procedure_registry::BUILTIN_PROCEDURE_SIGNATURES;
use cypherlite_core::PreviewFeatures;
use cypherlite_front::KNOWN_FUNCTIONS;
use cypherlite_model::CypherValue;
use indexmap::IndexMap;

type Row = IndexMap<String, CypherValue>;

/// One entry of a function's/procedure's `argumentDescription`/
/// `returnDescription` map column (CIP-84): `name`/`description`/`type`/
/// `isDeprecated` always present, `default` only when the parameter
/// actually has a default value (CIP-84 show-functions-deprecation.feature
/// [11]/show-procedures-deprecation.feature [10]/[12]). The entries live in
/// the canonical `catalog_meta` shape both registries share.
fn arg_meta_value(a: &ProcArgMeta) -> CypherValue {
    let mut m = IndexMap::new();
    m.insert("name".to_string(), CypherValue::Str(a.name.to_string()));
    m.insert(
        "description".to_string(),
        CypherValue::Str(a.description.to_string()),
    );
    m.insert("type".to_string(), CypherValue::Str(a.type_str.to_string()));
    m.insert("isDeprecated".to_string(), CypherValue::Bool(a.deprecated));
    if let Some(d) = a.default {
        m.insert("default".to_string(), CypherValue::Str(d.to_string()));
    }
    CypherValue::Map(m)
}

/// Renders a comma-separated `reqParamList, optParamList` (CIP-113's
/// abstract syntax: every parameter as `name :: type` or, when it carries a
/// default, `name = default :: type`). Required parameters are expected to
/// be listed before optional ones in `args` itself -- callers build `args`
/// in that order, so this function doesn't need to reorder anything (CIP-113
/// procedure-signatures.feature [15]).
fn render_param_list(args: &[ProcArgMeta]) -> String {
    args.iter()
        .map(|a| match a.default {
            Some(d) => format!("{} = {} :: {}", a.name, d, a.type_str),
            None => format!("{} :: {}", a.name, a.type_str),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a CIP-113 function signature: `name(paramList) :: returnType`,
/// with no parentheses around the (possibly closed-union) return type and
/// an empty `()` when `args` is empty.
fn render_function_signature(name: &str, args: &[ProcArgMeta], return_type: &str) -> String {
    format!("{}({}) :: {}", name, render_param_list(args), return_type)
}

/// Renders a CIP-113 procedure signature: `name(paramList) :: (ret :: type,
/// ...)`, with the return-type suffix dropped entirely (not `::VOID`,
/// `::()`, or `::NOTHING` -- all three were considered and rejected per the
/// CIP's Revision History) when the procedure yields no columns.
fn render_procedure_signature(name: &str, args: &[ProcArgMeta], returns: &[ProcArgMeta]) -> String {
    let params = render_param_list(args);
    if returns.is_empty() {
        format!("{name}({params})")
    } else {
        let rets = returns
            .iter()
            .map(|r| format!("{} :: {}", r.name, r.type_str))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{name}({params}) :: ({rets})")
    }
}

/// Maps a `validate::KNOWN_FUNCTIONS`/`CATALOG_ONLY_FUNCTIONS` entry (always
/// lowercase, since function-call matching is case-insensitive -- CIP-230's
/// "Casing sensitivity" section) to the properly-cased name Neo4j's own
/// `SHOW FUNCTIONS` catalog displays it under. Only the handful of entries a
/// CIP-113/CIP-230 scenario actually pins by exact display casing are
/// listed; every other name is already stored in its correct display form
/// (lowercase words, or already-mixed-case dotted segments) and passes
/// through unchanged.
fn canonical_function_display_name(raw: &str) -> &str {
    match raw {
        "db.namefromelementid" => "db.nameFromElementId",
        "duration.indays" => "duration.inDays",
        "duration.inmonths" => "duration.inMonths",
        "duration.inseconds" => "duration.inSeconds",
        "point.withinbbox" => "point.withinBBox",
        "startnode" => "startNode",
        "endnode" => "endNode",
        "tostring" => "toString",
        "isnan" => "isNaN",
        "toupper" => "toUpper",
        "graph.byname" => "graph.byName",
        "graph.byelementid" => "graph.byElementId",
        "graph.propertiesbyname" => "graph.propertiesByName",
        _ => raw,
    }
}

/// Build the function catalog as virtual rows, one per entry in the
/// known-function registry (`validate::KNOWN_FUNCTIONS`) that also backs
/// function-call validation — so every callable built-in is discoverable
/// and the catalog can't drift out of sync with what actually works. The
/// catalog-only `graph.*` names (`CATALOG_ONLY_FUNCTIONS`, CIP-230) ride
/// beside them through the same renderer.
pub fn function_catalog_rows() -> Vec<Row> {
    KNOWN_FUNCTIONS
        .iter()
        .chain(CATALOG_ONLY_FUNCTIONS.iter())
        .flat_map(|&raw| {
            let display = canonical_function_display_name(raw);
            let metas = function_metas(raw);
            if metas.is_empty() {
                vec![function_row(raw, display, None)]
            } else {
                metas
                    .iter()
                    .map(|meta| function_row(raw, display, Some(meta)))
                    .collect()
            }
        })
        .collect()
}

/// One function's catalog row: the one builder that renders both a
/// registry-backed entry (`meta: Some`) and the generic placeholder for a
/// callable name with no metadata entry (`meta: None`). The category reads
/// [`function_category`], so this renderer never re-derives it from the
/// name; the placeholder's empty description, `ANY` signature and
/// not-deprecated null replacement are CIP-84's own defaults for a name
/// with no richer row.
fn function_row(raw: &str, display: &str, meta: Option<&FunctionMeta>) -> Row {
    let mut r = Row::new();
    r.insert("name".to_string(), CypherValue::Str(display.to_string()));
    r.insert(
        "category".to_string(),
        CypherValue::Str(function_category(raw, meta).to_string()),
    );
    r.insert("isBuiltIn".to_string(), CypherValue::Bool(true));
    r.insert(
        "aggregating".to_string(),
        CypherValue::Bool(function_is_aggregating(raw)),
    );
    match meta {
        Some(meta) => {
            r.insert(
                "description".to_string(),
                CypherValue::Str(meta.description.to_string()),
            );
            let signature = meta
                .special_signature
                .map(str::to_string)
                .unwrap_or_else(|| render_function_signature(display, meta.args, meta.return_type));
            r.insert("signature".to_string(), CypherValue::Str(signature));
            r.insert(
                "argumentDescription".to_string(),
                CypherValue::List(meta.args.iter().map(arg_meta_value).collect()),
            );
            r.insert(
                "returnDescription".to_string(),
                CypherValue::Str(meta.return_type.to_string()),
            );
            r.insert(
                "isDeprecated".to_string(),
                CypherValue::Bool(meta.deprecated_by.is_some()),
            );
            r.insert(
                "deprecatedBy".to_string(),
                meta.deprecated_by
                    .map(|deprecated| CypherValue::Str(deprecated.display().to_string()))
                    .unwrap_or(CypherValue::Null),
            );
        }
        None => {
            r.insert("description".to_string(), CypherValue::Str(String::new()));
            r.insert(
                "signature".to_string(),
                CypherValue::Str(format!("{display}(...) :: ANY")),
            );
            r.insert("argumentDescription".to_string(), CypherValue::List(vec![]));
            r.insert(
                "returnDescription".to_string(),
                CypherValue::Str("ANY".to_string()),
            );
            r.insert("isDeprecated".to_string(), CypherValue::Bool(false));
            r.insert("deprecatedBy".to_string(), CypherValue::Null);
        }
    }
    r
}

/// `SHOW PROCEDURES` catalog rows, rendered from the canonical
/// `procedure_registry` table (the same table the executor's `CALL`
/// dispatch and `Database::validate_calls` bind against), in registry
/// order. Several entries are catalog-only (`executable: false`): CIP-84/
/// CIP-113/CIP-157 scenarios observe them through `SHOW PROCEDURES` but
/// never call them (`db.resampleIndex`, `dbms.setConfigValue`, the `cdc.*`
/// family, the two `dbms.cluster.*` deprecated procedures, …). No scenario
/// asserts `SHOW PROCEDURES` completeness beyond the listed names.
///
/// `preview` is the store-level preview-feature set (ADR-0371, ADR-0378):
/// a signature whose gating feature is disabled is omitted, so discovery
/// never advertises a procedure the `CALL` gate would refuse. The mapping
/// is the executor's one [`procedure_preview_feature`](crate::procedure_preview_feature),
/// and with the gate enabled every row is identical to the ungated table.
pub fn procedure_catalog_rows(preview: PreviewFeatures) -> Vec<Row> {
    BUILTIN_PROCEDURE_SIGNATURES
        .iter()
        .filter(|signature| signature.listed)
        .filter(|signature| {
            crate::procedure_gate::procedure_preview_feature(signature.name)
                .is_none_or(|feature| preview.enabled(feature))
        })
        .map(|signature| {
            let mut r = Row::new();
            r.insert(
                "name".to_string(),
                CypherValue::Str(signature.name.to_string()),
            );
            r.insert(
                "description".into(),
                CypherValue::Str(signature.description.to_string()),
            );
            r.insert("mode".into(), CypherValue::Str(signature.mode.to_string()));
            r.insert(
                "worksOnSystem".into(),
                CypherValue::Bool(signature.works_on_system),
            );
            r.insert("admin".into(), CypherValue::Bool(signature.admin));
            r.insert(
                "signature".into(),
                CypherValue::Str(render_procedure_signature(
                    signature.name,
                    signature.args,
                    signature.returns,
                )),
            );
            r.insert(
                "argumentDescription".into(),
                CypherValue::List(signature.args.iter().map(arg_meta_value).collect()),
            );
            r.insert(
                "returnDescription".into(),
                CypherValue::List(signature.returns.iter().map(arg_meta_value).collect()),
            );
            let deprecated = signature.deprecated_by.is_some();
            r.insert("isDeprecated".into(), CypherValue::Bool(deprecated));
            let mut option = IndexMap::new();
            option.insert("deprecated".into(), CypherValue::Bool(deprecated));
            r.insert("option".into(), CypherValue::Map(option));
            r.insert(
                "deprecatedBy".into(),
                signature
                    .deprecated_by
                    .map(|deprecated| CypherValue::Str(deprecated.display().to_string()))
                    .unwrap_or(CypherValue::Null),
            );
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure_registry::is_ontology_procedure;
    use crate::Graph;

    fn name_of(row: &Row) -> &str {
        row["name"]
            .as_str()
            .expect("every catalog row carries a string name")
    }

    fn is_ontology_row(row: &Row) -> bool {
        is_ontology_procedure(name_of(row))
    }

    #[test]
    fn show_procedures_hides_the_ontology_family_while_the_preview_is_disabled() {
        // ADR-0378: a default executor graph carries the empty preview set,
        // so no ontology-family row is discoverable.
        let disabled = procedure_catalog_rows(Graph::default().preview());
        assert!(
            disabled.iter().all(|row| !is_ontology_row(row)),
            "a disabled store lists no db.ontology.* row"
        );

        let mut enabled = Graph::default();
        enabled.configure_preview(PreviewFeatures::parse("ontology").unwrap());
        let listed = procedure_catalog_rows(enabled.preview());
        // Enabling the gate adds exactly the registry's ontology-family rows
        // that the catalog lists — the count applies the catalog's own
        // `listed` filter as well as the family predicate — and leaves every
        // other row identical and in order.
        let catalog_family: Vec<_> = BUILTIN_PROCEDURE_SIGNATURES
            .iter()
            .filter(|signature| signature.listed)
            .filter(|signature| is_ontology_procedure(signature.name))
            .collect();
        assert_eq!(listed.len(), disabled.len() + catalog_family.len());
        let other_rows: Vec<Row> = listed
            .iter()
            .filter(|row| !is_ontology_row(row))
            .cloned()
            .collect();
        assert_eq!(other_rows, disabled);

        // Each listed row carries the registry's own name, mode,
        // description and rendered signature: the gate changes availability
        // only.
        for signature in catalog_family {
            let row = listed
                .iter()
                .find(|row| name_of(row) == signature.name)
                .unwrap_or_else(|| panic!("{} must be listed while enabled", signature.name));
            assert_eq!(row["mode"], CypherValue::Str(signature.mode.to_string()));
            assert_eq!(
                row["description"],
                CypherValue::Str(signature.description.to_string())
            );
            assert_eq!(
                row["signature"],
                CypherValue::Str(render_procedure_signature(
                    signature.name,
                    signature.args,
                    signature.returns,
                ))
            );
        }
    }
}
