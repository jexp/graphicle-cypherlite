//! CypherLite parser front-end lowered to the CypherLite IR (ADR-0046).

pub mod agg;
pub(crate) mod ast;
mod call_scope;
pub(crate) mod local_callables;
mod lower;
pub mod show_columns;
pub mod validate;

use cypherlite_core::{Error, Result};
use cypherlite_ir::Query;

pub use lower::{lower_expr, lower_statement, lower_type_expr};
pub use validate::{is_known_function, KNOWN_FUNCTIONS, UNADVERTISED_FUNCTIONS};

/// A parsed and lowered top-level statement: the planner input plus the
/// statement's `EXPLAIN`/`PROFILE` preamble flags.
///
/// Only the top-level statement carries the flags (the preamble grammar is
/// top-level only); nested bodies (`CALL { }`, `NEXT` stages, expression
/// subqueries) always report `false`. The flags are deliberately *not* part
/// of [`Query`]: honoring them happens above planning, at the connection
/// layer, so no execution path can mistake an `EXPLAIN` for its wrapped
/// statement.
pub struct LoweredStatement {
    pub query: Query,
    pub explain: bool,
    pub profile: bool,
}

/// Maps a `cypherlite-parser` failure to a front-end error.
///
/// ADR-0116 §2: a parse failure tagged
/// [`UNSUPPORTED_ADMINISTRATION_DETAIL`](cypherlite_parser::parser::UNSUPPORTED_ADMINISTRATION_DETAIL)
/// is a recognized database administration command outside CE's supported
/// set; its message is the verbatim statement text (without a leading
/// `CYPHER n`), echoed in CE's `UnsupportedAdministrationCommand` (51N27)
/// *semantic* error. Every other parse failure stays a syntax error,
/// prepending the TCK error-detail tag when one is present.
fn map_parse_error(e: cypherlite_parser::ParseError) -> Error {
    if e.detail == Some(cypherlite_parser::parser::UNSUPPORTED_ADMINISTRATION_DETAIL) {
        return cypherlite_core::ce_messages::unsupported_administration_command(&e.message);
    }
    if let Some(detail) = e.detail {
        Error::syntax(format!("{detail}: {e}"))
    } else {
        Error::syntax(e.to_string())
    }
}

/// Parse with `cypherlite-parser`, resolve CIP-231 local callables away
/// (the rewrite pass that eliminates `DEFINE` blocks and their call
/// sites, ADR-0382), and lower into a planner-IR `Query`.
pub fn parse_and_lower(src: &str) -> Result<Query> {
    let mut stmt = cypherlite_parser::parse(src).map_err(map_parse_error)?;
    // The ADR-0257 depth guard runs before the CIP-231 resolver walks the
    // tree: a too-deep body gets the typed depth error here, never a
    // stack overflow inside the resolver's own recursion.
    crate::validate::validate_nesting_depth(&stmt)
        .map_err(|error| Error::syntax(error.to_string()))?;
    local_callables::resolve(&mut stmt)?;
    lower_statement(&stmt)
}

/// The resolve-then-lower tail every top-level entry shares: the CIP-231
/// rewrite pass (ADR-0382) eliminates `DEFINE` blocks and their call
/// sites, then lowering runs on the plain pre-existing Cypher that
/// remains, keeping the statement's own `EXPLAIN`/`PROFILE` flags.
fn resolve_and_lower(mut stmt: cypherlite_parser::Statement) -> Result<LoweredStatement> {
    crate::validate::validate_nesting_depth(&stmt)
        .map_err(|error| Error::syntax(error.to_string()))?;
    local_callables::resolve(&mut stmt)?;
    Ok(LoweredStatement {
        explain: stmt.explain,
        profile: stmt.profile,
        query: lower_statement(&stmt)?,
    })
}

/// Parse and lower a top-level statement, keeping its `EXPLAIN`/`PROFILE`
/// preamble flags alongside the lowered query.
pub fn parse_and_lower_statement(src: &str) -> Result<LoweredStatement> {
    // ADR-0257: enforce the query-size limit before parsing. The parser
    // enforces the same cap; this check keeps the error on the front-end
    // path even if a caller reaches lowering another way.
    resolve_and_lower(parse_sized(src)?)
}

/// Parse and lower a top-level statement behind the ADR-0371 preview gate:
/// a statement whose AST uses a gated preview construct fails closed with
/// the authored disabled-feature error unless `preview` enables it, before
/// anything of it lowers or plans. The engine's statement admission calls
/// this with the store's `CYPHERLITE_PREVIEW`-resolved set; the ungated
/// [`parse_and_lower_statement`] stays the dialect-floor entry for callers
/// without a store.
pub fn parse_and_lower_statement_with_preview(
    src: &str,
    preview: cypherlite_core::PreviewFeatures,
) -> Result<LoweredStatement> {
    let stmt = parse_sized(src)?;
    // The one gate loop, derived from the registry itself: every
    // registered preview feature is visited (registry order), its
    // construct-detector resolved by the exhaustive match below -- a new
    // registry row without a detector arm is a compile error, never a
    // silent gate hole. Reserved rows without syntax yet carry no
    // detector.
    for feature in cypherlite_core::PREVIEW_FEATURES {
        let feature = *feature;
        let uses = match feature {
            cypherlite_core::PreviewFeature::HybridSearch => {
                validate::statement_uses_hybrid_search(&stmt)
            }
            cypherlite_core::PreviewFeature::LocalCallables => {
                local_callables::statement_uses_local_callables(&stmt)
            }
            cypherlite_core::PreviewFeature::GqlFor => validate::statement_uses_gql_for(&stmt),
            // ADR-0378: `ontology` gates at procedure dispatch
            // (`procedure_gate.rs`), never at parse, so it has no AST
            // detector here. The graph-type rows are reserved syntax and
            // gain a detector when it lands.
            cypherlite_core::PreviewFeature::Ontology
            | cypherlite_core::PreviewFeature::GraphTypeOptProps
            | cypherlite_core::PreviewFeature::GraphTypeDefaultValues
            | cypherlite_core::PreviewFeature::GraphTypeImpliedLabels => Ok(false),
        };
        if uses.map_err(|error| Error::syntax(error.to_string()))? {
            preview.require(feature)?;
        }
    }
    // The CIP graph-type trio's AST-level gates (CIP-283/262/266): the
    // one walk collects the preview features the statement's own text
    // uses; each fails closed here unless enabled. The constructs that
    // only materialize against stored graph-type state fail closed in
    // the executor's graph-type layer with the same authored error.
    let graph_type_gates = validate::statement_graph_type_gates(&stmt)
        .map_err(|error| Error::syntax(error.to_string()))?;
    for feature in graph_type_gates {
        preview.require(feature)?;
    }
    resolve_and_lower(stmt)
}

/// The shared size-capped parse of [`parse_and_lower_statement`] and
/// [`parse_and_lower_statement_with_preview`]: ADR-0079's query-size limit
/// before parsing (the parser enforces the same cap; this check keeps the
/// error on the front-end path even if a caller reaches lowering another
/// way), then the parse itself.
fn parse_sized(src: &str) -> Result<cypherlite_parser::Statement> {
    if src.len() > cypherlite_parser::parser::MAX_QUERY_BYTES {
        return Err(Error::syntax(format!(
            "query text of {} bytes exceeds the maximum query size of {} bytes",
            src.len(),
            cypherlite_parser::parser::MAX_QUERY_BYTES
        )));
    }
    cypherlite_parser::parse(src).map_err(map_parse_error)
}

/// Parse Cypher for `Database::query`.
///
/// CypherLite is the product front-end: `cypherlite-parser` lowered into
/// planner IR (ADR-0046, ADR-0277).
///
/// Callers that honor the `EXPLAIN`/`PROFILE` prefixes (the connection
/// layer) use [`parse_and_lower_statement`]; this wrapper drops the flags.
pub fn parse_cypher(src: &str) -> Result<Query> {
    Ok(parse_and_lower_statement(src)?.query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_ir::{Clause, Expr};

    #[test]
    fn oversized_query_is_rejected() {
        let oversized = "a".repeat(cypherlite_parser::parser::MAX_QUERY_BYTES + 1);
        let error = match parse_and_lower_statement(&oversized) {
            Ok(_) => panic!("oversized query unexpectedly parsed"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("maximum query size"), "{error}");
        assert!(
            error.contains(&cypherlite_parser::parser::MAX_QUERY_BYTES.to_string()),
            "{error}"
        );
    }

    /// The CIP-278 showcase query, shared by the two preview-gate tests.
    const COMBINE_QUERY: &str = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX v FOR [1.0] LIMIT 10 WEIGHT 0.7, \
            FULLTEXT INDEX f FOR 'text' LIMIT 10 WEIGHT 0.3 \
          ) USING WRRF WITH CONSTANT 60 \
        ) SCORE AS score \
        RETURN node, score";

    #[test]
    fn hybrid_search_gate_fails_closed_without_the_opt_in() {
        // ADR-0371: a COMBINE query behind a preview set that does not
        // include `hybrid-search` fails with the authored error, before
        // anything of it lowers.
        let error = match parse_and_lower_statement_with_preview(
            COMBINE_QUERY,
            cypherlite_core::PreviewFeatures::none(),
        ) {
            Ok(_) => panic!("a gated query unexpectedly lowered without the opt-in"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(
                "preview feature 'hybrid-search' is not enabled: \
                 set CYPHERLITE_PREVIEW=hybrid-search"
            ),
            "{error}"
        );
        // An ungated query lowers unchanged through the same entry.
        parse_and_lower_statement_with_preview(
            "RETURN 1",
            cypherlite_core::PreviewFeatures::none(),
        )
        .unwrap();
    }

    #[test]
    fn hybrid_search_gate_admits_the_opt_in() {
        let enabled = cypherlite_core::PreviewFeatures::parse("hybrid-search").unwrap();
        let lowered = parse_and_lower_statement_with_preview(COMBINE_QUERY, enabled)
            .expect("the gated entry lowers a COMBINE under the opt-in");
        assert!(!lowered.explain && !lowered.profile);
        let cypherlite_ir::Clause::Match {
            search: Some(_), ..
        } = &lowered.query.parts[0].clauses[0]
        else {
            panic!("expected a MATCH with a SEARCH clause");
        };
    }

    /// The CIP-231 showcase query, shared by the two preview-gate tests.
    const LOCAL_CALLABLES_QUERY: &str = "DEFINE FUNCTION isEven(num) = \
        CASE WHEN num % 2 = 0 THEN true ELSE false END \
        UNWIND range(1,4) AS x RETURN x, isEven(x) AS isEven";

    #[test]
    fn local_callables_gate_fails_closed_without_the_opt_in() {
        // ADR-0371: a DEFINE query behind a preview set that does not
        // include `local-callables` fails with the authored error, before
        // anything of it rewrites or lowers.
        let error = match parse_and_lower_statement_with_preview(
            LOCAL_CALLABLES_QUERY,
            cypherlite_core::PreviewFeatures::none(),
        ) {
            Ok(_) => panic!("a gated query unexpectedly lowered without the opt-in"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(
                "preview feature 'local-callables' is not enabled: \
                 set CYPHERLITE_PREVIEW=local-callables"
            ),
            "{error}"
        );
    }

    #[test]
    fn local_callables_gate_admits_the_opt_in() {
        let enabled = cypherlite_core::PreviewFeatures::parse("local-callables").unwrap();
        let lowered = parse_and_lower_statement_with_preview(LOCAL_CALLABLES_QUERY, enabled)
            .expect("the gated entry lowers a local callable under the opt-in");
        assert!(!lowered.explain && !lowered.profile);
        // The rewrite eliminated the definition: the lowered query is
        // plain pre-existing Cypher (a COLLECT subquery under `head`).
        assert!(lowered.query.parts[0]
            .clauses
            .iter()
            .any(|clause| matches!(clause, cypherlite_ir::Clause::Unwind { .. })));
    }

    /// The CIP-250 showcase query, shared by the two preview-gate tests.
    const GQL_FOR_QUERY: &str = "FOR x IN [1, 2, 3] RETURN x";

    #[test]
    fn gql_for_gate_fails_closed_without_the_opt_in() {
        // ADR-0371: a FOR-clause query behind a preview set that does not
        // include `gql-for` fails with the authored error, before anything
        // of it validates or lowers. The equivalent UNWIND spelling stays
        // ungated dialect floor.
        let error = match parse_and_lower_statement_with_preview(
            GQL_FOR_QUERY,
            cypherlite_core::PreviewFeatures::none(),
        ) {
            Ok(_) => panic!("a gated query unexpectedly lowered without the opt-in"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(
                "preview feature 'gql-for' is not enabled: \
                 set CYPHERLITE_PREVIEW=gql-for"
            ),
            "{error}"
        );
    }

    #[test]
    fn gql_for_gate_admits_the_opt_in() {
        let enabled = cypherlite_core::PreviewFeatures::parse("gql-for").unwrap();
        let lowered = parse_and_lower_statement_with_preview(GQL_FOR_QUERY, enabled)
            .expect("the gated entry lowers a FOR clause under the opt-in");
        assert!(!lowered.explain && !lowered.profile);
        // CIP-250: the FOR clause lowers onto the existing UNWIND
        // operator -- no new IR clause shape.
        assert!(matches!(
            &lowered.query.parts[0].clauses[0],
            cypherlite_ir::Clause::Unwind { .. }
        ));
    }

    #[test]
    fn return_one() {
        let q = parse_and_lower("RETURN 1 AS n").unwrap();
        assert!(matches!(q.parts[0].clauses[0], Clause::Return { .. }));
        let Clause::Return { items, .. } = &q.parts[0].clauses[0] else {
            panic!("expected return");
        };
        assert!(matches!(items[0].expr, Expr::Int(1)));
        assert_eq!(items[0].alias.as_deref(), Some("n"));
    }

    #[test]
    fn create_match_roundtrip_shape() {
        let q = parse_and_lower("CREATE (a:Person {name: 'Ada'}) RETURN a.name").unwrap();
        assert!(matches!(q.parts[0].clauses[0], Clause::Create { .. }));
        assert!(matches!(q.parts[0].clauses[1], Clause::Return { .. }));
    }

    #[test]
    fn rejects_parameter_maps_in_patterns() {
        for cypher in [
            "MATCH (n $param) RETURN n",
            "MATCH ()-[r:FOO $param]->() RETURN r",
            "MERGE (n $param)",
        ] {
            let error = parse_and_lower(cypher).unwrap_err().to_string();
            assert!(
                error.contains("InvalidParameterUse"),
                "wrong error for {cypher}: {error}"
            );
        }
    }

    #[test]
    fn yield_star_is_marked_on_call() {
        let q = parse_and_lower("CALL test.my.proc() YIELD *").unwrap();
        let Clause::Call { yield_items, .. } = &q.parts[0].clauses[0] else {
            panic!("expected call");
        };
        assert!(matches!(yield_items, cypherlite_ir::YieldItems::Star));
    }

    #[test]
    fn malformed_var_length_is_invalid_relationship_pattern() {
        let error = parse_and_lower("MATCH (a)-[:LIKES..]->(c) RETURN a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("InvalidRelationshipPattern"), "{error}");
    }

    #[test]
    fn with_order_by_complex_agg_is_ambiguous() {
        let error = parse_and_lower(
            "MATCH (me: Person)--(you: Person)
             WITH me.age + you.age, count(*) AS cnt
             ORDER BY me.age + you.age + count(*)
             RETURN *",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("AmbiguousAggregationExpression"), "{error}");
    }
}
