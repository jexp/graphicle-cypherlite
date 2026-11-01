//! `run_script_wire`: the script runner every binding shares (ADR-0131).

use cypherlite::{run_script_wire, Database};

fn conn() -> (Database, cypherlite::Connection) {
    let db = Database::open(":memory:").expect("open");
    let conn = db.connect().expect("connect");
    (db, conn)
}

#[test]
fn runs_each_statement_with_the_same_parameters() {
    let (_db, mut conn) = conn();
    let envelopes = run_script_wire(
        &mut conn,
        "CREATE (:T {v: $v}); CREATE (:T {v: $v + 1}); MATCH (t:T) RETURN sum(t.v) AS s",
        Some(r#"{"v": 1.0}"#),
    );
    assert_eq!(envelopes.len(), 3);
    assert_eq!(envelopes[0]["stats"]["nodes_created"], 1);
    assert_eq!(envelopes[2]["rows"][0]["s"], 3.0);
    assert!(envelopes.iter().all(|e| e["error"].is_null()));
}

#[test]
fn stops_after_the_first_error_and_keeps_earlier_statements() {
    let (_db, mut conn) = conn();
    let envelopes = run_script_wire(&mut conn, "CREATE (:A); RETURN 1 +; CREATE (:B)", None);
    assert_eq!(envelopes.len(), 2);
    assert!(envelopes[1]["error"].is_string());
    let count = run_script_wire(&mut conn, "MATCH (n) RETURN count(n) AS n", None);
    assert_eq!(count[0]["rows"][0]["n"], 1);
}

#[test]
fn blank_and_comment_only_scripts_run_nothing() {
    let (_db, mut conn) = conn();
    assert!(run_script_wire(&mut conn, "", None).is_empty());
    assert!(run_script_wire(&mut conn, "  // a note;\n/* another; */ ;", Some("")).is_empty());
}

#[test]
fn parameters_that_do_not_decode_run_nothing() {
    let (_db, mut conn) = conn();
    for params in ["[1]", "{", r#"{"p": 18446744073709551615}"#] {
        let envelopes = run_script_wire(&mut conn, "CREATE (:X)", Some(params));
        assert_eq!(envelopes.len(), 1, "{params}");
        assert!(envelopes[0]["error"].is_string(), "{params}");
    }
    let count = run_script_wire(&mut conn, "MATCH (n) RETURN count(n) AS n", None);
    assert_eq!(count[0]["rows"][0]["n"], 0);
}

#[test]
fn a_parameter_map_with_a_type_key_stays_a_map() {
    let (_db, mut conn) = conn();
    let envelopes = run_script_wire(
        &mut conn,
        "RETURN $p.__type AS t",
        Some(r#"{"p": {"__type": "point", "x": 1}}"#),
    );
    assert_eq!(envelopes[0]["rows"][0]["t"], "point");
}
