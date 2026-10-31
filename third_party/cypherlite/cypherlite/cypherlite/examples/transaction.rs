//! Explicit multi-statement transaction (ADR-0028).

use cypherlite::Database;

fn main() -> cypherlite::Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.begin()?;
    conn.query("CREATE (n:Person {name: 'Ada'})")?;
    let result = conn.query("MATCH (n:Person) RETURN n.name AS name")?;
    assert_eq!(result.rows[0][0].as_string(), Some("Ada"));
    conn.commit()?;
    Ok(())
}
