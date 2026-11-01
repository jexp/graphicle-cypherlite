//! Per-statement mutation counters (`QueryStats`), split out of the graph
//! module to keep it under the file-size budget.

/// Generates [`QueryStats`] and its `merge` from one counter list, so a
/// counter cannot exist without its merge arm (ADR-0309's two families are
/// documented per field below).
macro_rules! query_stats {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
        /// A statement's mutation counters. Two families count different things:
        ///
        /// - The openCypher graph-diff counters the TCK side effects check:
        ///   `labels_added`/`labels_removed` count label *names* that appear in or
        ///   vanish from the graph, and `properties_set`/`properties_removed` count
        ///   property additions and removals (an overwrite is one of each; a deleted
        ///   entity's properties are removed).
        /// - Neo4j's update counters, which drivers read from a result summary
        ///   (ADR-0309): `node_labels_added`/`node_labels_removed` count labels per
        ///   node (a `REMOVE`, never a delete), and `properties_written` counts one
        ///   per property a `CREATE`, `SET` or `REMOVE` writes. The wire rendering
        ///   (`cypherlite::query_stats_to_wire`) uses these.
        ///
        /// The node, relationship, index and constraint counters are shared.
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub struct QueryStats {
            $(
                $(#[$doc])*
                pub $name: i64,
            )+
        }

        impl QueryStats {
            /// Merge another statement's counters into this one. Generated
            /// from the one counter list, so a counter cannot exist without
            /// its merge arm.
            pub fn merge(&mut self, other: &QueryStats) {
                $(
                    self.$name += other.$name;
                )+
            }
        }
    };
}

query_stats! {
    nodes_created,
    nodes_deleted,
    relationships_created,
    relationships_deleted,
    properties_set,
    properties_removed,
    labels_added,
    labels_removed,
    /// Indexes created by index DDL (ADR-0309). The range index backing a
    /// `UNIQUE`/`KEY` constraint is part of that constraint, not counted here.
    indexes_added,
    /// Indexes dropped by index DDL (ADR-0309); a constraint's backing index
    /// dropped with it is not counted here.
    indexes_removed,
    /// Constraints created, by constraint DDL or a graph-type change (ADR-0309).
    constraints_added,
    /// Constraints dropped, by constraint DDL or a graph-type change (ADR-0309).
    constraints_removed,
    /// Labels added to nodes, counted per node (Neo4j's `labels-added`,
    /// ADR-0309).
    node_labels_added,
    /// Labels removed from nodes by `REMOVE`, counted per node (Neo4j's
    /// `labels-removed`, ADR-0309); deleting a node counts none.
    node_labels_removed,
    /// Property writes: one per property a `CREATE`, `SET` or `REMOVE`
    /// writes, overwrites and removals included (Neo4j's `properties-set`,
    /// ADR-0309); deleting an entity counts none.
    properties_written,
}
