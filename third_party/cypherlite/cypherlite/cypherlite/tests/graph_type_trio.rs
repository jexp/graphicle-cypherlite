//! The CIP graph-type trio behind the ADR-0371 preview gates:
//! CIP-283 optional properties (`graph-type-opt-props`), CIP-262
//! property default values (`graph-type-default-values`), and CIP-266
//! implied labels identifying node element types --
//! `graph-type-implied-labels` (inheritance). Expectations are
//! doc-derived: the CIPs' own example tables (the first implementations
//! are not in a release, so there is no live instance to verify against),
//! adjusted only where this engine's statement-is-the-transaction commit
//! boundary names itself (NOTES.md in the cip-eval scenario directories).
//!
//! The gates are process-level (ADR-0371: `Database::open` resolves
//! `CYPHERLITE_PREVIEW` once), and an integration-test crate is its own
//! process -- set the three features before any store opens. The
//! ungated fail-closed errors are pinned in `tests/graph_type.rs` (a
//! separate, ungated process).

mod common;

use cypherlite::Value;

use common::{connection, err_text, run, scalar_one, string};

/// The gated connection: the trio's three preview features, set before
/// any store opens (fail loudly rather than run half-gated, the same
/// contract as the CIP-278 hybrid-search fixture).
fn gated_connection() -> cypherlite::Connection {
    common::set_preview_once(
        "graph-type-opt-props,graph-type-default-values,graph-type-implied-labels",
    );
    connection()
}

fn spec_of(c: &mut cypherlite::Connection) -> Value {
    scalar_one(c, "SHOW CURRENT GRAPH TYPE")
}

/// The specification string (the caller pinned a `Value::String`).
fn spec_string(c: &mut cypherlite::Connection) -> String {
    let Value::String(spec) = spec_of(c) else {
        panic!("expected a specification string");
    };
    spec.to_string()
}

// ---------------------------------------------------------------------
// CIP-283: optional properties (`prop :: ANY`)
// ---------------------------------------------------------------------

#[test]
fn optional_properties_set_show_and_drop() {
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {prop :: ANY}),
            ()-[:REL_TYPE => {prop1 :: ANY, prop2 :: ANY, prop3 :: ANY}]->()
        }",
    );
    let spec = spec_string(&mut c);
    assert!(spec.contains(":`Label` => {`prop` :: ANY}"), "{spec}");
    assert!(
        spec.contains("[:`REL_TYPE` => {`prop1` :: ANY, `prop2` :: ANY, `prop3` :: ANY}]"),
        "{spec}"
    );

    // PE3: the key-constraint shorthand works on an optional property
    // (on ADD: the engine's ALTER verb does not accept constraint specs).
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE ADD { (:K => {prop :: ANY IS KEY}) }",
    );
    let spec = spec_string(&mut c);
    assert!(spec.contains(":`K` => {`prop` :: ANY}"), "{spec}");

    // PE6: dropping restates the full definition or the bare label.
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE DROP { (:K =>), (:Label =>), ()-[:REL_TYPE =>]->() }",
    );
    // The K type's inline KEY constraint survives the element-type drop
    // as the body's own (undesignated) constraint; the element types are
    // gone.
    let spec = spec_string(&mut c);
    assert!(
        spec.contains(
            "CONSTRAINT `constraint_key_K_prop` FOR (`n`:`K`) REQUIRE (`n`.`prop`) IS KEY"
        ),
        "{spec}"
    );
    assert!(!spec.contains("=>"), "no element type remains: {spec}");
}

#[test]
fn an_optional_property_restricts_nothing() {
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {prop :: ANY}) }",
    );
    // The ANY property type restricts nothing: any storable value fits,
    // and an absent property violates nothing (open graph type
    // semantics).
    run(
        &mut c,
        "CREATE (:Label {prop: 42}), (:Label {prop: 'forty-two'})",
    );
    run(&mut c, "CREATE (:Label)");
    let count = scalar_one(&mut c, "MATCH (n:Label) RETURN count(n)");
    assert_eq!(count, Value::Int(3));
}

#[test]
fn optional_properties_inherit_and_restrict() {
    // PE5 (CIP-283) over CIP-266 inheritance: an ANY-typed parent
    // property is restricted by the child.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label1 => {prop :: ANY}),
            (:Label2 => :Label1 {prop :: STRING}),
            (:Label3 => :Label1 {prop :: ANY NOT NULL}),
            (:Label4 => :Label1 {prop :: STRING NOT NULL})
        }",
    );
    run(&mut c, "CREATE (:Label1 {prop: true})");
    run(&mut c, "CREATE (:Label1:Label2 {prop: 'child'})");
    // Label3/Label4 require the property's existence.
    let err = err_text(c.query("CREATE (:Label3)"));
    assert!(err.contains("requires properties"), "{err}");
    run(&mut c, "CREATE (:Label1:Label4 {prop: 'x'})");
    // Label2's restriction is enforced on its own nodes.
    let err = err_text(c.query("CREATE (:Label1:Label2 {prop: 42})"));
    assert!(err.contains("property type"), "{err}");
}

// ---------------------------------------------------------------------
// CIP-262: property default values (`DEFAULT ON COMMIT`)
// ---------------------------------------------------------------------

#[test]
fn default_values_show_canonically() {
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()}),
            ()-[:REL_TYPE => {prop :: STRING DEFAULT ON COMMIT \"Hello\"}]->()
        }",
    );
    let spec = spec_string(&mut c);
    assert!(
        spec.contains("`id` :: UUID NOT NULL DEFAULT ON COMMIT uuid()"),
        "{spec}"
    );
    // ME5: the canonical string literal is single-quoted.
    assert!(
        spec.contains("`prop` :: STRING DEFAULT ON COMMIT 'Hello'"),
        "{spec}"
    );
}

#[test]
fn a_created_node_gets_the_default_at_commit_not_before() {
    // CE1 (CIP-262): the default lands at the statement boundary -- the
    // creating statement's own RETURN still sees null; the next
    // statement sees the generated value.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()}) }",
    );
    let seen = scalar_one(&mut c, "CREATE (n:Label {}) RETURN n.id");
    assert_eq!(seen, Value::Null);
    let id = scalar_one(&mut c, "MATCH (n:Label) RETURN n.id");
    let rendered = format!("{id}");
    // The engine renders a UUID as `uuid('…')` (44 characters).
    assert!(
        rendered.starts_with("uuid('") && rendered.len() == 44,
        "a generated UUID value: {rendered}"
    );
}

#[test]
fn an_explicit_value_wins_and_an_explicit_null_takes_the_default() {
    // CE2/CE3: an explicit non-null value is kept; an explicit null is
    // the same as an absent property -- the default applies at commit.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {id :: INTEGER DEFAULT ON COMMIT 42}) }",
    );
    let seen = scalar_one(&mut c, "CREATE (n:Label {id: 7}) RETURN n.id");
    assert_eq!(seen, Value::Int(7));
    let seen = scalar_one(&mut c, "CREATE (n:Label {id: null}) RETURN n.id");
    assert_eq!(seen, Value::Null);
    let after = scalar_one(&mut c, "MATCH (n:Label) WHERE n.id = 42 RETURN count(n)");
    assert_eq!(after, Value::Int(1));
}

#[test]
fn a_relationship_type_default_applies_on_create() {
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            ()-[:REL_TYPE => {prop :: STRING DEFAULT ON COMMIT \"Hello\"}]->()
        }",
    );
    let seen = scalar_one(&mut c, "CREATE ()-[r:REL_TYPE]->() RETURN r.prop");
    assert_eq!(seen, Value::Null);
    let after = scalar_one(&mut c, "MATCH ()-[r:REL_TYPE]->() RETURN r.prop");
    assert_eq!(after, string("Hello"));
    // CE2: an explicit value is kept.
    let seen = scalar_one(
        &mut c,
        "CREATE ()-[r:REL_TYPE {prop: 'Hi'}]->() RETURN r.prop",
    );
    assert_eq!(seen, string("Hi"));
}

#[test]
fn adding_the_identifying_label_applies_the_default() {
    // CE4/CE5 (CIP-262): SET n:IdentifyingLabel on a node without the
    // property applies the default; with the property it does not.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => {foo :: STRING NOT NULL DEFAULT ON COMMIT \"Hi\"}) }",
    );
    run(&mut c, "CREATE (n:B {foo: 'Hello!'})");
    run(&mut c, "CREATE (n:B)");
    // CE4/CE5: the statement's own RETURN still shows the pre-existing
    // value and null; the defaulted value only shows after the boundary.
    let seen = rows_of(&mut c, "MATCH (n:B) SET n:A RETURN n.foo");
    assert_eq!(seen, vec![string("Hello!"), Value::Null]);
    let after = rows_of(&mut c, "MATCH (n:A:B) RETURN n.foo");
    assert_eq!(after, vec![string("Hello!"), string("Hi")]);
}

#[test]
fn removing_the_property_in_the_same_statement_is_ignored() {
    // CE10 (CIP-262): SET n:A together with REMOVE n.foo (either order)
    // leaves the default to apply at the boundary.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => {foo :: STRING NOT NULL DEFAULT ON COMMIT \"Hi\"}) }",
    );
    run(&mut c, "CREATE (n:B {foo: 'Hello!'})");
    let seen = scalar_one(&mut c, "MATCH (n:B) SET n:A REMOVE n.foo RETURN n.foo");
    assert_eq!(seen, Value::Null);
    let after = scalar_one(&mut c, "MATCH (n:A:B) RETURN n.foo");
    assert_eq!(after, string("Hi"));
}

#[test]
fn a_later_removal_sticks_when_the_type_allows_null() {
    // CE7 (CIP-262): outside the creating statement, removing (or
    // nulling) the property sticks -- the default never backfills an
    // untouched label's nodes.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            ()-[:R => {foo :: STRING DEFAULT ON COMMIT \"Hello\"}]->()
        }",
    );
    run(&mut c, "CREATE ()-[r:R {foo: 'Hi!'}]->()");
    run(&mut c, "MATCH ()-[r:R]->() SET r.foo = null");
    let after = scalar_one(&mut c, "MATCH ()-[r:R]->() RETURN r.foo");
    assert_eq!(after, Value::Null);
    run(&mut c, "MATCH ()-[r:R]->() REMOVE r.foo");
    let after = scalar_one(&mut c, "MATCH ()-[r:R]->() RETURN r.foo");
    assert_eq!(after, Value::Null);
}

#[test]
fn default_values_on_existing_data_are_not_backfilled() {
    // ME6/ME7 (CIP-262): setting a default value never touches existing
    // nodes; with a null-allowing type the update succeeds over nodes
    // missing the property.
    let mut c = gated_connection();
    run(&mut c, "CREATE (:Label {prop: 'Hi'})");
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {prop :: STRING DEFAULT ON COMMIT 'Hello'}) }",
    );
    let value = scalar_one(&mut c, "MATCH (n:Label) RETURN n.prop");
    assert_eq!(value, string("Hi"));
}

#[test]
fn inherited_defaults_flow_down_and_child_declarations_override() {
    // ME10/CE8/CE9 (CIP-262 over CIP-266): an implicitly inherited
    // default applies to the child's nodes; a re-specified default wins.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label1 => {prop :: INTEGER DEFAULT ON COMMIT 1}),
            (:Label2 => :Label1 {prop :: INTEGER})
        }",
    );
    let seen = scalar_one(&mut c, "CREATE (n:Label1:Label2 {}) RETURN n.prop");
    assert_eq!(seen, Value::Null);
    let after = scalar_one(&mut c, "MATCH (n:Label2) RETURN n.prop");
    assert_eq!(after, Value::Int(1));
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE ALTER {
            (:Label2 => :Label1 {prop :: INTEGER DEFAULT ON COMMIT 2})
        }",
    );
    run(&mut c, "CREATE (n:Label1:Label2 {})");
    let after = scalar_one(&mut c, "MATCH (n:Label2) WHERE n.prop = 2 RETURN count(n)");
    assert_eq!(after, Value::Int(1));
}

#[test]
fn default_value_failures_fail_closed() {
    let mut c = gated_connection();
    // FE1: the default must conform to the declared value type.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {prop :: STRING DEFAULT ON COMMIT 42}) }",
    ));
    assert!(
        err.contains("Invalid input 42 for default property value")
            && err.contains("Expected to be STRING"),
        "{err}"
    );
    // FE2: null is not a default.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {prop :: STRING DEFAULT ON COMMIT null}) }",
    ));
    assert!(
        err.contains("Invalid input null for default property value"),
        "{err}"
    );
    // FE3: a general expression is not a default.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {prop :: STRING DEFAULT ON COMMIT toString(5 + $param)})
        }",
    ));
    assert!(
        err.contains("expected a literal or a type constructor expression"),
        "{err}"
    );
    // CIP-283 NE3: a map is not storable on a property even under ANY.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {prop :: ANY DEFAULT ON COMMIT {key: 'nope'}})
        }",
    ));
    assert!(err.contains("cannot be stored on a property"), "{err}");
    // uuid() takes no arguments in a default (CIP-262 Ap1).
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {id :: UUID DEFAULT ON COMMIT uuid('known')})
        }",
    ));
    assert!(err.contains("uuid() takes no arguments"), "{err}");
}

#[test]
fn a_drop_must_restate_the_default_value() {
    // FE4 (CIP-262): dropping by the full definition without the default
    // fails; with it, and by the bare label, it drops.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()}) }",
    );
    let err =
        err_text(c.query("ALTER CURRENT GRAPH TYPE DROP { (:Label => {id :: UUID NOT NULL}) }"));
    assert!(
        err.contains("node element type specified incorrectly"),
        "{err}"
    );
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE DROP {
            (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()})
        }",
    );
    assert_eq!(spec_of(&mut c), string("{}"));
}

#[test]
fn two_declaring_parents_without_a_child_default_fail() {
    // FE8 (CIP-262): implicitly inheriting two defaults for the same
    // property (identical values included) fails at declaration time.
    let mut c = gated_connection();
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label1 => {prop :: STRING DEFAULT ON COMMIT 'Hi'}),
            (:Label2 => {prop :: STRING DEFAULT ON COMMIT 'Hello'}),
            (:Label3 => :Label1&:Label2)
        }",
    ));
    assert!(
        err.contains("multiple default values coming from multiple implied node element types"),
        "{err}"
    );
}

#[test]
fn restricting_a_type_under_an_inherited_default_must_keep_it_valid() {
    // FE9 (CIP-262): the child's restriction must not invalidate the
    // inherited default.
    let mut c = gated_connection();
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label1 => {prop :: INTEGER | FLOAT DEFAULT ON COMMIT 1}),
            (:Label2 => :Label1 {prop :: FLOAT})
        }",
    ));
    assert!(
        err.contains("Invalid input 1 for default property value")
            && err.contains("Expected to be FLOAT"),
        "{err}"
    );
}

#[test]
fn a_node_with_two_declaring_labels_fails_without_an_explicit_value() {
    // FE10 (CIP-262): creating a node whose labels each declare a default
    // for the same property fails unless the property is explicitly set.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label1 => {prop :: STRING NOT NULL DEFAULT ON COMMIT 'Hi'}),
            (:Label2 => {prop :: STRING NOT NULL DEFAULT ON COMMIT 'Hello'})
        }",
    );
    let err = err_text(c.query("CREATE (:Label1:Label2)"));
    assert!(
        err.contains("more than one default value for the property"),
        "{err}"
    );
    // An explicit value settles it.
    run(&mut c, "CREATE (:Label1:Label2 {prop: 'settled'})");
}

// ---------------------------------------------------------------------
// CIP-266: implied labels identifying node element types (inheritance)
// ---------------------------------------------------------------------

#[test]
fn implicit_inheritance_declares_and_enforces_the_implied_label() {
    // PAGTE2/GE1/GE2 (CIP-266): the child inherits the parent's rules;
    // its nodes must carry the parent's implied labels and properties.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => :B&C {p1 :: STRING, p2 :: ANY NOT NULL, p3 :: INTEGER NOT NULL}),
            (:D => :A)
        }",
    );
    run(
        &mut c,
        "CREATE (:A:B:C {p1: 'x', p2: 'y', p3: 1}), (:A:B:C:D {p1: 'x', p2: 'y', p3: 2})",
    );
    // GE2: missing an inherited implied label fails.
    let debug_constraints = common::rows(&mut c, "SHOW CONSTRAINTS YIELD name RETURN name");
    println!("constraints: {debug_constraints:?}");
    let debug_err = c.query("CREATE (:A:B:D {p1: 'x', p2: 'y', p3: 3})");
    println!("GE2 outcome: {debug_err:?}");
    let err = err_text(debug_err);
    assert!(err.contains("must also have label"), "{err}");
    // GE3: missing an inherited property fails.
    let err = err_text(c.query("CREATE (:A:B:C {p1: 'x', p3: 3})"));
    assert!(err.contains("requires properties"), "{err}");
}

#[test]
fn a_child_restriction_is_enforced_beyond_the_parent_rules() {
    // PAGTE3/GE4 (CIP-266): a further restriction on the child applies to
    // the child's own nodes.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => {p :: INTEGER | FLOAT}),
            (:E => :A&B {p :: FLOAT NOT NULL, p2 :: BOOLEAN})
        }",
    );
    let err = err_text(c.query("CREATE (:A:B:E {p: 1})"));
    assert!(err.contains("property type"), "{err}");
    run(&mut c, "CREATE (:A:B:E {p: 1.5, p2: false})");
}

#[test]
fn inheritance_chains_and_multiple_parents() {
    // PAGTE4/PAGTE9 (CIP-266): chained inheritance and the diamond,
    // with the intersection table from PAGTE9.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => :B),
            (:C => :A),
            (:D => :C)
        }",
    );
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE ADD {
            (:L1 => {p1 :: STRING, p2 :: ANY NOT NULL, p4 :: FLOAT | INTEGER}),
            (:L2 => {p2 :: INTEGER, p3 :: ANY NOT NULL, p4 :: ANY NOT NULL}),
            (:E => :L1&:L2 {p3 :: BOOLEAN NOT NULL, p4 :: FLOAT NOT NULL})
        }",
    );
    // The effective intersection for p2 on E is INTEGER NOT NULL.
    let err = err_text(c.query("CREATE (:L1:L2:E {p1: 'x', p3: true, p4: 1.5, p2: 'no'})"));
    assert!(err.contains("property type"), "{err}");
    run(
        &mut c,
        "CREATE (:L1:L2:E {p1: 'x', p2: 2, p3: true, p4: 1.5})",
    );
}

#[test]
fn inheritance_failures_fail_closed() {
    let mut c = gated_connection();
    // NAGTE3: cycles.
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE SET { (:A => :B), (:B => :A) }"));
    assert!(err.contains("circular inheritance"), "{err}");
    // The self-reference keeps its historical Rule 7 own-set rejection
    // ("identifying label repeated in implied label set"): both errors
    // fail closed; the wording is an engine-consistent choice (NOTES.md).
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE SET { (:A => :A) }"));
    assert!(
        err.contains("circular inheritance")
            || err.contains("identifying label repeated in implied label set"),
        "{err}"
    );
    // NAGTE1: a conflicting child declaration.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => {p :: STRING}),
            (:B => :A {p :: INTEGER})
        }",
    ));
    assert!(
        err.contains("is not allowed as it is either in conflict with or more permissive"),
        "{err}"
    );
    // NAGTE2: a loosening child declaration.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => {p :: STRING NOT NULL}),
            (:B => :A {p :: STRING})
        }",
    ));
    assert!(
        err.contains("is not allowed as it is either in conflict with or more permissive"),
        "{err}"
    );
    // NAGTE5: an implicitly conflicting diamond.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => {p :: STRING}),
            (:B => {p :: INTEGER}),
            (:C => :A&B)
        }",
    ));
    assert!(
        err.contains("coming from the intersection of property definitions"),
        "{err}"
    );
}

#[test]
fn unrenderable_restated_drop_defaults_fail_typed_not_panic() {
    // Stellar round 1 (the false-invariant expect): a DROP body never
    // runs the graph-type body validator, so an unrenderable restated
    // default (a function call no default shape admits) must surface as
    // the exact-match typed error, never a renderer panic.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE ADD { (:Person => {id :: UUID DEFAULT ON COMMIT uuid(), name :: STRING}) }",
    );
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE DROP { (:Person => {id :: UUID DEFAULT ON COMMIT rand()}) }",
    ));
    assert!(
        err.contains("node element type specified incorrectly"),
        "expected the exact-match typed error, got: {err}"
    );
}

#[test]
fn an_inherited_node_element_type_cannot_be_dropped() {
    // NAGTE4 (CIP-266): dropping the parent without the child fails.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => {p :: STRING}), (:B => :A) }",
    );
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE DROP { (:A =>) }"));
    assert!(
        err.contains("node element type is still referenced and cannot be dropped"),
        "{err}"
    );
    // PAGTE8: dropping both, in one command, works.
    run(&mut c, "ALTER CURRENT GRAPH TYPE DROP { (:B =>), (:A =>) }");
    assert_eq!(spec_of(&mut c), string("{}"));
}

#[test]
fn adding_a_type_on_an_implied_label_is_inheritance() {
    // PAGTE6 (CIP-266): ADD of a type whose label another type already
    // implies turns the implication into inheritance -- allowed behind
    // the gate (the ungated store fails closed; pinned in
    // `tests/graph_type.rs`).
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => :B {p1 :: STRING}) }",
    );
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE ADD { (:B => {p2 :: INTEGER}) }",
    );
    // B's rules now apply to A's nodes: a nullable type allows an absent
    // property, but a wrong-typed one fails.
    run(&mut c, "CREATE (:A:B)");
    let err = err_text(c.query("CREATE (:A:B {p2: 'no'})"));
    assert!(err.contains("property type"), "{err}");
    run(&mut c, "CREATE (:A:B {p1: 'x', p2: 1})");
}

#[test]
fn show_renders_only_the_child_declarations() {
    // SGTE1 (CIP-266): the canonical specification keeps the child's own
    // declarations only -- inherited definitions are not inlined.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => :B&C {p1 :: STRING, p2 :: ANY NOT NULL, p3 :: INTEGER NOT NULL}),
            (:D => :C),
            (:F => :A&D {p1 :: STRING NOT NULL, p4 :: BOOLEAN})
        }",
    );
    let spec = spec_string(&mut c);
    assert!(spec.contains(":`D` => :`C`"), "{spec}");
    // F re-specifies p1 and adds p4 only: the inherited p2/p3 (from A)
    // and p1's nullable parent form are not inlined.
    assert!(
        spec.contains(":`F` => :`A`&`D` {`p1` :: STRING NOT NULL, `p4` :: BOOLEAN}"),
        "{spec}"
    );
    assert!(
        !spec.contains(":`F` => :`A`&`D` {`p1` :: STRING NOT NULL, `p2`"),
        "the inherited p2 is not inlined: {spec}"
    );
}

#[test]
fn show_as_graph_targets_the_parent_element_type_node() {
    // SGTE2 (CIP-266): an implied label that identifies a node element
    // type has its IMPLIES relationship go to the NodeElementType node,
    // not a separate NodeLabel node.
    let mut c = gated_connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => :B {p1 :: STRING}), (:D => :A) }",
    );
    let value = scalar_one(
        &mut c,
        "SHOW CURRENT GRAPH TYPE AS GRAPH YIELD nodes RETURN nodes",
    );
    let Value::List(nodes) = value else {
        panic!("expected a node list");
    };
    let mut element_labels = Vec::new();
    for node in nodes.iter() {
        let Value::Node(entity) = node else {
            continue;
        };
        if entity.labels.iter().any(|l| l == "NodeElementType") {
            let Some(Value::String(label)) = entity.properties.get("label") else {
                panic!("element-type node misses its label: {entity:?}");
            };
            element_labels.push(label.to_string());
        }
    }
    // A and D are element types; B (identifying no node element type
    // here) stays a NodeLabel node.
    element_labels.sort();
    assert_eq!(element_labels, vec!["A", "D"], "{nodes:?}");
}

/// The first column of every row, as a flat value list.
fn rows_of(c: &mut cypherlite::Connection, query: &str) -> Vec<Value> {
    common::rows(c, query)
        .into_iter()
        .map(|row| row.into_iter().next().expect("one column"))
        .collect()
}

// ---------------------------------------------------------------------
// ADR-0388 review (LABS-386): the depth cap and persisted replay
// ---------------------------------------------------------------------

#[test]
fn deep_inheritance_chains_fail_typed_not_abort() {
    // A flat comma-list chain of element types never trips the parser's
    // nesting-depth guard, so without the fold's own cap a ~25k-link
    // body would overflow the 8 MiB stacks of the HTTP/MCP/CLI threads
    // and abort the process. The cap fails the statement typed.
    let mut query = String::from("ALTER CURRENT GRAPH TYPE SET { (:N0 => :N1)");
    for i in 1..5000 {
        query.push_str(&format!(", (:N{i} => :N{})", i + 1));
    }
    query.push_str(" }");
    let mut c = gated_connection();
    let err = err_text(c.query(&query));
    assert!(
        err.contains("maximum inheritance depth of 500 exceeded"),
        "expected the typed depth error, got: {err}"
    );
}
