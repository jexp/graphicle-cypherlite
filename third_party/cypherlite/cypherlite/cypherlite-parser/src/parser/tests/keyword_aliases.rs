// ADR-0191: the alias and reference groups of the keyword-symbolic-names
// probe -- an unquoted keyword after `AS` in RETURN/WITH items, UNWIND,
// LOAD CSV and CALL/SHOW YIELD, and an unquoted keyword as a variable
// reference (a bare RETURN/WITH item, an arithmetic/comparison operand, a
// function and aggregate argument, a WHERE operand, an ORDER BY key, a
// property/subscript base, a list element). The existing meanings of the
// reference spellings (the TRUE/FALSE/NULL/INF/INFINITY/NAN literals,
// prefix NOT, the RETURN/WITH DISTINCT/ALL modifiers, the in-`fn(` set
// quantifiers, the CASE/COUNT/EXISTS/COLLECT forms and bare REDUCE) stay
// ahead of the variable fallthrough.

use super::*;

/// ADR-0191's `exception_rows` for the alias and reference groups (all 36
/// are reference rows): the 20 rows whose existing meaning wins and so stay
/// syntax errors (`NOT`, `DISTINCT` and `ALL` spellings) plus the 16
/// over-acceptance rows (literal and set-quantifier spellings CypherLite
/// accepts where Neo4j rejects). Each exception row keeps the `cypherlite`
/// outcome recorded in `probe.tsv`; every other row of the two groups with
/// `neo4j=accepted` parses.
const EXCEPTION_IDS: [&str; 36] = [
    "ref-aggregate-arg/NOT",
    "ref-arithmetic/ALL",
    "ref-arithmetic/DISTINCT",
    "ref-arithmetic/FALSE",
    "ref-arithmetic/TRUE",
    "ref-comparison/ALL",
    "ref-comparison/DISTINCT",
    "ref-comparison/NOT",
    "ref-function-arg/ALL",
    "ref-function-arg/DISTINCT",
    "ref-function-arg/FALSE",
    "ref-function-arg/NOT",
    "ref-function-arg/TRUE",
    "ref-list-element/NOT",
    "ref-order-by/NOT",
    "ref-property-base/ALL",
    "ref-property-base/DISTINCT",
    "ref-property-base/FALSE",
    "ref-property-base/INF",
    "ref-property-base/INFINITY",
    "ref-property-base/NAN",
    "ref-property-base/NOT",
    "ref-property-base/TRUE",
    "ref-return-bare/ALL",
    "ref-return-bare/DISTINCT",
    "ref-return-bare/NOT",
    "ref-subscript-base/FALSE",
    "ref-subscript-base/INF",
    "ref-subscript-base/INFINITY",
    "ref-subscript-base/NAN",
    "ref-subscript-base/TRUE",
    "ref-where/NOT",
    "ref-with-after-comma/NOT",
    "ref-with-first/ALL",
    "ref-with-first/DISTINCT",
    "ref-with-first/NOT",
];

/// The first item of a statement's (single) RETURN clause.
fn return_item(s: &Statement) -> &ReturnItem {
    match s.clauses.last() {
        Some(Clause::Return(r)) if r.items.len() == 1 => &r.items[0],
        other => panic!("expected a one-item RETURN, got {other:?}"),
    }
}

/// The single RETURN clause.
fn return_clause(s: &Statement) -> &ReturnClause {
    match s.clauses.last() {
        Some(Clause::Return(r)) => r,
        other => panic!("expected RETURN, got {other:?}"),
    }
}

/// The first WITH clause.
fn with_clause(s: &Statement) -> &WithClause {
    match &s.clauses[0] {
        Clause::With(w) => w,
        other => panic!("expected WITH, got {other:?}"),
    }
}

/// Extracts the alias a query binds at one alias site.
type AliasOf = fn(&Statement) -> &str;

fn return_alias(s: &Statement) -> &str {
    return_item(s).alias.as_deref().expect("RETURN item alias")
}

fn unwind_alias(s: &Statement) -> &str {
    match &s.clauses[0] {
        Clause::Unwind(u) => &u.alias,
        other => panic!("expected UNWIND, got {other:?}"),
    }
}

fn load_csv_alias(s: &Statement) -> &str {
    match &s.clauses[0] {
        Clause::LoadCSV(l) => &l.alias,
        other => panic!("expected LOAD CSV, got {other:?}"),
    }
}

/// The first `YIELD <col> AS <alias>` alias of a CALL ... YIELD or SHOW ...
/// YIELD clause (the two share `parse_show_yield_list`'s reader).
fn yield_alias(s: &Statement) -> &str {
    let yields = match &s.clauses[0] {
        Clause::CallProcedure(c) => &c.yields,
        Clause::ShowFunctions(f) => &f.yields,
        Clause::ShowIndexes(i) => &i.yields,
        other => panic!("expected CALL/SHOW YIELD, got {other:?}"),
    };
    yields
        .items
        .first()
        .and_then(|item| item.alias.as_deref())
        .expect("YIELD alias")
}

/// The shared `probe_group_rows` check for the alias group: the group is
/// non-empty and every row id is unique.
#[test]
fn probe_fixture_has_alias_rows() {
    super::probe_group_rows("alias");
}

/// ADR-0191's acceptance test for the two groups this module owns: every
/// alias and reference row with `neo4j=accepted` parses, and every
/// `exception_rows` id keeps the `cypherlite` outcome recorded in
/// `probe.tsv`.
#[test]
fn probe_alias_and_reference_rows_parse_as_decided() {
    let mut rows = super::probe_group_rows("alias");
    rows.extend(super::probe_group_rows("reference"));
    assert_eq!(rows.len(), 1207, "alias + reference rows");

    let mut accepted_checked = 0;
    let mut exception_checked = 0;
    for row in &rows {
        let parsed = parse(row.query);
        if EXCEPTION_IDS.contains(&row.id) {
            exception_checked += 1;
            assert_eq!(
                parsed.is_ok(),
                row.cypherlite == "accepted",
                "{}: the recorded cypherlite outcome {:?} must stay ({:?})",
                row.id,
                row.cypherlite,
                parsed.err()
            );
        } else if row.neo4j == "accepted" {
            accepted_checked += 1;
            assert!(
                parsed.is_ok(),
                "{}: {:?} must parse: {:?}",
                row.id,
                row.query,
                parsed.err()
            );
        }
    }
    assert_eq!(accepted_checked, 1155, "neo4j-accepted non-exception rows");
    assert_eq!(exception_checked, 36, "exception rows");
}

/// ADR-0191 Decision point 2: the keyword after `AS` is the alias at all
/// five alias sites, kept as written -- the recommendations walkthrough's
/// `AS as` and `AS union` included.
#[test]
fn keyword_alias_sites_keep_the_spelling() {
    let cases: [(&str, AliasOf, &str); 8] = [
        ("WITH 1 AS a RETURN a AS as", return_alias, "as"),
        ("WITH [1] AS s RETURN s AS union", return_alias, "union"),
        ("RETURN 1 AS MATCH", return_alias, "MATCH"),
        ("UNWIND [1] AS MATCH RETURN 1", unwind_alias, "MATCH"),
        (
            "LOAD CSV FROM 'file:///x.csv' AS MATCH RETURN 1",
            load_csv_alias,
            "MATCH",
        ),
        (
            "CALL db.labels() YIELD label AS MATCH RETURN 1",
            yield_alias,
            "MATCH",
        ),
        (
            "SHOW FUNCTIONS YIELD name AS MATCH RETURN 1",
            yield_alias,
            "MATCH",
        ),
        (
            "SHOW INDEXES YIELD name AS MATCH RETURN 1",
            yield_alias,
            "MATCH",
        ),
    ];
    for (query, extract, expected) in cases {
        assert_eq!(extract(&parse_ok(query)), expected, "{query}");
    }
}

/// A predicate over a parsed literal value.
type IsLiteral = fn(&LiteralValue) -> bool;

/// ADR-0191 Decision point 6: the literal spellings keep their literal
/// meaning ahead of the variable fallthrough (`res-null-ref`,
/// `res-true-ref`, `res-false-ref`, `res-inf-ref`, `res-nan-ref`).
#[test]
fn keyword_literals_keep_their_literal_meaning() {
    let cases: [(&str, IsLiteral, &str); 6] = [
        ("RETURN null", |v| *v == LiteralValue::Null, "null"),
        ("RETURN true", |v| *v == LiteralValue::Bool(true), "true"),
        ("RETURN false", |v| *v == LiteralValue::Bool(false), "false"),
        (
            "RETURN inf",
            |v| matches!(v, LiteralValue::Float(f) if f.is_infinite()),
            "inf",
        ),
        (
            "RETURN infinity",
            |v| matches!(v, LiteralValue::Float(f) if f.is_infinite()),
            "infinity",
        ),
        (
            "RETURN nan",
            |v| matches!(v, LiteralValue::Float(f) if f.is_nan()),
            "nan",
        ),
    ];
    for (query, is_expected, what) in cases {
        match return_expr(&parse_ok(query)) {
            Expr::Literal(v) => assert!(
                is_expected(v),
                "{query}: expected the {what} literal, got {v:?}"
            ),
            other => panic!("{query}: expected a literal, got {other:?}"),
        }
    }
}

/// ADR-0191 Decision point 6: prefix NOT keeps negation, the RETURN/WITH
/// DISTINCT/ALL modifiers and the in-`fn(` set quantifiers keep their
/// meaning, and the resolutions rows that record a syntax error stay
/// syntax errors (`res-return-distinct`, `res-with-distinct`,
/// `res-count-distinct`, `res-return-not`, `res-all-ref`).
#[test]
fn keyword_reference_existing_meanings_keep_priority() {
    // `RETURN NOT true` is negation, not a variable named NOT.
    match return_expr(&parse_ok("RETURN NOT true")) {
        Expr::Unary(u) => {
            assert_eq!(u.op, "NOT");
            assert_eq!(u.expr, Expr::Literal(LiteralValue::Bool(true)));
        }
        other => panic!("expected NOT negation, got {other:?}"),
    }

    // `RETURN DISTINCT 1` / `WITH DISTINCT 1` keep the modifier; `RETURN
    // ALL 1` keeps ALL as the explicit (default) modifier.
    assert!(return_clause(&parse_ok("RETURN DISTINCT 1")).distinct);
    assert!(with_clause(&parse_ok("WITH DISTINCT 1 RETURN 1")).distinct);
    let all_stmt = parse_ok("RETURN ALL 1");
    let all = return_clause(&all_stmt);
    assert!(!all.distinct);
    assert_eq!(all.items[0].expr, Expr::Literal(LiteralValue::Integer(1)));

    // `count(DISTINCT x)` keeps `distinct = true`; `count(ALL x)` keeps the
    // explicit-default quantifier.
    match return_expr(&parse_ok("RETURN count(DISTINCT x)")) {
        Expr::FunctionCall(f) => {
            assert_eq!(f.name, "count");
            assert!(f.distinct);
            assert_eq!(f.args, vec![Expr::Variable("x".to_string())]);
        }
        other => panic!("expected count(), got {other:?}"),
    }
    match return_expr(&parse_ok("RETURN count(ALL x)")) {
        Expr::FunctionCall(f) => {
            assert_eq!(f.name, "count");
            assert!(!f.distinct);
            assert_eq!(f.args, vec![Expr::Variable("x".to_string())]);
        }
        other => panic!("expected count(), got {other:?}"),
    }

    // `RETURN 1 + 2` is untouched.
    match return_expr(&parse_ok("RETURN 1 + 2")) {
        Expr::Binary(b) => {
            assert_eq!(b.op, "+");
            assert_eq!(b.left, Expr::Literal(LiteralValue::Integer(1)));
            assert_eq!(b.right, Expr::Literal(LiteralValue::Integer(2)));
        }
        other => panic!("expected 1 + 2, got {other:?}"),
    }

    // The resolutions rows that keep a syntax error.
    for query in [
        "RETURN distinct",
        "WITH distinct RETURN 1",
        "RETURN count(distinct)",
        "RETURN not",
        "RETURN NOT + 1",
        "WITH 1 AS `all` RETURN all AS r",
        "RETURN ALL",
        "WITH ALL RETURN 1",
    ] {
        assert!(parse(query).is_err(), "{query:?} must stay a syntax error");
    }

    // The two over-acceptance spellings stay accepted (`exception_rows`):
    // `DISTINCT`/`ALL` inside `fn(` keep the set-quantifier meaning even
    // with no argument.
    for query in ["RETURN abs(DISTINCT)", "RETURN abs(ALL)"] {
        assert!(parse(query).is_ok(), "{query:?} stays accepted");
    }
}

/// ADR-0191 Decision point 6: CASE/COUNT/EXISTS/COLLECT/REDUCE keep their
/// forms when their grammar applies, and fall back to a variable otherwise
/// (`res-case-ref`, `res-count-ref`, `res-exists-ref`, `res-collect-ref`);
/// the EXISTS fallback keeps the token's spelling.
#[test]
fn keyword_forms_and_variable_fallbacks() {
    // The forms.
    assert!(matches!(
        return_expr(&parse_ok("RETURN CASE WHEN true THEN 1 END AS r")),
        Expr::Case(_)
    ));
    for (query, name) in [
        ("RETURN count(x) AS r", "count"),
        ("RETURN collect(x) AS r", "collect"),
        ("RETURN exists(n.p) AS r", "exists"),
    ] {
        match return_expr(&parse_ok(query)) {
            Expr::FunctionCall(f) => assert_eq!(f.name, name, "{query}"),
            other => panic!("{query}: expected a function call, got {other:?}"),
        }
    }
    assert!(matches!(
        return_expr(&parse_ok("RETURN reduce(s = 0, x IN [1] | s + x) AS r")),
        Expr::Reduce(_)
    ));

    // The bare spellings fall back to variables, spelling kept as written.
    for (query, name) in [
        ("RETURN case AS r", "case"),
        ("RETURN count AS r", "count"),
        ("RETURN collect AS r", "collect"),
        ("RETURN exists AS r", "exists"),
        ("RETURN EXISTS AS r", "EXISTS"),
        ("RETURN REDUCE AS r", "REDUCE"),
        ("RETURN reduce AS r", "reduce"),
        ("RETURN VECTOR AS r", "VECTOR"),
    ] {
        assert_eq!(
            return_expr(&parse_ok(query)),
            &Expr::Variable(name.to_string()),
            "{query}"
        );
    }
}

// ---- ADR-0191 follow-up sweep: the remaining same-class name positions ----
//
// A second probe against the same Neo4j `2026.09.0 Community` container (all
// 71 `lookup_keyword` spellings through each shape with `EXPLAIN`) found
// every clause keyword accepted at the four positions the original 31-shape
// probe did not reach: the SEARCH binding variable reference, `SCORE AS`,
// `REPORT STATUS AS` (with `ON ERROR CONTINUE`, which Neo4j requires) and
// the SET/REMOVE target variable. They read `is_schema_name()` like the
// alias, reference, binding and key positions above (ADR-0191 Decision
// point 7).

/// The 22 `CLAUSE_KEYWORDS` spellings (`token.rs`), lowercase.
const CLAUSE_KEYWORD_SPELLINGS: [&str; 22] = [
    "match", "optional", "create", "insert", "merge", "delete", "detach", "set", "remove", "with",
    "unwind", "where", "order", "by", "limit", "skip", "as", "distinct", "foreach", "union",
    "require", "drop",
];

/// The leading MATCH clause's CIP-224 `<search clause>`.
fn search_clause(s: &Statement) -> &SearchClause {
    match_clause(s).search.as_ref().expect("SEARCH clause")
}

/// The statement's SET clause.
fn set_clause_of(s: &Statement) -> &SetClause {
    s.clauses
        .iter()
        .find_map(|c| match c {
            Clause::Set(sc) => Some(sc),
            _ => None,
        })
        .expect("no SET clause")
}

/// The statement's REMOVE clause.
fn remove_clause_of(s: &Statement) -> &RemoveClause {
    s.clauses
        .iter()
        .find_map(|c| match c {
            Clause::Remove(rc) => Some(rc),
            _ => None,
        })
        .expect("no REMOVE clause")
}

/// The variable a SET/REMOVE item's `.prop` target navigates from.
fn property_target_variable(item_property: &Option<PropertyAccess>, query: &str) -> String {
    match item_property {
        Some(pa) => match &pa.expr {
            Expr::Variable(v) => v.clone(),
            other => panic!("{query}: expected a variable base, got {other:?}"),
        },
        None => panic!("{query}: expected a property target"),
    }
}

/// ADR-0191 Decision point 7: the SEARCH binding variable reference reads
/// `is_schema_name()` -- Neo4j 2026.09 reads every keyword spelling there
/// as a variable name (the probe bound it backticked in the MATCH and
/// referenced it unquoted in SEARCH).
#[test]
fn keyword_search_binding_variable() {
    for kw in CLAUSE_KEYWORD_SPELLINGS {
        let query = format!(
            "MATCH (`{kw}`:Doc) SEARCH {kw} IN (VECTOR INDEX docs FOR [1.0] LIMIT 2) RETURN 1"
        );
        assert_eq!(search_clause(&parse_ok(&query)).binding_var, kw, "{query}");
    }

    // NULL reads as a variable too: Neo4j parses `SEARCH NULL IN (...)`
    // and rejects it semantically ("Variable `NULL` not defined" --
    // exactly what CIP-224 binding-variable scenario [4] expects). The
    // unbound-reference rejection is cypherlite-front's
    // `InvalidSearchVariableReference` check, not this read.
    let stmt =
        parse_ok("MATCH (n:Doc) SEARCH null IN (VECTOR INDEX docs FOR [1.0] LIMIT 2) RETURN 1");
    assert_eq!(search_clause(&stmt).binding_var, "null");

    // A non-name token is still rejected, as is a property access
    // (CIP-224 Example B2).
    assert!(
        parse("MATCH (n:Doc) SEARCH 5 IN (VECTOR INDEX docs FOR [1.0] LIMIT 2) RETURN 1").is_err()
    );
    assert!(parse(
        "MATCH (n:Doc) SEARCH n.title IN (VECTOR INDEX docs FOR [1.0] LIMIT 2) RETURN 1"
    )
    .is_err());
}

/// ADR-0191 Decision point 7: the `SCORE AS` variable reads
/// `is_schema_name()` -- Neo4j 2026.09 accepts every keyword spelling
/// there.
#[test]
fn keyword_score_as_variable() {
    for kw in CLAUSE_KEYWORD_SPELLINGS {
        let query = format!(
            "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX docs FOR [1.0] LIMIT 2) SCORE AS {kw} RETURN 1"
        );
        assert_eq!(
            search_clause(&parse_ok(&query)).score_var.as_deref(),
            Some(kw),
            "{query}"
        );
    }
}

/// ADR-0191 Decision point 7: the `REPORT STATUS AS` variable of a
/// `CALL { ... } IN TRANSACTIONS` subquery reads `is_schema_name()` --
/// Neo4j 2026.09 accepts every keyword spelling there.
#[test]
fn keyword_report_status_as_variable() {
    for kw in CLAUSE_KEYWORD_SPELLINGS {
        let query = format!(
            "CALL {{ CREATE (x) }} IN TRANSACTIONS ON ERROR CONTINUE REPORT STATUS AS {kw} RETURN 1"
        );
        let stmt = parse_ok(&query);
        let in_tx = call_subquery_of(&stmt)
            .in_transactions
            .as_ref()
            .expect("IN TRANSACTIONS");
        assert_eq!(in_tx.report_status.as_deref(), Some(kw), "{query}");
    }
}

/// ADR-0191 Decision point 7: the SET target variable reads
/// `is_schema_name()` -- Neo4j 2026.09 accepts every keyword spelling in
/// `SET <kw>.prop = 1`, `SET <kw>:Label`, `SET <kw> = ...` and
/// `SET <kw> += {...}`.
#[test]
fn keyword_set_target_variable() {
    for kw in CLAUSE_KEYWORD_SPELLINGS {
        let query = format!("MATCH (`{kw}`:Doc) SET {kw}.prop = 1 RETURN 1");
        let stmt = parse_ok(&query);
        let item = &set_clause_of(&stmt).items[0];
        assert_eq!(
            property_target_variable(&item.property, &query),
            kw,
            "{query}"
        );
    }

    // The label, whole-variable and `+=` forms too.
    let stmt = parse_ok("MATCH (`match`:Doc) SET match:Label RETURN 1");
    let item = &set_clause_of(&stmt).items[0];
    assert_eq!(item.variable.as_deref(), Some("match"));
    assert_eq!(item.labels.len(), 1);
    let stmt = parse_ok("MATCH (`match`:Doc) SET match = 1 RETURN 1");
    assert_eq!(
        set_clause_of(&stmt).items[0].variable.as_deref(),
        Some("match")
    );
    let stmt = parse_ok("MATCH (`match`:Doc) SET match += {p: 1} RETURN 1");
    assert_eq!(
        set_clause_of(&stmt).items[0].variable.as_deref(),
        Some("match")
    );

    // A non-name target is still rejected.
    assert!(parse("MATCH (n:Doc) SET 5.prop = 1 RETURN 1").is_err());
}

/// ADR-0191 Decision point 7: the REMOVE target variable reads
/// `is_schema_name()` -- Neo4j 2026.09 accepts every keyword spelling in
/// `REMOVE <kw>.prop` and `REMOVE <kw>:Label`.
#[test]
fn keyword_remove_target_variable() {
    for kw in CLAUSE_KEYWORD_SPELLINGS {
        let query = format!("MATCH (`{kw}`:Doc) REMOVE {kw}.prop RETURN 1");
        let stmt = parse_ok(&query);
        let item = &remove_clause_of(&stmt).items[0];
        assert_eq!(
            property_target_variable(&item.property, &query),
            kw,
            "{query}"
        );
    }

    // The label form too.
    let stmt = parse_ok("MATCH (`match`:Doc) REMOVE match:Label RETURN 1");
    assert_eq!(
        remove_clause_of(&stmt).items[0].variable.as_deref(),
        Some("match")
    );

    // A non-name target is still rejected.
    assert!(parse("MATCH (n:Doc) REMOVE 5.prop RETURN 1").is_err());
}
