//! Register a CDC slot, read WAL records, apply them on an apply-only replica.

use cypherlite::Database;

fn main() {
    let src = Database::open(":memory:").unwrap();
    src.register_cdc_slot("primary").unwrap();
    let mut c = src.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    let recs = src.cdc_records_after(0, 16).unwrap();
    src.ack_cdc_slot("primary", recs.last().unwrap().lsn)
        .unwrap();

    let dst = Database::open(":memory:").unwrap();
    dst.set_apply_only(true).unwrap();
    dst.apply_replicated(&recs[0]).unwrap();
    let mut r = dst.connect().unwrap();
    let out = r.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(out.rows[0][0].as_string(), Some("Ada"));
    assert!(r.query("CREATE (n:Nope)").is_err());
}
