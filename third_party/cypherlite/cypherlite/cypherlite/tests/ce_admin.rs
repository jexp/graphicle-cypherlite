//! ADR-0116 §2/§6: fixed namespaces with Neo4j Community Edition database
//! semantics. Every database administration command outside CE's
//! supported set fails with `UnsupportedAdministrationCommand` (51N27),
//! echoing the statement verbatim (without a leading `CYPHER n`);
//! `ALTER DATABASE <name> [IF EXISTS] SET DEFAULT LANGUAGE CYPHER 5|25`
//! is the one supported alteration, and `SHOW DATABASES` reports the
//! three fixed namespaces.

use cypherlite::{Database, Namespace, Value};

mod common;

use common::connection;

/// The statement is echoed verbatim; a leading `CYPHER n` preamble is
/// stripped from the echo.
fn assert_unsupported(src: &str, echoed: &str) {
    let err = connection().query(src).unwrap_err().to_string();
    let expected =
        format!("UnsupportedAdministrationCommand: Unsupported administration command: {echoed}");
    assert!(
        err.ends_with(&expected),
        "wrong outcome for {src:?}: {err:?} does not end with {expected:?}"
    );
}

/// The unsupported-administration command table (statement, echo), from
/// the one shared home (testdata/unsupported-admin-commands.tsv).
fn unsupported_admin_commands() -> impl Iterator<Item = (&'static str, &'static str)> {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/unsupported-admin-commands.tsv"
    ))
    .lines()
    .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    .map(|l| l.split_once('\t').expect("TSV row: statement<TAB>echo"))
}

#[test]
fn unsupported_administration_commands_echo_the_statement() {
    for (src, echoed) in unsupported_admin_commands() {
        assert_unsupported(src, echoed);
    }
}

#[test]
fn alter_database_set_default_language_succeeds_on_every_fixed_namespace() {
    let mut conn = connection();
    for (name, version) in [
        ("neo4j", 5),
        ("system", 25),
        ("system_ontologies", 5),
        // Names match case-insensitively (ADR-0116 §1).
        ("NEO4J", 25),
    ] {
        conn.query(&format!(
            "ALTER DATABASE {name} SET DEFAULT LANGUAGE CYPHER {version}"
        ))
        .unwrap();
        let shown = conn
            .query(&format!("SHOW DATABASE {name} YIELD name, defaultLanguage"))
            .unwrap();
        assert_eq!(
            shown.rows,
            vec![vec![
                Value::String(
                    Namespace::parse(name)
                        .unwrap_or_else(|| panic!("unexpected namespace {name}"))
                        .as_str()
                        .into(),
                ),
                Value::String(format!("CYPHER {version}").into()),
            ]],
            "wrong row after ALTER DATABASE {name}"
        );
    }
}

#[test]
fn alter_database_unknown_name_fails_unless_if_exists() {
    let mut conn = connection();
    let err = conn
        .query("ALTER DATABASE movies SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap_err()
        .to_string();
    assert!(
        err.ends_with("Failed to alter the specified database 'movies': Database does not exist."),
        "wrong error: {err}"
    );
    // `IF EXISTS` makes the same command a no-op.
    conn.query("ALTER DATABASE movies IF EXISTS SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap();
}

#[test]
fn show_databases_reports_the_three_fixed_namespaces() {
    let mut conn = connection();
    let result = conn
        .query(
            "SHOW DATABASES YIELD name, type, access, requestedStatus, currentStatus, \
             default, home, aliases, constituents, defaultLanguage \
             RETURN name, type, access, requestedStatus, currentStatus, \
             default, home, aliases, constituents, defaultLanguage",
        )
        .unwrap();
    let row = |name: &str, ty: &str, access: &str, default: bool, language: &str| -> Vec<Value> {
        vec![
            Value::String(name.into()),
            Value::String(ty.into()),
            Value::String(access.into()),
            Value::String("online".into()),
            Value::String("online".into()),
            Value::Bool(default),
            Value::Bool(default),
            Value::List(Vec::new().into()),
            Value::List(Vec::new().into()),
            Value::String(language.into()),
        ]
    };
    assert_eq!(
        result.rows,
        vec![
            row("neo4j", "standard", "read-write", true, "CYPHER 25"),
            row("system", "system", "read-write", false, "CYPHER 25"),
            row(
                "system_ontologies",
                "standard",
                "read-only",
                false,
                "CYPHER 25"
            ),
        ]
    );
}

/// ADR-0116 §8/§10 regression: the databases catalog is store-level, so an
/// `ALTER DATABASE` write (routed to the `neo4j` graph's catalog) and a
/// `SHOW DATABASES ... RETURN ...` read on a `system` session — composed
/// with RETURN, so the plan is not an all-Catalog admin command and runs on
/// the `system` graph — can never diverge on `defaultLanguage`. Per-graph
/// theater copies did: the `system` slot's statically seeded theater kept
/// showing `CYPHER 25`.
#[test]
fn alter_and_system_session_show_databases_agree_on_default_language() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap();

    // Column order varies with the projection (`RETURN *` sorts), so find
    // the columns by name and return neo4j's `defaultLanguage`.
    let neo4j_language = |result: cypherlite::QueryResult| {
        let name_col = result
            .header
            .columns
            .iter()
            .position(|c| c == "name")
            .expect("a name column");
        let language_col = result
            .header
            .columns
            .iter()
            .position(|c| c == "defaultLanguage")
            .expect("a defaultLanguage column");
        let row = result
            .rows
            .into_iter()
            .find(|row| row[name_col] == Value::String("neo4j".into()))
            .expect("SHOW DATABASES carries a neo4j row");
        row[language_col].clone()
    };

    // The all-Catalog spelling routes to the store's catalog (neo4j graph).
    let admin = conn
        .query("SHOW DATABASES YIELD name, defaultLanguage")
        .unwrap();
    assert_eq!(
        neo4j_language(admin),
        Value::String("CYPHER 5".into()),
        "admin-routed SHOW DATABASES"
    );

    // The RETURN-composed spelling runs on the session's own graph; on a
    // `system` session it reads the same store-level catalog.
    let mut system = db.connect_namespace("system").unwrap();
    for src in [
        "SHOW DATABASES YIELD name, defaultLanguage RETURN *",
        "SHOW DATABASES YIELD name, defaultLanguage RETURN name, defaultLanguage",
    ] {
        let result = system.query(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(
            neo4j_language(result),
            Value::String("CYPHER 5".into()),
            "{src}: system session disagrees with ALTER DATABASE"
        );
    }
    // A bare SHOW DATABASES on the same system session still runs (42N17
    // must not misclassify SHOW commands composed on `system`).
    system.query("SHOW DATABASES").unwrap();
}

/// ADR-0116 §2 / CIP-213 NE1.4: `SHOW DATABASES` composed with a `NEXT`
/// stage is rejected before execution (`SystemDatabaseRequired`). The
/// front-end's NEXT-chain validation fires first; the connection's
/// post-EXPLAIN gates keep the execution-phase backstop.
#[test]
fn show_databases_with_next_is_rejected() {
    let error = connection()
        .query(
            "SHOW DATABASES YIELD name RETURN count(DISTINCT name) AS count \
             NEXT RETURN count * 2 AS res",
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("SystemDatabaseRequired"),
        "{error}"
    );
}

#[test]
fn show_default_and_home_database_return_neo4j() {
    let mut conn = connection();
    for src in ["SHOW DEFAULT DATABASE", "SHOW HOME DATABASE"] {
        let result = conn.query(src).unwrap();
        assert_eq!(result.rows.len(), 1, "{src}");
        assert_eq!(result.rows[0][0], Value::String("neo4j".into()), "{src}");
    }
}

#[test]
fn show_database_matches_case_insensitively_and_unknown_names_return_no_rows() {
    let mut conn = connection();
    for src in [
        "SHOW DATABASE NEO4J YIELD name RETURN name",
        "SHOW DATABASE System_Ontologies YIELD name RETURN name",
    ] {
        let result = conn.query(src).unwrap();
        assert_eq!(result.rows.len(), 1, "{src}");
    }
    let result = conn
        .query("SHOW DATABASE movies YIELD name RETURN name")
        .unwrap();
    assert!(result.rows.is_empty());
}

#[test]
fn start_stop_alias_stay_usable_as_identifiers() {
    let mut conn = connection();
    conn.query("CREATE (start {alias: 'stop'})").unwrap();
    let result = conn
        .query("MATCH (start) WHERE start.alias = 'stop' RETURN start.alias AS alias")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("stop".into())]]);
}
