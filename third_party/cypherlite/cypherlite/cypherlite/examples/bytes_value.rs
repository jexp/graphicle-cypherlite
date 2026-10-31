//! The statement-scoped byte array (ADR-0327): a `Value::Bytes` parameter
//! comes back unchanged inside lists and maps and stamps the `BINARY`
//! column type, but cannot be stored as a property.
//!
//! Run with `cargo run -p cypherlite --example bytes_value`.

use std::collections::BTreeMap;

use cypherlite::{Database, Error, Result, Value};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    let blob = Value::Bytes(vec![0x00, 0x01, 0xFF].into());
    let params = BTreeMap::from([("b".to_string(), blob.clone())]);

    let result = conn.query_with_params("RETURN $b AS b, [$b] AS l", params.clone())?;
    assert_eq!(result.rows[0][0], blob);
    assert_eq!(result.rows[0][1], Value::List(vec![blob.clone()].into()));
    assert_eq!(result.header.column_types[0], "BINARY");
    println!("round trip ok: {}", result.rows[0][0]);

    match conn.query_with_params("CREATE (:Blob {b: $b})", params) {
        Err(Error::TypeError(detail)) => println!("property rejected: {detail}"),
        other => panic!("storing a byte array must be a TypeError, got {other:?}"),
    }
    let count = conn.query("MATCH (n:Blob) RETURN count(n) AS c")?;
    assert_eq!(count.rows[0][0], Value::Int(0));
    Ok(())
}
