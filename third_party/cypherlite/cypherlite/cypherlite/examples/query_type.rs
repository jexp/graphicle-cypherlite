//! Query type on every result (ADR-0162): `QueryResult.query_type` carries
//! the statement's Neo4j Query API v2 `queryType` — `"r"`, `"w"`, `"rw"`
//! or `"s"` — derived from the plan before execution, so `EXPLAIN` answers
//! "may this run on a read-only target?" without executing anything.
//!
//! Run with `cargo run -p cypherlite --example query_type`.

use cypherlite::{Database, QueryType, Result};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;

    // Every statement's result carries the type, plain runs included.
    assert_eq!(
        conn.query("MATCH (n) RETURN n")?.header.query_type,
        QueryType::Read
    );
    assert_eq!(
        conn.query("CREATE (n:Person)")?.header.query_type,
        QueryType::Write
    );
    assert_eq!(
        conn.query("MATCH (n:Person) SET n.seen = true RETURN n")?
            .header
            .query_type,
        QueryType::ReadWrite
    );
    assert_eq!(
        conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")?
            .header
            .query_type,
        QueryType::Schema
    );

    // EXPLAIN reports the wrapped statement's type and executes nothing.
    let explained = conn.query("EXPLAIN CREATE (n:Person)")?;
    assert_eq!(explained.header.query_type, QueryType::Write);
    assert_eq!(explained.header.query_type.as_str(), "w");
    let count = conn.query("MATCH (n:Person) RETURN count(n) AS c")?;
    assert_eq!(count.rows[0][0], cypherlite::Value::Int(1)); // only the one above

    // ADR-0332: a write splits on its returned columns — a MATCH that only
    // feeds a SET returns no columns, so it is `w`, not `rw`.
    assert_eq!(
        conn.query("MATCH (n:Person) SET n.seen = true")?
            .header
            .query_type,
        QueryType::Write
    );

    // PROFILE executes and reports the same type as EXPLAIN.
    let profiled = conn.query("PROFILE MATCH (n:Person) RETURN n")?;
    assert_eq!(profiled.header.query_type, QueryType::Read);

    println!("query types ok: r / w / rw / s stamped on every result");
    Ok(())
}
