use crate::{Connection, Database, Value};

fn wal_segment_names(path: &str) -> Vec<String> {
    let wal_dir = std::path::Path::new(path).join("wal");
    let mut names: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn cdc_slot_holds_closed_wal_until_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    db.register_cdc_slot("primary").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Hold'})").unwrap();
    db.checkpoint().unwrap();
    let names = wal_segment_names(path);
    assert!(
        names.iter().any(|n| n == "00000001.log"),
        "unacked slot must hold the closed segment, got {names:?}"
    );
    db.ack_cdc_slot("primary", 1).unwrap();
    db.checkpoint().unwrap();
    let names = wal_segment_names(path);
    assert!(
        !names.iter().any(|n| n == "00000001.log"),
        "acked slot should allow recycle, got {names:?}"
    );
}
#[test]
fn drop_cdc_slot_restores_today_recycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    db.register_cdc_slot("primary").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Drop'})").unwrap();
    db.drop_cdc_slot("primary").unwrap();
    db.checkpoint().unwrap();
    let names = wal_segment_names(path);
    assert!(
        !names.iter().any(|n| n == "00000001.log"),
        "dropped slot must recycle like today, got {names:?}"
    );
}

#[test]
fn two_cdc_slots_wait_for_slower_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    db.register_cdc_slot("fast").unwrap();
    db.register_cdc_slot("slow").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Two'})").unwrap();
    db.checkpoint().unwrap();
    db.ack_cdc_slot("fast", 1).unwrap();
    db.checkpoint().unwrap();
    let names = wal_segment_names(path);
    assert!(
        names.iter().any(|n| n == "00000001.log"),
        "slower slot must still hold WAL, got {names:?}"
    );
    db.ack_cdc_slot("slow", 1).unwrap();
    db.checkpoint().unwrap();
    let names = wal_segment_names(path);
    assert!(!names.iter().any(|n| n == "00000001.log"));
}

#[test]
fn cdc_records_after_skips_acked_prefix() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:A {n: 1})").unwrap();
    c.query("CREATE (n:B {n: 2})").unwrap();
    let all = db.cdc_records_after(0, 100).unwrap();
    assert_eq!(all.len(), 2);
    let tail = db.cdc_records_after(all[0].lsn, 100).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].lsn, all[1].lsn);
    let limited = db.cdc_records_after(0, 1).unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn cdc_records_after_file_wal_pages_and_caught_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    for i in 0..5 {
        c.query(&format!("CREATE (n:N {{n: {i}}})")).unwrap();
    }
    let page = db.cdc_records_after(0, 2).unwrap();
    assert_eq!(page.len(), 2);
    let next = db.cdc_records_after(page[1].lsn, 2).unwrap();
    assert_eq!(next.len(), 2);
    assert!(next[0].lsn > page[1].lsn);
    let last = db.cdc_records_after(next[1].lsn, 16).unwrap();
    assert_eq!(last.len(), 1);
    assert!(db.cdc_records_after(last[0].lsn, 8).unwrap().is_empty());
    c.query("CREATE (n:N {n: 5})").unwrap();
    let tail = db.cdc_records_after(last[0].lsn, 16).unwrap();
    assert_eq!(tail.len(), 1);
    assert!(tail[0].lsn > last[0].lsn);
}

#[test]
fn apply_only_rejects_cypher_writes_and_allows_replicated() {
    let src = Database::open(":memory:").unwrap();
    let mut sc = src.connect().unwrap();
    sc.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    let recs = src.cdc_records_after(0, 10).unwrap();
    assert_eq!(recs.len(), 1);

    let dst = Database::open(":memory:").unwrap();
    dst.set_apply_only(true).unwrap();
    assert!(dst.apply_only().unwrap());
    let mut dc = dst.connect().unwrap();
    let err = dc.query("CREATE (n:Nope)").unwrap_err();
    assert!(
        err.to_string().contains("CDC replica"),
        "unexpected error: {err}"
    );
    dst.apply_replicated(&recs[0]).unwrap();
    let r = dc.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
    dst.set_apply_only(false).unwrap();
    dc.query("CREATE (n:Person {name: 'Bob'})").unwrap();
    let r = dc.query("MATCH (n:Person) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(2));
}

#[test]
fn apply_replicated_rejects_source_database() {
    let db = Database::open(":memory:").unwrap();
    db.register_cdc_slot("primary").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:A)").unwrap();
    let recs = db.cdc_records_after(0, 1).unwrap();
    let err = db.apply_replicated(&recs[0]).unwrap_err();
    assert!(err.to_string().contains("source slot"));
}

#[test]
fn apply_only_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        db.set_apply_only(true).unwrap();
    }
    let db = Database::open(path).unwrap();
    assert!(db.apply_only().unwrap());
    let mut c = db.connect().unwrap();
    assert!(dc_write_fails(&mut c));
}

fn dc_write_fails(c: &mut Connection) -> bool {
    c.query("CREATE (n:X)")
        .unwrap_err()
        .to_string()
        .contains("CDC replica")
}
