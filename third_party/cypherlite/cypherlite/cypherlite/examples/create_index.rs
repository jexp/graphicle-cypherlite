//! Name a (label, property) pair, list it, then drop it.

use cypherlite::Database;

fn main() -> cypherlite::Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.query("CREATE (n:Person {name: 'Ada'})")?;
    conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")?;
    let shown = conn.query("SHOW INDEXES")?;
    let name_col = shown
        .header
        .columns
        .iter()
        .position(|c| c == "name")
        .unwrap();
    assert_eq!(shown.rows[0][name_col].as_string(), Some("person_name"));
    let hit = conn.query("MATCH (n:Person {name: 'Ada'}) RETURN n.name AS name")?;
    assert_eq!(hit.rows[0][0].as_string(), Some("Ada"));
    conn.query("DROP INDEX person_name")?;
    Ok(())
}
