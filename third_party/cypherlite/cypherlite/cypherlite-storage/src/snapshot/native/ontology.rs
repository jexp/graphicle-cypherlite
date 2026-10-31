//! The ontology object family's lifecycle at the Parquet codec
//! (ADR-0120 §5): immutable `ontology/<128-bit-hex>.json` objects published
//! under the ADR-0092/ADR-0089 rules (exclusive create, sync, durable name
//! before the manifest), loaded by manifest selection, validated against
//! alias tricks on a native root, and swept when the crash window orphans
//! them. Split out of `mod.rs`, which keeps the manifest-field wiring; the
//! publication sandwich and the sweep protocol are the shared helpers
//! there. The document is opaque to storage.

use super::*;

/// Publish the ontology `document` as one immutable
/// `ontology/<128-bit-hex>.json` object and return its durable name
/// (ADR-0120 §5, the ADR-0092/ADR-0089 publication rules shared with shard
/// parts: exclusive create, sync, durable name before the manifest). An
/// unchanged document keeps the object the previous manifest already
/// selects, so a checkpoint with no ontology change writes nothing new.
pub(super) fn publish_ontology(
    target: &StoreTarget,
    prev: Option<&Manifest>,
    document: &str,
    owned: &mut Vec<String>,
) -> Result<String> {
    if let Some(prev_path) = prev.and_then(|m| m.ontology_path.as_deref()) {
        if object_exists(target, prev_path)?
            && get_object(target, prev_path)? == document.as_bytes()
        {
            return Ok(prev_path.to_owned());
        }
    }
    reserve_owned_object(
        &*target.vfs,
        ONTOLOGY_DIR,
        || {
            let path = format!("{ONTOLOGY_DIR}/{}.json", crate::vfs::random_hex_16()?);
            check_bounded_len(&path, document.len() as u64)?;
            prepare_object_dir(target, ONTOLOGY_DIR, "ontology directory")?;
            Ok(path)
        },
        document.as_bytes(),
        owned,
        "ontology document",
    )
}

/// The verbatim ontology document a manifest selects (ADR-0120 §5): a
/// bounded read of the immutable object, opaque to storage. A selected
/// object that is missing or unreadable fails the open.
pub(super) fn load_ontology_document(target: &StoreTarget, path: &str) -> Result<String> {
    // The native alias proof every shard part gets on load
    // (`validate_shard_name` via `validate_part`): the `ontology/` object
    // the manifest selects must be a plain file in a real directory —
    // never a symlink — before its bytes are read. No-op on non-native
    // targets.
    validate_ontology_object(target, path)?;
    let bytes = get_object(target, path)?;
    String::from_utf8(bytes).map_err(|e| Error::storage(e.to_string()))
}

/// The ontology object family's orphan sweep (ADR-0120 §5): an
/// `ontology/<128-bit-hex>.json` object the published manifest does not
/// select is a crash-window orphan — the object publishes before the
/// manifest, so a crash between the two leaves exactly this state.
pub(super) fn sweep_ontology_orphans(target: &StoreTarget, manifest: &Manifest) {
    sweep_dir_orphans(
        target,
        ONTOLOGY_DIR,
        |path| matches!(object_name(path), Some(ObjectName::Ontology)),
        |path| manifest.ontology_path.as_deref() == Some(path),
    );
}

/// The native-side checks of [`validate_shard_name`] for the ontology
/// object family (ADR-0120 §5): the directory must be a real directory and
/// the object a plain file, never an alias.
pub(super) fn validate_ontology_object(target: &StoreTarget, name: &str) -> Result<()> {
    if let Some(root) = &target.native_root {
        validate_dir(root, ONTOLOGY_DIR, "ontology directory", false)
            .map_err(|_| Error::storage(format!("invalid ontology path: {name}")))?;
        validate_native_plain_file(root, name, "ontology")?;
    }
    Ok(())
}
