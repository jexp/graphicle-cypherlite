//! Value-operator features from the post-2026.09 CIP set: map addition
//! (CIP-273), map comprehension (CIP-272), string interpolation (CIP-261).
//! Expectations are doc-derived (first implementation 2026.09/2026.10) and
//! cross-checked against Neo4j where an image exists.

mod common;

use cypherlite::{Database, Value};
use std::collections::BTreeMap;

use common::connection;

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>()
            .into(),
    )
}

// CIP-273: MAP + MAP merges with last-write-wins; null on either side is
// null; MAP + LIST / LIST + MAP build a list; anything else is a type error.
#[test]
fn cip_273_map_addition() {
    let mut c = connection();

    let r = c.query("RETURN {a: 1, b: 2} + {b: 99, c: 3} AS r").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[
            ("a", Value::Int(1)),
            ("b", Value::Int(99)),
            ("c", Value::Int(3)),
        ])]]
    );

    // Null values overwrite on collision (last write wins, even null).
    let r = c.query("RETURN {a: 1, b: 2} + {b: null} AS r").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[("a", Value::Int(1)), ("b", Value::Null)])]]
    );

    // Chained merges: rightmost write wins per key.
    let r = c
        .query("RETURN {a: 1, b: 2} + {b: 99, c: 3} + {a: 100} + {b: 1} AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[
            ("a", Value::Int(100)),
            ("b", Value::Int(1)),
            ("c", Value::Int(3)),
        ])]]
    );

    // Null propagation.
    let r = c
        .query("RETURN {a: 1} + null AS rn, null + {b: 2} AS ln")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Null, Value::Null]]);

    // MAP + LIST / LIST + MAP: the map becomes a list element.
    let r = c.query("RETURN {a: 1} + [] AS r").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(vec![map(&[("a", Value::Int(1))])].into())]]
    );
    let r = c.query("RETURN [] + {a: 1} + {b: 2} AS r").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(
            vec![map(&[("a", Value::Int(1))]), map(&[("b", Value::Int(2))]),].into()
        )]]
    );

    // Non-map, non-list operands stay a type error.
    assert!(c.query("RETURN {a: 1} + 1 AS r").is_err());
    assert!(c.query("RETURN 1 + {a: 1} AS r").is_err());
}

// Map keys render sorted (BTreeMap): the merged map keeps canonical order.
#[test]
fn cip_273_merged_map_key_order() {
    let mut c = connection();
    let r = c.query("RETURN {z: 1} + {a: 2} AS r").unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::Map(
            BTreeMap::from([("a".into(), Value::Int(2)), ("z".into(), Value::Int(1)),]).into()
        )
    );
}

// CIP-261: interpolated string literals.
#[test]
fn cip_261_string_interpolation() {
    let mut c = connection();

    let r = c.query("RETURN s'a{1+1}b' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("a2b".into())]]);

    let r = c.query("RETURN S'{1}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("1".into())]]);

    let r = c.query("RETURN s' \\{x\\} {1}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(" {x} 1".into())]]);

    // Round-2 review: escapes inside `{...}` reach the embed's re-parse
    // verbatim, so a quoted string with an escaped quote is not corrupted.
    let r = c.query(r"RETURN s'{coalesce('a\'b','c')}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("a'b".into())]]);
    // The direct call evaluates to the same value.
    let direct = c.query(r"RETURN coalesce('a\'b','c') AS r").unwrap();
    assert_eq!(direct.rows, r.rows);

    // `\{` / `\}` inside an embed still parse (unknown escape stays literal).
    let r = c.query(r"RETURN s'{coalesce('x\{y','z')}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(r"x\{y".into())]]);

    let r = c.query("WITH 1 AS n RETURN s'{n}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("1".into())]]);

    let r = c.query("RETURN s'a{null}b' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Null]]);

    assert!(c.query("RETURN s'{[1, 2]}' AS r").is_err());
    assert!(c.query("RETURN s'{ {a: 1} }' AS r").is_err());

    // Round-3 review: an escaped backslash immediately before an embed is a
    // backslash followed by the embed, not an escaped literal brace. `\{`
    // is a literal brace; a doubled backslash decodes once.
    let r = c.query(r"RETURN s'\\{1}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("\\1".into())]]);
    let r = c.query(r"RETURN s'\{1}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("{1}".into())]]);
    let r = c.query(r"RETURN s'a\\b{1}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("a\\b1".into())]]);

    // Round-3 review: a backtick-quoted identifier inside an embed is inert,
    // so a `}` or `'` inside the name does not end the embed.
    let r = c.query("WITH 1 AS `a}b` RETURN s'{`a}b`}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("1".into())]]);
    let r = c.query("WITH 'q' AS `x'y` RETURN s'{`x'y`}' AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("q".into())]]);

    // Walker regression guard: an embed is a full expression, so the
    // compile-time aggregation passes must see inside it. Before CIP-261
    // walker support this escaped the NestedAggregation check and only
    // failed at runtime with a coercion error.
    let err = c
        .query("CREATE (n) WITH collect(n.name) AS c RETURN count(s'{collect(n)}') AS r")
        .unwrap_err();
    assert!(
        err.to_string().contains("NestedAggregation"),
        "expected a compile-time NestedAggregation, got: {err}"
    );

    // Same for the non-constant-argument check: rand() inside an embed of an
    // aggregation argument is still a compile-time NonConstantExpression.
    let err = c
        .query("UNWIND [1] AS x RETURN count(s'{rand()}') AS r")
        .unwrap_err();
    assert!(
        err.to_string().contains("NonConstantExpression"),
        "expected a compile-time NonConstantExpression, got: {err}"
    );
}

// CIP-272: map comprehensions.
#[test]
fn cip_272_map_comprehension() {
    let mut c = connection();

    let r = c
        .query("WITH {a: 1, b: 2} AS m RETURN {k : v IN m | k : v * 10} AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[("a", Value::Int(10)), ("b", Value::Int(20))])]]
    );

    let r = c
        .query("WITH [1, 2] AS l RETURN {i IN l | 'k' + toString(i): i} AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[("k1", Value::Int(1)), ("k2", Value::Int(2))])]]
    );

    let r = c
        .query("WITH [1, 2, 3] AS l RETURN {i IN l WHERE i > 1 | 'x': i} AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![map(&[("x", Value::Int(3))])]]);

    // Duplicate keys: last write wins.
    let r = c
        .query("WITH [1, 2, 3] AS l RETURN {i IN l | 'k': i * 2} AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![map(&[("k", Value::Int(6))])]]);

    // Null sources are null; a non-string key is a type error.
    let r = c.query("RETURN {k : v IN null | k : v} AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Null]]);
    assert!(c.query("RETURN {i IN [1] | i : i} AS r").is_err());

    // `{k: v IN m}` without a pipe stays a map literal (value = IN expr).
    let r = c
        .query("WITH [1] AS m, 5 AS v RETURN {k: v IN m} AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![map(&[("k", Value::Bool(false))])]]);

    // Bindings do not leak out of the comprehension.
    assert!(c
        .query("WITH [1] AS l WITH {i IN l | 'a': i} AS m RETURN i AS r")
        .is_err());

    // The comprehension source may contain aggregation (the
    // list-comprehension equivalent already worked); the runtime
    // aggregate evaluator needs a MapComp arm or this errors with
    // "collect() is an aggregating function".
    let r = c
        .query("UNWIND [1, 2] AS x RETURN {i IN collect(x) | 'k' + toString(i): i} AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![map(&[("k1", Value::Int(1)), ("k2", Value::Int(2))])]]
    );

    // Aggregation in the projection/WHERE is a compile-time error, same
    // as for list comprehensions.
    assert!(c
        .query("UNWIND [1, 2] AS x RETURN {i IN collect(x) | 'k': count(i)} AS r")
        .is_err());
    assert!(c
        .query("UNWIND [1, 2] AS x RETURN {i IN collect(x) WHERE count(i) > 0 | 'k': i} AS r")
        .is_err());
}

// CIP-272 round-3 review: a brace literal with no top-level `|` is a map
// literal, and the comprehension speculation must not full-parse it first. That
// speculation re-parsed the same tokens, doubling the work per nesting level
// (pre-fix, in a debug build: depth 12 took 6.5 ms, depth 16 took 105 ms, depth
// 20 took 1.7 s), so 50 pipeless levels is a bounded-time assertion the pre-fix
// parser cannot satisfy. Parsing and evaluating it recurse per level, deeper
// than a default test-thread stack allows, so the work runs on an explicitly
// sized one -- the same reason the CIP-eval gate sets RUST_MIN_STACK.
#[test]
fn cip_272_pipe_less_nesting_stays_a_map_literal() {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let mut c = connection();
            let depth = 50;
            // Each level is the reviewed shape: a map-pair prefix whose value is
            // an `IN` over the nested literal, wrapped in a list so the innermost
            // source is a real list and every level evaluates (`1 IN [<map>]` is
            // false, so the whole literal is one pair per level).
            let mut expr = String::from("m");
            for _ in 0..depth {
                expr = format!("{{a: b IN [{expr}]}}");
            }
            let r = c
                .query(&format!("WITH [1] AS m, 1 AS b RETURN {expr} AS r"))
                .unwrap();
            assert_eq!(r.rows, vec![vec![map(&[("a", Value::Bool(false))])]]);
        })
        .expect("spawning the big-stack test thread")
        .join()
        .expect("deep map-literal query must not panic");
}

// CIP-236: explicit GROUP BY keys.
#[test]
fn cip_236_group_by() {
    let mut c = connection();

    let r = c
        .query("UNWIND [1,1,2,2,3] AS x RETURN x AS k, count(*) AS c GROUP BY x ORDER BY k")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(2)],
            vec![Value::Int(3), Value::Int(1)],
        ]
    );

    // ALL = non-aggregating items as keys.
    let r = c
        .query("UNWIND [1,1,2] AS x RETURN x, count(*) AS c GROUP BY ALL ORDER BY x")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );

    // () = one global group (aggregate-only projections).
    let r = c
        .query("UNWIND [1,2] AS x RETURN count(*) AS c GROUP BY ()")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);

    // Key differs from the projection; alias reference; WHERE filters.
    let r = c
        .query("UNWIND [1,1,2,3] AS x RETURN x + 0 AS k, count(*) AS c GROUP BY x ORDER BY k")
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    let r = c
        .query("UNWIND [1,1,2] AS x WITH x AS k, count(*) AS c GROUP BY k WHERE c > 1 RETURN k, c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Int(2)]]);

    // Non-aggregating GROUP BY acts as DISTINCT.
    let r = c.query("UNWIND [1,1,2] AS x RETURN x GROUP BY x").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // Regression (PR #62 review): explicit non-empty keys over an empty
    // input yield zero rows, not a phantom global group. All-aggregate
    // projection items are the shape that used to slip through the
    // empty-input special case.
    let mut empty = connection();
    let r = empty
        .query("MATCH (n) RETURN count(*) AS c GROUP BY n.x")
        .unwrap();
    assert!(r.rows.is_empty(), "phantom global group: {:?}", r.rows);
    // ... while the keyless forms still emit their one global group.
    let r = empty
        .query("MATCH (n) RETURN count(*) AS c GROUP BY ()")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    let r = empty.query("MATCH (n) RETURN count(*) AS c").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);

    // Regression (PR #62 review): the one-hop fold keys on non-aggregating
    // projection items, so with an explicit GROUP BY it must stand down --
    // else it groups by `n.y` here instead of the key `n.x` (all null: one
    // group) and returns two rows.
    c.query(
        "CREATE (a1:A {y: 1}), (b1:B {x: 9}), (a1)-[:R]->(b1)              CREATE (a2:A {y: 2}), (b2:B {x: 9}), (a2)-[:R]->(b2)",
    )
    .unwrap();
    let r = c
        .query("MATCH (n:A)-[r:R]->(m:B) RETURN n.y AS y, count(*) AS c GROUP BY n.x")
        .unwrap();
    assert_eq!(r.rows.len(), 1, "fold ignored GROUP BY: {:?}", r.rows);
    assert_eq!(r.rows[0][1], Value::Int(2));

    // Regression (PR #62 round 2): a grouping key mixed into an aggregation
    // expression is valid -- `x` is grouped, so `count(*) + x` is not
    // ambiguous. The grouping-key set must come from the explicit GROUP BY,
    // not only from non-aggregating projection items.
    let r = c
        .query("UNWIND [1,1,2] AS x RETURN count(*) + x AS c GROUP BY x ORDER BY c")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(3)], vec![Value::Int(3)]],
        "GROUP BY key rejected inside aggregation expression"
    );
    let r = c
        .query("UNWIND [1,1,2] AS x WITH count(*) + x AS c GROUP BY x RETURN c ORDER BY c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(3)], vec![Value::Int(3)]]);

    // Without the GROUP BY the same expression is still rejected: `x` is
    // not a grouping key then.
    assert!(
        c.query("UNWIND [1,1,2] AS x RETURN count(*) + x AS c")
            .is_err(),
        "ungrouped variable must stay ambiguous"
    );
    // A variable that is not a grouping key stays ambiguous even with keys.
    assert!(
        c.query("UNWIND [1,1,2] AS x UNWIND [1] AS y RETURN count(*) + y AS c GROUP BY x")
            .is_err(),
        "non-key variable must stay ambiguous"
    );

    // Regression (PR #62 round 2): `GROUP BY ()` before a subquery's closing
    // brace -- the `()` must be recognised as the empty grouping set, not as
    // a parenthesized key.
    let r = c
        .query("CALL { UNWIND [1,2] AS x RETURN count(*) AS c GROUP BY () } RETURN c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
}

// CIP-241: SET resolves against pre-clause state (dynamic targets must not
// observe earlier assignments in the same clause).
#[test]
fn cip_241_set_lhs_precedence() {
    let mut c = connection();
    c.query("CREATE (:A {x: 0, y: 1})").unwrap();
    c.query("MATCH (n:A) SET n['x'] = n.y, n['y'] = n.x")
        .unwrap();
    let r = c.query("MATCH (n) RETURN n.x AS x, n.y AS y").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Int(0)]]);

    c.query("CREATE (:B {x: 0, y: 1})").unwrap();
    c.query("MATCH (n:B) SET n.x = n.y, n.y = n.x").unwrap();
    let r = c.query("MATCH (n:B) RETURN n.x AS x, n.y AS y").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Int(0)]]);

    // Dynamic key resolved pre-clause: writes go to the old key's name.
    c.query("CREATE (:C {k: 'target', target: 'old'})").unwrap();
    c.query("MATCH (n:C) SET n[n.k] = 'new', n.k = 'other'")
        .unwrap();
    let r = c
        .query("MATCH (n:C) RETURN n.k AS k, n.other AS other, n.target AS target")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::String("other".into()),
            Value::Null,
            Value::String("new".into())
        ]]
    );
}

// CIP-246: UUID type.
#[test]
fn cip_246_uuid_type() {
    let mut c = connection();
    let id = "f066ecfc-5151-4121-8a36-b35e00d72918";

    let r = c
        .query(&format!("RETURN toString(uuid('{id}')) AS r"))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(id.into())]]);

    // The live-Neo4j-verified two-integer round-trip (the CIP's own example
    // numbers are stale; see CIP-246 NOTES.md).
    let r = c
        .query("RETURN toString(uuid(-1123963282200444639, -8487445722420950760)) AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::String(
            "f066e159-8fdc-4121-8a36-891578b0c918".into()
        )]]
    );

    let r = c
        .query(&format!(
            "RETURN uuid('{id}') IS :: UUID AS p, valueType(uuid('{id}')) AS t"
        ))
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Bool(true),
            Value::String("UUID NOT NULL".into())
        ]]
    );
}

// CIP-205: relationship property value access (static and dynamic).
#[test]
fn cip_205_rel_value_access() {
    let mut c = connection();
    c.query("CREATE (:A)-[:R {k: 'v'}]->(:B)").unwrap();
    let r = c
        .query("MATCH ()-[r:R]->() RETURN r['k'] AS a, r.k AS b")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::String("v".into()), Value::String("v".into())]]
    );
    c.query("MATCH ()-[r:R]->() SET r['a'] = 1 REMOVE r['k']")
        .unwrap();
    let r = c
        .query("MATCH ()-[r:R]->() RETURN r.a AS a, r.k AS k")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Null]]);
}

// CIP-225: fulltext SEARCH + vector search property filters.
#[test]
fn cip_225_fulltext_and_vector_filters() {
    let mut c = connection();
    c.query("CREATE FULLTEXT INDEX ft FOR (p:Person) ON EACH [p.name]")
        .unwrap();
    c.query("CREATE (:Person {name: 'foo bar', age: 30}), (:Person {name: 'bar baz', age: 10}), (:Person {name: 'other'})").unwrap();

    let r = c
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'foo baz' LIMIT 10) RETURN DISTINCT p.name AS n")
        .unwrap();
    assert_eq!(r.rows.len(), 2);

    // Case-insensitive; SCORE AS positive; empty on no match.
    let r = c
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'Foo' LIMIT 10) SCORE AS s RETURN DISTINCT p.name AS n, s > 0 AS pos")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::String("foo bar".into()), Value::Bool(true)]]
    );
    let r = c
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'nope' LIMIT 10) RETURN p.name AS n")
        .unwrap();
    assert!(r.rows.is_empty());

    // WHERE with FULLTEXT is rejected; missing index raises.
    assert!(c
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'foo' WHERE p.age > 20 LIMIT 10) RETURN p.name")
        .is_err());
    assert!(c
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX nope FOR 'foo' LIMIT 10) RETURN p.name")
        .is_err());

    // Vector search with a WITH-declared property filter; SKIP is
    // fulltext-only.
    c.query("CREATE VECTOR INDEX vidx FOR (p:Person) ON p.embed WITH [p.age]")
        .unwrap();
    c.query("CREATE (:Person {name: 'v', age: 30, embed: [1.0, 0.0]})")
        .unwrap();
    let r = c
        .query("MATCH (p) SEARCH p IN (VECTOR INDEX vidx FOR [1.0, 0.0] WHERE p.age > 20 LIMIT 10) RETURN DISTINCT p.name AS n")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("v".into())]]);
    assert!(c
        .query("MATCH (p) SEARCH p IN (VECTOR INDEX vidx FOR [1.0, 0.0] SKIP 1 LIMIT 10) RETURN p.name")
        .is_err());
}

// CIP-225 hybrid: chained SEARCH clauses intersect; an already-bound
// binding variable is not re-bound.
#[test]
fn cip_225_hybrid_search_intersection() {
    let mut c = connection();
    c.query("CREATE VECTOR INDEX vidx FOR (p:Person) ON p.embed")
        .unwrap();
    c.query("CREATE FULLTEXT INDEX ft FOR (p:Person) ON EACH [p.name]")
        .unwrap();
    c.query("CREATE (:Person {name: 'foo', embed: [1.0, 0.0]}), (:Person {name: 'bar', embed: [0.0, 1.0]})").unwrap();

    let r = c
        .query("MATCH (p) SEARCH p IN (VECTOR INDEX vidx FOR [1.0, 0.0] LIMIT 10) WITH p MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'foo' LIMIT 10) RETURN DISTINCT p.name AS n")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("foo".into())]]);

    let r = c
        .query("MATCH (p) SEARCH p IN (VECTOR INDEX vidx FOR [1.0, 0.0] LIMIT 10) WITH p MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'nomatch' LIMIT 10) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);

    assert!(c
        .query("MATCH (p) SEARCH p IN (VECTOR INDEX vidx FOR [1.0, 0.0] LIMIT 10) SEARCH p IN (FULLTEXT INDEX ft FOR 'foo' LIMIT 10) RETURN p.name")
        .is_err());
}

// CIP-246: UUID type (cherry-picked from #55).
#[test]
fn cip_246_uuid_property_values() {
    let mut c = connection();
    let id = "f066ecfc-5151-4121-8a36-b35e00d72918";
    let other = "00000000-0000-0000-0000-00000000000a";

    // CREATE with a UUID property.
    let r = c
        .query(&format!(
            "CREATE (n:P {{id: uuid('{id}')}}) RETURN toString(n.id) AS r"
        ))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(id.into())]]);

    // SET a UUID property.
    let r = c
        .query(&format!(
            "MATCH (n:P) SET n.other = uuid('{other}') RETURN toString(n.other) AS r"
        ))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(other.into())]]);

    // SET += map with a UUID value.
    let r = c
        .query(&format!(
            "MATCH (n:P) SET n += {{tag: uuid('{other}')}} RETURN toString(n.tag) AS r"
        ))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(other.into())]]);

    // MERGE keyed on a UUID property.
    let r = c
        .query(&format!(
            "MERGE (m:Q {{k: uuid('{id}')}}) RETURN toString(m.k) AS r"
        ))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(id.into())]]);

    // Relationship property.
    let r = c
        .query(&format!(
            "CREATE (:A)-[rel:R {{u: uuid('{id}')}}]->(:B) RETURN toString(rel.u) AS r"
        ))
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(id.into())]]);

    // Lists of UUIDs (homogeneous list kind must accept UUID).
    let r = c
        .query(&format!(
            "CREATE (n:L {{ids: [uuid('{id}'), uuid('{other}')]}}) \
             RETURN [x IN n.ids | toString(x)] AS r"
        ))
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(
            vec![Value::String(id.into()), Value::String(other.into())].into()
        )]]
    );

    // Non-property types are still rejected.
    assert!(c.query("CREATE (n:Bad {m: {a: 1}})").is_err());
}

// CIP-246: a UUID property survives a durable WAL replay (file reopen).
#[test]
fn cip_246_uuid_property_wal_roundtrip() {
    let id = "f066ecfc-5151-4121-8a36-b35e00d72918";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query(&format!("CREATE (:P {{id: uuid('{id}')}})"))
            .unwrap();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:P) RETURN toString(n.id) AS r").unwrap();
    assert_eq!(r.rows, vec![vec![Value::String(id.into())]]);
}

// CIP-246: persistence round-trip of the UUID value through the WAL tag.
#[test]
fn cip_246_uuid_wal_roundtrip() {
    let id = "f066ecfc-5151-4121-8a36-b35e00d72918";
    let v = Value::Uuid(cypherlite::uuid_parse(id).unwrap());
    assert_eq!(
        cypherlite::uuid_hyphenated(match &v {
            Value::Uuid(u) => *u,
            _ => unreachable!(),
        }),
        id
    );
    let tagged = v.to_json_tagged();
    assert_eq!(Value::from_json_tagged(&tagged).unwrap(), v);
}
