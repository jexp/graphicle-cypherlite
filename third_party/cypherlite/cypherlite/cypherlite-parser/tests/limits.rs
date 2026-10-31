//! ADR-0257 input-limit tests (LABS-69 / LABS-81): query-size caps,
//! nesting-depth guards across every guarded grammar class, and checked
//! numeric bounds. These exercise only the public parser API; the depth
//! tests run on the workspace-shared big-stack harness because the guards
//! are calibrated for release/production stacks and debug frames are
//! larger, so ~500 recursion levels overflow the default 2 MiB test-thread
//! stack before the guard can fire.

use cypherlite_parser::parser::{MAX_NESTING_DEPTH, MAX_QUERY_BYTES};
use cypherlite_parser::{parse, parse_graph_type_body, Clause, PatternElement};
use cypherlite_test_support::run_with_big_stack;

#[test]
fn test_parse_rejects_input_larger_than_max_query_bytes() {
    // The cap fires *before lexing*: an input that would otherwise
    // amplify into hundreds of MB of tokens (a wall of commas) is
    // rejected as a typed error naming the limit.
    let oversized = ",".repeat(MAX_QUERY_BYTES + 1);
    let err = parse(&oversized).unwrap_err();
    assert!(
        err.message.contains("maximum query size")
            && err.message.contains(&MAX_QUERY_BYTES.to_string()),
        "error should name the limit, got: {}",
        err.message
    );

    // Exactly at the cap, input lexes and parses fine.
    let at_cap = format!("RETURN 1{}", " ".repeat(MAX_QUERY_BYTES - "RETURN 1".len()));
    assert_eq!(at_cap.len(), MAX_QUERY_BYTES);
    assert!(parse(&at_cap).is_ok());
}

#[test]
fn test_parse_graph_type_body_rejects_oversized_input() {
    // parse_graph_type_body tokenizes its input too, so it carries the
    // same pre-lexing cap (the two wrapping braces count against it).
    let oversized = "x".repeat(MAX_QUERY_BYTES);
    let err = parse_graph_type_body(&oversized).unwrap_err();
    assert!(
        err.message.contains("maximum query size"),
        "{}",
        err.message
    );
}

#[test]
fn test_parse_deeply_nested_parens_hit_depth_limit() {
    // LABS-69: ~5000 nested parens used to abort the process
    // (SIGABRT) via unbounded recursion; now a typed error naming the
    // depth limit. The process surviving to assert *is* the test.
    run_with_big_stack(|| {
        let q = format!("RETURN {}1{}", "(".repeat(5000), ")".repeat(5000));
        let err = parse(&q).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_search_combine_nesting_rejected_not_overflowing() {
    run_with_big_stack(|| {
        // ADR-0079: the CIP-278 nested-COMBINE cycle is a recursion
        // family like the label and expression cycles, so it is
        // depth-guarded -- a crafted deep chain is a clean typed error,
        // not a stack overflow. The parse runs before the
        // `hybrid-search` preview check, so ungated deployments are
        // exposed to the same input.
        let q = format!(
            "MATCH (n) SEARCH n IN ({}{}",
            "COMBINE (".repeat(MAX_NESTING_DEPTH as usize + 50),
            "VECTOR INDEX i FOR [1.0] LIMIT 1"
        );
        let err = parse(&q).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_local_definition_nesting_rejected_not_overflowing() {
    run_with_big_stack(|| {
        // ADR-0079: the CIP-231 `DEFINE PROCEDURE f() { ... }` cycle is a
        // new recursion family (a definition body is itself a full
        // statement-block parse, so definitions nest inside definitions),
        // so it is depth-guarded through the same `block_depth` choke
        // point as every other statement-nesting cycle -- a crafted deep
        // chain of nested definition bodies is a clean typed error, not a
        // stack overflow. The parse runs before the `local-callables`
        // preview check, so ungated deployments are exposed to the same
        // input.
        let q = format!(
            "{}RETURN 1 AS one",
            "DEFINE PROCEDURE f() { ".repeat(MAX_NESTING_DEPTH as usize + 50)
        );
        let err = parse(&q).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_not_chain_at_and_beyond_depth_limit() {
    run_with_big_stack(|| {
        // The enclosing RETURN expression itself occupies depth 1, so a
        // NOT chain of MAX_NESTING_DEPTH - 1 sits exactly at the
        // limit and is accepted...
        let at_limit = format!(
            "RETURN {}true",
            "NOT ".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "chain at the limit must parse");

        // ...and a chain beyond the limit is a typed error, not a stack
        // overflow.
        let beyond = format!("RETURN {}true", "NOT ".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth"),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_unary_minus_chain_at_and_beyond_depth_limit() {
    // LABS-69: unary `-` chains recurse inside `parse_unary` without
    // passing through `parse_expression`; ~5000 of them used to abort
    // the process (SIGABRT). The guard in `parse_unary` must turn that
    // into a typed error naming the depth limit. The process surviving
    // to assert *is* the test.
    run_with_big_stack(|| {
        // The enclosing RETURN expression itself occupies depth 1, so a
        // unary chain of MAX_NESTING_DEPTH - 1 sits exactly at the
        // limit and is accepted...
        let at_limit = format!("RETURN {}1", "- ".repeat(MAX_NESTING_DEPTH as usize - 1));
        assert!(parse(&at_limit).is_ok(), "chain at the limit must parse");

        // ...and a chain beyond the limit is a typed error, not a stack
        // overflow.
        let beyond = format!("RETURN {}1", "- ".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_label_bang_chain_at_and_beyond_depth_limit() {
    // LABS-69: `!` chains in a GQL label expression recurse inside
    // `parse_label_unary` without passing through `parse_expression`;
    // ~5000 of them used to abort the process (SIGABRT).
    run_with_big_stack(|| {
        // Label expressions in a MATCH pattern do not sit under an
        // enclosing `parse_expression`, so a chain of
        // MAX_NESTING_DEPTH - 1 bangs is accepted...
        let at_limit = format!(
            "MATCH (n:{}A) RETURN n",
            "!".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "chain at the limit must parse");

        // ...and a chain beyond the limit is a typed error, not a stack
        // overflow.
        let beyond = format!("MATCH (n:{}A) RETURN n", "!".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_label_nested_parens_at_and_beyond_depth_limit() {
    // LABS-69: nested label parens recurse through
    // `parse_label_primary` -> `parse_label_or` -> ... ->
    // `parse_label_primary` without passing through `parse_expression`;
    // ~5000 of them used to abort the process (SIGABRT).
    run_with_big_stack(|| {
        let at_limit = format!(
            "MATCH (n:{}A{}) RETURN n",
            "(".repeat(MAX_NESTING_DEPTH as usize - 1),
            ")".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        let beyond = format!("MATCH (n:{}A{}) RETURN n", "(".repeat(600), ")".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_nested_subpath_parens_at_and_beyond_depth_limit() {
    // LABS-69: nested parenthesized subpath groups recurse inside
    // `parse_pattern_path_body` without passing through
    // `parse_expression`; ~5000 of them used to abort the process
    // (SIGABRT).
    run_with_big_stack(|| {
        let at_limit = format!(
            "MATCH {}a{} RETURN a",
            "(".repeat(MAX_NESTING_DEPTH as usize - 1),
            ")".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        let beyond = format!("MATCH {}a{} RETURN a", "(".repeat(600), ")".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_nested_foreach_at_and_beyond_depth_limit() {
    // LABS-69: nested FOREACH bodies recurse `parse_clause` ->
    // `parse_clause_inner` -> `parse_foreach` -> `parse_clause`,
    // bypassing `parse_statement`, so `statement_depth` never engaged
    // and ~100000 nested FOREACHs aborted the process (SIGABRT). The
    // `clause_depth` guard in `parse_clause` must turn that into a
    // typed error naming the depth limit. The process surviving to
    // assert *is* the test.
    //
    // Big-stack harness: the guard is calibrated for release/
    // production stacks; debug frames are larger, and 4+ frames per
    // FOREACH level at depth ~500 exceeds the default 2 MiB test
    // stack before the guard can fire.
    run_with_big_stack(|| {
        // Each nested FOREACH is one `parse_clause` level and the
        // innermost CREATE body one more, so MAX_NESTING_DEPTH - 1
        // FOREACHs sit exactly at the limit and are accepted...
        let at_limit = format!(
            "{}CREATE (:A){}",
            "FOREACH (x IN [1] | ".repeat(MAX_NESTING_DEPTH as usize - 1),
            ")".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        // ...and nesting beyond the limit is a typed error, not a
        // stack overflow.
        let beyond = format!(
            "{}CREATE (:A){}",
            "FOREACH (x IN [1] | ".repeat(600),
            ")".repeat(600)
        );
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_nested_braced_single_queries_at_and_beyond_depth_limit() {
    // LABS-69: `{ { { ... } } }` single queries recurse
    // `parse_single_query` -> `parse_braced_single_query` ->
    // `parse_statement_block` without passing through `parse_statement`
    // (or `parse_clause`/`parse_expression`), so no depth counter
    // engaged and ~3000 nested braces on a ~6 KB input aborted the
    // process (SIGABRT) on an 8 MiB release stack. The `block_depth`
    // guard in `parse_statement_block` must turn that into a typed
    // error naming the depth limit. The process surviving to assert
    // *is* the test.
    run_with_big_stack(|| {
        // The top-level statement block occupies depth 1 and each
        // brace one more, so MAX_NESTING_DEPTH - 1 braces sit exactly
        // at the limit and are accepted...
        let at_limit = format!(
            "{}RETURN 1{}",
            "{ ".repeat(MAX_NESTING_DEPTH as usize - 1),
            " }".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        // ...and nesting beyond the limit is a typed error, not a
        // stack overflow.
        let beyond = format!("{}RETURN 1{}", "{ ".repeat(600), " }".repeat(600));
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_nested_when_bodies_at_and_beyond_depth_limit() {
    // LABS-69: `WHEN ... THEN { WHEN ... }` nests through the same
    // unguarded `parse_single_query` -> `parse_braced_single_query` ->
    // `parse_statement_block` cycle as plain braces and used to parse
    // past MAX_NESTING_DEPTH unguarded. The `block_depth` guard must
    // catch it too.
    run_with_big_stack(|| {
        // The top-level statement block occupies depth 1 and each
        // WHEN body's braces one more, so MAX_NESTING_DEPTH - 1
        // nested WHENs sit exactly at the limit and are accepted...
        let at_limit = format!(
            "{}RETURN 1{}",
            "WHEN true THEN { ".repeat(MAX_NESTING_DEPTH as usize - 1),
            " }".repeat(MAX_NESTING_DEPTH as usize - 1)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        // ...and nesting beyond the limit is a typed error, not a
        // stack overflow.
        let beyond = format!(
            "{}RETURN 1{}",
            "WHEN true THEN { ".repeat(600),
            " }".repeat(600)
        );
        let err = parse(&beyond).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_all_statement_nesting_classes_hit_depth_limit() {
    // Every statement-nesting class funnels through
    // `parse_statement_block`, so each must error with the depth-limit
    // message at depth 600 -- CALL/EXISTS/COUNT/COLLECT subquery
    // bodies (previously caught by `statement_depth`/`clause_depth`,
    // now caught same-or-earlier by `block_depth`) and the braced /
    // WHEN-THEN cycles (previously unguarded).
    run_with_big_stack(|| {
        let cases: [(&str, String); 6] = [
            (
                "CALL",
                format!("{}RETURN 1{}", "CALL () { ".repeat(600), " }".repeat(600)),
            ),
            (
                "EXISTS",
                format!(
                    "RETURN {}1{}",
                    "EXISTS { RETURN ".repeat(600),
                    " }".repeat(600)
                ),
            ),
            (
                "COUNT",
                format!(
                    "RETURN {}1{}",
                    "COUNT { RETURN ".repeat(600),
                    " }".repeat(600)
                ),
            ),
            (
                "COLLECT",
                format!(
                    "RETURN {}1{}",
                    "COLLECT { RETURN ".repeat(600),
                    " }".repeat(600)
                ),
            ),
            (
                "braced single query",
                format!("{}RETURN 1{}", "{ ".repeat(600), " }".repeat(600)),
            ),
            (
                "WHEN/THEN body",
                format!(
                    "{}RETURN 1{}",
                    "WHEN true THEN { ".repeat(600),
                    " }".repeat(600)
                ),
            ),
        ];
        for (name, q) in cases {
            let err = parse(&q).unwrap_err();
            assert!(
                err.message.contains("nesting depth")
                    && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
                "{name}: error should name the depth limit, got: {}",
                err.message
            );
        }
    });
}

#[test]
fn test_parse_top_level_brace_query_depth_and_label_scoping_unchanged() {
    // The `block_depth` guard must not change behavior for shallow
    // legit queries: a top-level `{ ... }` body costs exactly one
    // extra block-depth level and parses fine...
    assert!(parse("{ RETURN 1 }").is_ok());
    assert!(parse("{ { { RETURN 1 } } }").is_ok());

    // ...and it must NOT inflate `statement_depth` (the reason a
    // dedicated counter is used): `statement_depth <= 1` is the
    // `is_outer_clause` signal scoping label-expression mixing to one
    // top-level statement, so a mix inside the single top-level clause
    // of a brace body must error exactly as its unbraced twin. If
    // braces bumped `statement_depth`, the inner clause would no
    // longer be "outer", the mixing flags would never be checked, and
    // the braced form would silently parse.
    let mixed = "MATCH (n:A:B) WHERE EXISTS { MATCH (m:C|D) } RETURN n";
    for q in [mixed, &format!("{{ {mixed} }}")] {
        let err = parse(q).unwrap_err();
        assert!(
            err.message.contains("InvalidLabelExpression"),
            "label-mixing scoping must be unchanged by top-level braces, got: {}",
            err.message
        );
    }
}

#[test]
fn test_parse_nested_value_types_at_and_beyond_depth_limit() {
    // LABS-69: nested `LIST<...>`/`ANY<...>` type brackets recurse
    // `parse_value_type` -> `parse_type_term` -> `parse_type_leaf` ->
    // `parse_value_type`, bypassing `parse_expression`; ~300000 nested
    // `LIST<` aborted the process (SIGABRT). The `expr_depth` guard in
    // `parse_value_type` must turn that into a typed error naming the
    // depth limit. The process surviving to assert *is* the test.
    //
    // Big-stack harness: the guard is calibrated for release/
    // production stacks; debug frames are larger, and 4 frames per
    // type level at depth ~500 exceeds the default 2 MiB test stack
    // before the guard can fire.
    run_with_big_stack(|| {
        // The enclosing RETURN expression occupies depth 1 and the
        // outermost `parse_value_type` call depth 2, so
        // MAX_NESTING_DEPTH - 2 nested LIST< brackets sit exactly at
        // the limit and are accepted...
        let at_limit = format!(
            "RETURN 1::{}INTEGER{} AS x",
            "LIST<".repeat(MAX_NESTING_DEPTH as usize - 2),
            ">".repeat(MAX_NESTING_DEPTH as usize - 2)
        );
        assert!(parse(&at_limit).is_ok(), "nesting at the limit must parse");

        // ...and nesting beyond the limit is a typed error, not a
        // stack overflow -- for both the `::TYPE` cast spelling and
        // the `IS ::` type-predicate spelling.
        for beyond in [
            format!(
                "RETURN 1::{}INTEGER{} AS x",
                "LIST<".repeat(600),
                ">".repeat(600)
            ),
            format!(
                "RETURN 1 IS :: {}INTEGER{}",
                "LIST<".repeat(600),
                ">".repeat(600)
            ),
        ] {
            let err = parse(&beyond).unwrap_err();
            assert!(
                err.message.contains("nesting depth")
                    && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
                "error should name the depth limit, got: {}",
                err.message
            );
        }
    });
}

#[test]
fn test_parse_minus_chain_beyond_depth_limit_default_stack() {
    // Thin-grammar companion to the big-stack tests: unary `-`
    // recursion costs ~1 stack frame per level, so the `expr_depth`
    // guard provably fires before stack exhaustion even on the
    // default 2 MiB test-worker stack (the fat-frame grammars need
    // the big-stack harness because the guard is calibrated for
    // release/production stacks and debug frames are larger). Runs
    // with no explicit stack size on purpose.
    let beyond = format!("RETURN {}1", "- ".repeat(600));
    let err = parse(&beyond).unwrap_err();
    assert!(
        err.message.contains("nesting depth")
            && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
        "error should name the depth limit, got: {}",
        err.message
    );
}

#[test]
fn test_parse_oversized_hop_and_path_bounds_are_errors() {
    // LABS-81: oversized bounds used to be silently clamped
    // (`[*99999999999..2]` parsed as min_hops = 1, `[*..99999999999]`
    // as `[*1..1]`, `ANY 4294967296 PATHS` as selector 0). Each must
    // now be a parse error naming the bound.
    for (q, what) in [
        ("MATCH (a)-[*4294967296]->(b) RETURN a", "hop lower bound"),
        (
            "MATCH (a)-[*99999999999..2]->(b) RETURN a",
            "hop lower bound",
        ),
        (
            "MATCH (a)-[*..99999999999]->(b) RETURN a",
            "hop upper bound",
        ),
        (
            "MATCH (a)-[*1..99999999999]->(b) RETURN a",
            "hop upper bound",
        ),
        (
            "MATCH p = ANY 4294967296 PATHS (a)-->(b) RETURN p",
            "number of paths",
        ),
        (
            "MATCH p = ((a)-->(b)){4294967296,2} RETURN p",
            "quantifier lower bound",
        ),
        (
            "MATCH p = ((a)-->(b)){1,99999999999} RETURN p",
            "quantifier upper bound",
        ),
    ] {
        let err = parse(q).unwrap_err();
        assert!(
            err.message.contains(what),
            "{q}: error should name the {what}, got: {}",
            err.message
        );
        assert_eq!(
            err.detail,
            Some("IntegerOverflow"),
            "{q}: detail tag, got: {}",
            err.message
        );
    }

    // In-range bounds still parse, with exact values.
    let stmt = parse("MATCH (a)-[*2..5]->(b) RETURN a").unwrap();
    let Clause::Match(m) = &stmt.clauses[0] else {
        panic!("expected match");
    };
    let PatternElement::Relationship(rp) = &m.pattern[0].elements[1] else {
        panic!("expected relationship");
    };
    assert_eq!(rp.min_hops, Some(2));
    assert_eq!(rp.max_hops, Some(5));
}

#[test]
fn test_parse_default_value_expression_hits_depth_limit() {
    // ADR-0079: the CIP-262 `DEFAULT ON COMMIT <value expression>` clause
    // reuses the ordinary expression parser, so it joins the existing
    // depth-guarded expression recursion family -- no new family of its
    // own. A crafted deeply nested default value is a clean typed error
    // naming the limit, never a stack overflow; the process surviving to
    // assert is the test.
    run_with_big_stack(|| {
        let q = format!(
            "ALTER CURRENT GRAPH TYPE SET {{ (:L => {{p :: STRING DEFAULT ON COMMIT {}'x'{}}}) }}",
            "(".repeat(5000),
            ")".repeat(5000)
        );
        let err = parse(&q).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "error should name the depth limit, got: {}",
            err.message
        );
    });
}

#[test]
fn test_parse_for_clause_introduces_no_new_recursion_family() {
    run_with_big_stack(|| {
        // ADR-0079/CIP-250: a FOR clause is a flat sibling clause in an
        // existing statement body (the parser normalizes it onto the
        // UNWIND slot), so it adds no recursion family of its own — the
        // depth guard it rides is the subquery-statement family: a chain
        // of FOR clauses at one level parses unbounded by clause count,
        // while a FOR inside an over-deep CALL chain hits the same typed
        // depth error the UNWIND spelling does.
        let _ = parse("FOR x IN [1] RETURN x").unwrap();
        let deep_for = format!(
            "FOR x IN [1] {}CALL {{ FOR y IN [2] RETURN y }} RETURN y{}",
            "CALL { ".repeat(MAX_NESTING_DEPTH as usize + 50),
            " }".repeat(MAX_NESTING_DEPTH as usize + 50)
        );
        let err = parse(&deep_for).unwrap_err();
        assert!(
            err.message.contains("nesting depth")
                && err.message.contains(&MAX_NESTING_DEPTH.to_string()),
            "FOR spelling should ride the subquery depth guard, got: {}",
            err.message
        );
        // The same chain under the UNWIND spelling errors identically --
        // the guard is the shared one, not a FOR-specific rule.
        let deep_unwind = deep_for
            .replace("FOR y IN [2]", "UNWIND [2] AS y")
            .replace("FOR x IN [1]", "UNWIND [1] AS x");
        let err = parse(&deep_unwind).unwrap_err();
        assert!(err.message.contains("nesting depth"), "{}", err.message);
    });
}
