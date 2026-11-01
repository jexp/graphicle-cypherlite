//! `round()` semantics: ADR-0336 bounds its precision to 0..=15 (a larger
//! precision fails with the same NumericValueOutOfRange treatment as a
//! negative one instead of returning NaN), and the implicit mode is HALF_UP
//! toward positive infinity rather than the wire `HALF_UP` (away from zero).

mod common;

use cypherlite::{Error, Value};

use common::{connection, row};

#[test]
fn round_precision_bounds_are_typed_errors() {
    let mut conn = connection();
    assert_eq!(
        conn.query("RETURN round(2.5, 400) AS r").unwrap_err(),
        Error::execution("NumericValueOutOfRange: round precision must be at most 15")
    );
    assert_eq!(
        conn.query("RETURN round(2.5, -1) AS r").unwrap_err(),
        Error::execution("NumericValueOutOfRange: round precision must be non-negative")
    );
    assert_eq!(
        conn.query("RETURN round(2.5, 2) AS r").unwrap().rows,
        vec![vec![Value::Float(2.5)]]
    );
}

// The implicit `round()` mode is HALF_UP toward positive infinity — not the
// wire `HALF_UP` (away from zero) — and values past 2^52 are the identity.
#[test]
fn round_implicit_mode_and_large_magnitudes() {
    let mut c = connection();
    assert_eq!(
        row(
            &mut c,
            "RETURN round(-1.5) AS a, round(-1.5, 0, 'HALF_UP') AS b, \
                    round(-0.5) AS c, round(0.5) AS d, round(1.5) AS e",
        ),
        vec![
            Value::Float(-1.0),
            Value::Float(-2.0),
            Value::Float(0.0),
            Value::Float(1.0),
            Value::Float(2.0)
        ]
    );
    // Beyond 2^52 there is no fractional part to round: scaling by the
    // precision used to inject ulp noise (sqrt(1e308) with precision 5
    // came back as 1.0000000000000002e+154).
    assert_eq!(
        row(&mut c, "RETURN round(sqrt(1e308), 5) AS a")[0],
        Value::Float(1e308_f64.sqrt())
    );
    assert_eq!(
        row(&mut c, "RETURN round(1e308) AS a")[0],
        Value::Float(1e308)
    );
    assert_eq!(
        row(&mut c, "RETURN round(1.5) = round(1.5, 0) AS same")[0],
        Value::Bool(true)
    );
}
