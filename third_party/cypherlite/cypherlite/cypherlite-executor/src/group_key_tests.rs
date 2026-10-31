//! Behavior tests pinning one equivalence relation across all five
//! dedup/grouping sites: `RETURN DISTINCT`, `GROUP BY`, `UNION` dedup,
//! `count(DISTINCT ...)`, and `coll.distinct(...)` — all keyed on the typed
//! [`crate::group_key::GroupKey`].
//!
//! Chosen semantics (the Int/Float rule is the exact
//! `cypherlite_model::canonical_key` comparison shared with `=`/`<`
//! evaluation; ground-truthed against live Neo4j):
//!
//! * numbers are equivalent by mathematical value: `1 ≡ 1.0` (and
//!   `0.0 ≡ -0.0`), recursively inside lists/maps/vectors;
//! * `null ≡ null`;
//! * **every NaN is its own equivalence class — never equivalent, even to
//!   another NaN**. So `UNWIND [0.0/0.0, 0.0/0.0] AS x ...` yields 2 rows /
//!   2 groups / `count = 2` / a 2-element list at *every* site. This is the
//!   one place where equivalence intentionally diverges from the pre-GroupKey
//!   string keys (which collapsed all NaNs to `"NaN"`).

#[cfg(test)]
mod tests {
    use crate::exec::test_support::exec_cypher;
    use crate::graph::Graph;
    use cypherlite_core::Value;

    #[test]
    fn int_and_float_unify_at_all_five_sites() {
        let g = Graph::default();

        // 1. RETURN DISTINCT: 1 ≡ 1.0 -> one row, first-seen representation.
        let r = exec_cypher(&g, "UNWIND [1, 1.0] AS x RETURN DISTINCT x");
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

        // 2. UNION dedup: one row, first-seen representation.
        let r = exec_cypher(&g, "RETURN 1 AS x UNION RETURN 1.0 AS x");
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

        // 3. GROUP BY: one group of two rows.
        let r = exec_cypher(&g, "UNWIND [1, 1.0] AS x RETURN x, count(*)");
        assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Int(2)]]);

        // 4. Aggregate DISTINCT: one distinct value.
        let r = exec_cypher(&g, "UNWIND [1, 1.0] AS x RETURN count(DISTINCT x)");
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

        // 5. coll.distinct: one element.
        let r = exec_cypher(&g, "RETURN coll.distinct([1, 1.0]) AS d");
        assert_eq!(r.rows, vec![vec![Value::List(vec![Value::Int(1)].into())]]);
    }

    #[test]
    fn distinct_keeps_first_seen_representation() {
        let g = Graph::default();
        let r = exec_cypher(&g, "UNWIND [1.0, 1] AS x RETURN DISTINCT x");
        assert_eq!(r.rows, vec![vec![Value::Float(1.0)]]);
    }

    #[test]
    fn nan_is_its_own_equivalence_class_at_all_five_sites() {
        // Semantics: every NaN occurrence is distinct, never equivalent even
        // to another NaN (the shared exact-rule module's Neo4j ground truth).
        // All five sites must agree.
        let g = Graph::default();

        let r = exec_cypher(&g, "UNWIND [0.0/0.0, 0.0/0.0] AS x RETURN DISTINCT x");
        assert_eq!(r.rows.len(), 2, "RETURN DISTINCT keeps both NaNs");

        let r = exec_cypher(&g, "RETURN 0.0/0.0 AS x UNION RETURN 0.0/0.0 AS x");
        assert_eq!(r.rows.len(), 2, "UNION keeps both NaNs");

        let r = exec_cypher(&g, "UNWIND [0.0/0.0, 0.0/0.0] AS x RETURN x, count(*)");
        assert_eq!(r.rows.len(), 2, "GROUP BY yields two NaN groups");
        assert!(
            r.rows.iter().all(|row| row[1] == Value::Int(1)),
            "each NaN group has one row: {:?}",
            r.rows
        );

        let r = exec_cypher(
            &g,
            "UNWIND [0.0/0.0, 0.0/0.0] AS x RETURN count(DISTINCT x)",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(2)]]);

        let r = exec_cypher(&g, "RETURN coll.distinct([0.0/0.0, 0.0/0.0]) AS d");
        assert_eq!(r.rows.len(), 1);
        let Value::List(items) = &r.rows[0][0] else {
            panic!("expected list, got {:?}", r.rows[0][0]);
        };
        assert_eq!(items.len(), 2, "coll.distinct keeps both NaNs");
        assert!(items
            .iter()
            .all(|v| matches!(v, Value::Float(f) if f.is_nan())));
    }

    #[test]
    fn null_is_one_equivalence_class_at_all_sites() {
        let g = Graph::default();

        let r = exec_cypher(&g, "UNWIND [null, null] AS x RETURN DISTINCT x");
        assert_eq!(r.rows, vec![vec![Value::Null]]);

        let r = exec_cypher(&g, "RETURN null AS x UNION RETURN null AS x");
        assert_eq!(r.rows, vec![vec![Value::Null]]);

        let r = exec_cypher(&g, "UNWIND [null, null] AS x RETURN x, count(*)");
        assert_eq!(r.rows, vec![vec![Value::Null, Value::Int(2)]]);

        // count(DISTINCT x) ignores nulls per openCypher.
        let r = exec_cypher(&g, "UNWIND [null, null] AS x RETURN count(DISTINCT x)");
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);

        let r = exec_cypher(&g, "RETURN coll.distinct([null, null]) AS d");
        assert_eq!(r.rows, vec![vec![Value::List(vec![Value::Null].into())]]);
    }

    #[test]
    fn nested_numeric_equivalence_in_lists_and_maps() {
        let g = Graph::default();
        let r = exec_cypher(&g, "UNWIND [[1], [1.0]] AS x RETURN DISTINCT x");
        assert_eq!(r.rows.len(), 1);
        let r = exec_cypher(&g, "UNWIND [{a: 1}, {a: 1.0}] AS x RETURN x, count(*)");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][1], Value::Int(2));
    }

    #[test]
    fn strings_never_alias_lists_or_other_strings() {
        let g = Graph::default();
        // The old string key joined row columns on "\u{1e}"; a value
        // containing the separator must not alias a two-column row.
        let r = exec_cypher(
            &g,
            "UNWIND ['a,b', ['a', 'b']] AS x RETURN count(DISTINCT x)",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
        let r = exec_cypher(
            &g,
            "UNWIND [\"a'b\", 'a\\'b'] AS x RETURN count(DISTINCT x)",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    }

    #[test]
    fn temporal_instants_group_by_instant_not_offset() {
        let g = Graph::default();
        // 12:00+01:00 and 13:00+02:00 denote the same instant; equivalence
        // follows Temporal::compare (and hence UNION/eq_non_null).
        let r = exec_cypher(
            &g,
            "UNWIND [time('12:00+01:00'), time('13:00+02:00')] AS x RETURN count(DISTINCT x)",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
        let r = exec_cypher(
            &g,
            "RETURN time('12:00+01:00') AS x UNION RETURN time('13:00+02:00') AS x",
        );
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn union_all_keeps_duplicates() {
        let g = Graph::default();
        let r = exec_cypher(&g, "RETURN 1 AS x UNION ALL RETURN 1.0 AS x");
        assert_eq!(r.rows.len(), 2);
    }
}
