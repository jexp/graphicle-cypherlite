//! Client retries `Error::Conflict` (ADR-0017). The engine does not retry.

use cypherlite::{Database, Error};

fn main() -> cypherlite::Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.query("CREATE (n:Person {name: 'Ada', n: 0})")?;
    loop {
        match conn.query("MATCH (n:Person {name: 'Ada'}) SET n.n = 1") {
            Ok(_) => break,
            Err(Error::Conflict) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
