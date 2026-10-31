use super::*;

mod admin;
mod braces;
mod conditionals;
mod cypher_prefix;
mod expressions;
mod gql_for;
mod graph_types;
mod hybrid_search;
mod keyword_aliases;
mod keyword_label_names;
mod keyword_map_keys;
mod keyword_variables;
mod label_expressions;
mod let_filter;
mod literals;
mod next;
mod patterns;
mod qpp;
mod schema;
mod statements;
mod strings;
mod subqueries;
mod type_predicates;
mod unicode_identifiers;

// ADR-0116 §2: every database administration command outside CE's
// supported set fails the parse with a `ParseError` tagged
// `UNSUPPORTED_ADMINISTRATION_DETAIL` carrying the verbatim command
// text (without a leading `CYPHER n` preamble), so the front-end can
// reject it with CE's `UnsupportedAdministrationCommand` instead of a
// syntax error -- and no executable clause is ever produced.
/// The unsupported-administration command table (statement, echo), from
/// the one shared home (testdata/unsupported-admin-commands.tsv).
fn unsupported_admin_commands() -> impl Iterator<Item = (&'static str, &'static str)> {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/unsupported-admin-commands.tsv"
    ))
    .lines()
    .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    .map(|l| l.split_once('\t').expect("TSV row: statement<TAB>echo"))
}

/// Parses a single-`RETURN` statement and returns its first item's
/// `IS [NOT] :: <type>` predicate, so the type-predicate tests share one
/// extraction instead of re-matching the clause/item/expression chain.
fn is_type_of(query: &str) -> IsTypeExpr {
    let stmt = parse(query).unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::IsType(it) => (**it).clone(),
            other => panic!("expected IsType, got {other:?}"),
        },
        other => panic!("expected Return, got {other:?}"),
    }
}

fn qpp_quantifier(query: &str) -> PathQuantifier {
    let stmt = parse(query).unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => match &mc.pattern[0].elements[1] {
            PatternElement::Quantified(qpp) => qpp.quantifier,
            other => panic!("expected a Quantified element, got {other:?}"),
        },
        other => panic!("expected match, got {other:?}"),
    }
}

fn interp_parts(q: &str) -> Vec<InterpPart> {
    let stmt = parse(q).unwrap();
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected Return");
    };
    let Expr::StringInterp(parts) = &r.items[0].expr else {
        panic!("expected StringInterp, got {:?}", r.items[0].expr);
    };
    parts.clone()
}

fn return_group_by(q: &str) -> Option<GroupBySpec> {
    let stmt = parse(q).unwrap();
    stmt.clauses
        .iter()
        .find_map(|c| match c {
            Clause::Return(r) => Some(r.group_by.clone()),
            _ => None,
        })
        .expect("no RETURN clause")
}

/// The `CALL { ... }` clause of a parsed statement. `find_map` locates the
/// clause regardless of how many `WITH`/`MATCH` clauses precede it, so the
/// CIP-141 scope and CIP-32 `IN TRANSACTIONS` tests share one extraction
/// instead of re-matching the clause chain at a varying index; each caller
/// selects the field it cares about (`scope`, `in_transactions`).
fn call_subquery_of(s: &Statement) -> &CallSubqueryClause {
    s.clauses
        .iter()
        .find_map(|c| match c {
            Clause::CallSubquery(csq) => Some(csq),
            _ => None,
        })
        .expect("no CALL subquery clause")
}

/// One row of the keyword-symbolic-names probe fixture
/// (`testdata/keyword-symbolic-names/probe.tsv`, ADR-0191): the unique
/// `<shape>/<keyword>` id, the name group (`alias`, `reference`, `binding`
/// or `key`), the query shape, the keyword spelling, both engines'
/// recorded outcomes and the probe query.
struct ProbeRow {
    id: &'static str,
    group: &'static str,
    shape: &'static str,
    keyword: &'static str,
    neo4j: &'static str,
    cypherlite: &'static str,
    query: &'static str,
}

/// The probe fixture's rows for one name group (ADR-0191). The fixture is
/// the shared input for `keyword_aliases`, `keyword_variables` and
/// `keyword_map_keys`; each reads its own group so the fixture stays the
/// one source of truth for the alias, reference, binding and key
/// positions.
fn probe_rows(group: &str) -> Vec<ProbeRow> {
    let mut lines = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/keyword-symbolic-names/probe.tsv"
    ))
    .lines();
    assert_eq!(
        lines.next(),
        Some("id\tgroup\tshape\tkeyword\tneo4j\tcypherlite\tquery"),
        "probe.tsv header"
    );
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut fields = l.split('\t');
            let row = ProbeRow {
                id: fields.next().expect("probe row: id"),
                group: fields.next().expect("probe row: group"),
                shape: fields.next().expect("probe row: shape"),
                keyword: fields.next().expect("probe row: keyword"),
                neo4j: fields.next().expect("probe row: neo4j outcome"),
                cypherlite: fields.next().expect("probe row: cypherlite outcome"),
                query: fields.next().expect("probe row: query"),
            };
            assert!(fields.next().is_none(), "probe row: extra fields in {l:?}");
            row
        })
        .filter(|row| row.group == group)
        .collect()
}

/// The probe fixture's rows for one name group with the group-level checks
/// every group test needs (ADR-0191): the group is non-empty and every row
/// id (`<shape>/<keyword>`) is unique. Each group module calls this instead
/// of repeating the checks.
fn probe_group_rows(group: &str) -> Vec<ProbeRow> {
    let rows = probe_rows(group);
    assert!(!rows.is_empty(), "probe.tsv has no {group} rows");
    let mut ids = std::collections::HashSet::new();
    for row in &rows {
        assert!(ids.insert(row.id), "duplicate {group} row id {}", row.id);
    }
    rows
}

/// ADR-0191 point 5: no row of `group` is in the ADR's `exception_rows`,
/// so every Neo4j-accepted row of the group must parse. Shared by the
/// group modules' all-rows-parse tests (`test_probe_binding_rows_parse`,
/// `probe_key_rows_accepted_by_neo4j_parse`) so they stay one-line
/// delegates like the `probe_fixture_has_*_rows` tests.
fn probe_accepted_rows_parse(group: &str) {
    let rows = probe_group_rows(group);
    let accepted = rows.iter().filter(|row| row.neo4j == "accepted").count();
    assert_eq!(
        accepted,
        rows.len(),
        "every {group} row is Neo4j-accepted; the fixture grew a rejected row"
    );
    for row in &rows {
        assert!(
            parse(row.query).is_ok(),
            "{}: {:?} must parse after ADR-0191, got {:?}",
            row.id,
            row.query,
            parse(row.query).err()
        );
    }
}

/// Parses `q`, which must succeed, naming the query text when it does not.
fn parse_ok(q: &str) -> Statement {
    parse(q).unwrap_or_else(|e| panic!("{q:?} must parse: {e}"))
}

/// The leading MATCH clause of `s`, which must be a MATCH.
fn match_clause(s: &Statement) -> &MatchClause {
    match &s.clauses[0] {
        Clause::Match(m) => m,
        other => panic!("expected MATCH, got {other:?}"),
    }
}

/// The first item expression of the statement's last RETURN clause.
fn return_expr(s: &Statement) -> &Expr {
    match s.clauses.last() {
        Some(Clause::Return(rc)) => &rc.items[0].expr,
        other => panic!("expected RETURN, got {other:?}"),
    }
}

/// ADR-0191's probe fixture covers all four name groups and every row id
/// (`<shape>/<keyword>`) is unique across the fixture, so each group module
/// can index its rows by id.
#[test]
fn test_probe_fixture_covers_all_groups_with_unique_ids() {
    let mut seen = std::collections::HashSet::new();
    for group in ["alias", "reference", "binding", "key"] {
        let rows = probe_group_rows(group);
        for row in &rows {
            assert!(!row.shape.is_empty(), "{}: empty shape", row.id);
            assert!(!row.keyword.is_empty(), "{}: empty keyword", row.id);
            assert!(!row.query.is_empty(), "{}: empty query", row.id);
            assert!(!row.neo4j.is_empty(), "{}: empty neo4j outcome", row.id);
            assert!(
                !row.cypherlite.is_empty(),
                "{}: empty cypherlite outcome",
                row.id
            );
            assert!(seen.insert(row.id), "duplicate probe row id {}", row.id);
        }
    }
}
