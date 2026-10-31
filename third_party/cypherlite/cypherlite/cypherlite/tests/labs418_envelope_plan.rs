//! The wire envelope carries the structured plan document (LABS-418,
//! ADR-0381): a `cypherlite.plan/v1` object on EXPLAIN/PROFILE, null on a
//! plain run — the key the WASM surface and any C-ABI envelope reader sees.

use cypherlite::{query_result_to_wire, Database};

#[test]
fn the_wire_envelope_carries_the_plan_document() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.execute("CREATE (:Person)").unwrap();

    let explained = conn.execute("EXPLAIN MATCH (p:Person) RETURN p").unwrap();
    let envelope = query_result_to_wire(&explained);
    let plan = &envelope["plan"];
    assert_eq!(plan["schema"], "cypherlite.plan/v1");
    assert_eq!(plan["profiled"], false);
    assert_eq!(plan["root"]["operator"], "ProduceResults");
    assert!(
        plan["root"]["actual"].is_null(),
        "EXPLAIN carries no metrics"
    );

    let profiled = conn.execute("PROFILE MATCH (p:Person) RETURN p").unwrap();
    assert_eq!(query_result_to_wire(&profiled)["plan"]["profiled"], true);

    // A plain run carries null, never an absent key or an empty document.
    let plain = conn.execute("RETURN 1 AS n").unwrap();
    let envelope = query_result_to_wire(&plain);
    assert!(
        envelope.as_object().unwrap().contains_key("plan"),
        "the key is always present"
    );
    assert!(envelope["plan"].is_null(), "a plain run carries no plan");
}
