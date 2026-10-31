//! Engine-reported statement elapsed (ADR-0221): every `QueryResult`
//! carries `elapsed` — parse, plan, gates and dispatch, measured once
//! around `Connection::run_query`, never around row serialization.
//!
//! Run with `cargo run -p cypherlite --example elapsed`.

use std::time::Duration;

use cypherlite::{Database, Result};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;

    // Plain runs, EXPLAIN and PROFILE all carry the stamp.
    for stmt in [
        "CREATE (n:Person {name: 'Alice'})",
        "MATCH (n:Person) RETURN n.name AS name",
        "EXPLAIN MATCH (n:Person) RETURN n",
        "PROFILE MATCH (n:Person) RETURN n.name AS name",
    ] {
        let result = conn.query(stmt)?;
        assert!(
            result.header.elapsed < Duration::from_secs(60),
            "elapsed is the plan+run duration, got {:?} for {stmt}",
            result.header.elapsed,
        );
        println!("{stmt} -> {:.3}ms", result.elapsed_ms());
    }
    Ok(())
}
