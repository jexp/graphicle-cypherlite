//! `trim(...)` parsing: the spec-word fallback path (a `both`/`leading`/
//! `trailing` name with no FROM is a plain 1-arg trim over a variable) and
//! the complexity regression guard for deeply nested trim calls. Public
//! parser API only.

use cypherlite_parser::{parse, Clause, Expr};
use cypherlite_test_support::run_with_big_stack;

fn parse_deeply_nested_trim() -> std::time::Duration {
    let depth = 64;
    let mut q = String::from("RETURN ");
    for _ in 0..depth {
        q.push_str("trim(");
    }
    q.push('x');
    for _ in 0..depth {
        q.push(')');
    }
    let start = std::time::Instant::now();
    let stmt = parse(&q).unwrap();
    let elapsed = start.elapsed();
    // Sanity: the outermost call is a 1-arg plain trim.
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected return");
    };
    match &r.items[0].expr {
        Expr::FunctionCall(fc) => {
            assert_eq!(fc.name, "trim");
            assert_eq!(fc.args.len(), 1);
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
    elapsed
}

#[test]
fn test_parse_trim_deeply_nested_is_not_exponential() {
    // Complexity regression guard (LABS-81): parse_trim_call used to
    // speculatively parse the trim-character-string expression and then
    // restore+re-parse it whenever no FROM followed, so nested
    // trim(trim(...)) cost T(k) = 2·T(k-1) — k=28 took ~80 s. The parse
    // must now be linear; 64 deep was effectively infinite before.
    //
    // Run on the shared big-stack harness: the recursive-descent parse of
    // 64 nested calls overflows the default 2 MiB test-worker stack in
    // debug builds.
    let elapsed = run_with_big_stack(parse_deeply_nested_trim);
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "64-deep nested trim() parse took {elapsed:?} (exponential backtracking?)"
    );
}

#[test]
fn test_parse_trim_variable_named_like_spec_word() {
    // `both`/`leading`/`trailing` are not reserved keywords: with no FROM
    // present they must parse as ordinary variables, i.e. trim(both) is
    // the plain 1-arg form over a variable named `both`. This is the
    // fallback path that used to restore+re-parse the spec word.
    for name in ["both", "leading", "trailing", "BOTH", "Leading"] {
        let stmt = parse(&format!("RETURN trim({name})")).unwrap();
        let Clause::Return(r) = &stmt.clauses[0] else {
            panic!("expected return for trim({name})");
        };
        match &r.items[0].expr {
            Expr::FunctionCall(fc) => {
                assert_eq!(fc.name, "trim", "trim({name})");
                assert_eq!(fc.args.len(), 1, "trim({name})");
                assert!(
                    matches!(&fc.args[0], Expr::Variable(v) if v == name),
                    "trim({name}) arg should be Variable({name}), got {:?}",
                    fc.args[0]
                );
            }
            other => panic!("expected FunctionCall for trim({name}), got {other:?}"),
        }
    }
    // The spec word as the head of a larger expression must also stay
    // plain-trim: trim(both - 1) trims the value of `both - 1`.
    let stmt = parse("RETURN trim(both - 1)").unwrap();
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected return");
    };
    match &r.items[0].expr {
        Expr::FunctionCall(fc) => {
            assert_eq!(fc.name, "trim");
            assert_eq!(fc.args.len(), 1);
            assert!(
                matches!(&fc.args[0], Expr::Binary(b) if b.op == "-"),
                "trim(both - 1) arg should be a subtraction, got {:?}",
                fc.args[0]
            );
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}
