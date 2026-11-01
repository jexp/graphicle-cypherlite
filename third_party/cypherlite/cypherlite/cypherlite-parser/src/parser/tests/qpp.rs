use super::*;

// ---- CIP-28 quantified path patterns ----

#[test]
fn test_qpp_quantifier_star_is_zero_or_more() {
    let q = qpp_quantifier("MATCH (x)((a)-->(b))*(y) RETURN 1");
    assert_eq!(q, PathQuantifier { min: 0, max: None });
}

#[test]
fn test_qpp_quantifier_plus_is_one_or_more() {
    let q = qpp_quantifier("MATCH (x)((a)-->(b))+(y) RETURN 1");
    assert_eq!(q, PathQuantifier { min: 1, max: None });
}

#[test]
fn test_qpp_quantifier_normalizes_empty_braces_to_zero_unbounded() {
    // Rule 1: `{,}` normalizes to `{0,}`.
    let q = qpp_quantifier("MATCH (x)((a)-->(b)){,}(y) RETURN 1");
    assert_eq!(q, PathQuantifier { min: 0, max: None });
}

#[test]
fn test_qpp_quantifier_fixed_braces_equivalent_to_matching_bounds() {
    // Rule 2: `{m,m}` is a fixed quantifier, same as `{m}`.
    let fixed = qpp_quantifier("MATCH (x)((a)-->(b)){2}(y) RETURN 1");
    let general = qpp_quantifier("MATCH (x)((a)-->(b)){2,2}(y) RETURN 1");
    assert_eq!(fixed, general);
    assert_eq!(
        fixed,
        PathQuantifier {
            min: 2,
            max: Some(2)
        }
    );
}

#[test]
fn test_qpp_quantifier_zero_upper_bound_rejected() {
    assert!(parse("MATCH (x)-[r]->{0}(y) RETURN 1").is_err());
    assert!(parse("MATCH (x)-[r]->{,0}(y) RETURN 1").is_err());
}

#[test]
fn test_qpp_quantifier_lower_exceeds_upper_rejected() {
    assert!(parse("MATCH (x)-[r]->{3,2}(y) RETURN 1").is_err());
}

#[test]
fn test_qpp_top_level_star_alone_is_minimum_node_count_zero() {
    // Rule 6: a lone star-quantified pattern has no guaranteed node.
    assert!(parse("MATCH ((a)-[]->(b))* RETURN 1").is_err());
    // But anchored on either side, it's fine (Form 2/3/4).
    assert!(parse("MATCH (x)((a)-[]->(b))* RETURN 1").is_ok());
    assert!(parse("MATCH ((a)-[]->(b))*(y) RETURN 1").is_ok());
}

#[test]
fn test_qpp_single_node_operand_is_minimum_path_length_zero() {
    // Rule 3: the operand must have a minimum path length > 0.
    assert!(parse("MATCH ((a)){1,5} RETURN 1").is_err());
}

#[test]
fn test_qpp_nested_quantification_rejected() {
    // Rule 4: no nesting, whether direct or through a quantified
    // relationship inside the operand.
    assert!(parse("MATCH (((a)-[b]->(c))*)+ RETURN 1").is_err());
    assert!(parse("MATCH ((a)-->(b)-[r]->*(c))+ RETURN 1").is_err());
}

#[test]
fn test_qpp_unquantified_parenthesized_pattern_cannot_be_juxtaposed() {
    assert!(parse("MATCH (a)-->(b)((c)-->(d)) RETURN 1").is_err());
}

#[test]
fn test_qpp_quantified_relationship_form5_lowers_to_var_length_rel_pattern() {
    // `-[r]->*` normalizes to an ordinary variable-length RelPattern,
    // not a `Quantified` element, and its bare-star default (0 or more)
    // differs from the legacy bracket form's bare-star default (1 or
    // more).
    let stmt = parse("MATCH (x)-[r:R]->*(y) RETURN 1").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => match &mc.pattern[0].elements[1] {
            PatternElement::Relationship(rp) => {
                assert!(rp.quantified);
                assert_eq!(rp.min_hops, Some(0));
                assert_eq!(rp.max_hops, None);
            }
            other => panic!("expected a Relationship element, got {other:?}"),
        },
        other => panic!("expected match, got {other:?}"),
    }
}

#[test]
fn test_qpp_subpath_variable_inside_quantification_parses() {
    let stmt = parse("MATCH (x)(p2 = (a)-->(b))*(y) RETURN p2").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => match &mc.pattern[0].elements[1] {
            PatternElement::Quantified(qpp) => {
                assert_eq!(qpp.subpath_variable.as_deref(), Some("p2"));
            }
            other => panic!("expected a Quantified element, got {other:?}"),
        },
        other => panic!("expected match, got {other:?}"),
    }
}
