//! Exclusive Native database ownership, retained connections, and release (ADR-0097).
//! Run: cargo run -p cypherlite --example storage_lock

use cypherlite::{Database, Error};

fn main() -> cypherlite::Result<()> {
    let directory = tempfile::tempdir().expect("temporary database parent");
    let path = directory.path().join("graph");
    let uri = path.to_str().expect("UTF-8 temporary path");
    let owner = Database::open(uri)?;
    let connection = owner.connect()?;
    let error = Database::open(uri).err().expect("second open must fail");
    assert!(matches!(error, Error::Locked(_)), "{error}");
    assert!(error.to_string().starts_with("storage locked: "));
    drop(owner);
    // The connection still owns the store, even though the Database is gone.
    assert!(matches!(Database::open(uri), Err(Error::Locked(_))));
    drop(connection);
    let reopened = Database::open(uri)?;
    drop(reopened);
    Ok(())
}
