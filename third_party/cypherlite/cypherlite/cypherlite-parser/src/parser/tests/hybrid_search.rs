use super::*;

// ---- CIP-278 hybrid search: the COMBINE source specification ----

/// `parse_ok` bound to a local, so the borrowed SEARCH clause outlives the
/// statement (the temporary of `search_clause(&parse_ok(q))` does not).
fn parsed(query: &str) -> Statement {
    parse_ok(query)
}

/// The SEARCH clause of `s`'s leading MATCH clause.
fn search_clause(s: &Statement) -> &SearchClause {
    match_clause(s).search.as_ref().expect("SEARCH clause")
}

/// The one index source of a single-source spec (test convenience only).
fn single_index(spec: &SearchSpec) -> &SearchIndexSpec {
    match spec {
        SearchSpec::Index(index) => index,
        SearchSpec::Combine(_) => panic!("expected an index spec, got {spec:?}"),
    }
}

#[test]
fn parses_the_canonical_combine_query() {
    // The CIP-278 "Proposal" showcase query: one weighted vector source,
    // one weighted fulltext source, WITH CONSTANT, and the combined
    // specification's own LIMIT.
    let query = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX moviePlotEmbeddings \
              FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.7, \
            FULLTEXT INDEX moviePlotFulltext \
              FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
          ) USING WRRF WITH CONSTANT 60 LIMIT 5 \
        ) SCORE AS score \
        RETURN node, score";
    let stmt = parsed(query);
    let search = search_clause(&stmt);
    assert_eq!(search.binding_var, "node");
    assert_eq!(search.score_var.as_deref(), Some("score"));
    let SearchSpec::Combine(combine) = &search.spec else {
        panic!("expected a combined spec, got {:?}", search.spec);
    };
    assert_eq!(combine.sources.len(), 2);
    let Expr::Literal(LiteralValue::Float(weight)) = combine.sources[0]
        .weight_expr
        .as_ref()
        .expect("weight literal")
    else {
        panic!("expected a float weight literal");
    };
    assert_eq!(*weight, 0.7);
    assert!(combine.sources[1].weight_expr.is_some());
    assert!(combine.constant_expr.is_some());
    assert!(combine.limit_expr.is_some());
    let SearchSpec::Index(index) = &combine.sources[0].spec else {
        panic!("expected an index source");
    };
    assert_eq!(index.kind, SearchKind::Vector);
    assert_eq!(index.index_name, "moviePlotEmbeddings");
}

#[test]
fn defaults_weight_one_and_constant_sixty() {
    // Rules 2/4: an omitted WEIGHT defaults to 1 and an omitted WITH
    // CONSTANT defaults to 60 -- the AST carries both as `None`.
    let query = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX v FOR [1.0] LIMIT 10, \
            FULLTEXT INDEX f FOR 'text' LIMIT 10 \
          ) USING WRRF \
        ) \
        RETURN node";
    let stmt = parsed(query);
    let search = search_clause(&stmt);
    let SearchSpec::Combine(combine) = &search.spec else {
        panic!("expected a combined spec");
    };
    assert!(combine.sources.iter().all(|s| s.weight_expr.is_none()));
    assert!(combine.constant_expr.is_none());
    assert!(combine.limit_expr.is_none());
}

#[test]
fn parses_a_nested_combine_source() {
    // Example P3: a COMBINE source inside another COMBINE, its own WEIGHT
    // following its USING WRRF.
    let query = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX a FOR [1.0] LIMIT 10 WEIGHT 1.0, \
            COMBINE ( \
              VECTOR INDEX b FOR [1.0] LIMIT 10 WEIGHT 1.3, \
              FULLTEXT INDEX c FOR 'text' LIMIT 10 WEIGHT 0.7 \
            ) USING WRRF WEIGHT 0.7 \
          ) USING WRRF \
        ) \
        RETURN node";
    let stmt = parsed(query);
    let search = search_clause(&stmt);
    let SearchSpec::Combine(outer) = &search.spec else {
        panic!("expected a combined spec");
    };
    assert!(matches!(outer.sources[0].spec, SearchSpec::Index(_)));
    let SearchSpec::Combine(inner) = &outer.sources[1].spec else {
        panic!("expected a nested combined spec");
    };
    assert_eq!(inner.sources.len(), 2);
    assert!(outer.sources[1].weight_expr.is_some());
}

#[test]
fn a_single_source_fails_the_rule_5_parse_check() {
    // Rule 5 / Example R5.1: the combined specification body must hold
    // two or more weighted source clauses -- the cardinality is a parse
    // error at the list itself.
    let query = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX v FOR [1.0] LIMIT 10 WEIGHT 0.3 \
          ) USING WRRF WITH CONSTANT 60 \
        ) \
        RETURN node";
    let error = parse(query).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("The COMBINE clause requires two or more sources provided"),
        "{error}"
    );
}

#[test]
fn combine_sources_keep_their_own_control_clauses() {
    // The CIP-225 `<index for and control clause>` keeps its own inner
    // WHERE / SKIP / WITH ANALYZER slots inside a COMBINE source, exactly
    // as at the top level.
    let query = "MATCH (node) \
        SEARCH node IN ( \
          COMBINE ( \
            VECTOR INDEX v FOR [1.0] WHERE node.year < 2000 LIMIT 10, \
            FULLTEXT INDEX f FOR 'text' WITH ANALYZER 'english' SKIP 1 LIMIT 10 \
          ) USING WRRF \
        ) \
        RETURN node";
    let stmt = parsed(query);
    let search = search_clause(&stmt);
    let SearchSpec::Combine(combine) = &search.spec else {
        panic!("expected a combined spec");
    };
    let SearchSpec::Index(vector) = &combine.sources[0].spec else {
        panic!("expected a vector source");
    };
    assert!(vector.inner_where.is_some());
    let SearchSpec::Index(fulltext) = &combine.sources[1].spec else {
        panic!("expected a fulltext source");
    };
    assert!(fulltext.analyzer.is_some());
    assert!(fulltext.offset_expr.is_some());
}

#[test]
fn a_plain_index_search_still_parses_unchanged() {
    // The CIP-224/CIP-225 top-level shape is unchanged by the source
    // specification split.
    let query = "MATCH (p) SEARCH p IN (VECTOR INDEX v FOR [1.0] LIMIT 5) \
        SCORE AS s RETURN p, s";
    let stmt = parsed(query);
    let search = search_clause(&stmt);
    let index = single_index(&search.spec);
    assert_eq!(index.kind, SearchKind::Vector);
    assert_eq!(index.index_name, "v");
    assert_eq!(search.score_var.as_deref(), Some("s"));
}
