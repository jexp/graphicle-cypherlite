//! Query notifications for `EXPLAIN`/`PROFILE` (ADR-0363): the
//! `QueryResult::notifications()` list, its vocabulary metadata, the
//! plain-run empty list, and the native and Neo4j-shaped wire forms.
//!
//! Run with `cargo run -p cypherlite --example notifications`.

use cypherlite::{
    notification_to_neo4j_json, notification_to_wire_json, Database, NotificationKind, Result,
};

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.query("CREATE (:Person)")?;
    conn.query("CREATE (:City)")?;

    // A multi-pattern MATCH with no shared node variable plans a cartesian
    // product: EXPLAIN derives one operator-scoped notification from the
    // tree's typed warning, naming the warned operator's id.
    let statement = "MATCH (a:Person), (b:City) RETURN a, b";
    let explained = conn.query(&format!("EXPLAIN {statement}"))?;
    let notification = explained
        .notifications()
        .first()
        .expect("the disconnected MATCH raises one notification");
    assert_eq!(notification.kind(), NotificationKind::CartesianProduct);
    assert_eq!(
        notification.code(),
        NotificationKind::CartesianProduct.code()
    );
    assert_eq!(notification.gql_status(), "03N90");
    assert_eq!(notification.severity().as_str(), "INFORMATION");
    assert_eq!(notification.category().as_str(), "PERFORMANCE");
    assert_eq!(notification.title(), "Cartesian product");
    assert_eq!(
        notification.description(),
        "the MATCH patterns share no join anchor"
    );
    assert!(notification.remedy().contains("join anchor"));
    println!(
        "INFORMATION {} {}: {}\nRemedy: {}",
        notification.gql_status(),
        notification.code(),
        notification.description(),
        notification.remedy()
    );

    // The engine-native form keeps the diagnosis and the action as separate
    // keys, plus the operator association.
    let native = notification_to_wire_json(notification);
    assert_eq!(native["code"], notification.code());
    assert_eq!(native["gql_status"], notification.gql_status());
    assert!(native["remedy"].is_string());
    assert!(native["operator_id"].is_number());

    // The Neo4j-shaped form folds the remedy into `description` and emits
    // neither a `remedy` nor a `position` key — Bolt's and the HTTP Query
    // API v2's contract.
    let neo4j = notification_to_neo4j_json(notification);
    let entry = neo4j.as_object().expect("a notification is a JSON object");
    assert_eq!(entry.len(), 5);
    assert_eq!(
        neo4j["description"],
        format!(
            "{} (remedy: {})",
            notification.description(),
            notification.remedy()
        )
    );
    assert!(!entry.contains_key("remedy"));
    assert!(!entry.contains_key("position"));

    // A plain run builds no plan tree, so its list is empty — derivation is
    // opt-in with EXPLAIN/PROFILE (ADR-0363 decision 3).
    let plain = conn.query(statement)?;
    assert!(plain.notifications().is_empty());
    println!(
        "the plain run carries {} notifications",
        plain.notifications().len()
    );
    Ok(())
}
