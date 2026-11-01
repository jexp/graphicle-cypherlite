use cypherlite::Database;

fn main() -> cypherlite::Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.query("CREATE (n:Person {name: 'Ada'})")?;
    let result = conn.query("MATCH (n:Person) RETURN n.name AS name")?;
    assert_eq!(result.header.columns, vec!["name"]);
    assert_eq!(result.rows[0][0].as_string(), Some("Ada"));
    println!("{}", result.rows[0][0]);
    Ok(())
}
