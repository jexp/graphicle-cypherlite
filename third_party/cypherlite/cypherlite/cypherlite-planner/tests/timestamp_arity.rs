//! `timestamp()` takes no arguments: Neo4j rejects `timestamp(1)` at compile
//! time with `InvalidNumberOfArguments`, and the planner's
//! `FIXED_ARITY_FUNCTIONS` table enforces the same at plan time. The check is
//! case-insensitive (the binder lowercases the call name) and fires on any
//! argument expression, `null` included.

fn planned(src: &str) -> Result<cypherlite_planner::LogicalPlan, cypherlite_core::Error> {
    let query = cypherlite_front::parse_and_lower(src)?;
    cypherlite_planner::plan(query, &mut |_| false)
}

#[test]
fn timestamp_with_arguments_is_rejected_at_plan_time() {
    for src in [
        "RETURN timestamp(1) AS t",
        "RETURN timestamp(null) AS t",
        "RETURN TIMESTAMP(1, 2) AS t",
    ] {
        match planned(src) {
            Err(cypherlite_core::Error::Syntax(msg)) => assert!(
                msg.contains("InvalidNumberOfArguments"),
                "{src}: expected InvalidNumberOfArguments, got: {msg}"
            ),
            other => panic!("{src}: expected Syntax error, got: {other:?}"),
        }
    }
}

#[test]
fn timestamp_with_no_arguments_plans() {
    assert!(planned("RETURN timestamp() AS t").is_ok());
}
