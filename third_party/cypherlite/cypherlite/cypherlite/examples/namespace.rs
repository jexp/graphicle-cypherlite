//! Namespace sessions (ADR-0116, ADR-0117): connect with a session
//! namespace, switch it between transactions, and select a namespace with
//! `USE`.
//!
//! Run with `cargo run -p cypherlite --example namespace`.

use cypherlite::{Database, Namespace, Result, Value};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;

    // The default session namespace is `neo4j`, the user data graph.
    let mut conn = db.connect()?;
    assert_eq!(conn.namespace(), Namespace::Neo4j);
    conn.query("CREATE (:Thing {name: 'cypherlite'})")?;

    // `USE` names a statement's namespace; without it the session namespace
    // applies. The result reports the namespace the statement ran in.
    let result = conn.query("USE neo4j MATCH (t:Thing) RETURN t.name AS name")?;
    assert_eq!(result.rows[0][0], Value::String("cypherlite".into()));
    assert_eq!(result.header.namespace, Namespace::Neo4j);

    // Namespace names match case-insensitively (ADR-0117 §6).
    let mut system = db.connect_namespace("SYSTEM")?;
    assert_eq!(system.namespace(), Namespace::System);
    // `system` runs SHOW commands, administration commands and procedure
    // calls only; it holds no graph data (ADR-0116 §4).
    let databases = system.query("SHOW DATABASES")?;
    assert!(!databases.rows.is_empty());
    assert_eq!(databases.header.namespace, Namespace::System);

    // A typed `Namespace` connects without any name parsing (ADR-0150).
    let typed = db.connect_typed(Namespace::SystemOntologies)?;
    assert_eq!(typed.namespace(), Namespace::SystemOntologies);

    // The session namespace changes only between transactions (ADR-0116 §5).
    conn.set_namespace("system_ontologies")?;
    // `system_ontologies` is listed but not queryable (ADR-0229): selecting
    // it works, and the one statement kind that still runs there is the
    // database-administration set (ADR-0116 §2) — every other statement,
    // reads included, fails with the 42N17 text.
    let result = conn.query("SHOW DATABASES")?;
    assert_eq!(result.header.namespace, Namespace::SystemOntologies);
    let err = conn
        .query("MATCH (n) RETURN count(n) AS c")
        .expect_err("system_ontologies is not queryable");
    assert!(err.to_string().ends_with(
        "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
    ));
    conn.set_namespace("neo4j")?;

    println!(
        "namespaces ok: {} SHOW DATABASES rows on system",
        databases.rows.len()
    );
    Ok(())
}
