use crate::{Database, Value};

#[test]
fn catalog_show_schema_database_and_graph_type() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE TEXT INDEX books FOR (n:Book) ON (n.title)")
        .unwrap();
    let indexes = c
        .query("SHOW TEXT INDEXES YIELD name, type, indexProvider")
        .unwrap();
    assert_eq!(indexes.rows.len(), 1);
    assert_eq!(indexes.rows[0][0], Value::String("books".into()));
    assert_eq!(indexes.rows[0][1], Value::String("TEXT".into()));
    assert_eq!(indexes.rows[0][2], Value::String("text-3.0".into()));

    c.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap();
    let databases = c
        .query("SHOW DATABASE neo4j YIELD name, defaultLanguage")
        .unwrap();
    assert_eq!(
        databases.rows[0],
        vec![
            Value::String("neo4j".into()),
            Value::String("CYPHER 5".into())
        ]
    );

    c.query("CREATE GRAPH TYPE social AS { (:Person => {name :: STRING}) }")
        .unwrap();
    let graph_type = c.query("SHOW GRAPH TYPE social YIELD name, body").unwrap();
    assert_eq!(graph_type.rows.len(), 1);
    assert_eq!(graph_type.rows[0][0], Value::String("social".into()));
}

#[test]
fn builtin_catalogs_are_composable() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let result = c
        .query("SHOW FUNCTIONS YIELD name WHERE name = 'toString' RETURN name")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("toString".into())]]);
    let roles = c.query("SHOW POPULATED ROLES WITH USERS").unwrap();
    assert_eq!(roles.rows.len(), 1);
}
#[test]
fn catalog_constraints_cover_nodes_relationships_and_types() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE CONSTRAINT person_id FOR (n:Person) REQUIRE n.id IS UNIQUE")
        .unwrap();
    c.query("CREATE (:Person {id: 1})").unwrap();
    assert!(c.query("CREATE (:Person {id: 1})").is_err());

    c.query("CREATE CONSTRAINT rel_id FOR ()-[r:LINK]-() REQUIRE r.id IS RELATIONSHIP KEY")
        .unwrap();
    assert!(c.query("CREATE ()-[:LINK]->()").is_err());

    c.query("CREATE CONSTRAINT person_age FOR (n:Person) REQUIRE n.age IS :: INTEGER")
        .unwrap();
    assert!(c.query("CREATE (:Person {id: 2, age: 'old'})").is_err());
    c.query("CREATE (:Person {id: 2, age: 42})").unwrap();
}

#[test]
fn constraints_reject_updates_and_existing_invalid_data_atomically() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (:Person {id: 1, name: 'Ada'}), (:Person {id: 2, name: 'Bob'})")
        .unwrap();
    c.query("CREATE CONSTRAINT person_id FOR (n:Person) REQUIRE n.id IS UNIQUE")
        .unwrap();

    assert!(c.query("MATCH (n:Person {id: 2}) SET n.id = 1").is_err());
    let ids = c
        .query("MATCH (n:Person) RETURN n.id AS id ORDER BY id")
        .unwrap();
    assert_eq!(ids.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    assert!(c
        .query("CREATE CONSTRAINT person_name FOR (n:Person) REQUIRE n.name IS NOT NULL")
        .is_ok());
    assert!(c.query("MATCH (n:Person {id: 1}) REMOVE n.name").is_err());
    assert!(c
        .query("CREATE CONSTRAINT person_name_type FOR (n:Person) REQUIRE n.name IS :: INTEGER")
        .is_err());

    // Numeric equality is Cypher equality for uniqueness purposes.
    assert!(c.query("CREATE (:Person {id: 1.0, name: 'Eve'})").is_err());
}

#[test]
fn equivalent_constraint_if_not_exists_is_a_noop() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE CONSTRAINT first FOR (n:Person) REQUIRE n.id IS UNIQUE")
        .unwrap();
    c.query("CREATE CONSTRAINT second IF NOT EXISTS FOR (n:Person) REQUIRE n.id IS UNIQUE")
        .unwrap();
}
#[test]
fn create_drop_show_index() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    c.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
        .unwrap();
    let shown = c.query("SHOW INDEXES").unwrap();
    assert_eq!(
        shown.header.columns,
        vec![
            "id",
            "name",
            "state",
            "populationPercent",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "indexProvider",
            "owningConstraint",
            "lastRead",
            "readCount",
        ]
    );
    assert_eq!(shown.rows.len(), 1);
    let details = c
        .query(
            "SHOW INDEXES YIELD name, labelsOrTypes, properties, type \
             RETURN name, labelsOrTypes, properties, type",
        )
        .unwrap();
    assert_eq!(
        details.header.columns,
        vec!["name", "labelsOrTypes", "properties", "type"]
    );
    assert_eq!(details.rows[0][0].as_string(), Some("person_name"));
    assert_eq!(
        details.rows[0][1],
        Value::List(vec![Value::String("Person".into())].into())
    );
    assert_eq!(
        details.rows[0][2],
        Value::List(vec![Value::String("name".into())].into())
    );
    assert_eq!(details.rows[0][3].as_string(), Some("RANGE"));
    let hit = c
        .query("MATCH (n:Person {name: 'Ada'}) RETURN n.name AS name")
        .unwrap();
    assert_eq!(hit.rows[0][0].as_string(), Some("Ada"));
    let dup = match c.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)") {
        Ok(_) => panic!("expected IndexAlreadyExists"),
        Err(e) => e.to_string(),
    };
    assert!(dup.contains("IndexAlreadyExists"), "{dup}");
    c.query("CREATE INDEX IF NOT EXISTS person_name FOR (n:Person) ON (n.name)")
        .unwrap();
    let other = match c.query("CREATE INDEX other FOR (n:Person) ON (n.name)") {
        Ok(_) => panic!("expected EquivalentSchemaRuleAlreadyExists"),
        Err(e) => e.to_string(),
    };
    assert!(
        other.contains("EquivalentSchemaRuleAlreadyExists"),
        "{other}"
    );
    c.query("DROP INDEX person_name").unwrap();
    assert!(c.query("SHOW INDEXES").unwrap().rows.is_empty());
    let missing = match c.query("DROP INDEX person_name") {
        Ok(_) => panic!("expected NoSuchIndex"),
        Err(e) => e.to_string(),
    };
    assert!(missing.contains("NoSuchIndex"), "{missing}");
    c.query("DROP INDEX IF EXISTS person_name").unwrap();
    c.query("CREATE INDEX ON :Person(name)").unwrap();
    let auto = c.query("SHOW INDEXES YIELD name RETURN name").unwrap();
    assert_eq!(auto.rows[0][0].as_string(), Some("Person_name"));
    c.query("DROP INDEX ON :Person(name)").unwrap();
}

#[test]
fn match_seeks_without_create_index() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    assert!(c.query("SHOW INDEXES").unwrap().rows.is_empty());
    let r = c
        .query("MATCH (n:Person {name: 'Ada'}) RETURN n.name AS name")
        .unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
}
#[test]
fn vector_search_and_single_stage_filter_use_native_graph() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE VECTOR INDEX docs FOR (n:Doc) ON n.embedding WITH [n.year] \
         OPTIONS {indexConfig: {`vector.dimensions`: 2, `vector.similarity_function`: 'cosine'}}",
    )
    .unwrap();
    c.query(
        "CREATE (:Doc {name: 'a', year: 2020, embedding: [1.0, 0.0]}), \
                (:Doc {name: 'b', year: 2010, embedding: [0.0, 1.0]}), \
                (:Doc {name: 'c', year: 2022, embedding: [0.9, 0.1]})",
    )
    .unwrap();
    let result = c
        .query(
            "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX docs FOR [1.0, 0.0] \
             WHERE n.year >= 2020 LIMIT 2) SCORE AS score \
             RETURN n.name AS name, score",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::String("a".into()));
    assert_eq!(result.rows[1][0], Value::String("c".into()));

    let procedure = c
        .query(
            "CALL db.index.vector.queryNodes('docs', 2, [1.0, 0.0]) \
             YIELD node, score RETURN node.name AS name, score",
        )
        .unwrap();
    assert_eq!(procedure.rows.len(), 2);
    assert_eq!(procedure.rows[0][0], Value::String("a".into()));
}
#[test]
fn show_constraint_paging_precedes_where() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE CONSTRAINT node_unique FOR (n:Foo) REQUIRE n.bar IS UNIQUE")
        .unwrap();
    c.query("CREATE CONSTRAINT rel_unique FOR ()-[r:FOO]-() REQUIRE r.bar IS UNIQUE")
        .unwrap();
    let result = c
        .query(
            "SHOW UNIQUENESS CONSTRAINTS \
             YIELD name, entityType ORDER BY name SKIP 1 LIMIT 1 \
             WHERE entityType = 'RELATIONSHIP' RETURN name",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("rel_unique".into())]]);
}
#[test]
fn graph_type_constraints_keep_dependent_classification() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "ALTER CURRENT GRAPH TYPE SET { \
         (:Label => {property :: ANY NOT NULL}), \
         ()-[:REL_TYPE => {prop :: STRING}]->() }",
    )
    .unwrap();
    let result = c
        .query(
            "SHOW CONSTRAINTS YIELD name, classification, createStatement \
             ORDER BY name",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert!(result
        .rows
        .iter()
        .all(|row| { row[1] == Value::String("dependent".into()) && row[2] == Value::Null }));
}
