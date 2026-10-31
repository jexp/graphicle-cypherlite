//! Canonical-registry binding tests: the planner consumes the single
//! SHOW-column table and known-function registry owned by `cypherlite-front`
//! instead of keeping local copies, so what front validation accepts and
//! what `SHOW FUNCTIONS` advertises is exactly what the binder accepts.

fn planned(src: &str) -> Result<cypherlite_planner::LogicalPlan, cypherlite_core::Error> {
    let query = cypherlite_front::parse_and_lower(src)?;
    cypherlite_planner::plan(query, &mut |_| false)
}

#[test]
fn show_indexes_where_state_is_a_default_column() {
    // `state` is a real default column of `SHOW INDEXES`; a bare WHERE on it
    // used to be rejected by front's drifted column table.
    planned("SHOW INDEXES WHERE state = 'ONLINE' RETURN name").unwrap();
    planned("SHOW INDEXES WHERE id IS NOT NULL RETURN name").unwrap();
}

#[test]
fn show_constraints_where_id_is_a_default_column() {
    planned("SHOW CONSTRAINTS WHERE id IS NOT NULL RETURN name").unwrap();
}

#[test]
fn show_indexes_where_options_is_yield_only() {
    // `options` is only reachable via an explicit YIELD, so a bare WHERE on
    // it must be rejected -- front, planner, and executor agree on this.
    let err = planned("SHOW INDEXES WHERE options IS NULL RETURN name")
        .unwrap_err()
        .to_string();
    assert!(err.contains("UndefinedVariable"), "{err}");
    // ...and with the YIELD it compiles.
    planned("SHOW INDEXES YIELD options RETURN options").unwrap();
}

#[test]
fn load_csv_linenumber_is_a_known_function() {
    // `linenumber` is advertised by `SHOW FUNCTIONS` (front's
    // KNOWN_FUNCTIONS); the planner's old private copy rejected it.
    planned("LOAD CSV FROM 'file:///tmp/data.csv' AS line RETURN linenumber()").unwrap();
}

#[test]
fn planner_accepts_every_advertised_nullary_function() {
    // Systemic drift guard: every advertised function name (the `SHOW
    // FUNCTIONS` source) must bind when called in a plausible shape. Names
    // that need arguments are called with a single literal; the point is the
    // known-function check, not arity.
    for name in cypherlite_front::KNOWN_FUNCTIONS {
        for call in [format!("RETURN {name}()"), format!("RETURN {name}(1)")] {
            let query = match cypherlite_front::parse_and_lower(&call) {
                Ok(query) => query,
                // Front-level semantic rules (arity, aggregate position,
                // argument types) are allowed to reject; only the binder's
                // UnknownFunction would prove registry drift.
                Err(_) => continue,
            };
            if let Err(err) = cypherlite_planner::plan(query, &mut |_| false) {
                assert!(
                    !err.to_string().contains("UnknownFunction")
                        && !err.to_string().contains("unknown function"),
                    "advertised function {name} rejected by the binder: {err}"
                );
            }
        }
    }
}

#[test]
fn binder_accepts_lowered_internal_names() {
    // `toString` over a GQL temporal constructor lowers to the internal
    // `tostring_full`; the binder must accept the lowered spelling even
    // though users cannot write it.
    planned("RETURN toString(local_time('12:00')) AS t").unwrap();
}

#[test]
fn aggregate_aliases_bind_as_aggregates() {
    // The CIP-128 snake_case spellings come from the single alias table in
    // `front::agg`; the planner must see them as aggregates (so they may
    // not nest) and as known functions.
    planned("UNWIND [1, 2, 3] AS x RETURN percentile_cont(x, 0.5)").unwrap();
    planned("UNWIND [1, 2, 3] AS x RETURN collect_list(x)").unwrap();
    let err = planned("UNWIND [1, 2] AS x RETURN sum(stddev_samp(x))")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ggregat") || err.contains("InvalidFunctionUse") || err.contains("nested"),
        "nested aggregate must be rejected: {err}"
    );
}
