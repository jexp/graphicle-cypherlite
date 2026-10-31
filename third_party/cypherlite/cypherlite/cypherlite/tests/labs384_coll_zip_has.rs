//! LABS-384 / CIP-274 + CIP-286: the `coll.zip(list1, list2, ...)` and
//! `coll.hasAnyOf` / `coll.hasAllOf` / `coll.hasNoneOf` collection functions.
//! `coll.zip` zips to the longest input, padding with null; the `coll.has*Of`
//! functions are quantifiers over the canonical three-valued `IN`
//! membership, so a null element acts as an unknown value exactly like
//! `any()` / `all()` / `none()`.

mod common;

use common::{connection, err_text, list, rows, scalar_one, string};
use cypherlite::Value;

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// `RETURN <expr> AS result` → the single result value.
fn eval(query: &str) -> Value {
    let mut conn = connection();
    scalar_one(&mut conn, query)
}

fn eval_err(query: &str) -> String {
    let mut conn = connection();
    err_text(conn.query(query).map(|_| ()))
}

#[test]
fn coll_zip_pairs_equal_length_lists() {
    assert_eq!(
        eval("RETURN coll.zip([1, 2, 3], ['a', 'b', 'c']) AS result"),
        list(vec![
            list(vec![i(1), string("a")]),
            list(vec![i(2), string("b")]),
            list(vec![i(3), string("c")])
        ])
    );
}

#[test]
fn coll_zip_takes_three_or_more_lists() {
    assert_eq!(
        eval("RETURN coll.zip([1, 2], ['a', 'b'], [true, false]) AS result"),
        list(vec![
            list(vec![i(1), string("a"), Value::Bool(true)]),
            list(vec![i(2), string("b"), Value::Bool(false)])
        ])
    );
}

#[test]
fn coll_zip_pads_short_lists_with_null() {
    // Longer first list: the second list's missing tail is null.
    assert_eq!(
        eval("RETURN coll.zip([1, 2, 3, 4], ['a', 'b', 'c']) AS result"),
        list(vec![
            list(vec![i(1), string("a")]),
            list(vec![i(2), string("b")]),
            list(vec![i(3), string("c")]),
            list(vec![i(4), Value::Null])
        ])
    );
    // Longer second list: the first list's missing tail is null.
    assert_eq!(
        eval("RETURN coll.zip([1, 2], ['a', 'b', 'c']) AS result"),
        list(vec![
            list(vec![i(1), string("a")]),
            list(vec![i(2), string("b")]),
            list(vec![Value::Null, string("c")])
        ])
    );
    // Only one of several lists is short: each column pads independently.
    assert_eq!(
        eval("RETURN coll.zip([1, 2, 3], [], ['a']) AS result"),
        list(vec![
            list(vec![i(1), Value::Null, string("a")]),
            list(vec![i(2), Value::Null, Value::Null]),
            list(vec![i(3), Value::Null, Value::Null])
        ])
    );
}

#[test]
fn coll_zip_empty_lists_zip_to_an_empty_list() {
    assert_eq!(eval("RETURN coll.zip([], []) AS result"), list(vec![]));
    // A non-empty list against an empty one still zips to its own length.
    assert_eq!(
        eval("RETURN coll.zip([1, 2, 3], []) AS result"),
        list(vec![
            list(vec![i(1), Value::Null]),
            list(vec![i(2), Value::Null]),
            list(vec![i(3), Value::Null])
        ])
    );
}

#[test]
fn coll_zip_keeps_null_elements_of_the_input_lists() {
    assert_eq!(
        eval("RETURN coll.zip([1, null, 3], []) AS result"),
        list(vec![
            list(vec![i(1), Value::Null]),
            list(vec![Value::Null, Value::Null]),
            list(vec![i(3), Value::Null])
        ])
    );
}

#[test]
fn coll_zip_propagates_a_null_argument() {
    for query in [
        "RETURN coll.zip([1, 2, 3], null) AS result",
        "RETURN coll.zip(null, ['a', 'b']) AS result",
        "RETURN coll.zip(null, []) AS result",
        "RETURN coll.zip([1], [1], [1], [1], null) AS result",
    ] {
        assert_eq!(eval(query), Value::Null, "{query}");
    }
}

#[test]
fn coll_zip_requires_two_or_more_list_arguments() {
    assert!(
        eval_err("RETURN coll.zip([1, 2, 3]) AS result").contains("InvalidNumberOfArguments"),
        "one argument must be an arity error"
    );
    // The non-list argument is a type error in either position.
    for query in [
        "RETURN coll.zip(5, []) AS result",
        "RETURN coll.zip([], 5) AS result",
    ] {
        assert!(
            eval_err(query).contains("type error: coll.zip() requires a list"),
            "{query}: expected the coll.* family's type error"
        );
    }
}

/// One `coll.has*Of` truth row, its columns named.
struct HasCase {
    /// The first argument (the list to search).
    list: &'static str,
    /// The second argument (the values looked for).
    values: &'static str,
    /// The expected `coll.hasAnyOf` result.
    any: Value,
    /// The expected `coll.hasAllOf` result.
    all: Value,
    /// The expected `coll.hasNoneOf` result.
    none: Value,
}

/// The shared table of `coll.has*Of` truth.
fn has_cases() -> Vec<HasCase> {
    let b = Value::Bool;
    vec![
        // Intersecting lists: Any true, All true, None false (both orders).
        HasCase {
            list: "[1, 2, 3, 4]",
            values: "[1]",
            any: b(true),
            all: b(true),
            none: b(false),
        },
        // Disjoint lists: Any false, All false, None true.
        HasCase {
            list: "[1, 2, 3, 4]",
            values: "['1', true, date()]",
            any: b(false),
            all: b(false),
            none: b(true),
        },
        // Numeric equality across INTEGER and FLOAT (1 = 1.0).
        HasCase {
            list: "[1]",
            values: "[1.0]",
            any: b(true),
            all: b(true),
            none: b(false),
        },
        // NaN <> NaN in Cypher: a list of NaN has no intersection with itself.
        HasCase {
            list: "[0.0/0.0]",
            values: "[0.0/0.0]",
            any: b(false),
            all: b(false),
            none: b(true),
        },
        // Set logic, not multiset logic: duplicates in the subset are not new.
        HasCase {
            list: "[1, 2, 3, 4, 5]",
            values: "[1, 1, 2, 2, 3, 3, 4, 4]",
            any: b(true),
            all: b(true),
            none: b(false),
        },
        // `hasAllOf` is not symmetric: the subset direction matters.
        HasCase {
            list: "[1, 2, 3, 4]",
            values: "[1, 5]",
            any: b(true),
            all: b(false),
            none: b(false),
        },
        // Nested lists and maps compare structurally.
        HasCase {
            list: "[[1, 2], [3, 4]]",
            values: "[[1, 2]]",
            any: b(true),
            all: b(true),
            none: b(false),
        },
        HasCase {
            list: "[{a: 1}, {b: 2}]",
            values: "[{a: 1}]",
            any: b(true),
            all: b(true),
            none: b(false),
        },
        // Empty values list: Any false, All (vacuously) true, None true.
        HasCase {
            list: "[1, 2, 3]",
            values: "[]",
            any: b(false),
            all: b(true),
            none: b(true),
        },
        HasCase {
            list: "[]",
            values: "[]",
            any: b(false),
            all: b(true),
            none: b(true),
        },
        // A null element of values is unknown for Any/None; for All the
        // known-missing 3 decides false even beside the null.
        HasCase {
            list: "[1, 2]",
            values: "[3, null]",
            any: Value::Null,
            all: b(false),
            none: Value::Null,
        },
        // A null element is irrelevant once another value decides.
        HasCase {
            list: "[1, 2]",
            values: "[1, null]",
            any: b(true),
            all: Value::Null,
            none: b(false),
        },
        // The reversed direction of the row above (null in the list, not in
        // the values): the missing subset element could still match the null
        // list element, so hasAllOf stays unknown.
        HasCase {
            list: "[1, null]",
            values: "[1, 2]",
            any: b(true),
            all: Value::Null,
            none: b(false),
        },
        // Null in list: the missing subset element could still match it.
        HasCase {
            list: "[1, null]",
            values: "[1, 3]",
            any: b(true),
            all: Value::Null,
            none: b(false),
        },
        // Null in both lists: null <> null, so nothing is decided.
        HasCase {
            list: "[1, null]",
            values: "[1, null]",
            any: b(true),
            all: Value::Null,
            none: b(false),
        },
        HasCase {
            list: "[null]",
            values: "[null]",
            any: Value::Null,
            all: Value::Null,
            none: Value::Null,
        },
        // A null value against an empty list is decided: no intersection.
        HasCase {
            list: "[]",
            values: "[null]",
            any: b(false),
            all: b(false),
            none: b(true),
        },
    ]
}

#[test]
fn coll_has_functions_follow_the_quantifier_semantics() {
    for HasCase {
        list,
        values,
        any,
        all,
        none,
    } in has_cases()
    {
        // `Any` and `None` are symmetric in their two arguments (the
        // quantifier runs over the second one either way); `All` is not
        // (subset direction), so the reversed call is only checked there
        // through the table's own asymmetric rows.
        for (function, expected, symmetric) in [
            ("coll.hasAnyOf", any, true),
            ("coll.hasAllOf", all, false),
            ("coll.hasNoneOf", none, true),
        ] {
            let query = format!("RETURN {function}({list}, {values}) AS result");
            assert_eq!(eval(&query), expected, "{query}");
            if symmetric {
                let reversed = format!("RETURN {function}({values}, {list}) AS result");
                assert_eq!(eval(&reversed), expected, "{reversed}");
            }
        }
    }
}

#[test]
fn coll_has_functions_propagate_a_null_argument() {
    for function in ["coll.hasAnyOf", "coll.hasAllOf", "coll.hasNoneOf"] {
        let query = format!("RETURN {function}([1, 2, 3, 4], null) AS result");
        assert_eq!(eval(&query), Value::Null, "{query}");
        let query = format!("RETURN {function}(null, [1, 2]) AS result");
        assert_eq!(eval(&query), Value::Null, "{query}");
        // Left-to-right like coll.zip: a null first argument propagates
        // null before the second argument's type is checked.
        let query = format!("RETURN {function}(null, 5) AS result");
        assert_eq!(eval(&query), Value::Null, "{query}");
    }
}

#[test]
fn coll_has_functions_require_two_list_arguments() {
    for function in ["coll.hasAnyOf", "coll.hasAllOf", "coll.hasNoneOf"] {
        // One or three arguments is an arity error, mirroring coll.zip's
        // arity guard.
        for args in ["[1, 2, 3, 4]", "[1, 2, 3, 4], [1], [5]"] {
            let query = format!("RETURN {function}({args}) AS result");
            assert!(
                eval_err(&query).contains("InvalidNumberOfArguments"),
                "{query}: expected an InvalidNumberOfArguments error"
            );
        }
        // A non-list argument is the coll.* family's type error in either
        // position.
        for args in ["[1, 2, 3, 4], 1", "1, [1, 2, 3, 4]"] {
            let query = format!("RETURN {function}({args}) AS result");
            assert!(
                eval_err(&query).contains(&format!("type error: {function}() requires a list")),
                "{query}: expected the coll.* family's type error"
            );
        }
    }
}

#[test]
fn coll_zip_and_has_functions_appear_in_show_functions() {
    let mut conn = connection();
    let mut names: Vec<String> = rows(
        &mut conn,
        "SHOW FUNCTIONS YIELD name WHERE name IN ['coll.zip', 'coll.hasanyof', \
         'coll.hasallof', 'coll.hasnoneof'] RETURN name",
    )
    .into_iter()
    .flatten()
    .map(|name| {
        name.as_string()
            .expect("SHOW FUNCTIONS name is a string")
            .to_ascii_lowercase()
    })
    .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            "coll.hasallof",
            "coll.hasanyof",
            "coll.hasnoneof",
            "coll.zip"
        ],
        "every new function must be advertised by SHOW FUNCTIONS"
    );
}
