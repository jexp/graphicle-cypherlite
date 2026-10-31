//! The store-level service bundle and its `Graph` accessors; the bundle's
//! contract is documented on [`StoreServices`].

use std::sync::Arc;

use cypherlite_core::PreviewFeatures;
use dashmap::DashMap;

use crate::csv::CsvPolicy;

use super::{DatabasesCatalog, Graph, Procedure};

/// Store-level services shared by every namespace graph of a store
/// (ADR-0116 §10): the procedure registry, the LOAD CSV capability, the
/// store's resolved preview set (ADR-0371, ADR-0378) and the databases
/// catalog (ADR-0276). One bundle lives behind one `Arc`; every namespace
/// graph of the store holds a clone of it (installed via
/// [`Graph::share_services_from`]), so a procedure registered on the store
/// is callable from any namespace session, the CSV policy and the preview
/// set are identical in every namespace, and `ALTER DATABASE` /
/// `SHOW DATABASES` read and write one shared databases table (ADR-0276;
/// ADR-0116 §8 invariants) regardless of the session's namespace. Open
/// installs the preview set once with [`Graph::configure_preview`]; both
/// the catalog and the `CALL` gate read it back through [`Graph::preview`].
///
/// The members split into two kinds. The procedure registry, CSV policy and
/// preview set are configuration the store owns and a follower keeps across
/// a reload. The databases catalog is durable state: a checkpoint restores
/// it into the loaded graph's fresh bundle, so the follow reload installs
/// it with [`Graph::install_databases_from`] before adopting the surviving
/// bundle instead of letting that bundle's stale copy win.
#[derive(Debug, Default)]
pub(crate) struct StoreServices {
    /// Registered procedures (TCK / user). Store-level state (ADR-0116 §10).
    pub(crate) procedures: DashMap<String, Procedure>,
    /// LOAD CSV capability (ADR-0081). The policy is store-level
    /// (ADR-0116 §10), so it lives in the shared bundle.
    pub(crate) csv_policy: CsvPolicy,
    /// The store's resolved preview set (ADR-0371, ADR-0378), installed at
    /// open and on follow reload; read by the catalog and the `CALL` gate.
    pub(crate) preview: PreviewFeatures,
    /// The store-level databases catalog (ADR-0276; ADR-0116 §8
    /// invariants): the three fixed
    /// namespaces' settings. One table for the whole store — per-graph
    /// copies diverged when `ALTER DATABASE` wrote the `neo4j` graph's
    /// theater while a `system` session read its own statically seeded one.
    pub(crate) databases: DatabasesCatalog,
}

impl Graph {
    /// The store's one `StoreServices` bundle, mutable only before it is
    /// shared with another graph. Both open-time installers
    /// ([`Graph::configure_load_csv`], [`Graph::configure_preview`]) run
    /// through this one accessor, so the pre-share contract and its panic
    /// are stated once: after [`Graph::share_services_from`] the bundle's
    /// `Arc` identity is the store's one-bundle invariant (ADR-0116 §10),
    /// and rebuilding it would silently detach this graph from every other
    /// namespace.
    fn services_mut(&mut self) -> &mut StoreServices {
        Arc::get_mut(&mut self.services).expect(
            "the StoreServices bundle must be configured before \
             share_services_from: it is already shared (ADR-0116 §10)",
        )
    }

    /// Install a validated local CSV capability before sharing the graph with connections.
    ///
    /// Mutates the store's single `StoreServices` bundle in place; panics
    /// if the bundle is already shared via [`Graph::share_services_from`]
    /// (see `Graph::services_mut`).
    #[doc(hidden)]
    pub fn configure_load_csv(&mut self, policy: CsvPolicy) {
        self.services_mut().csv_policy = policy;
    }

    /// Install the databases catalog a checkpoint loaded on `loaded` into
    /// this graph's store-level bundle (ADR-0276), so a replacement graph
    /// can then adopt that bundle without losing durable state.
    ///
    /// The catalog is the one durable member of [`StoreServices`]: the
    /// store's configuration (procedure registry, CSV policy, preview set)
    /// belongs to the surviving bundle, while `open_store_readonly` restores
    /// this table into the fresh graph's own bundle through
    /// [`Graph::restore_checkpoint_theater`]. The follow reload
    /// (`follow::reload`) calls this before [`Graph::share_services_from`];
    /// the catalog's interior mutability lets it write through `&Graph`
    /// without rebuilding the bundle's `Arc`. Not public API.
    #[doc(hidden)]
    pub fn install_databases_from(&self, loaded: &Graph) {
        self.databases().install(loaded.databases().snapshot());
    }

    /// Point this graph at the store-level service bundle of `other`
    /// (ADR-0116 §10), so every namespace of a store shares one procedure
    /// registry, one LOAD CSV policy, one resolved preview set and one
    /// databases catalog. Called when the store creates or swaps a
    /// namespace slot, and by the follow reload (`follow::reload`). The
    /// bundle mixes configuration the store owns (procedure registry, CSV
    /// policy, preview set) with durable state restored from a checkpoint
    /// (the databases catalog, ADR-0276), so a caller that loaded this graph
    /// from disk must first install its catalog into `other` with
    /// [`Graph::install_databases_from`]; a plain share replaces the loaded
    /// bundle wholesale and would discard the checkpoint's catalog. Not
    /// public API.
    #[doc(hidden)]
    pub fn share_services_from(&mut self, other: &Graph) {
        self.services = Arc::clone(&other.services);
    }

    /// Look up a registered procedure in the store's shared registry
    /// (ADR-0116 §10). Read-only: the registry itself is interior-mutable,
    /// so lookups never touch the bundle's `Arc` identity.
    #[doc(hidden)]
    pub fn procedure(&self, name: &str) -> Option<Procedure> {
        self.services.procedures.get(name).map(|p| p.clone())
    }

    /// Whether `name` is registered in the store's shared procedure
    /// registry (ADR-0116 §10).
    #[doc(hidden)]
    pub fn has_procedure(&self, name: &str) -> bool {
        self.services.procedures.contains_key(name)
    }

    /// Register a procedure into the store's shared registry (ADR-0116
    /// §10), visible to every namespace graph of the store. Interior
    /// mutation only; the bundle's `Arc` identity is untouched.
    #[doc(hidden)]
    pub fn register_procedure(&self, proc: Procedure) {
        self.services.procedures.insert(proc.name.clone(), proc);
    }

    /// The store-level LOAD CSV policy (ADR-0081, ADR-0116 §10), read-only.
    pub(crate) fn csv_policy(&self) -> &CsvPolicy {
        &self.services.csv_policy
    }

    /// Install the store's resolved preview set (ADR-0371, ADR-0378) on the
    /// one bundle; same pre-share contract as [`Graph::configure_load_csv`].
    #[doc(hidden)]
    pub fn configure_preview(&mut self, preview: PreviewFeatures) {
        self.services_mut().preview = preview;
    }

    /// The store-level preview set (ADR-0371, ADR-0378) the `SHOW
    /// PROCEDURES` catalog and the `CALL` gate read.
    #[doc(hidden)]
    pub fn preview(&self) -> PreviewFeatures {
        self.services.preview
    }

    /// The store-level databases catalog (ADR-0276; ADR-0116 §8
    /// invariants), shared by every namespace graph of the store.
    pub(crate) fn databases(&self) -> &DatabasesCatalog {
        &self.services.databases
    }
}
