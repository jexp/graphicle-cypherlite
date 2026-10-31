//! List quantifiers (`all`/`any`/`none`/`single`) and reduce expressions.

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Expr, QuantifierKind};

use crate::ctx::EvalCtx;

use super::{bool_or_null, eval, fold_quantifier, into_items};

pub(crate) fn eval_quantifier(
    ctx: EvalCtx<'_>,
    kind: QuantifierKind,
    ident: &str,
    list: &Expr,
    predicate: &Expr,
) -> Result<Value> {
    let EvalCtx {
        graph,
        row,
        params,
        cypher_version,
    } = ctx;
    let list_v = eval(graph, row, list, params, cypher_version)?;
    let items = match list_v {
        Value::List(xs) => xs,
        Value::Null => return Ok(Value::Null),
        _ => return Err(Error::ty("quantifier needs a list")),
    };
    fold_quantifier(kind, into_items(items), |item| {
        let mut r = row.clone();
        r.insert(ident.to_string(), item);
        let p = eval(graph, &r, predicate, params, cypher_version)?;
        bool_or_null(&p, "quantifier WHERE")
    })
}

pub(crate) fn eval_reduce(
    ctx: EvalCtx<'_>,
    accumulator: &str,
    initial: &Expr,
    ident: &str,
    list: &Expr,
    expression: &Expr,
) -> Result<Value> {
    let EvalCtx {
        graph,
        row,
        params,
        cypher_version,
    } = ctx;
    let mut acc = eval(graph, row, initial, params, cypher_version)?;
    let items = match eval(graph, row, list, params, cypher_version)? {
        Value::List(items) => items,
        Value::Null => return Ok(Value::Null),
        _ => return Err(Error::ty("reduce() requires a list")),
    };
    let mut inner = row.clone();
    for item in into_items(items) {
        inner.insert(accumulator.to_string(), acc);
        inner.insert(ident.to_string(), item);
        acc = eval(graph, &inner, expression, params, cypher_version)?;
    }
    Ok(acc)
}

pub(crate) fn eval_all_reduce(
    ctx: EvalCtx<'_>,
    accumulator: &str,
    initial: &Expr,
    ident: &str,
    list: &Expr,
    expression: &Expr,
    predicate: &Expr,
) -> Result<Value> {
    let EvalCtx {
        graph,
        row,
        params,
        cypher_version,
    } = ctx;
    let mut acc = eval(graph, row, initial, params, cypher_version)?;
    let started_as_integer = matches!(acc, Value::Int(_));
    let items = match eval(graph, row, list, params, cypher_version)? {
        Value::List(items) => items,
        Value::Null => return Ok(Value::Null),
        _ => return Err(Error::ty("allReduce() requires a list")),
    };
    let mut saw_null = false;
    let mut inner = row.clone();
    for item in into_items(items) {
        inner.insert(accumulator.to_string(), acc.clone());
        inner.insert(ident.to_string(), item);
        let next = eval(graph, &inner, expression, params, cypher_version)?;
        if started_as_integer && matches!(next, Value::Float(_)) {
            return Err(Error::ty(
                "Invalid input 'FLOAT' for accumulator. Expected to be INTEGER.",
            ));
        }
        acc = next;
        if matches!(acc, Value::Float(f) if !f.is_finite()) {
            return Err(Error::ty("DivisionByZero"));
        }
        inner.insert(accumulator.to_string(), acc.clone());
        match eval(graph, &inner, predicate, params, cypher_version)? {
            Value::Bool(false) => return Ok(Value::Bool(false)),
            Value::Bool(true) => {}
            Value::Null => saw_null = true,
            _ => return Err(Error::ty("allReduce() predicate must be boolean")),
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Bool(true)
    })
}

#[cfg(test)]
mod tests {
    use cypherlite_core::Value;

    use crate::exec::test_support::exec_cypher;
    use crate::graph::Graph;

    fn single_value(cypher: &str) -> Value {
        let rows = exec_cypher(&Graph::default(), cypher).rows;
        assert_eq!(rows.len(), 1, "{cypher}: {rows:?}");
        rows[0][0].clone()
    }

    /// The deciding element ends the scan: the `1 / 0` element after it is
    /// never evaluated, and the list bound in the row stays readable.
    #[test]
    fn quantifiers_stop_at_the_deciding_element() {
        for (cypher, expected) in [
            (
                "WITH [1, 0] AS xs RETURN any(x IN xs WHERE 1 / x = 1)",
                true,
            ),
            (
                "WITH [1, 0] AS xs RETURN all(x IN xs WHERE 1 / x = 2)",
                false,
            ),
            (
                "WITH [1, 0] AS xs RETURN none(x IN xs WHERE 1 / x = 1)",
                false,
            ),
            (
                "WITH [1, 1, 0] AS xs RETURN single(x IN xs WHERE 1 / x = 1)",
                false,
            ),
            (
                "WITH [1, 0] AS xs \
                 RETURN allReduce(acc = 0, x IN xs | acc + 1 / x, acc > 5)",
                false,
            ),
        ] {
            assert_eq!(single_value(cypher), Value::Bool(expected), "{cypher}");
        }
        let rows = exec_cypher(
            &Graph::default(),
            "WITH [1, 2, 3] AS xs RETURN any(x IN xs WHERE x = 1) AS hit, xs",
        )
        .rows;
        assert_eq!(rows[0][0], Value::Bool(true));
        assert_eq!(
            rows[0][1],
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
        );
    }

    /// A full scan still sees every element in order, including nulls.
    #[test]
    fn quantifiers_scan_every_element_without_a_decision() {
        for (cypher, expected) in [
            (
                "WITH [1, null, 3] AS xs RETURN any(x IN xs WHERE x > 5)",
                Value::Null,
            ),
            (
                "WITH [1, 2, 3] AS xs RETURN all(x IN xs WHERE x > 0)",
                Value::Bool(true),
            ),
            (
                "WITH [1, 2, 3] AS xs RETURN single(x IN xs WHERE x = 2)",
                Value::Bool(true),
            ),
            (
                "WITH [1, 2, 3] AS xs \
                 RETURN allReduce(acc = 0, x IN xs | acc + x, acc < 10)",
                Value::Bool(true),
            ),
        ] {
            assert_eq!(single_value(cypher), expected, "{cypher}");
        }
    }
}
