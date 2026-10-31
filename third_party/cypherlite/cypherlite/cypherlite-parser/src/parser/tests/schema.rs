use super::*;
use crate::YieldMode;

#[test]
fn test_parse_create_index() {
    let stmt = parse("CREATE INDEX myidx FOR (n:Person) ON (n.name)").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.name, Some(SchemaName::Literal("myidx".to_string())));
            assert_eq!(idx.labels, vec!["Person".to_string()]);
            assert_eq!(idx.properties, vec!["name"]);
            assert_eq!(idx.entity_type, SchemaEntityType::Node);
            assert_eq!(idx.kind, IndexKind::Range);
        }
        _ => panic!("expected create index"),
    }
}

#[test]
fn test_parse_drop_index() {
    let stmt = parse("DROP INDEX myidx IF EXISTS").unwrap();
    match &stmt.clauses[0] {
        Clause::DropIndex(d) => {
            assert_eq!(d.name, SchemaName::Literal("myidx".to_string()));
            assert!(d.if_exists);
        }
        _ => panic!("expected drop index"),
    }
}

// `IF EXISTS` may precede or follow the DROP INDEX/CONSTRAINT name, once.
#[test]
fn test_parse_drop_if_exists_positions() {
    for (q, want_if_exists) in [
        ("DROP INDEX x", false),
        ("DROP INDEX IF EXISTS x", true),
        ("DROP INDEX x IF EXISTS", true),
        ("DROP CONSTRAINT x", false),
        ("DROP CONSTRAINT IF EXISTS x", true),
        ("DROP CONSTRAINT x IF EXISTS", true),
    ] {
        let stmt = parse(q).unwrap();
        let (name, if_exists) = match &stmt.clauses[0] {
            Clause::DropIndex(d) => (&d.name, d.if_exists),
            Clause::DropConstraint(d) => (&d.name, d.if_exists),
            other => panic!("{q}: expected a drop clause, got {other:?}"),
        };
        assert_eq!(name, &SchemaName::Literal("x".to_string()), "{q}");
        assert_eq!(if_exists, want_if_exists, "{q}");
    }
    assert!(parse("DROP INDEX IF EXISTS x IF EXISTS").is_err());
    assert!(parse("DROP CONSTRAINT IF EXISTS x IF EXISTS").is_err());
}

// Legacy `ON :Label(prop[, ...])` index targets: CREATE INDEX takes a
// property list, DROP INDEX exactly one property.
#[test]
fn test_parse_legacy_index_targets() {
    let stmt = parse("CREATE INDEX ON :L(a, b)").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.labels, vec!["L".to_string()]);
            assert_eq!(idx.properties, vec!["a", "b"]);
        }
        _ => panic!("expected create index"),
    }
    let stmt = parse("DROP INDEX ON :L(a)").unwrap();
    match &stmt.clauses[0] {
        Clause::DropIndex(d) => {
            assert_eq!(d.legacy_target, Some(("L".to_string(), "a".to_string())));
        }
        _ => panic!("expected drop index"),
    }
    let err = parse("DROP INDEX ON :L(a, b)").unwrap_err();
    assert_eq!(err.detail, None, "{}", err.message);
    assert_eq!(
        err.message,
        "legacy DROP INDEX ON :Label(prop) takes exactly one property"
    );
    assert_eq!((err.line, err.col), (1, 12), "{err}");
    let err = parse("CREATE INDEX ON :L(a, 1)").unwrap_err();
    assert!(
        err.message.starts_with("expected property name"),
        "{}",
        err.message
    );
}

#[test]
fn test_parse_create_constraint() {
    let stmt = parse("CREATE CONSTRAINT mycon FOR (n:Person) REQUIRE n.name IS UNIQUE").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateConstraint(c) => {
            assert_eq!(c.name, Some(SchemaName::Literal("mycon".to_string())));
            assert_eq!(c.label, "Person");
            assert_eq!(c.constraint_type, "unique");
        }
        _ => panic!("expected create constraint"),
    }
}

// CIP-134: $param in the name position of CREATE/DROP INDEX/CONSTRAINT.
#[test]
fn test_parse_create_index_param_name() {
    let stmt = parse("CREATE INDEX $param FOR (n:Person) ON (n.name)").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.name, Some(SchemaName::Parameter("param".to_string())));
        }
        _ => panic!("expected create index"),
    }
}

#[test]
fn test_parse_drop_index_param_name() {
    let stmt = parse("DROP INDEX $param IF EXISTS").unwrap();
    match &stmt.clauses[0] {
        Clause::DropIndex(d) => {
            assert_eq!(d.name, SchemaName::Parameter("param".to_string()));
            assert!(d.if_exists);
        }
        _ => panic!("expected drop index"),
    }
}

#[test]
fn test_parse_create_constraint_param_name() {
    let stmt = parse("CREATE CONSTRAINT $param FOR (n:Person) REQUIRE n.name IS UNIQUE").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateConstraint(c) => {
            assert_eq!(c.name, Some(SchemaName::Parameter("param".to_string())));
        }
        _ => panic!("expected create constraint"),
    }
}

#[test]
fn test_parse_drop_constraint_param_name() {
    let stmt = parse("DROP CONSTRAINT $param IF EXISTS").unwrap();
    match &stmt.clauses[0] {
        Clause::DropConstraint(d) => {
            assert_eq!(d.name, SchemaName::Parameter("param".to_string()));
            assert!(d.if_exists);
        }
        _ => panic!("expected drop constraint"),
    }
}

// CIP-137/CIP-11: a keyword like INDEX or CONSTRAINT can itself be used
// unescaped as the name of an index or constraint.
#[test]
fn test_parse_create_constraint_keyword_name() {
    let stmt =
        parse("CREATE CONSTRAINT constraint FOR (n:Label) REQUIRE (n.property) IS KEY").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateConstraint(c) => {
            assert_eq!(c.name, Some(SchemaName::Literal("constraint".to_string())));
        }
        _ => panic!("expected create constraint"),
    }
}

#[test]
fn test_parse_create_index_keyword_name() {
    let stmt = parse("CREATE INDEX index FOR (n:Label) ON (n.property)").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.name, Some(SchemaName::Literal("index".to_string())));
        }
        _ => panic!("expected create index"),
    }
}

// CIP-134 [19]/[20]: `$$param` (double-dollar) is not recognized
// parameter syntax and must be rejected as a parse error.
#[test]
fn test_parse_create_index_double_dollar_name_rejected() {
    assert!(parse("CREATE INDEX $$param FOR (n:Label) ON (n.property)").is_err());
}

#[test]
fn test_parse_drop_index_double_dollar_name_rejected() {
    assert!(parse("DROP INDEX $$param").is_err());
}

#[test]
fn test_parse_create_constraint_double_dollar_name_rejected() {
    assert!(
        parse("CREATE CONSTRAINT $$param FOR (n:Label) REQUIRE (n.property) IS NODE KEY").is_err()
    );
}

/// CIP-62: every `IS [entity] (UNIQUE|KEY)` keyword-surface spelling
/// parses to the plain `"unique"`/`"key"` constraint_type -- the entity
/// keyword itself (NODE/REL/RELATIONSHIP) is documentary only, since
/// the real entity comes from the FOR-clause target.
#[test]
fn test_parse_create_constraint_entity_keyword_surface() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.x IS NODE UNIQUE",
            "unique",
            "NODE",
        ),
        (
            "CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.x IS RELATIONSHIP UNIQUE",
            "unique",
            "RELATIONSHIP",
        ),
        (
            "CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.x IS REL UNIQUE",
            "unique",
            "RELATIONSHIP",
        ),
        (
            "CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.x IS RELATIONSHIP KEY",
            "key",
            "RELATIONSHIP",
        ),
        (
            "CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.x IS REL KEY",
            "key",
            "RELATIONSHIP",
        ),
        (
            "CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.x IS KEY",
            "key",
            "RELATIONSHIP",
        ),
        (
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.x IS KEY",
            "key",
            "NODE",
        ),
    ];
    for (query, want_type, want_entity) in cases {
        let stmt = parse(query).unwrap();
        match &stmt.clauses[0] {
            Clause::CreateConstraint(c) => {
                assert_eq!(c.constraint_type, *want_type, "{query}");
                assert_eq!(c.entity_type.as_str(), *want_entity, "{query}");
            }
            _ => panic!("expected create constraint for {query}"),
        }
    }
}

/// CIP-62/CIP-64/CIP-74: `SHOW <type filter> CONSTRAINTS` parses the
/// entity/kind filter surface, including the CIP-74 `PROPERTY` keyword.
#[test]
fn test_parse_show_constraints_type_filter() {
    let cases: &[(&str, ConstraintEntityFilter, Option<ConstraintKindFilter>)] = &[
        ("SHOW CONSTRAINTS", ConstraintEntityFilter::Any, None),
        ("SHOW ALL CONSTRAINTS", ConstraintEntityFilter::Any, None),
        (
            "SHOW UNIQUE CONSTRAINTS",
            ConstraintEntityFilter::Any,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW UNIQUENESS CONSTRAINTS",
            ConstraintEntityFilter::Any,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW NODE UNIQUE CONSTRAINTS",
            ConstraintEntityFilter::Node,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW RELATIONSHIP UNIQUENESS CONSTRAINTS",
            ConstraintEntityFilter::Relationship,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW REL UNIQUE CONSTRAINTS",
            ConstraintEntityFilter::Relationship,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW KEY CONSTRAINTS",
            ConstraintEntityFilter::Any,
            Some(ConstraintKindFilter::Key),
        ),
        (
            "SHOW REL KEY CONSTRAINTS",
            ConstraintEntityFilter::Relationship,
            Some(ConstraintKindFilter::Key),
        ),
        (
            "SHOW PROPERTY UNIQUENESS CONSTRAINTS",
            ConstraintEntityFilter::Any,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW NODE PROPERTY UNIQUE CONSTRAINTS",
            ConstraintEntityFilter::Node,
            Some(ConstraintKindFilter::Unique),
        ),
        (
            "SHOW RELATIONSHIP PROPERTY UNIQUENESS CONSTRAINT",
            ConstraintEntityFilter::Relationship,
            Some(ConstraintKindFilter::Unique),
        ),
    ];
    for (query, want_entity, want_kind) in cases {
        let stmt = parse(query).unwrap();
        match &stmt.clauses[0] {
            Clause::ShowConstraints(scc) => {
                assert_eq!(scc.type_filter.entity, *want_entity, "{query}");
                assert_eq!(scc.type_filter.kind, *want_kind, "{query}");
            }
            _ => panic!("expected show constraints for {query}"),
        }
    }
}

/// CIP-74: `PROPERTY` never combines with `KEY` and never stands alone.
#[test]
fn test_parse_show_constraints_property_filter_errors() {
    assert!(parse("SHOW PROPERTY KEY CONSTRAINTS").is_err());
    assert!(parse("SHOW PROPERTY CONSTRAINTS").is_err());
}

/// CIP-11: all four relationship-pattern direction spellings resolve to
/// the same RELATIONSHIP-entity index target.
#[test]
fn test_parse_create_index_relationship_pattern_all_directions() {
    for query in [
        "CREATE RANGE INDEX ri FOR ()-[r:KNOWS]-() ON (r.since)",
        "CREATE RANGE INDEX ri FOR ()-[r:KNOWS]->() ON (r.since)",
        "CREATE RANGE INDEX ri FOR ()<-[r:KNOWS]-() ON (r.since)",
    ] {
        let stmt = parse(query).unwrap();
        match &stmt.clauses[0] {
            Clause::CreateIndex(idx) => {
                assert_eq!(idx.entity_type, SchemaEntityType::Relationship, "{query}");
                assert_eq!(idx.labels, vec!["KNOWS".to_string()], "{query}");
                assert_eq!(idx.properties, vec!["since".to_string()], "{query}");
                assert_eq!(idx.kind, IndexKind::Range, "{query}");
            }
            _ => panic!("expected create index for {query}"),
        }
    }
}

/// CIP-38/CIP-235: `FOR (n:A|B)` multi-label disjunction on a DDL
/// target.
#[test]
fn test_parse_create_index_multi_label() {
    let stmt = parse("CREATE FULLTEXT INDEX fi FOR (n:Post|Article) ON EACH [n.title]").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.labels, vec!["Post".to_string(), "Article".to_string()]);
            assert_eq!(idx.kind, IndexKind::Fulltext);
            assert!(idx.on_each);
            assert_eq!(idx.properties, vec!["title".to_string()]);
        }
        _ => panic!("expected create index"),
    }
}

/// CIP-11 CR[10]: the paren-less single-property `ON n.prop` form.
#[test]
fn test_parse_create_index_paren_less_on() {
    let stmt = parse("CREATE TEXT INDEX ti FOR (n:Book) ON n.character").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateIndex(idx) => {
            assert_eq!(idx.properties, vec!["character".to_string()]);
            assert_eq!(idx.kind, IndexKind::Text);
        }
        _ => panic!("expected create index"),
    }
}

/// CIP-38 S8: `ON EACH [p.title, p.title]` is rejected as a duplicated
/// index key, not silently accepted.
#[test]
fn test_parse_create_fulltext_index_rejects_duplicate_on_each_property() {
    let err =
        parse("CREATE FULLTEXT INDEX fi FOR (p:Post) ON EACH [p.title, p.title]").unwrap_err();
    assert!(err.to_string().contains("duplicate"), "{err}");
}

/// CIP-38 S7: `ON EACH []` (empty list) is rejected.
#[test]
fn test_parse_create_fulltext_index_rejects_empty_on_each() {
    assert!(parse("CREATE FULLTEXT INDEX fi FOR (p:Post) ON EACH []").is_err());
}

/// CIP-11 CR[18]: `CREATE BTREE INDEX` is a hard syntax error, not
/// silently accepted as some other kind.
#[test]
fn test_parse_create_btree_index_rejected() {
    assert!(parse("CREATE BTREE INDEX bi FOR (n:Person) ON (n.name)").is_err());
}

/// CIP-11: `SHOW RANGE INDEXES` / `SHOW ALL INDEXES` parse with the
/// right kind filter.
#[test]
fn test_parse_show_indexes_kind_filter() {
    let stmt = parse("SHOW RANGE INDEXES YIELD name").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowIndexes(sic) => {
            assert_eq!(sic.kind_filter, Some(IndexKind::Range));
            assert_eq!(sic.yields.names().collect::<Vec<_>>(), vec!["name"]);
        }
        _ => panic!("expected show indexes"),
    }

    let stmt = parse("SHOW ALL INDEXES").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowIndexes(sic) => assert_eq!(sic.kind_filter, None),
        _ => panic!("expected show indexes"),
    }
}

// -- SHOW FUNCTIONS (CIP-245/CIP-247/CIP-263) --

#[test]
fn test_parse_show_functions_with_yield_where() {
    let stmt = parse("SHOW FUNCTIONS YIELD name WHERE name = 'ceiling' RETURN name").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowFunctions(sfc) => {
            assert_eq!(sfc.yields.names().collect::<Vec<_>>(), vec!["name"]);
            assert!(sfc.where_expr.is_some());
            assert!(!sfc.yields.all);
        }
        other => panic!("expected ShowFunctions, got {other:?}"),
    }
}

#[test]
fn test_parse_show_functions_bare() {
    let stmt = parse("SHOW FUNCTIONS").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    match &stmt.clauses[0] {
        Clause::ShowFunctions(sfc) => {
            assert_eq!(sfc.yields.mode(), YieldMode::Default);
            assert!(!sfc.yields.all);
            assert!(sfc.where_expr.is_none());
        }
        other => panic!("expected ShowFunctions, got {other:?}"),
    }
}

// -- SHOW USERS vs SHOW USER DEFINED FUNCTIONS dispatch carve-out (CIP-68) --

/// `SHOW USERS` / `SHOW USER` / `SHOW CURRENT USER` route to the users
/// command, never to the functions catch-all.
#[test]
fn test_parse_show_users_routing() {
    for query in ["SHOW USERS", "SHOW USER", "SHOW CURRENT USER"] {
        let stmt = parse(query).unwrap();
        assert_eq!(stmt.clauses.len(), 1, "{query}");
        match &stmt.clauses[0] {
            Clause::ShowUsers(suc) => {
                assert_eq!(suc.current_only, query == "SHOW CURRENT USER", "{query}");
            }
            other => panic!("expected ShowUsers for {query}, got {other:?}"),
        }
    }
}

/// `SHOW USER DEFINED FUNCTIONS` must fall through the users arm's
/// `!is_word(&next2, "defined")` guard and route to the functions command
/// with `FunctionScope::UserDefined`.
#[test]
fn test_parse_show_user_defined_functions_routing() {
    let stmt = parse("SHOW USER DEFINED FUNCTIONS").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    match &stmt.clauses[0] {
        Clause::ShowFunctions(sfc) => {
            assert_eq!(sfc.scope, FunctionScope::UserDefined);
        }
        other => panic!("expected ShowFunctions, got {other:?}"),
    }
}
