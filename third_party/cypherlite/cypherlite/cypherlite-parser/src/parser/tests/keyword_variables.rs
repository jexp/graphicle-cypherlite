// ADR-0191: the binding group of the keyword-symbolic-names probe -- an
// unquoted keyword as a node, relationship or path variable, a FOREACH,
// comprehension, quantifier or reduce variable, or a `CALL (<var>)` import.
// Beyond the shared fixture check, the module covers Decision point 4 (every
// Neo4j-accepted binding row parses, and the keyword spelling round-trips as
// the bound name at every site) plus the lookaheads the widened readers
// need: the inline-element WHERE and variable-less `IS` label-spec
// disambiguations, the list- vs. pattern-comprehension readings of
// `[true IN ..]` and `[k = (pattern)]`, and the CIP-51 rejection of a path
// variable after a mode prefix.

use super::*;

/// The first node pattern of the leading MATCH clause.
fn first_node(s: &Statement) -> &NodePattern {
    match &match_clause(s).pattern[0].elements[0] {
        PatternElement::Node(n) => n,
        other => panic!("expected a node pattern, got {other:?}"),
    }
}

/// The relationship pattern of the leading MATCH clause.
fn second_rel(s: &Statement) -> &RelPattern {
    match &match_clause(s).pattern[0].elements[1] {
        PatternElement::Relationship(r) => r,
        other => panic!("expected a relationship pattern, got {other:?}"),
    }
}

// ---- extraction of the name each binding site binds, one per shape ----

fn node_variable(s: &Statement) -> String {
    first_node(s)
        .variable
        .clone()
        .expect("node pattern variable")
}

fn rel_variable(s: &Statement) -> String {
    second_rel(s)
        .variable
        .clone()
        .expect("relationship pattern variable")
}

fn path_variable(s: &Statement) -> String {
    match_clause(s).pattern[0]
        .variable
        .clone()
        .expect("named path variable")
}

fn subpath_variable(s: &Statement) -> String {
    match &match_clause(s).pattern[0].elements[0] {
        PatternElement::Quantified(q) => q
            .subpath_variable
            .clone()
            .expect("quantified subpath variable"),
        other => panic!("expected a quantified path pattern, got {other:?}"),
    }
}

fn foreach_variable(s: &Statement) -> String {
    match &s.clauses[0] {
        Clause::Foreach(f) => f.variable.clone(),
        other => panic!("expected FOREACH, got {other:?}"),
    }
}

fn list_comprehension_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::ListComprehension(lc) => lc.variable.clone(),
        other => panic!("expected a list comprehension, got {other:?}"),
    }
}

fn pattern_comprehension_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::PatternComprehension(pc) => match &pc.pattern.elements[0] {
            PatternElement::Node(n) => n.variable.clone().expect("node variable"),
            other => panic!("expected a node pattern, got {other:?}"),
        },
        other => panic!("expected a pattern comprehension, got {other:?}"),
    }
}

fn pattern_comprehension_path_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::PatternComprehension(pc) => pc
            .path_variable
            .clone()
            .expect("pattern comprehension path variable"),
        other => panic!("expected a pattern comprehension, got {other:?}"),
    }
}

fn quantifier_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::Quantifier(q) => q.variable.clone(),
        other => panic!("expected a quantifier, got {other:?}"),
    }
}

fn reduce_accumulator(s: &Statement) -> String {
    match return_expr(s) {
        Expr::Reduce(r) => r.accumulator.clone(),
        other => panic!("expected a reduce(), got {other:?}"),
    }
}

fn reduce_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::Reduce(r) => r.variable.clone(),
        other => panic!("expected a reduce(), got {other:?}"),
    }
}

fn allreduce_accumulator(s: &Statement) -> String {
    match return_expr(s) {
        Expr::AllReduce(r) => r.accumulator.clone(),
        other => panic!("expected an allReduce(), got {other:?}"),
    }
}

fn allreduce_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::AllReduce(r) => r.variable.clone(),
        other => panic!("expected an allReduce(), got {other:?}"),
    }
}

fn map_comprehension_value_variable(s: &Statement) -> String {
    match return_expr(s) {
        Expr::MapComprehension(mc) => mc.value_var.clone(),
        other => panic!("expected a map comprehension, got {other:?}"),
    }
}

fn call_import_variable(s: &Statement) -> String {
    match &call_subquery_of(s).scope {
        CallScope::Vars(vars) => vars[0].clone(),
        other => panic!("expected CALL (var) scope, got {other:?}"),
    }
}

/// The shared `probe_group_rows` check for the binding group: the group is
/// non-empty and every row id is unique.
#[test]
fn probe_fixture_has_binding_rows() {
    super::probe_group_rows("binding");
}

/// ADR-0191 Decision point 4: every binding row the probe recorded as
/// accepted on Neo4j parses. No binding row is an `exception_rows` id, so
/// that is all 639 rows.
#[test]
fn test_probe_binding_rows_parse() {
    super::probe_accepted_rows_parse("binding");
}

/// ADR-0191 Decision point 4: the reader at every binding site is
/// `is_schema_name()`, so the keyword spelling round-trips as the bound
/// name at each site. The spellings come from the probe fixture (the
/// `lookup_keyword` set), and the sites are the twelve Decision point 4
/// names plus the reduce/allReduce loop variable and the map-comprehension
/// key side.
#[test]
fn test_binding_variables_are_symbolic_names_at_every_site() {
    type Extract = fn(&Statement) -> String;
    let shapes: [(&str, Extract); 15] = [
        ("MATCH ({K}) RETURN `{K}`", node_variable),
        ("MATCH ()-[{K}]->() RETURN `{K}`", rel_variable),
        ("MATCH {K} = ()-->() RETURN `{K}`", path_variable),
        ("MATCH (({K} = (a)-->(b))+) RETURN `{K}`", subpath_variable),
        (
            "FOREACH ({K} IN [1] | CREATE (n {v: `{K}`}))",
            foreach_variable,
        ),
        ("RETURN [{K} IN [1] | `{K}`]", list_comprehension_variable),
        (
            "RETURN [({K})-->() | `{K}`]",
            pattern_comprehension_variable,
        ),
        (
            "RETURN [{K} = (n)-->() | n]",
            pattern_comprehension_path_variable,
        ),
        (
            "RETURN all({K} IN [1] WHERE `{K}` = 1)",
            quantifier_variable,
        ),
        (
            "RETURN reduce({K} = 0, x IN [1] | `{K}` + x)",
            reduce_accumulator,
        ),
        (
            "RETURN reduce(acc = 0, {K} IN [1] | acc + `{K}`)",
            reduce_variable,
        ),
        (
            "RETURN allReduce({K} = 0, x IN [1] | `{K}` + x, `{K}` < 10)",
            allreduce_accumulator,
        ),
        (
            "RETURN allReduce(acc = 0, {K} IN [1] | acc + `{K}`, acc < 10)",
            allreduce_variable,
        ),
        (
            "RETURN { {K} IN [1] | `{K}` : `{K}` }",
            map_comprehension_value_variable,
        ),
        (
            "WITH 1 AS `{K}` CALL ({K}) { RETURN `{K}` AS y } RETURN y",
            call_import_variable,
        ),
    ];

    let mut spellings: Vec<&str> = super::probe_group_rows("binding")
        .iter()
        .map(|row| row.keyword)
        .collect();
    spellings.sort_unstable();
    spellings.dedup();
    assert_eq!(spellings.len(), 71, "one spelling per keyword");

    for k in spellings {
        for (template, extract) in shapes {
            let q = template.replace("{K}", k);
            assert_eq!(extract(&parse_ok(&q)), k, "{q}");
        }
    }
}

/// ADR-0191 Decision point 4, inline element WHERE: a leading `WHERE` is
/// the variable only when the next token cannot start the inline predicate
/// (`)`, `]`, `:`, `IS`, `$`, `-`, `<-`, `WHERE`); otherwise the
/// inline-predicate meaning wins. The first pair is the acceptance
/// criterion's named existing meaning: `MATCH (n WHERE n.x = 1) RETURN n`
/// keeps variable `n` with an inline WHERE.
#[test]
fn test_inline_element_where_keeps_its_meaning() {
    // An ordinary variable with an inline predicate is untouched.
    let stmt = parse_ok("MATCH (n WHERE n.x = 1) RETURN n");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("n"));
    assert!(node.where_expr.is_some(), "{node:?}");

    // `(WHERE 1 = 1)`: the next token starts the predicate, so the
    // anonymous element with the inline predicate wins -- WHERE is not a
    // variable.
    let stmt = parse_ok("MATCH (WHERE 1 = 1) RETURN 1 AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable, None);
    assert!(
        matches!(node.where_expr, Some(Expr::Comparison(_))),
        "{node:?}"
    );

    // `()-[WHERE true]->()`: same for relationship patterns.
    let stmt = parse_ok("MATCH ()-[WHERE true]->() RETURN 1 AS r");
    let rel = second_rel(&stmt);
    assert_eq!(rel.variable, None);
    assert!(
        matches!(
            rel.where_expr,
            Some(Expr::Literal(LiteralValue::Bool(true)))
        ),
        "{rel:?}"
    );

    // `(where WHERE true)`: the next token is `WHERE` itself, which cannot
    // start the predicate, so the first WHERE is the variable.
    let stmt = parse_ok("MATCH (where WHERE true) RETURN `where` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("where"));
    assert!(
        matches!(
            node.where_expr,
            Some(Expr::Literal(LiteralValue::Bool(true)))
        ),
        "{node:?}"
    );

    // `()-[where WHERE true]->()`: same for relationship patterns.
    let stmt = parse_ok("MATCH ()-[where WHERE true]->() RETURN `where` AS r");
    let rel = second_rel(&stmt);
    assert_eq!(rel.variable.as_deref(), Some("where"));
    assert!(
        matches!(
            rel.where_expr,
            Some(Expr::Literal(LiteralValue::Bool(true)))
        ),
        "{rel:?}"
    );
}

/// The `WHERE` lookahead's next-token set, one row per listed token: the
/// variable reading must win for `)`, `]`, `:`, `IS`, `$`, `-`, `<-` and
/// `WHERE`, and the inline-predicate reading for anything that can start an
/// expression.
#[test]
fn test_inline_where_lookahead_tokens() {
    // The element extractor is part of the case table (mirroring
    // `test_binding_variables_are_symbolic_names_at_every_site`), so no
    // case is dispatched by sniffing the query text.
    type Element = fn(&Statement) -> (Option<String>, bool);
    fn node_element(s: &Statement) -> (Option<String>, bool) {
        let node = first_node(s);
        (node.variable.clone(), node.where_expr.is_some())
    }
    fn rel_element(s: &Statement) -> (Option<String>, bool) {
        let rel = second_rel(s);
        (rel.variable.clone(), rel.where_expr.is_some())
    }

    // (query, element extractor, expected variable of the element the
    // keyword sits in, whether the element carries an inline WHERE)
    let cases: [(&str, Element, Option<&str>, bool); 10] = [
        (
            "MATCH (WHERE) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH ()-[WHERE]->() RETURN `WHERE`",
            rel_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (WHERE:Label) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (WHERE IS Label) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (WHERE $props) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (WHERE)-->(b) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (WHERE)<--(b) RETURN `WHERE`",
            node_element,
            Some("WHERE"),
            false,
        ),
        (
            "MATCH (where WHERE true) RETURN `where`",
            node_element,
            Some("where"),
            true,
        ),
        (
            "MATCH (WHERE 1 = 1) RETURN 1 AS r",
            node_element,
            None,
            true,
        ),
        (
            "MATCH ()-[WHERE true]->() RETURN 1 AS r",
            rel_element,
            None,
            true,
        ),
    ];
    for (q, extract, variable, has_where) in cases {
        let (got_variable, got_where) = extract(&parse_ok(q));
        assert_eq!(got_variable.as_deref(), variable, "{q}");
        assert_eq!(got_where, has_where, "{q}");
    }
}

/// ADR-0191 Decision point 4, variable-less GPM label spec: a leading `IS`
/// is the variable only when the next token cannot start a label spec;
/// `(IS)`/`[IS]` read as variables while `(IS A&B)`/`-[IS R]->` keep `IS`
/// as the label introducer.
#[test]
fn test_variable_less_gpm_label_spec_keeps_is_introducer() {
    // `)` cannot start a label spec: IS is the node variable.
    let stmt = parse_ok("MATCH (IS) RETURN `IS` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("IS"));
    assert!(node.label_expr.is_none(), "{node:?}");

    // `]` cannot start a label spec: IS is the relationship variable.
    let stmt = parse_ok("MATCH ()-[IS]->() RETURN `IS` AS r");
    let rel = second_rel(&stmt);
    assert_eq!(rel.variable.as_deref(), Some("IS"));
    assert!(rel.type_expr.is_none(), "{rel:?}");

    // `:` cannot start a label spec (the colon itself introduces one): IS is
    // the variable.
    let stmt = parse_ok("MATCH (IS:Label) RETURN `IS` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("IS"));
    assert_eq!(node.label_expr, Some(LabelExpr::Label("Label".to_string())));

    // `$props` is not a dynamic atom: IS is the variable, `$props` is the
    // whole-property-map shorthand.
    let stmt = parse_ok("MATCH (IS $props) RETURN `IS` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("IS"));
    assert!(node.properties_param.is_some(), "{node:?}");

    // A label name starts a label spec: IS introduces it, the element is
    // anonymous.
    let stmt = parse_ok("MATCH (IS A&B) RETURN 1 AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable, None);
    assert_eq!(
        node.label_expr,
        Some(LabelExpr::And(vec![
            LabelExpr::Label("A".to_string()),
            LabelExpr::Label("B".to_string()),
        ]))
    );

    // Same for a relationship type name.
    let stmt = parse_ok("MATCH ()-[IS R]->() RETURN 1 AS r");
    let rel = second_rel(&stmt);
    assert_eq!(rel.variable, None);
    assert_eq!(rel.type_expr, Some(LabelExpr::Label("R".to_string())));

    // `[`/`(`/`%`/`!` start a label-expression group/term too.
    for q in [
        "MATCH (IS [A&B]) RETURN 1 AS r",
        "MATCH (IS (A&B)) RETURN 1 AS r",
        "MATCH (IS % ) RETURN 1 AS r",
        "MATCH (IS !A) RETURN 1 AS r",
    ] {
        let stmt = parse_ok(q);
        let node = first_node(&stmt);
        assert_eq!(node.variable, None, "{q}");
        assert!(node.label_expr.is_some(), "{q}: {node:?}");
    }

    // A dynamic label atom `$(...)` starts a label spec, so IS introduces
    // it rather than binding.
    let stmt = parse_ok("MATCH (IS $('A')) RETURN 1 AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable, None, "{node:?}");
    assert!(
        matches!(node.label_expr, Some(LabelExpr::Dynamic(_))),
        "{node:?}"
    );
}

/// The `IS` arm of `peek_pattern_name_is_variable` and the expression-level
/// label reader share one offset-aware starter predicate
/// (`peek_can_start_label_expr_at`): the lookahead accepts exactly what the
/// label reader can read, so `(IS)` reads `IS` as the variable while
/// `(IS $(foo))` -- the `$`-parameter + `(` dynamic-atom opener -- keeps
/// `IS` as the label introducer.
#[test]
fn test_is_label_starter_lookahead_is_shared_and_offset_aware() {
    // `)` cannot start a label expression: IS is the variable.
    let stmt = parse_ok("MATCH (IS) RETURN `IS` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("IS"));
    assert!(node.label_expr.is_none(), "{node:?}");

    // The `$`-parameter + `(` dynamic-atom opener is a label starter, so IS
    // introduces the label spec rather than binding: `$(foo)` (Default),
    // `$all(...)` and `$any(...)`.
    for q in [
        "MATCH (IS $(foo)) RETURN 1 AS r",
        "MATCH (IS $all(['A'])) RETURN 1 AS r",
        "MATCH (IS $any(['A'])) RETURN 1 AS r",
    ] {
        let stmt = parse_ok(q);
        let node = first_node(&stmt);
        assert_eq!(node.variable, None, "{q}");
        assert!(
            matches!(node.label_expr, Some(LabelExpr::Dynamic(_))),
            "{q}: {node:?}"
        );
    }

    // A `$`-parameter that is not a dynamic-atom opener is not a label
    // starter: `(IS $props)` reads IS as the variable and `$props` as the
    // whole-property-map shorthand.
    let stmt = parse_ok("MATCH (IS $props) RETURN `IS` AS r");
    let node = first_node(&stmt);
    assert_eq!(node.variable.as_deref(), Some("IS"));
    assert!(node.properties_param.is_some(), "{node:?}");

    // The expression-level reader uses the same predicate: a dynamic label
    // predicate and the `[` grouping both start a label expression.
    let stmt = parse_ok("MATCH (n) RETURN n IS $(foo) AS r");
    match return_expr(&stmt) {
        Expr::HasLabel(hl) => assert!(matches!(hl.label_expr, LabelExpr::Dynamic(_)), "{hl:?}"),
        other => panic!("expected a label predicate, got {other:?}"),
    }
    let stmt = parse_ok("MATCH (n) WHERE n IS [A] RETURN n");
    match match_clause(&stmt).where_expr.as_ref() {
        Some(Expr::HasLabel(hl)) => {
            assert_eq!(hl.label_expr, LabelExpr::Label("A".to_string()), "{hl:?}")
        }
        other => panic!("expected a label predicate, got {other:?}"),
    }
}

/// ADR-0191 Consequences: `[true IN [1]]` (no `|`) becomes a list
/// comprehension returning `[1]`, because the comprehension variable check
/// now accepts the keyword spelling `true` and the next token is `IN`. A
/// boolean list literal stays a list literal, and the acceptance
/// criterion's named existing meaning
/// `RETURN [x IN [1] WHERE x > 0 | x]` stays a list comprehension.
#[test]
fn test_true_in_list_is_a_list_comprehension() {
    let stmt = parse_ok("RETURN [true IN [1]] AS r");
    match return_expr(&stmt) {
        Expr::ListComprehension(lc) => {
            assert_eq!(lc.variable, "true");
            assert!(matches!(lc.list_expr, Expr::List(_)), "{lc:?}");
            assert!(lc.where_expr.is_none(), "{lc:?}");
            assert!(lc.map_expr.is_none(), "{lc:?}");
        }
        other => panic!("expected a list comprehension, got {other:?}"),
    }

    // With an explicit map expression the comprehension shape is unchanged.
    let stmt = parse_ok("RETURN [true IN [1] | true] AS r");
    match return_expr(&stmt) {
        Expr::ListComprehension(lc) => {
            assert_eq!(lc.variable, "true");
            assert!(
                matches!(lc.map_expr, Some(Expr::Literal(LiteralValue::Bool(true)))),
                "{lc:?}"
            );
        }
        other => panic!("expected a list comprehension, got {other:?}"),
    }

    // Boolean list literals keep their literal meaning.
    for q in ["RETURN [true, false] AS r", "RETURN [true] AS r"] {
        assert!(matches!(return_expr(&parse_ok(q)), Expr::List(_)), "{q}");
    }

    // The named existing meaning: an identifier comprehension with a WHERE
    // and a map expression is untouched.
    let stmt = parse_ok("RETURN [x IN [1] WHERE x > 0 | x] AS r");
    match return_expr(&stmt) {
        Expr::ListComprehension(lc) => {
            assert_eq!(lc.variable, "x");
            assert!(lc.where_expr.is_some(), "{lc:?}");
            assert!(
                matches!(lc.map_expr, Some(Expr::Variable(ref v)) if v == "x"),
                "{lc:?}"
            );
        }
        other => panic!("expected a list comprehension, got {other:?}"),
    }
}

/// The pattern-comprehension path variable read in `parse_list_literal`
/// only wins when a pattern path follows the `=`. A literal keyword keeps
/// its literal meaning in a list element (ADR-0191 point 6), matching
/// Neo4j (`RETURN [true = false]` is `[false]`, while
/// `RETURN [MATCH = (n)-->() | n]` is a pattern comprehension).
#[test]
fn test_pattern_comprehension_path_variable_requires_a_pattern() {
    // `false` cannot start a pattern: the list-literal reading wins and
    // `true` stays the literal.
    let stmt = parse_ok("RETURN [true = false] AS r");
    match return_expr(&stmt) {
        Expr::List(items) => {
            assert_eq!(items.len(), 1, "{items:?}");
            assert!(
                matches!(
                    &items[0],
                    Expr::Comparison(c)
                        if matches!(c.left, Expr::Literal(LiteralValue::Bool(true)))
                            && matches!(c.right, Expr::Literal(LiteralValue::Bool(false)))
                ),
                "{items:?}"
            );
        }
        other => panic!("expected a list literal, got {other:?}"),
    }

    // Same for `null` and for an ordinary variable (`[x = 1]` is a list
    // literal whose element compares `x`; Neo4j rejects the undefined `x`
    // only at semantic analysis).
    for q in ["RETURN [null = null] AS r", "RETURN [x = 1] AS r"] {
        assert!(matches!(return_expr(&parse_ok(q)), Expr::List(_)), "{q}");
    }

    // A `(` after the `=` opens the pattern-comprehension reading, with the
    // keyword as the path variable.
    let stmt = parse_ok("RETURN [MATCH = (n)-->() | n] AS r");
    match return_expr(&stmt) {
        Expr::PatternComprehension(pc) => {
            assert_eq!(pc.path_variable.as_deref(), Some("MATCH"));
            assert!(
                matches!(pc.map_expr, Expr::Variable(ref v) if v == "n"),
                "{pc:?}"
            );
        }
        other => panic!("expected a pattern comprehension, got {other:?}"),
    }

    // A literal spelling can still be the path variable when a pattern
    // follows (Neo4j accepts `MATCH TRUE = ()-->()` too).
    let stmt = parse_ok("RETURN [true = (n)-->() | n] AS r");
    match return_expr(&stmt) {
        Expr::PatternComprehension(pc) => {
            assert_eq!(pc.path_variable.as_deref(), Some("true"))
        }
        other => panic!("expected a pattern comprehension, got {other:?}"),
    }
}

/// Regression guard: a keyword or literal on the left of `=` does not by
/// itself open the pattern-comprehension reading -- the tokens after the
/// `=` must actually start a pattern path. `x`, `null` and `(1 = 1)` cannot,
/// so the list-literal reading wins; for `x` the `UndefinedVariable` error
/// is left to semantic analysis. The lookahead still commits when a real
/// pattern follows, including behind a `shortestPath(...)` wrapper or a
/// selector/mode prefix.
#[test]
fn test_equality_after_keyword_stays_a_list_literal() {
    for q in [
        "RETURN [true = x] AS r",
        "RETURN [null = x] AS r",
        "RETURN [true = (1 = 1)] AS r",
    ] {
        match return_expr(&parse_ok(q)) {
            Expr::List(items) => {
                assert_eq!(items.len(), 1, "{q}: {items:?}");
                assert!(matches!(&items[0], Expr::Comparison(_)), "{q}: {items:?}");
            }
            other => panic!("{q}: expected a list literal, got {other:?}"),
        }
    }

    // `(x)` is a node pattern on its own, but no relationship follows it,
    // so `[true = (x)]` stays a list literal too.
    assert!(
        matches!(
            return_expr(&parse_ok("RETURN [true = (x)] AS r")),
            Expr::List(_)
        ),
        "expected a list literal"
    );

    // The same lookahead keeps the real pattern-comprehension readings.
    for q in [
        "RETURN [p = (n)-->() | n] AS r",
        "RETURN [p = WALK (n)-->() | n] AS r",
        "RETURN [p = shortestPath((n)-->()) | n] AS r",
        "RETURN [p = ((n)-->()) | n] AS r",
    ] {
        assert!(
            matches!(return_expr(&parse_ok(q)), Expr::PatternComprehension(_)),
            "{q}"
        );
    }
}

/// The `=` gate of a would-be pattern comprehension runs the real
/// `parse_pattern_path` reader speculatively
/// (`pattern_comprehension_follows_equals`), so every pattern shape the
/// grammar accepts is recognized after `p =` -- not just the plain
/// node-relationship bodies the old hand-rolled mirror lookahead knew.
/// The pattern reading is kept only when a `|` or `WHERE` follows the
/// pattern; anything else stays a list literal.
#[test]
fn test_pattern_comprehension_gate_reads_the_real_grammar() {
    // Label expressions and GPM forms the mirror lookahead could not skip.
    for q in [
        "RETURN [p = (n:A|B)-->() | p] AS r",
        "RETURN [p = (n:!A)-->() | p] AS r",
        "RETURN [p = (n:A&B)-->() | p] AS r",
        "RETURN [p = (n:%)-->() | p] AS r",
        "RETURN [p = (n:(A|B))-->() | p] AS r",
        "RETURN [p = (n:$(x))-->() | p] AS r",
        "RETURN [p = (IS A&B)-->() | p] AS r",
    ] {
        match return_expr(&parse_ok(q)) {
            Expr::PatternComprehension(pc) => {
                assert_eq!(pc.path_variable.as_deref(), Some("p"), "{q}: {pc:?}")
            }
            other => panic!("{q}: expected a pattern comprehension, got {other:?}"),
        }
    }

    // Bodies that are not plain node-relationship: a bare node, juxtaposed
    // nodes, a parenthesized subpath (with its own subpath variable), and
    // the shortestPath wrapper around one.
    for q in [
        "RETURN [p = (n) | p] AS r",
        "RETURN [p = (a)(b) | p] AS r",
        "RETURN [p = ((q = (a)-->(b))) | p] AS r",
        "RETURN [p = shortestPath(((a)-->(b))) | p] AS r",
    ] {
        match return_expr(&parse_ok(q)) {
            Expr::PatternComprehension(pc) => {
                assert_eq!(pc.path_variable.as_deref(), Some("p"), "{q}: {pc:?}")
            }
            other => panic!("{q}: expected a pattern comprehension, got {other:?}"),
        }
    }

    // A keyword spelling composes with the real-grammar gate, and a `WHERE`
    // after the pattern commits to the comprehension reading just like `|`.
    for q in [
        "RETURN [MATCH = (n:A|B)-->() | n] AS r",
        "RETURN [p = (n)-->() WHERE n.active | n] AS r",
    ] {
        assert!(
            matches!(return_expr(&parse_ok(q)), Expr::PatternComprehension(_)),
            "{q}"
        );
    }

    // The list-literal reading still wins when no pattern path (followed by
    // `|`/`WHERE`) can follow the `=`.
    for q in [
        "RETURN [true = x] AS r",
        "RETURN [null = x] AS r",
        "RETURN [true = (1 = 1)] AS r",
        "RETURN [true = false] AS r",
        "RETURN [x = 1] AS r",
    ] {
        match return_expr(&parse_ok(q)) {
            Expr::List(items) => {
                assert_eq!(items.len(), 1, "{q}: {items:?}");
                assert!(matches!(&items[0], Expr::Comparison(_)), "{q}: {items:?}");
            }
            other => panic!("{q}: expected a list literal, got {other:?}"),
        }
    }
}

/// A speculative `=`-probe that does not commit (the pattern parses but no
/// `|`/`WHERE` follows, so the list-literal reading wins) must leave no
/// parser state behind: the clause-level `label_saw_*`/`quantifier_saw_*`
/// flags it set while reading the pattern are restored
/// (`restore_pattern_side_effect_flags`). If they leaked, the clause-end
/// mixing checks in `parse_clause_at_current_depth` would fire on a later
/// label expression in the same clause. The colon-predicate fallback
/// (`true = (n:A|B)` read as `true = (n : A|B)`) sets none of those flags
/// itself, so a clean parse proves the probe was rolled back.
#[test]
fn test_failed_pattern_probe_leaves_no_clause_flags_behind() {
    // The probe reads `(n:A|B)` as a node pattern with a GQL `|` label
    // expression (setting `label_saw_gql_symbol`) before the missing `|`
    // sends it back; the second item's legacy `:A:B` colon-conjunction must
    // not then trip the InvalidLabelExpression mixing check.
    let stmt = parse_ok("RETURN [true = (n:A|B)], [q = (m:A:B)-->() | q] AS r");
    match return_expr(&stmt) {
        Expr::List(items) => {
            assert_eq!(items.len(), 1, "{items:?}");
            assert!(matches!(&items[0], Expr::Comparison(_)), "{items:?}");
        }
        other => panic!("expected a list literal, got {other:?}"),
    }

    // Same rollback for `(n IS A)`, whose probe sets
    // `label_saw_is_introducer` (the expression-level `IS` fallback does
    // not): a leak would trip MixedIsAndColonConjunction on the `:A:B`.
    assert!(
        matches!(
            return_expr(&parse_ok(
                "RETURN [true = (n IS A)], [q = (m:A:B)-->() | q] AS r"
            )),
            Expr::List(_)
        ),
        "expected a list literal"
    );

    // The commit path is not over-restored either: two comprehensions that
    // really parse in one clause still apply their quantifier flags, so the
    // legacy `[*1..2]` and the GPM `->+` mix is rejected.
    assert!(parse("RETURN [p = (a)-->+(b) | a], [q = (c)-[*1..2]->(d) | c] AS r").is_err());
}

/// ADR-0191 widened the named-path readers, so the CIP-51 "the path
/// variable declaration must precede the mode/selector prefix" check must
/// reject a keyword path variable after a mode prefix exactly like an
/// identifier one.
#[test]
fn test_keyword_path_variable_after_mode_prefix_still_rejected() {
    assert!(parse("MATCH WALK p = ()-->() RETURN p").is_err());
    assert!(parse("MATCH WALK WHERE = ()-->() RETURN `WHERE`").is_err());
}
