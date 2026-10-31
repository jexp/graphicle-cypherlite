//! Regression test: `SHOW INDEXES`' `createStatement` for LOOKUP indexes
//! renders the `ON EACH` form (Neo4j CE) instead of broken label/property
//! syntax.

mod common;

use common::{connection, string};

#[test]
fn lookup_index_create_statement_renders_the_each_form() {
    let mut conn = connection();
    conn.query("CREATE LOOKUP INDEX li FOR (n) ON EACH labels(n)")
        .unwrap();
    conn.query("CREATE LOOKUP INDEX ri FOR ()-[r]-() ON EACH type(r)")
        .unwrap();

    let result = conn
        .query(
            "SHOW INDEXES YIELD name, createStatement RETURN name, createStatement ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![
                string("li"),
                string("CREATE LOOKUP INDEX `li` FOR (n) ON EACH labels(n)"),
            ],
            vec![
                string("ri"),
                string("CREATE LOOKUP INDEX `ri` FOR ()-[r]-() ON EACH type(r)"),
            ],
        ]
    );
}
