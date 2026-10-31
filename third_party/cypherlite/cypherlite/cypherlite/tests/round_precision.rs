//! ADR-0336: `round()` bounds its precision to 0..=15; a larger precision
//! fails with the same NumericValueOutOfRange treatment as a negative one
//! instead of returning NaN at large precisions.

mod common;

use cypherlite::{Error, Value};

use common::connection;

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
