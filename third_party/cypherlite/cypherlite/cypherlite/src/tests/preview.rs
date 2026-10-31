//! The preview-feature gate fixture (ADR-0371): the store-level set that
//! `CYPHERLITE_PREVIEW` resolves at open, wired through the same
//! fail-closed gate the registry pins in `cypherlite-core`. The first
//! gated feature is the `db.ontology.*` procedure family (ADR-0378,
//! `CYPHERLITE_PREVIEW=ontology`); the fixture calls the resolved set
//! directly so every registry row stays pinned.

use cypherlite_core::{PreviewFeatures, PREVIEW_FEATURES};

use crate::Database;

/// An open store with no `CYPHERLITE_PREVIEW` opt-in gates every preview
/// feature closed with the authored disabled-feature error.
#[test]
fn gate_fails_closed_without_the_opt_in() {
    // The precondition the whole fixture rests on: a dev shell exporting
    // CYPHERLITE_PREVIEW changes what `Database::open` resolves — fail loudly
    // instead through the one shared predicate this fixture and the
    // `ontology_preview_gate` integration crate both call.
    crate::test_opts::assert_preview_unset();
    let db = Database::open(":memory:").unwrap();
    // ADR-0378: the resolved set is stored once, on the graph's
    // `StoreServices` bundle that the `SHOW PROCEDURES` catalog and the
    // `CALL` gate read; open installs it before the bundle is shared, so
    // there is no second copy to compare against.
    let read = db.inner.read_guard().unwrap();
    let preview = db.inner.graph.preview();
    drop(read);
    assert_eq!(preview, PreviewFeatures::none());
    for feature in PREVIEW_FEATURES {
        let error = preview.require(*feature).unwrap_err();
        let display = error.to_string();
        let name = feature.kebab_name();
        assert!(
            display.contains(&format!(
                "preview feature '{name}' is not enabled: \
                 set CYPHERLITE_PREVIEW={name}"
            )),
            "{name}: {display}"
        );
    }
}
