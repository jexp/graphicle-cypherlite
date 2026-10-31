//! SumAcc / agg_eval tests (F-1-19: the test module outgrew the
//! implementation file).

use super::aggregate::{agg_eval, desugar_star_keys};
use super::test_support::duration;
use crate::eval::Row;
use crate::graph::Graph;
use crate::overlay::ExecGraph;
use cypherlite_core::Value;
use cypherlite_ir::Expr;
use std::borrow::Cow;
use std::collections::BTreeMap;

fn group_with_xs(xs: &[i64]) -> Vec<Row> {
    xs.iter()
        .map(|i| {
            let mut row = Row::new();
            row.insert("x".into(), Value::Int(*i));
            row
        })
        .collect()
}

fn group_with_vals(vals: &[Value]) -> Vec<Row> {
    vals.iter()
        .map(|v| {
            let mut row = Row::new();
            row.insert("x".into(), v.clone());
            row
        })
        .collect()
}

fn agg_fn(name: &str) -> Expr {
    Expr::Function {
        name: name.into(),
        distinct: false,
        args: vec![Expr::Variable("x".into())],
    }
}

/// The fallible form of [`run`]: the agg_eval error propagates so tests
/// can assert it.
fn run_result(group: &[Row], expr: &Expr) -> cypherlite_core::Result<Value> {
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    agg_eval(&graph, group, expr, &BTreeMap::new(), None)
}

fn run(group: &[Row], expr: &Expr) -> Value {
    run_result(group, expr).unwrap()
}

// ADR-0205: every aggregation except count(*) skips null input values.
#[test]
fn sum_skips_nulls_and_empty_is_zero() {
    let mixed = group_with_vals(&[Value::Int(1), Value::Null, Value::Int(3)]);
    assert_eq!(run(&mixed, &agg_fn("sum")), Value::Int(4));
    let all_null = group_with_vals(&[Value::Null, Value::Null]);
    assert_eq!(run(&all_null, &agg_fn("sum")), Value::Int(0));
    assert_eq!(run(&[], &agg_fn("sum")), Value::Int(0));
}

#[test]
fn avg_min_max_skip_nulls_and_all_null_is_null() {
    let mixed = group_with_vals(&[Value::Int(1), Value::Null, Value::Int(3)]);
    assert_eq!(run(&mixed, &agg_fn("avg")), Value::Float(2.0));
    assert_eq!(run(&mixed, &agg_fn("min")), Value::Int(1));
    assert_eq!(run(&mixed, &agg_fn("max")), Value::Int(3));
    let all_null = group_with_vals(&[Value::Null]);
    assert_eq!(run(&all_null, &agg_fn("avg")), Value::Null);
    assert_eq!(run(&all_null, &agg_fn("min")), Value::Null);
    assert_eq!(run(&all_null, &agg_fn("max")), Value::Null);
}

#[test]
fn collect_ignores_nulls_and_count_splits_null_from_star() {
    let mixed = group_with_vals(&[Value::Int(1), Value::Null, Value::Int(3)]);
    assert_eq!(
        run(&mixed, &agg_fn("collect")),
        Value::List(vec![Value::Int(1), Value::Int(3)].into())
    );
    assert_eq!(run(&mixed, &agg_fn("count")), Value::Int(2));
    assert_eq!(run(&mixed, &Expr::CountStar), Value::Int(3));
}

#[test]
fn sum_and_avg_accept_durations() {
    let group = group_with_vals(&[
        duration(0, 0, 1_000_000_000),
        Value::Null,
        duration(0, 0, 3_000_000_000),
    ]);
    assert_eq!(run(&group, &agg_fn("sum")), duration(0, 0, 4_000_000_000));
    assert_eq!(run(&group, &agg_fn("avg")), duration(0, 0, 2_000_000_000));
    // Months and days accumulate component-wise.
    let group = group_with_vals(&[duration(1, 2, 0), duration(3, 4, 0)]);
    assert_eq!(run(&group, &agg_fn("sum")), duration(4, 6, 0));
}

#[test]
fn sum_and_avg_reject_mixed_numbers_and_durations() {
    let group = group_with_vals(&[Value::Int(1), duration(0, 0, 1)]);
    for name in ["sum", "avg"] {
        let err = run_result(&group, &agg_fn(name)).unwrap_err();
        assert!(
            err.to_string().contains("cannot mix numbers and durations"),
            "{name} should reject mixed input, got {err}"
        );
    }
    // Order does not matter: number first or duration first.
    let group = group_with_vals(&[duration(0, 0, 1), Value::Int(1)]);
    assert!(run_result(&group, &agg_fn("sum")).is_err());
}

#[test]
fn star_key_desugar_preserves_borrows_scope_and_empty_inputs() {
    let star = Expr::Star;
    let key = Expr::Variable("x".into());
    let rows = [Row::from([
        ("b".into(), Value::Int(2)),
        ("__agg_0".into(), Value::Int(9)),
        ("a".into(), Value::Int(1)),
    ])];
    let expanded = desugar_star_keys(vec![Cow::Borrowed(&star), Cow::Borrowed(&key)], &rows);
    assert!(matches!(&expanded[0], Cow::Owned(Expr::Variable(n)) if n == "a"));
    assert!(matches!(&expanded[1], Cow::Owned(Expr::Variable(n)) if n == "b"));
    assert!(matches!(&expanded[2], Cow::Borrowed(e) if std::ptr::eq(*e, &key)));

    let repeated = desugar_star_keys(vec![Cow::Borrowed(&star); 2], &rows);
    assert_eq!(repeated.len(), 4);
    assert_eq!(&repeated[..2], &repeated[2..]);

    let empty = desugar_star_keys(vec![Cow::Borrowed(&star)], &[]);
    assert!(matches!(empty.as_slice(), [Cow::Borrowed(Expr::Star)]));
    assert!(desugar_star_keys(vec![], &[]).is_empty());
    assert!(desugar_star_keys(vec![Cow::Borrowed(&star)], &[Row::new()]).is_empty());
    let no_star = desugar_star_keys(vec![Cow::Borrowed(&key)], &[]);
    assert!(matches!(no_star.as_slice(), [Cow::Borrowed(e)] if std::ptr::eq(*e, &key)));

    let no_star = vec![Cow::Borrowed(&key)];
    let allocation = no_star.as_ptr();
    let no_star = desugar_star_keys(no_star, &rows);
    assert_eq!(no_star.as_ptr(), allocation);
    assert!(matches!(no_star.as_slice(), [Cow::Borrowed(e)] if std::ptr::eq(*e, &key)));
}

#[test]
fn slice_of_collect_with_end() {
    // collect(x)[..1]
    let group = group_with_xs(&[1, 2, 3]);
    let expr = Expr::Slice {
        expr: Box::new(agg_fn("collect")),
        start: None,
        end: Some(Box::new(Expr::Int(1))),
    };
    assert_eq!(run(&group, &expr), Value::List(vec![Value::Int(1)].into()));
}

#[test]
fn subscript_of_collect() {
    // collect(x)[0]
    let group = group_with_xs(&[1, 2, 3]);
    let expr = Expr::Subscript {
        expr: Box::new(agg_fn("collect")),
        index: Box::new(Expr::Int(0)),
    };
    assert_eq!(run(&group, &expr), Value::Int(1));
}

#[test]
fn open_ended_slice_of_collect() {
    // collect(x)[1..]
    let group = group_with_xs(&[1, 2, 3]);
    let expr = Expr::Slice {
        expr: Box::new(agg_fn("collect")),
        start: Some(Box::new(Expr::Int(1))),
        end: None,
    };
    assert_eq!(
        run(&group, &expr),
        Value::List(vec![Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn slice_of_collect_on_empty_group() {
    // Keyless global group with zero rows: collect(x)[..1] is [].
    let expr = Expr::Slice {
        expr: Box::new(agg_fn("collect")),
        start: None,
        end: Some(Box::new(Expr::Int(1))),
    };
    assert_eq!(run(&[], &expr), Value::List(vec![].into()));
}

// ADR-0198 ruling 2: an aggregate call inside a CASE arm belongs to the
// enclosing aggregating projection.
#[test]
fn case_arm_collects_over_the_group() {
    // CASE WHEN true THEN collect(x) + [3] ELSE [] END
    let group = group_with_xs(&[1, 2]);
    let expr = Expr::Case {
        operand: None,
        when_then: vec![(
            Expr::Bool(true),
            Expr::Binary {
                op: cypherlite_ir::BinaryOp::Add,
                left: Box::new(agg_fn("collect")),
                right: Box::new(Expr::List(vec![Expr::Int(3)])),
            },
            false,
        )],
        else_expr: Some(Box::new(Expr::List(vec![]))),
    };
    assert_eq!(
        run(&group, &expr),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn case_not_taken_arm_still_belongs_to_the_projection() {
    // CASE WHEN false THEN [] ELSE collect(x) + [3] END
    let group = group_with_xs(&[1, 2]);
    let expr = Expr::Case {
        operand: None,
        when_then: vec![(Expr::Bool(false), Expr::List(vec![]), false)],
        else_expr: Some(Box::new(Expr::Binary {
            op: cypherlite_ir::BinaryOp::Add,
            left: Box::new(agg_fn("collect")),
            right: Box::new(Expr::List(vec![Expr::Int(3)])),
        })),
    };
    assert_eq!(
        run(&group, &expr),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
    );
}
