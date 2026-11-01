//! Shared `Value` payloads (ADR-0154, ADR-0170): `Value::String`,
//! `Value::List`, `Value::Map` and `Value::Vector` keep their payload behind
//! an `Arc`, so cloning a value shares the payload in O(1) and values are
//! immutable. Build payloads with `.into()` or `Arc::new(..)`; take an owned
//! payload with `Arc::unwrap_or_clone`.
//!
//! Run with `cargo run -p cypherlite --example value_sharing`.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite::{Database, Result, Value};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;

    let result = conn.query(
        "RETURN [1, 2, 3] AS list, {name: 'Alice', age: 30} AS map, 'hello' AS text, \
         vector([1.0, 2.0, 3.0], 3, FLOAT32) AS embedding",
    )?;
    let row = &result.rows[0];
    let (list, map, text, embedding) = (&row[0], &row[1], &row[2], &row[3]);

    // A clone points at the same payload: one refcount increment, no copy.
    // (`Arc::ptr_eq` shows the sharing; compare values with `==`.)
    match (list, list.clone()) {
        (Value::List(a), Value::List(b)) => assert!(Arc::ptr_eq(a, &b)),
        other => panic!("expected a list, got {other:?}"),
    }
    match (map, map.clone()) {
        (Value::Map(a), Value::Map(b)) => assert!(Arc::ptr_eq(a, &b)),
        other => panic!("expected a map, got {other:?}"),
    }
    match (text, text.clone()) {
        (Value::String(a), Value::String(b)) => assert!(Arc::ptr_eq(a, &b)),
        other => panic!("expected a string, got {other:?}"),
    }
    match (embedding, embedding.clone()) {
        (Value::Vector { values: a, .. }, Value::Vector { values: b, .. }) => {
            assert!(Arc::ptr_eq(a, &b))
        }
        other => panic!("expected a vector, got {other:?}"),
    }

    // Read through deref: `&[Value]`, `&BTreeMap<String, Value>`, `&str`.
    let (Value::List(items), Value::Map(fields), Value::String(s)) = (list, map, text) else {
        unreachable!("variants checked above")
    };
    assert_eq!(items.first(), Some(&Value::Int(1)));
    assert_eq!(fields.get("name"), Some(&Value::String("Alice".into())));
    assert_eq!(&**s, "hello");

    // Take an owned payload: `Arc::unwrap_or_clone` moves it when the Arc is
    // unique and copies it once when it is shared, as here. Values are
    // immutable, so editing the copy leaves the result row unchanged.
    let mut owned: BTreeMap<String, Value> = Arc::unwrap_or_clone(Arc::clone(fields));
    owned.insert("city".into(), Value::String("Malmö".into()));
    assert_eq!(fields.len(), 2);

    // Build parameters with `.into()` or `Arc::new(..)`; they come back equal.
    let tags = Value::List(vec![Value::String("a".into()), Value::Int(1)].into());
    let person = Value::Map(Arc::new(owned));
    let params = BTreeMap::from([
        ("tags".to_string(), tags.clone()),
        ("person".to_string(), person.clone()),
    ]);
    let back = conn.query_with_params("RETURN $tags AS tags, $person AS person", params)?;
    assert_eq!(back.rows[0][0], tags);
    assert_eq!(back.rows[0][1], person);

    println!("value sharing ok: clones share String, List, Map and Vector payloads");
    Ok(())
}
