//! Parquet shard writer/reader (ADR-0022) — the Parquet-only part of
//! [`crate::snapshot`], split out of `snapshot.rs` as a directory module. It
//! is compiled only when the Parquet codec is (`parquet` feature on a
//! non-wasm32 target, ADR-0105); the parent gates the whole module.

use super::*;

#[cfg(test)]
use std::ops::Range;

use cypherlite_executor::Graph;

use crate::codec::parquet::{load_nodes, load_rels, Loader, Origin, MAX_FILE};
// Test modules reach the preflighting wrappers through `use super::*`.
#[cfg(test)]
use crate::codec::parquet::{nodes_parquet_ids, rels_parquet_ids};
use crate::vfs::{native, BoundedSink, Vfs};
use crate::{storage_context, Manifest, SnapshotCodecKind};

mod coalesce;
mod layout;
mod ontology;
mod shard_name;
pub(crate) use shard_name::ShardName;

/// Per-kind manifest/graph pairing: the four choices `rewrite_kind` and
/// the manifest recheck must keep consistent, stated once here.
impl Kind {
    fn parts(self, m: &Manifest) -> &[ShardPart] {
        match self {
            Self::Node => &m.node_parts,
            Self::Rel => &m.rel_parts,
        }
    }

    #[cfg(test)]
    fn parts_mut(self, m: &mut Manifest) -> &mut Vec<ShardPart> {
        match self {
            Self::Node => &mut m.node_parts,
            Self::Rel => &mut m.rel_parts,
        }
    }

    fn bytes_per_row(self, m: &Manifest) -> f64 {
        match self {
            Self::Node => m.bytes_per_node,
            Self::Rel => m.bytes_per_rel,
        }
    }

    /// Ids changed since the last checkpoint, sorted.
    fn dirty_ids(self, graph: &Graph) -> Vec<i64> {
        let mut ids: Vec<i64> = match self {
            Self::Node => graph.dirty_node_ids(),
            Self::Rel => graph.dirty_rel_ids(),
        }
        .into_iter()
        .collect();
        ids.sort_unstable();
        ids
    }

    fn all_ids(self, graph: &Graph, ts: u64) -> Vec<i64> {
        match self {
            Self::Node => graph.all_node_ids_at(ts),
            Self::Rel => graph.all_rel_ids_at(ts),
        }
    }

    fn loader(self) -> Loader {
        match self {
            Self::Node => load_nodes,
            Self::Rel => load_rels,
        }
    }
}

// The version marker lives in the store-version registry (ADR-0127);
// `FORMAT_VERSION` is re-exported for the existing checkpoint/load sites.
pub(crate) use crate::version::manifest::CURRENT_VERSION as FORMAT_VERSION;

const PARQUET_SHARD: BoundedRead = BoundedRead {
    max_len: MAX_FILE,
    too_large: |name| {
        Error::storage(format!(
            "Parquet shard {name} exceeds 1 GiB ({MAX_FILE} bytes)"
        ))
    },
    changed: |name| Error::storage(format!("Parquet shard {name} changed length during read")),
};

/// Default shard byte budget (override with `CYPHERLITE_SHARD_TARGET_BYTES`).
pub const DEFAULT_SHARD_TARGET_BYTES: u64 = 256 * 1024 * 1024;

const MIN_WINDOW: i64 = 4096;
const MAX_WINDOW: i64 = 1_000_000;
const FLOOR_BYTES_PER_ROW: f64 = 256.0;
const ASSUMED_BYTES_PER_ROW: f64 = 1024.0;

pub fn shard_target_bytes() -> Result<u64> {
    match std::env::var("CYPHERLITE_SHARD_TARGET_BYTES") {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SHARD_TARGET_BYTES),
        Ok(value) => parse_shard_target(&value),
        Err(_) => Err(Error::storage(format!(
            "CYPHERLITE_SHARD_TARGET_BYTES must be valid UTF-8 in 1..={MAX_FILE}"
        ))),
    }
}

fn parse_shard_target(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|&n| (1..=MAX_FILE).contains(&n))
        .ok_or_else(|| {
            Error::storage(format!(
                "CYPHERLITE_SHARD_TARGET_BYTES must be an integer in 1..={MAX_FILE}"
            ))
        })
}

pub fn window_width(target_bytes: u64, bytes_per_row: f64) -> i64 {
    let bpr = if bytes_per_row.is_finite() && bytes_per_row > 0.0 {
        bytes_per_row.max(FLOOR_BYTES_PER_ROW)
    } else {
        ASSUMED_BYTES_PER_ROW
    };
    let w = (target_bytes as f64 / bpr).floor() as i64;
    w.clamp(MIN_WINDOW, MAX_WINDOW)
}

// Stop serializing before retaining an unbounded schema image in the v3
// manifest. The same 16 MiB limit applies on both checkpoint and recovery.
fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut writer =
            BoundedSink::capture(&mut bytes, MAX_JSON_OBJECT, "manifest exceeds 16 MiB");
        let result = serde_json::to_writer_pretty(&mut writer, manifest);
        if writer.exceeded() {
            check_bounded_len(MANIFEST_JSON, MAX_JSON_OBJECT + 1)?;
        }
        result.map_err(|e| Error::storage(e.to_string()))?;
    }
    Ok(bytes)
}

/// Resolve the configured shard byte budget and snapshot.
pub fn checkpoint_at(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    ontology: Option<&str>,
) -> Result<()> {
    checkpoint_at_with_bytes(
        target,
        graph,
        checkpoint_lsn,
        snapshot_ts,
        shard_target_bytes()?,
        ontology,
    )
}
pub(crate) fn checkpoint_at_with_bytes(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    target_bytes: u64,
    ontology: Option<&str>,
) -> Result<()> {
    if !(1..=MAX_FILE).contains(&target_bytes) {
        return Err(Error::storage(format!(
            "Parquet shard target must be in 1..={MAX_FILE} bytes"
        )));
    }
    if snapshot_ts == u64::MAX || graph.next_node_id() < 0 || graph.next_rel_id() < 0 {
        return Err(Error::storage(
            "snapshot has unsupported timestamp or next ID",
        ));
    }
    // Validate against the named catalog before creating *any* shard.
    let theater = graph.checkpoint_theater_for_write()?;
    let prev = read_manifest(target)?.map(|(m, _)| m);
    // A kind with no previous parts is not checked by read_manifest. Reject
    // even an unused alias before a new leaf can be created through it.
    for kind in Kind::ALL {
        if let Some(root) = &target.native_root {
            validate_dir(root, kind.dir(), "shard directory", true)?;
        }
    }
    let mut owned = Vec::new();
    let prepared = (|| {
        let (node_parts, retired_nodes) = {
            let mut ctx = ShardWrite {
                target,
                graph,
                ts: snapshot_ts,
                kind: Kind::Node,
                target_bytes,
                owned: &mut owned,
            };
            rewrite_kind(&mut ctx, prev.as_ref())?
        };
        let (rel_parts, retired_rels) = {
            let mut ctx = ShardWrite {
                target,
                graph,
                ts: snapshot_ts,
                kind: Kind::Rel,
                target_bytes,
                owned: &mut owned,
            };
            rewrite_kind(&mut ctx, prev.as_ref())?
        };
        let bytes_per_node = ewma_density(
            prev.as_ref().map(|m| m.bytes_per_node).unwrap_or(0.0),
            &node_parts,
        )?;
        let bytes_per_rel = ewma_density(
            prev.as_ref().map(|m| m.bytes_per_rel).unwrap_or(0.0),
            &rel_parts,
        )?;
        // The ontology object publishes before the manifest that selects it
        // (ADR-0120 §5, the ADR-0092/ADR-0089 rules): exclusive create,
        // sync, durable name — and `owned` collects it for the crash and
        // failure cleanup paths.
        let ontology_path = ontology
            .map(|document| ontology::publish_ontology(target, prev.as_ref(), document, &mut owned))
            .transpose()?;
        let manifest = Manifest {
            format_version: crate::version::manifest::version_for_write(ontology_path.is_some()),
            next_node_id: graph.next_node_id(),
            next_rel_id: graph.next_rel_id(),
            checkpoint_lsn,
            commit_ts: snapshot_ts,
            node_parts,
            rel_parts,
            bytes_per_node,
            bytes_per_rel,
            codec: SnapshotCodecKind::Parquet,
            indexes: graph.named_indexes(),
            constraints: graph.named_constraints(),
            theater: Some(theater),
            ontology_path,
        };
        // Unchanged (including legacy fixed-name) parts are published again.
        // Recheck every proposed reference after writing, not just new leaves.
        for kind in Kind::ALL {
            for part in kind.parts(&manifest) {
                validate_part(target, kind, part)?;
            }
        }
        let bytes = encode_manifest(&manifest)?;
        Ok::<_, Error>((manifest, bytes, retired_nodes, retired_rels))
    })();
    // The publish phase is known statically: a prepare failure cleans up as
    // not-publishing, a publish failure as publishing.
    let (manifest, bytes, retired_nodes, retired_rels) = match prepared {
        Ok(result) => result,
        Err(error) => return Err(cleanup_unreferenced(target, &owned, false, error)),
    };
    if let Err(error) = put_object(target, MANIFEST_JSON, &bytes) {
        return Err(cleanup_unreferenced(target, &owned, true, error));
    }
    retire_superseded(
        target,
        prev.as_ref(),
        &manifest,
        retired_nodes,
        retired_rels,
    );
    graph.clear_checkpoint_dirty();
    Ok(())
}

/// Retire superseded and legacy-monolith objects after a successful publish.
/// Windows cannot prove namespace persistence of a renamed manifest after
/// power loss, so older generations are retained there for a possible
/// fallback: a restored old manifest can still need its monolithic objects,
/// and the retired generations may still be referenced.
fn retire_superseded(
    target: &StoreTarget,
    prev: Option<&Manifest>,
    manifest: &Manifest,
    retired_nodes: Vec<String>,
    retired_rels: Vec<String>,
) {
    if cfg!(windows) && target.native_root.is_some() {
        return;
    }
    let mut retired: Vec<String> = Vec::new();
    for p in retired_nodes.into_iter().chain(retired_rels) {
        if !referenced(manifest, &p) {
            retired.push(p);
        }
    }
    for kind in Kind::ALL {
        if prev.is_none_or(|m| kind.parts(m).is_empty()) {
            retired.extend(kind.legacy_names().map(str::to_string));
        }
    }
    // The superseded ontology object retires only now, after the manifest
    // that no longer selects it has published (ADR-0120 §5).
    if let Some(old) = prev.and_then(|m| m.ontology_path.as_deref()) {
        if manifest.ontology_path.as_deref() != Some(old) {
            retired.push(old.to_owned());
        }
    }
    // One batch keeps a `.cl` retirement to a single catalog generation.
    delete_objects(target, &retired);
}
#[cfg(test)]
pub fn load(target: &StoreTarget) -> Result<Graph> {
    Ok(load_with_checkpoint_lsn(target)?.graph)
}

/// Load the snapshot and the checkpoint LSN from the one manifest read,
/// reporting the decoded manifest version marker (ADR-0127 Decision 2) and
/// the ontology document the manifest selects (ADR-0120 §5; `None` when
/// the store holds none). The LSN is 0 when no manifest exists yet.
pub(crate) fn load_with_checkpoint_lsn(target: &StoreTarget) -> Result<LoadedSnapshot> {
    // read_manifest rejects an unsupported format_version and classifies the
    // marker at its gate before anything else inspects the manifest
    // (ADR-0114 Decision 2, ADR-0127 Decision 2).
    let Some((manifest, marker)) = read_manifest(target)? else {
        return Ok(LoadedSnapshot::empty(0));
    };
    let checkpoint_lsn = manifest.checkpoint_lsn;
    match load_manifest_body(target, &manifest) {
        Ok((graph, ontology)) => Ok(LoadedSnapshot {
            graph,
            checkpoint_lsn,
            ontology,
            marker,
        }),
        // A legacy-store failure names the version where the loader knows
        // it (ADR-0127 Decision 4); a current store keeps its error verbatim.
        Err(error) => Err(crate::version::annotate_legacy_failure(error, marker)),
    }
}

/// Load one kind's shards: the pre-shard monolith object when the manifest
/// predates sharding (format ≤ 2 with no parts), else every manifest part,
/// each held to the shard rules over its `[id_lo, id_hi)`;
/// [`Origin::Monolith`] says why the monolith is not. A failure names the
/// object that failed.
/// All per-kind projections derive from `kind` itself.
fn load_kind(target: &StoreTarget, graph: &Graph, manifest: &Manifest, kind: Kind) -> Result<()> {
    let load = kind.loader();
    if manifest.format_version <= 2 && kind.parts(manifest).is_empty() {
        let name = kind.legacy_names()[0];
        if let Some(b) = get_parquet_opt(target, name)? {
            load(graph, b, Origin::Monolith)
                .map_err(|e| storage_context(e, format_args!("legacy shard {name}")))?;
        }
        return Ok(());
    }
    for part in kind.parts(manifest) {
        let b = get_parquet(target, &part.path)?;
        load(graph, b, Origin::Part(part.id_lo..part.id_hi))
            .map_err(|e| storage_context(e, format_args!("manifest part {}", part.path)))?;
    }
    Ok(())
}

fn load_manifest_body(
    target: &StoreTarget,
    manifest: &Manifest,
) -> Result<(Graph, Option<String>)> {
    let graph = Graph::default();
    graph.set_next_node_id(manifest.next_node_id);
    graph.set_next_rel_id(manifest.next_rel_id);
    graph.set_commit_ts(manifest.commit_ts);
    for kind in Kind::ALL {
        load_kind(target, &graph, manifest, kind)?;
    }
    if graph.commit_ts() == 0 && graph.live_node_count() > 0 {
        graph.set_commit_ts(1);
    }
    for idx in &manifest.indexes {
        graph.install_named_index(idx.clone())?;
    }
    for constraint in &manifest.constraints {
        graph.install_named_constraint(constraint.clone())?;
    }
    if let Some(theater) = &manifest.theater {
        graph.restore_checkpoint_theater(theater.clone())?;
    } else {
        graph.mark_legacy_unenforced();
    }
    graph.finish_restore();
    let ontology = manifest
        .ontology_path
        .as_deref()
        .map(|path| ontology::load_ontology_document(target, path))
        .transpose()?;
    Ok((graph, ontology))
}

/// Sum of live snapshot objects (manifest + Parquet parts). Excludes WAL
/// and `.tmp`.
pub fn snapshot_data_bytes(target: &StoreTarget) -> Result<u64> {
    let Some((manifest, _)) = read_manifest(target)? else {
        return Ok(0);
    };
    let mut names = vec![(MANIFEST_JSON, false)];
    for kind in Kind::ALL {
        if manifest.format_version < FORMAT_VERSION && kind.parts(&manifest).is_empty() {
            names.push((kind.legacy_names()[0], false));
        }
    }
    for p in manifest.node_parts.iter().chain(manifest.rel_parts.iter()) {
        names.push((&p.path, true));
    }
    // The selected ontology object is a live snapshot object too
    // (ADR-0120 §5).
    if let Some(path) = &manifest.ontology_path {
        names.push((path.as_str(), true));
    }
    let mut sum = 0u64;
    for (name, required) in names {
        if !target.vfs.exists(name)? {
            if required {
                return Err(Error::storage(format!("missing manifest shard {name}")));
            }
            continue;
        }
        let meta = target.vfs.metadata(name)?;
        check_bounded_len(name, meta.len)?;
        sum = sum
            .checked_add(meta.len)
            .ok_or_else(|| Error::storage("snapshot byte accounting overflow"))?;
    }
    Ok(sum)
}

#[cfg(test)]
pub(crate) fn read_manifest_for_test(target: &StoreTarget) -> Result<Option<Manifest>> {
    Ok(read_manifest(target)?.map(|(m, _)| m))
}

/// Refuse a `root`-relative directory that is missing, is not a directory, or
/// is reachable only through a symlink/junction/reparse point. `allow_missing`
/// treats an absent entry as acceptable, for callers about to create it.
fn validate_dir(root: &std::path::Path, rel: &str, what: &str, allow_missing: bool) -> Result<()> {
    let path = root.join(rel);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() && native::plain_entry(&meta) => Ok(()),
        Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error::storage(format!(
            "{what} is not a regular directory (symlinks refused): {}",
            path.display()
        ))),
    }
}

fn get_parquet(target: &StoreTarget, name: &str) -> Result<Vec<u8>> {
    get_parquet_opt(target, name)?.ok_or_else(|| Error::storage(format!("missing {name}")))
}

fn get_parquet_opt(target: &StoreTarget, name: &str) -> Result<Option<Vec<u8>>> {
    read_bounded_object(target, name, &PARQUET_SHARD)
}
pub(crate) fn read_manifest(
    target: &StoreTarget,
) -> Result<Option<(Manifest, crate::version::SnapshotMarker)>> {
    let Some(bytes) = get_object_opt(target, MANIFEST_JSON)? else {
        return Ok(None);
    };
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::storage(e.to_string()))?;
    // The format marker gates everything else: a manifest written by a newer
    // CypherLite must fail before any selector, sentinel or shard check runs
    // (ADR-0114 Decision 2). The registry's per-subsystem gate is the one
    // gate (ADR-0127 Decision 2); the marker is classified here, once,
    // against the content the manifest carries (ADR-0120 §8), and handed to
    // the caller with the manifest.
    let marker = crate::version::manifest::gate(m.format_version, m.ontology_path.is_some())?;
    if let Some(path) = &m.ontology_path {
        if !is_ontology_object_name(path) {
            return Err(Error::storage(format!(
                "invalid manifest ontology path: {path}"
            )));
        }
    }
    if m.commit_ts == u64::MAX || m.next_node_id < 0 || m.next_rel_id < 0 {
        return Err(Error::storage(
            "manifest has unsupported timestamp or next ID",
        ));
    }
    for kind in Kind::ALL {
        let (mut rows, mut bytes) = (0u64, 0u64);
        for part in kind.parts(&m) {
            validate_part(target, kind, part)?;
            if part.rows == 0
                || part.rows > MAX_WINDOW as u64
                || part.rows > (part.id_hi - part.id_lo) as u64
                || part.bytes == 0
                || part.bytes > MAX_FILE
            {
                return Err(Error::storage(format!(
                    "invalid manifest shard metrics: {} (rows {}, bytes {})",
                    part.path, part.rows, part.bytes
                )));
            }
            rows = rows
                .checked_add(part.rows)
                .ok_or_else(|| Error::storage("manifest shard row total overflow"))?;
            bytes = bytes
                .checked_add(part.bytes)
                .ok_or_else(|| Error::storage("manifest shard byte total overflow"))?;
        }
    }
    Ok(Some((m, marker)))
}

fn validate_part(target: &StoreTarget, kind: Kind, part: &ShardPart) -> Result<()> {
    let invalid = || Error::storage(format!("invalid manifest shard path: {}", part.path));
    let name = ShardName::parse(&part.path).ok_or_else(invalid)?;
    if name.kind != kind || name.lo != part.id_lo || name.hi != part.id_hi {
        return Err(invalid());
    }
    validate_shard_name(target, name).map_err(|_| invalid())
}

/// Whether the `root`-relative `rel` resolves to a real directory, never
/// through a symlink/junction/reparse point. Missing or not-a-directory is
/// `false`: the orphan sweeps skip rather than fail.
fn native_plain_dir(root: &std::path::Path, rel: &str) -> bool {
    std::fs::symlink_metadata(root.join(rel))
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

/// Whether the `root`-relative `rel` resolves to a plain file, never an
/// alias: the orphan sweeps' delete condition.
fn native_plain_file(root: &std::path::Path, rel: &str) -> bool {
    std::fs::symlink_metadata(root.join(rel))
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
}

/// The native-side proof that a valid-looking object name does not alias
/// the lock or another object through a directory junction, reparse point
/// or leaf symlink: `rel` under `root` must metadata as a plain file
/// (ADR-0089, ADR-0120 §5). `what` names the object family in the errors.
fn validate_native_plain_file(root: &std::path::Path, rel: &str, what: &str) -> Result<()> {
    let file = root.join(rel);
    let meta = std::fs::symlink_metadata(&file)
        .map_err(|e| Error::storage(format!("invalid {what} {}: {e}", file.display())))?;
    if !meta.is_file() || !native::plain_entry(&meta) {
        return Err(Error::storage(format!("invalid {what} path: {rel}")));
    }
    Ok(())
}

fn validate_shard_name(target: &StoreTarget, name: ShardName) -> Result<()> {
    let path = name.path();
    let invalid = || Error::storage(format!("invalid manifest shard path: {path}"));
    if let Some(root) = &target.native_root {
        validate_dir(root, name.kind.dir(), "shard directory", false).map_err(|_| invalid())?;
        validate_native_plain_file(root, &path, "shard")?;
    }
    Ok(())
}

/// The invariant bundle threaded through the shard-write recursion
/// (`rewrite_kind` → `coalesce::rewrite` → `publish_part`): everything
/// except the varying id window.
struct ShardWrite<'a> {
    target: &'a StoreTarget,
    graph: &'a Graph,
    ts: u64,
    kind: Kind,
    target_bytes: u64,
    owned: &'a mut Vec<String>,
}

fn rewrite_kind(
    ctx: &mut ShardWrite<'_>,
    prev: Option<&Manifest>,
) -> Result<(Vec<ShardPart>, Vec<String>)> {
    let parts = prev.map(|m| ctx.kind.parts(m).to_vec()).unwrap_or_default();
    let dirty = ctx.kind.dirty_ids(ctx.graph);
    let bytes_per_row = prev.map(|m| ctx.kind.bytes_per_row(m)).unwrap_or(0.0);
    let all_ids = ctx.kind.all_ids(ctx.graph, ctx.ts);
    let w = window_width(ctx.target_bytes, bytes_per_row);
    // ADR-0183: heal, cover, plan and write id ranges as coalesced units,
    // not per previous part.
    coalesce::rewrite(ctx, parts, &all_ids, &dirty, w)
}

/// Exclusive-reservation loop shared by generated immutable objects: pick a
/// fresh name, create it without overwriting, record it in `owned` for
/// cleanup, write and fsync its bytes, then persist the containing directory.
/// A collision retries with a new name; all other failures abort. Callers own
/// any directory preparation before the first attempt.
fn reserve_owned_object(
    vfs: &dyn Vfs,
    dir: &str,
    mut make_name: impl FnMut() -> Result<String>,
    bytes: &[u8],
    owned: &mut Vec<String>,
    what: &str,
) -> Result<String> {
    for _ in 0..8 {
        let path = make_name()?;
        let Some(mut file) = vfs.open_exclusive(&path)? else {
            continue;
        };
        owned.push(path.clone());
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        result?;
        vfs.sync_dir(dir)?;
        return Ok(path);
    }
    Err(Error::storage(format!(
        "could not reserve a unique {what} path"
    )))
}

/// The validate/create/re-validate directory sandwich every immutable
/// object publish performs on its family directory before the first
/// reservation attempt: refuse an alias or non-directory both before and
/// after creating the directory, so a race into the create window is
/// caught (ADR-0089, ADR-0120 §5).
fn prepare_object_dir(target: &StoreTarget, dir: &str, what: &str) -> Result<()> {
    if let Some(root) = &target.native_root {
        validate_dir(root, dir, what, true)?;
    }
    target.vfs.create_dir_all_for_atomic(dir)?;
    if let Some(root) = &target.native_root {
        validate_dir(root, dir, what, false)?;
    }
    Ok(())
}

fn referenced(m: &Manifest, path: &str) -> bool {
    m.ontology_path.as_deref() == Some(path)
        || m.node_parts
            .iter()
            .chain(&m.rel_parts)
            .any(|p| p.path == path)
}

fn cleanup_unreferenced(
    target: &StoreTarget,
    owned: &[String],
    publishing: bool,
    error: Error,
) -> Error {
    if owned.is_empty() {
        return error;
    }
    // Manifest replacement can return an error *after* the rename. Never
    // remove a part unless a readable manifest proves it is unreferenced.
    let manifest = match read_manifest(target) {
        Ok(Some((m, _))) => Some(m),
        Ok(None) if !publishing => None,
        _ => return error,
    };
    let mut failures = Vec::new();
    for path in owned {
        if manifest.as_ref().is_none_or(|m| !referenced(m, path)) {
            if let Err(e) = delete_object(target, path) {
                failures.push(format!("{path}: {e}"));
            }
        }
    }
    if failures.is_empty() {
        error
    } else {
        Error::storage(format!(
            "{error}; checkpoint cleanup failed: {}",
            failures.join(", ")
        ))
    }
}

/// Run only after a complete snapshot and WAL replay have established the
/// authoritative manifest. Fixed-name legacy shards are never crash orphans.
pub(crate) fn sweep_orphans(target: &StoreTarget) -> Result<()> {
    let Some((manifest, _)) = read_manifest(target)? else {
        return Ok(());
    };
    #[cfg(windows)]
    if target.native_root.is_some() {
        return Ok(());
    }
    for dir in Kind::ALL.map(Kind::dir) {
        sweep_dir_orphans(
            target,
            dir,
            |path| matches!(object_name(path), Some(ObjectName::Shard(shard)) if shard.generation.is_some()),
            |path| referenced(&manifest, path),
        );
    }
    ontology::sweep_ontology_orphans(target, &manifest);
    Ok(())
}

/// The orphan-sweep protocol for one immutable object family directory
/// (ADR-0089, ADR-0120 §5): an entry `is_candidate` accepts (the family's
/// generated-name grammar) that `keep` rejects (the published manifest
/// does not reference it) is a crash-window orphan — the object publishes
/// before the manifest that selects it, so a crash between the two leaves
/// exactly this state. Housekeeping is retryable: a missing or unreadable
/// directory skips the family, a failed delete skips the entry, and
/// neither fails the open. A symlink presented as a family object is never
/// followed.
fn sweep_dir_orphans(
    target: &StoreTarget,
    dir: &str,
    is_candidate: impl Fn(&str) -> bool,
    keep: impl Fn(&str) -> bool,
) {
    if !matches!(target.vfs.exists(dir), Ok(true)) {
        return;
    }
    if let Some(root) = &target.native_root {
        if !native_plain_dir(root, dir) {
            return;
        }
    }
    let Ok(names) = target.vfs.list_dir(dir) else {
        return;
    };
    for name in names {
        let path = format!("{dir}/{name}");
        if !is_candidate(&path) || keep(&path) {
            continue;
        }
        if let Some(root) = &target.native_root {
            if !native_plain_file(root, &path) {
                continue;
            }
        }
        let _ = delete_object(target, &path);
    }
}

fn ewma_density(prev: f64, parts: &[ShardPart]) -> Result<f64> {
    let mut bytes = 0u64;
    let mut rows = 0u64;
    for p in parts {
        bytes = bytes
            .checked_add(p.bytes)
            .ok_or_else(|| Error::storage("manifest shard byte total overflow"))?;
        rows = rows
            .checked_add(p.rows)
            .ok_or_else(|| Error::storage("manifest shard row total overflow"))?;
    }
    if rows == 0 {
        return Ok(prev);
    }
    let sample = bytes as f64 / rows as f64;
    if prev > 0.0 && prev.is_finite() {
        Ok(0.7 * prev + 0.3 * sample)
    } else {
        Ok(sample)
    }
}
pub(crate) fn delete_object(target: &StoreTarget, name: &str) -> Result<()> {
    validate_deletable(target, name)?;
    target.vfs.remove_file(name)
}

/// Best-effort removal of a batch of snapshot objects. Every path is validated
/// exactly as [`delete_object`]; invalid paths are skipped. One
/// [`Vfs::remove_files`] call lets the Bundle retire the whole batch with a
/// single catalog generation instead of one persist per shard.
pub(crate) fn delete_objects(target: &StoreTarget, names: &[String]) {
    let batch: Vec<String> = names
        .iter()
        .filter(|name| validate_deletable(target, name).is_ok())
        .cloned()
        .collect();
    if !batch.is_empty() {
        let _ = target.vfs.remove_files(&batch);
    }
}

fn validate_deletable(target: &StoreTarget, name: &str) -> Result<()> {
    match object_name(name) {
        Some(ObjectName::LegacyShard) => Ok(()),
        Some(ObjectName::Ontology) => ontology::validate_ontology_object(target, name),
        Some(ObjectName::Shard(shard)) => validate_shard_name(target, shard),
        _ => Err(Error::storage(format!(
            "invalid manifest shard path: {name}"
        ))),
    }
}

#[cfg(test)]
mod tests;
