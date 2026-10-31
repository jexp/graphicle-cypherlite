use super::super::function;
use super::string::{parse_decimal_integer, DecimalInteger};
use super::*;
use crate::eval::Row;
use crate::graph::Graph;
use cypherlite_ir::Expr;
use std::collections::BTreeMap;

// CIP-271: the LIST<ANY>-only acceptance rule, emulated dynamically.
// The rendering itself is covered by to_display_string tests in
// cypherlite-core.
#[test]
fn list_tostring_acceptance_matches_neo4j_2026_09() {
    let s = |v: &str| Value::String(v.into());
    // Heterogeneous lists are accepted.
    assert!(list_tostring_accepted(&[Value::Int(1), s("a")]));
    assert!(list_tostring_accepted(&[
        Value::List(vec![Value::Int(1)].into()),
        s("a")
    ]));
    // Any null makes the list LIST<ANY>-ish: accepted.
    assert!(list_tostring_accepted(&[Value::Bool(true), Value::Null]));
    assert!(list_tostring_accepted(&[Value::Null]));
    assert!(list_tostring_accepted(&[]));
    // Homogeneous lists are rejected; numbers are one class.
    assert!(!list_tostring_accepted(&[Value::Int(1), Value::Int(2)]));
    assert!(!list_tostring_accepted(&[Value::Int(1), Value::Float(2.0)]));
    assert!(!list_tostring_accepted(&[s("a"), s("b")]));
    assert!(!list_tostring_accepted(&[Value::List(
        vec![Value::Int(1)].into()
    )]));
}

#[test]
fn to_boolean_or_null_and_to_string_or_null_match_neo4j() {
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    let row = Row::new();
    let params = BTreeMap::new();
    let call =
        |name: &str, arg: Expr| function(&graph, &row, &params, name, false, &[arg], None).unwrap();

    // success passthrough
    assert_eq!(
        call("toBooleanOrNull", Expr::String("true".into())),
        Value::Bool(true)
    );
    assert_eq!(
        call("toBooleanOrNull", Expr::Bool(false)),
        Value::Bool(false)
    );
    // parse failure and null stay null
    assert_eq!(
        call("toBooleanOrNull", Expr::String("nope".into())),
        Value::Null
    );
    assert_eq!(call("toBooleanOrNull", Expr::Null), Value::Null);
    assert_eq!(
        call("toStringOrNull", Expr::Int(7)),
        Value::String("7".into())
    );
    assert_eq!(call("toStringOrNull", Expr::Null), Value::Null);
    // non-convertible input: null instead of the base functions' error
    assert_eq!(
        call(
            "toStringOrNull",
            Expr::List(vec![Expr::Int(1), Expr::Int(2)])
        ),
        Value::Null
    );
    // heterogeneous list still renders (CIP-271), through both spellings
    let mixed = Expr::List(vec![Expr::Int(1), Expr::String("a".into())]);
    assert!(matches!(call("toString", mixed.clone()), Value::String(_)));
    assert!(matches!(call("toStringOrNull", mixed), Value::String(_)));
}

#[test]
fn to_integer_or_null_and_to_float_or_null_match_neo4j() {
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    let row = Row::new();
    let params = BTreeMap::new();
    let call =
        |name: &str, arg: Expr| function(&graph, &row, &params, name, false, &[arg], None).unwrap();

    // Success cases behave exactly like toInteger()/toFloat().
    assert_eq!(
        call("toIntegerOrNull", Expr::String("42".into())),
        Value::Int(42)
    );
    assert_eq!(
        call("toIntegerOrNull", Expr::String("3.5".into())),
        Value::Int(3)
    );
    assert_eq!(call("toIntegerOrNull", Expr::Int(7)), Value::Int(7));
    assert_eq!(call("toIntegerOrNull", Expr::Float(2.9)), Value::Int(2));
    assert_eq!(call("toIntegerOrNull", Expr::Null), Value::Null);

    assert_eq!(
        call("toFloatOrNull", Expr::String("3.5".into())),
        Value::Float(3.5)
    );
    assert_eq!(
        call("toFloatOrNull", Expr::String("42".into())),
        Value::Float(42.0)
    );
    assert_eq!(call("toFloatOrNull", Expr::Int(3)), Value::Float(3.0));
    assert_eq!(call("toFloatOrNull", Expr::Float(1.25)), Value::Float(1.25));
    assert_eq!(call("toFloatOrNull", Expr::Null), Value::Null);

    // Parse failures and non-convertible inputs yield null, never an error.
    assert_eq!(
        call("toIntegerOrNull", Expr::String("abc".into())),
        Value::Null
    );
    assert_eq!(
        call("toFloatOrNull", Expr::String("nope".into())),
        Value::Null
    );
    assert_eq!(call("toIntegerOrNull", Expr::Bool(true)), Value::Null);
    assert_eq!(call("toFloatOrNull", Expr::Bool(true)), Value::Null);
    assert_eq!(
        call("toIntegerOrNull", Expr::List(vec![Expr::Int(1)])),
        Value::Null
    );
    assert_eq!(
        call("toFloatOrNull", Expr::List(vec![Expr::Int(1)])),
        Value::Null
    );

    // The base functions keep erroring on the same non-convertible input.
    assert!(function(
        &graph,
        &row,
        &params,
        "toInteger",
        false,
        &[Expr::Bool(true)],
        None
    )
    .is_err());
    assert!(function(
        &graph,
        &row,
        &params,
        "toFloat",
        false,
        &[Expr::Bool(true)],
        None
    )
    .is_err());
}

/// The expected outcome of one conversion probe.
enum Want {
    Is(Value),
    OutOfRange,
}

const NULL: Want = Want::Is(Value::Null);
const OUT: Want = Want::OutOfRange;
const OUT_OF_RANGE: &str = "type error: NumberOutOfRange: toInteger()";

fn int(i: i64) -> Want {
    Want::Is(Value::Int(i))
}

fn float(f: f64) -> Want {
    Want::Is(Value::Float(f))
}

fn ints(xs: &[i64]) -> Want {
    Want::Is(Value::List(
        xs.iter()
            .copied()
            .map(Value::Int)
            .collect::<Vec<_>>()
            .into(),
    ))
}

fn list(xs: Vec<Value>) -> Want {
    Want::Is(Value::List(xs.into()))
}

/// Exact equality, except that NaN matches NaN.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::List(xs), Value::List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| same(x, y))
        }
        _ => a == b,
    }
}

/// Check every `(cypher, want)` row: `cypher` is a whole expression, as
/// the ADR's tables write it, parsed by the product parser, lowered by
/// the product front-end and evaluated against an empty graph and row.
fn check_probes(rows: Vec<(&str, Want)>) {
    let g = Graph::default();
    let graph = ExecGraph::read(&g);
    for (cypher, want) in rows {
        let parsed = cypherlite_parser::parser::parse_expression_str(cypher).expect(cypher);
        let expr = cypherlite_front::lower_expr(&parsed).expect(cypher);
        let got = crate::eval::eval(&graph, &Row::new(), &expr, &BTreeMap::new(), None);
        match (want, got) {
            (Want::Is(want), Ok(got)) => {
                assert!(same(&got, &want), "{cypher}: got {got:?}, want {want:?}")
            }
            (Want::OutOfRange, Err(e)) => assert!(
                e.to_string().starts_with(OUT_OF_RANGE),
                "{cypher}: got error {e}, want {OUT_OF_RANGE}"
            ),
            (Want::Is(want), Err(e)) => panic!("{cypher}: got error {e}, want {want:?}"),
            (Want::OutOfRange, Ok(got)) => {
                panic!("{cypher}: got {got:?}, want error {OUT_OF_RANGE}")
            }
        }
    }
}

// ADR-0168 "Probe results", verified against neo4j:2026.09-community:
// every row's "CypherLite after" cell. Floats keep Java's `(long)` cast
// (NaN → 0, saturate at the INTEGER limits, truncate toward zero),
// out-of-range numeric strings raise NumberOutOfRange, toIntegerList()
// keeps INTEGER elements exact, and toFloat() accepts only Java's
// non-finite spellings.
#[test]
fn conversions_match_adr_0168_probe_table() {
    let (max, min) = (i64::MAX, i64::MIN);
    let (nan, inf, ninf) = (f64::NAN, f64::INFINITY, f64::NEG_INFINITY);
    check_probes(vec![
        ("toInteger(0.0/0.0)", int(0)),
        ("toInteger(1.0/0.0)", int(max)),
        ("toInteger(-1.0/0.0)", int(min)),
        ("toInteger(1e300)", int(max)),
        ("toInteger(-1e300)", int(min)),
        ("toInteger(9.3e18)", int(max)),
        ("toInteger(-9.3e18)", int(min)),
        ("toInteger(9.2e18)", int(9_200_000_000_000_000_000)),
        ("toInteger('NaN')", NULL),
        ("toInteger('nan')", NULL),
        ("toInteger('Infinity')", NULL),
        ("toInteger('-Infinity')", NULL),
        ("toInteger('inf')", NULL),
        ("toInteger('1e300')", OUT),
        ("toInteger('9.3e18')", OUT),
        ("toIntegerOrNull(0.0/0.0)", int(0)),
        ("toIntegerOrNull(1.0/0.0)", int(max)),
        ("toIntegerOrNull(-1.0/0.0)", int(min)),
        ("toIntegerOrNull(1e300)", int(max)),
        ("toIntegerOrNull('NaN')", NULL),
        ("toIntegerOrNull('1e300')", NULL),
        ("toIntegerList([0.0/0.0])", ints(&[0])),
        ("toIntegerList([1.0/0.0])", ints(&[max])),
        ("toIntegerList([-1.0/0.0])", ints(&[min])),
        ("toIntegerList([1e300])", ints(&[max])),
        (
            "toIntegerList([2.5, 9007199254740993])",
            ints(&[2, 9_007_199_254_740_993]),
        ),
        ("toFloat('NaN')", float(nan)),
        ("toFloat('-NaN')", float(nan)),
        ("toFloat('nan')", NULL),
        ("toFloat('Infinity')", float(inf)),
        ("toFloat('-Infinity')", float(ninf)),
        ("toFloat('+Infinity')", float(inf)),
        ("toFloat('infinity')", NULL),
        ("toFloat('inf')", NULL),
        ("toFloat('Inf')", NULL),
        ("toFloat('-inf')", NULL),
        ("toFloat('1e400')", float(inf)),
        ("toFloat(0.0/0.0)", float(nan)),
        ("toFloat(1.0/0.0)", float(inf)),
        ("toFloatOrNull('NaN')", float(nan)),
        ("toFloatOrNull('nan')", NULL),
        ("toFloatOrNull('Infinity')", float(inf)),
        ("toFloatOrNull('inf')", NULL),
        (
            "toFloatList([0.0/0.0, 1.0/0.0])",
            Want::Is(Value::List(
                vec![Value::Float(nan), Value::Float(inf)].into(),
            )),
        ),
    ]);
}

// ADR-0168 "Supplementary probes", verified against
// neo4j:2026.09-community: the boundaries of the exact-decimal string
// rule, INTEGER list elements versus VECTOR coordinates, and the
// toFloat() spellings. `toInteger('1e3000000000')` was ADR-0168's
// deliberate deviation (Neo4j saturates, CypherLite errored); ADR-0175
// supersedes it — the giant exponent diverts to the Cypher25-literal
// fallback, so the value saturates like Neo4j.
#[test]
fn conversions_match_adr_0168_supplementary_probes() {
    let (max, min) = (i64::MAX, i64::MIN);
    check_probes(vec![
        ("toInteger('-1e300')", OUT),
        ("toInteger('-9.3e18')", OUT),
        ("toInteger('1e400')", OUT),
        ("toInteger('-1e400')", OUT),
        ("toInteger(' 1e300 ')", OUT),
        ("toIntegerOrNull('1e400')", NULL),
        ("toIntegerOrNull('9.3e18')", NULL),
        ("toInteger('9223372036854775807')", int(max)),
        ("toInteger('9223372036854775808')", OUT),
        ("toIntegerOrNull('9223372036854775808')", NULL),
        ("toInteger('-9223372036854775808')", int(min)),
        ("toInteger('-9223372036854775809')", OUT),
        ("toInteger('9223372036854775807.0')", int(max)),
        ("toInteger('9223372036854775807.5')", OUT),
        (
            "toInteger('9223372036854775806.5')",
            int(9_223_372_036_854_775_806),
        ),
        ("toInteger('-9223372036854775808.0')", int(min)),
        ("toInteger('-9223372036854775808.5')", OUT),
        (
            "toInteger('9007199254740993.5')",
            int(9_007_199_254_740_993),
        ),
        ("toInteger('9.2e18')", int(9_200_000_000_000_000_000)),
        ("toInteger('-0.5')", int(0)),
        ("toInteger('1e-400')", int(0)),
        // ADR-0175 supersedes ADR-0168's deliberate deviation here: the
        // giant exponent overflows the BigDecimal path's int32 gate and
        // the literal fallback's float parse saturates, as in Neo4j.
        ("toInteger('1e3000000000')", int(max)),
        ("toInteger('+Infinity')", NULL),
        ("toInteger('-NaN')", NULL),
        (
            "toIntegerList([9223372036854775806])",
            ints(&[9_223_372_036_854_775_806]),
        ),
        (
            "toIntegerList([-9223372036854775807])",
            ints(&[-9_223_372_036_854_775_807]),
        ),
        ("toIntegerList([9.3e18, -1e300])", ints(&[max, min])),
        (
            "toIntegerList([9223372036854775807, -9223372036854775808])",
            ints(&[max, min]),
        ),
        ("toFloat('+NaN')", float(f64::NAN)),
        ("toFloat('NAN')", NULL),
        ("toFloat('INFINITY')", NULL),
        ("toFloat('+inf')", NULL),
        ("toFloat('-1e400')", float(f64::NEG_INFINITY)),
        ("toFloatOrNull('-Infinity')", float(f64::NEG_INFINITY)),
        ("toFloatOrNull('Inf')", NULL),
        // VECTOR coordinates convert through FLOAT, so an INTEGER64
        // coordinate above 2^53 rounds, as in Neo4j.
        (
            "toIntegerList(vector([9007199254740993, 2], 2, INTEGER64))",
            ints(&[9_007_199_254_740_992, 2]),
        ),
        (
            "toIntegerList(vector([2.5, 1e300], 2, FLOAT64))",
            ints(&[2, max]),
        ),
    ]);
}

// ADR-0168: the exact-decimal grammar `toInteger()` applies to a trimmed
// string. TCK TypeConversion2's decimal rows ('2.9' → 2, '1.7' → 1) and
// every form Rust's f64 parser accepted keep converting; the
// non-decimal forms stay non-numeric. ADR-0175: an exponent beyond the
// i32 range makes the BigDecimal-equivalent path report `None` (the
// string diverts to the Cypher25-literal fallback, whose float parse
// still yields 0 for `0e…`/`5e-…` and saturates to i64::MAX for
// `5e…` — pinned end-to-end by the '1e3000000000' probe rows).
#[test]
fn decimal_integer_parse_is_exact_and_rejects_non_decimals() {
    use DecimalInteger::{InRange, OutOfRange};
    for (input, want) in [
        ("2.9", InRange(2)),
        ("1.7", InRange(1)),
        ("-2.9", InRange(-2)),
        ("+42", InRange(42)),
        ("007", InRange(7)),
        ("7.", InRange(7)),
        (".5", InRange(0)),
        ("-.5", InRange(0)),
        ("1E3", InRange(1000)),
        ("1e+3", InRange(1000)),
        ("1234e-2", InRange(12)),
        ("0.000123e7", InRange(1230)),
        ("10000000000000000000", OutOfRange),
        ("-9223372036854775808e0", InRange(i64::MIN)),
        ("922337203685477580.75e1", OutOfRange),
        ("922337203685477580.70e1", InRange(i64::MAX)),
    ] {
        assert_eq!(parse_decimal_integer(input), Some(want), "{input}");
    }
    for input in [
        "", "+", "-", ".", "e5", "1e", "1e+", "1.2.3", "1e5e3", "--1", "+-1", "0x10", "1_000",
        "1.5d", " 1", "NaN", "Infinity", "inf", "nan",
    ] {
        assert_eq!(parse_decimal_integer(input), None, "{input:?}");
    }
    // An exponent's digits are checked even where its value overflows.
    for input in ["1e99999999999999999999x", "1e-99999999999999999999x"] {
        assert_eq!(parse_decimal_integer(input), None, "{input:?}");
    }
    // ADR-0175's int32-exponent gate: beyond it, BigDecimal throws and
    // the string is the literal fallback's, not this function's.
    for input in [
        "1e3000000000",
        "-1e3000000000",
        "1e-3000000000",
        "1e2147483648",
        "0e99999999999999999999999",
        "5e-99999999999999999999999",
        "5e99999999999999999999999",
    ] {
        assert_eq!(parse_decimal_integer(input), None, "{input:?}");
    }
    // The gate's boundary: i32::MAX stays on the exact-decimal path.
    assert_eq!(
        parse_decimal_integer("1e2147483647"),
        Some(OutOfRange),
        "1e2147483647"
    );
}

// ADR-0175 "Probe results", list-elements and string-forms groups,
// verified against neo4j:2026.09-community: every row's "CypherLite
// after" cell. String elements of the list conversions parse with the
// scalar OrNull string rules and never raise (an out-of-range numeric
// string becomes a NULL element); boolean elements are 1/0 in
// toIntegerList() and null in toFloatList(). The string forms follow
// Neo4j's pipeline: <= U+0020 trim, parseLong / parseDouble (hex
// floats, f/F/d/D suffixes, exact-case NaN/Infinity), the exact-decimal
// parse with the int32-exponent gate, then the Cypher25-literal
// fallback (Unicode trim, lowercase 0x/0o, underscores, suffixed
// decimal doubles).
#[test]
fn conversions_match_adr_0175_list_elements_and_string_forms() {
    let (max, min) = (i64::MAX, i64::MIN);
    let (nan, inf, ninf) = (f64::NAN, f64::INFINITY, f64::NEG_INFINITY);
    check_probes(vec![
        // --- list-elements ---
        (
            "toIntegerList(['1', 'NaN'])",
            list(vec![Value::Int(1), Value::Null]),
        ),
        (
            "toFloatList(['1.5', 'NaN'])",
            list(vec![Value::Float(1.5), Value::Float(nan)]),
        ),
        ("toIntegerList(['1e300'])", list(vec![Value::Null])),
        (
            "toIntegerList(['9223372036854775808', 'x', 1.5])",
            list(vec![Value::Null, Value::Null, Value::Int(1)]),
        ),
        (
            "toIntegerList([' 7 ', '2.9', '-0.5'])",
            list(vec![Value::Int(7), Value::Int(2), Value::Int(0)]),
        ),
        (
            "toFloatList(['Infinity', 'inf', ' 1.5 ', '1e400'])",
            list(vec![
                Value::Float(inf),
                Value::Null,
                Value::Float(1.5),
                Value::Float(inf),
            ]),
        ),
        (
            "toIntegerList(['0x1F', '1_000', '1.5f'])",
            list(vec![Value::Int(31), Value::Int(1000), Value::Int(1)]),
        ),
        (
            "toFloatList(['0x1p3', '1.5d'])",
            list(vec![Value::Float(8.0), Value::Float(1.5)]),
        ),
        (
            "toIntegerList([true, 'true'])",
            list(vec![Value::Int(1), Value::Null]),
        ),
        (
            "toFloatList([true, 'true'])",
            list(vec![Value::Null, Value::Null]),
        ),
        (
            "toIntegerList(['1e3000000000'])",
            list(vec![Value::Int(max)]),
        ),
        ("toIntegerList(['1e400'])", list(vec![Value::Null])),
        (
            "toFloatList(['1e3000000000'])",
            list(vec![Value::Float(inf)]),
        ),
        (
            "toIntegerList(['9223372036854775807.5'])",
            list(vec![Value::Null]),
        ),
        (
            "toIntegerList(['9223372036854775807.0'])",
            list(vec![Value::Int(max)]),
        ),
        (
            "toFloatList(['0x1F', '1_000', '0o17'])",
            list(vec![
                Value::Float(31.0),
                Value::Float(1000.0),
                Value::Float(15.0),
            ]),
        ),
        (
            "toIntegerList(['nan', 'inf', '-Infinity'])",
            list(vec![Value::Null, Value::Null, Value::Null]),
        ),
        (
            "toFloatList(['nan', 'inf', '-Infinity'])",
            list(vec![Value::Null, Value::Null, Value::Float(ninf)]),
        ),
        (
            "toIntegerList([null, '42'])",
            list(vec![Value::Null, Value::Int(42)]),
        ),
        (
            "toFloatList([null, '42'])",
            list(vec![Value::Null, Value::Float(42.0)]),
        ),
        (
            "toIntegerList(['+1', '-0', '0001'])",
            list(vec![Value::Int(1), Value::Int(0), Value::Int(1)]),
        ),
        (
            "toFloatList(['.5', '5.', '+.5', '1e3'])",
            list(vec![
                Value::Float(0.5),
                Value::Float(5.0),
                Value::Float(0.5),
                Value::Float(1000.0),
            ]),
        ),
        (
            "toIntegerList(['.5', '5.', '1e3'])",
            list(vec![Value::Int(0), Value::Int(5), Value::Int(1000)]),
        ),
        (
            "toIntegerList([true, false])",
            list(vec![Value::Int(1), Value::Int(0)]),
        ),
        (
            "toFloatList([true, false])",
            list(vec![Value::Null, Value::Null]),
        ),
        ("toIntegerList(['\\t7\\n'])", list(vec![Value::Int(7)])),
        ("toIntegerList([' 0x1F '])", list(vec![Value::Int(31)])),
        ("toFloatList([' 0x1p3 '])", list(vec![Value::Float(8.0)])),
        (
            "toIntegerList(['2L', '2f'])",
            list(vec![Value::Null, Value::Null]),
        ),
        (
            "toFloatList(['2L', '2f'])",
            list(vec![Value::Null, Value::Float(2.0)]),
        ),
        // --- string-forms ---
        ("toInteger(' 1.5 ')", int(1)),
        ("toFloat(' 1.5 ')", float(1.5)),
        ("toIntegerOrNull(' 1.5 ')", int(1)),
        ("toFloatOrNull(' 1.5 ')", float(1.5)),
        ("toInteger('1.5f')", int(1)),
        ("toFloat('1.5f')", float(1.5)),
        ("toIntegerOrNull('1.5f')", int(1)),
        ("toFloatOrNull('1.5f')", float(1.5)),
        ("toInteger('1.5F')", int(1)),
        ("toFloat('1.5F')", float(1.5)),
        ("toIntegerOrNull('1.5F')", int(1)),
        ("toFloatOrNull('1.5F')", float(1.5)),
        ("toInteger('1.5d')", int(1)),
        ("toFloat('1.5d')", float(1.5)),
        ("toIntegerOrNull('1.5d')", int(1)),
        ("toFloatOrNull('1.5d')", float(1.5)),
        ("toInteger('1.5D')", int(1)),
        ("toFloat('1.5D')", float(1.5)),
        ("toIntegerOrNull('1.5D')", int(1)),
        ("toFloatOrNull('1.5D')", float(1.5)),
        ("toInteger('2f')", NULL),
        ("toFloat('2f')", float(2.0)),
        ("toIntegerOrNull('2f')", NULL),
        ("toFloatOrNull('2f')", float(2.0)),
        ("toInteger('2L')", NULL),
        ("toFloat('2L')", NULL),
        ("toIntegerOrNull('2L')", NULL),
        ("toFloatOrNull('2L')", NULL),
        ("toInteger('0x1F')", int(31)),
        ("toFloat('0x1F')", float(31.0)),
        ("toIntegerOrNull('0x1F')", int(31)),
        ("toFloatOrNull('0x1F')", float(31.0)),
        ("toInteger('-0x1F')", int(-31)),
        ("toFloat('-0x1F')", float(-31.0)),
        ("toIntegerOrNull('-0x1F')", int(-31)),
        ("toFloatOrNull('-0x1F')", float(-31.0)),
        ("toInteger('0o17')", int(15)),
        ("toFloat('0o17')", float(15.0)),
        ("toIntegerOrNull('0o17')", int(15)),
        ("toFloatOrNull('0o17')", float(15.0)),
        ("toInteger('017')", int(17)),
        ("toFloat('017')", float(17.0)),
        ("toIntegerOrNull('017')", int(17)),
        ("toFloatOrNull('017')", float(17.0)),
        ("toInteger('1_000')", int(1000)),
        ("toFloat('1_000')", float(1000.0)),
        ("toIntegerOrNull('1_000')", int(1000)),
        ("toFloatOrNull('1_000')", float(1000.0)),
        ("toInteger('1_000.5')", int(1000)),
        ("toFloat('1_000.5')", float(1000.5)),
        ("toIntegerOrNull('1_000.5')", int(1000)),
        ("toFloatOrNull('1_000.5')", float(1000.5)),
        ("toInteger('0x1p3')", NULL),
        ("toFloat('0x1p3')", float(8.0)),
        ("toIntegerOrNull('0x1p3')", NULL),
        ("toFloatOrNull('0x1p3')", float(8.0)),
        ("toInteger('0x1.8p1')", NULL),
        ("toFloat('0x1.8p1')", float(3.0)),
        ("toIntegerOrNull('0x1.8p1')", NULL),
        ("toFloatOrNull('0x1.8p1')", float(3.0)),
        ("toInteger('1e3000000000')", int(max)),
        ("toFloat('1e3000000000')", float(inf)),
        ("toIntegerOrNull('1e3000000000')", int(max)),
        ("toFloatOrNull('1e3000000000')", float(inf)),
        ("toInteger('-1e3000000000')", int(min)),
        ("toFloat('-1e3000000000')", float(ninf)),
        ("toIntegerOrNull('-1e3000000000')", int(min)),
        ("toFloatOrNull('-1e3000000000')", float(ninf)),
        ("toInteger('1e-3000000000')", int(0)),
        ("toFloat('1e-3000000000')", float(0.0)),
        ("toIntegerOrNull('1e-3000000000')", int(0)),
        ("toFloatOrNull('1e-3000000000')", float(0.0)),
        ("toInteger('\\t1.5\\t')", int(1)),
        ("toFloat('\\t1.5\\t')", float(1.5)),
        ("toIntegerOrNull('\\t1.5\\t')", int(1)),
        ("toFloatOrNull('\\t1.5\\t')", float(1.5)),
        ("toInteger('\\n1.5\\n')", int(1)),
        ("toFloat('\\n1.5\\n')", float(1.5)),
        ("toIntegerOrNull('\\n1.5\\n')", int(1)),
        ("toFloatOrNull('\\n1.5\\n')", float(1.5)),
        ("toInteger('1e-400')", int(0)),
        ("toFloat('1e-400')", float(0.0)),
        ("toIntegerOrNull('1e-400')", int(0)),
        ("toFloatOrNull('1e-400')", float(0.0)),
        ("toInteger('.5f')", int(0)),
        ("toFloat('.5f')", float(0.5)),
        ("toIntegerOrNull('.5f')", int(0)),
        ("toFloatOrNull('.5f')", float(0.5)),
        ("toInteger('0x')", NULL),
        ("toFloat('0x')", NULL),
        ("toInteger('0X1F')", NULL),
        ("toFloat('0X1F')", NULL),
        ("toInteger('+0x1F')", NULL),
        ("toFloat('+0x1F')", NULL),
        ("toInteger('0X1P3')", NULL),
        ("toFloat('0X1P3')", float(8.0)),
        ("toInteger('-1.5f')", int(-1)),
        ("toFloat('-1.5f')", float(-1.5)),
        ("toInteger('1.5e2f')", int(150)),
        ("toFloat('1.5e2f')", float(150.0)),
        ("toFloat(' Infinity ')", float(inf)),
        ("toFloat('NaN ')", float(nan)),
        ("toFloat('\\u00A01.5\\u00A0')", float(1.5)),
        ("toInteger('\\u00A07\\u00A0')", int(7)),
        ("toInteger('')", NULL),
        ("toFloat('')", NULL),
    ]);
}
