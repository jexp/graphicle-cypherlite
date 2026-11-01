//! Move a database dir -> `.cl` -> dir (ADR-0282): clone a native
//! directory into a single-file bundle, then clone the bundle into a fresh
//! directory. The source is opened read-only and is never written.

use cypherlite::{CloneOptions, Database, Value};

fn main() -> cypherlite::Result<()> {
    // tempfile cleans the root up on drop, failure paths included.
    let root = tempfile::tempdir().expect("create temp root");
    let data = root.path().join("data");
    let bundle = root.path().join("backup.cl");
    let restored = root.path().join("restored");
    let data = data.to_str().expect("temp dir is valid UTF-8");
    let bundle = bundle.to_str().expect("temp dir is valid UTF-8");
    let restored = restored.to_str().expect("temp dir is valid UTF-8");

    let db = Database::open(data)?;
    db.connect()?.execute("CREATE (:Person {name: 'Ada'})")?;
    db.checkpoint()?;
    drop(db);

    Database::clone(data, bundle, CloneOptions::default())?;
    Database::clone(bundle, restored, CloneOptions::default())?;

    let db = Database::open(restored)?;
    let rows = db.connect()?.query("MATCH (p:Person) RETURN p.name")?.rows;
    assert_eq!(rows, vec![vec![Value::String("Ada".into())]]);
    Ok(())
}
