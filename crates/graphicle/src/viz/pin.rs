//! The viz pin (software-visualization-plan §3): a view that must outlive
//! the question it answered — a ranking cited in a review, a CI verdict — is
//! recorded by reference, and re-derived on demand.
//!
//! The shape follows `supply_chain::on_demand::pin`: a write path that runs
//! only when asked, and a read path that matches on the pinned `as_of`. The
//! content differs. A viz view is a pure function of the stored facts
//! ([`super`]'s determinism contract), so the pin does not store the
//! rendered view — the derived data (the `CO_CHANGED` windows and the rest)
//! is already stored keyed, and nothing else accumulates. The pin is the
//! reference tuple that reproduces the view:
//!
//! - `family` and `params`: the canonical [`Params::json`] echo;
//! - `window_keys`: the stored co-change window the call reads;
//! - `as_of`: the stamp the caller pinned the view at;
//! - `analysis`: the caller's label for what cites the view;
//! - `input_hash`: the digest of `(family, params, window_keys, as_of)` —
//!   the node's key, so the same inputs pin once;
//! - `output_sha256`: the digest of the rendered text and view-model at
//!   pin time. It is the verification, not the content: a re-run recomputes
//!   it and compares, so the reproducibility check is real.
//!
//! # Replay and the changed world
//!
//! [`replay`] re-renders the pinned family with the pinned parameters
//! through the normal render path, recomputes the output digest and compares
//! it with the recorded one. It reads no clock: the pinned `as_of` is part of
//! the reference, so advancing the wall clock changes nothing the replay
//! sees. A match returns the view, `verified: true`.
//!
//! A mismatch is a named error, never a silent re-derivation. The facts a
//! view reads (commits, changes, co-change windows, rules, advisories) can
//! change after the pin was taken; a changed store means the pinned output
//! is void, and a re-run that quietly rendered the new view under the old
//! reference would make the pin dishonest. The error names the input hash,
//! the family, the pinned `as_of` and both digests. To cite the new view,
//! pin it anew: a new `as_of` is a new input tuple, a new node.
//!
//! # Writes
//!
//! Only [`pin`] writes, and only one `:ViewPin` node (catalogued in
//! `deletion::LABELS` as derived content, not history). Pinning the same
//! tuple again is idempotent when the store still renders the recorded
//! output; if it does not, or the analysis label differs, [`pin`] refuses
//! rather than overwrite a reference someone may already cite. Unpinned
//! renders write nothing: [`super::render`] is a read.
//!
//! No job kind is involved: the pin is written inline by the caller, and the
//! derived data it reads is produced by the existing `history`, `coupling`,
//! `enrich` and OSV jobs.

use serde_json::{Value, json};

use super::{Format, Params, View, family_of, render_text};
use crate::store_graph::{self, GraphStore, s};
use crate::{Error, Result, digest};

/// The node label a pin is stored under.
pub const LABEL: &str = "ViewPin";

/// The input tuple a pin is keyed by: `(family, params, window keys, as_of)`.
/// Its digest is the pin's `input_hash`.
fn input(family: &str, params: &Params, as_of: &str) -> Value {
    json!({"family": family, "params": params.json(),
        "window_keys": [params.window.key()], "as_of": as_of})
}

/// The family's view of `(repository, params)` and its output digest: the
/// rendered text and view-model, digested together, so a change to either
/// form is a change the verification sees.
fn render_view(
    graph: &GraphStore,
    repository_id: &str,
    slug: &str,
    params: &Params,
) -> Result<(View, String)> {
    let family = family_of(slug)?;
    let view = (family.render)(graph, repository_id, params)?;
    let output = digest(&json!({"text": render_text(&view), "view": view.model}));
    Ok((view, output))
}

/// The pin's record, as the caller reads it back.
fn record(row: &Value) -> Value {
    json!({"input_hash": s(row, "input_hash"), "family": s(row, "family"),
        "params": s(row, "params"), "window_keys": s(row, "window_keys"),
        "as_of": s(row, "as_of"), "analysis": s(row, "analysis"),
        "output_sha256": s(row, "output_sha256")})
}

/// The refusal a changed world earns: the recorded output is no longer what
/// the store renders for the same inputs.
fn diverged(input_hash: &str, family: &str, as_of: &str, recorded: &str, rendered: &str) -> Error {
    Error::conflict(format!(
        "viz pin {input_hash} ({family}, as_of {as_of}) diverged: it records output \
         {recorded} but the store now renders {rendered}; the facts the view reads changed \
         after the pin was taken, so the pinned output is void. Pin the view anew to cite it."
    ))
}

/// Pins `family` at `params` under `analysis`, stamped `as_of`. Writes the
/// one `:ViewPin` node and returns its record with `created` telling a fresh
/// pin from an idempotent repeat. Refuses an unknown family (the render's
/// refusal), and refuses a repeat whose recorded output or analysis differs
/// from the store's present answer.
pub fn pin(
    graph: &GraphStore,
    repository_id: &str,
    family: &str,
    params: &Params,
    analysis: &str,
    as_of: &str,
) -> Result<Value> {
    let input_hash = digest(&input(family, params, as_of));
    let (_, output_sha256) = render_view(graph, repository_id, family, params)?;
    let key = json!({"repo": repository_id, "hash": input_hash});
    let existing = graph.read(|db| {
        store_graph::one(
            db,
            "MATCH (vp:ViewPin {repository_id: $repo, input_hash: $hash}) \
             RETURN vp.input_hash AS input_hash, vp.family AS family, vp.params AS params, \
                    vp.window_keys AS window_keys, vp.as_of AS as_of, \
                    vp.analysis AS analysis, vp.output_sha256 AS output_sha256",
            store_graph::params(&key),
        )
    })?;
    if let Some(row) = existing {
        let recorded = record(&row);
        if s(&row, "analysis") != analysis {
            return Err(Error::conflict(format!(
                "viz pin {input_hash} already records analysis {}, not {analysis}",
                s(&row, "analysis")
            )));
        }
        if s(&row, "output_sha256") != output_sha256 {
            return Err(diverged(
                &input_hash,
                family,
                as_of,
                s(&row, "output_sha256"),
                &output_sha256,
            ));
        }
        let mut answer = recorded;
        answer["created"] = json!(false);
        return Ok(answer);
    }
    let params_text = graphicle_memory::text::canonical(&params.json());
    let window_keys = graphicle_memory::text::canonical(&json!([params.window.key()]));
    let fields = json!({"repo": repository_id, "hash": input_hash, "family": family,
        "params": params_text, "window_keys": window_keys, "as_of": as_of,
        "analysis": analysis, "output": output_sha256});
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MERGE (vp:ViewPin {repository_id: $repo, input_hash: $hash}) \
             ON CREATE SET vp.family = $family, vp.params = $params, \
                           vp.window_keys = $window_keys, vp.as_of = $as_of, \
                           vp.analysis = $analysis, vp.output_sha256 = $output",
            store_graph::params(&fields),
        )?;
        Ok(())
    })?;
    Ok(json!({"input_hash": input_hash, "family": family,
        "params": params_text, "window_keys": window_keys, "as_of": as_of,
        "analysis": analysis, "output_sha256": output_sha256, "created": true}))
}

/// Re-runs the pinned view of `input_hash` in `repository_id`: the pinned
/// family at the pinned parameters through the normal render, verified
/// against the recorded output digest. Returns the view-model with the pin
/// and `verified: true`. A pin the store cannot reproduce is the named
/// divergence refusal (see the module docs); a pin that is not in the store
/// is a refusal too. Reads no clock.
pub fn replay(graph: &GraphStore, repository_id: &str, input_hash: &str) -> Result<Value> {
    let row = graph
        .read(|db| {
            store_graph::one(
                db,
                "MATCH (vp:ViewPin {repository_id: $repo, input_hash: $hash}) \
                 RETURN vp.input_hash AS input_hash, vp.family AS family, vp.params AS params, \
                        vp.window_keys AS window_keys, vp.as_of AS as_of, \
                        vp.analysis AS analysis, vp.output_sha256 AS output_sha256",
                store_graph::params(&json!({"repo": repository_id, "hash": input_hash})),
            )
        })?
        .ok_or_else(|| Error::bad(format!("no viz pin {input_hash} in this repository")))?;
    let family = s(&row, "family");
    let stored: Value = serde_json::from_str(s(&row, "params")).map_err(|_| {
        Error::conflict(format!("viz pin {input_hash} holds unreadable parameters"))
    })?;
    let params = Params::parse(&stored)?;
    let as_of = s(&row, "as_of");
    // The record must still be the reference its key names: the inputs it
    // holds hash to the key it is stored under.
    if digest(&input(family, &params, as_of)) != input_hash {
        return Err(Error::conflict(format!(
            "viz pin {input_hash} no longer matches its own inputs; the record was altered"
        )));
    }
    let (view, output) = render_view(graph, repository_id, family, &params)?;
    let recorded = s(&row, "output_sha256");
    if output != recorded {
        return Err(diverged(input_hash, family, as_of, recorded, &output));
    }
    Ok(json!({"family": family, "format": Format::Json.name(),
        "params": params.json(), "view": view.model,
        "pin": record(&row), "verified": true}))
}
