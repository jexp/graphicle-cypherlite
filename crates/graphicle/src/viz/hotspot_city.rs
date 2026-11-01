//! The hotspot-city family (the plan's §5, the D2 view): a
//! squarified-treemap city over the canonical folder rollup of the
//! repository's files, with the top-N hot list beside it. CodeScene's
//! semantics in a CodeCity skin, as a layout model — the view-model
//! carries the channels and the cells, and the drawing is the dashboard
//! project's (the plan's §4 defers every renderer).
//!
//! - **districts** are the folder prefixes of the files' paths, flat: the
//!   stored facts name folders, and no `:Community` exists to group them
//!   another way (the plan's §6 gap);
//! - **members** are the files with at least one stored change: a file
//!   never changed has no fact to size a cell from;
//! - the **height** channel is the all-time change count per file — line
//!   count is not stored, so churn-count is the size proxy this family
//!   names, and the model's `channels` say so;
//! - the **color** channel is the same count inside the window the call
//!   named, keyed like the coupling windows (`all`, `90d` — no new ones).
//!   The 90-day window trails the repository's own newest commit, not the
//!   wall clock, so the view stays a pure function of the stored facts.
//!
//! The layout is the standard squarify algorithm (Bruls, Huizing and van
//! Wijk) run in Rust over the canonical sort — districts by total with
//! ties by path, members by height with ties by path — so no coordinates
//! are stored and the same facts always lay out the same city. The layout
//! id [`LAYOUT`] versions the algorithm: a relayout is a new id, never a
//! mutation.
//!
//! Every statement anchors on `repository_id` and never passes through
//! `(:Project)` (the supernode rule), and every district and member row
//! cites its path as `evidence`.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use super::{Overlay, Params, View, Window};
use crate::Result;
use crate::store_graph::{self, GraphStore};

/// The layout algorithm's id, versioned in every model: the algorithm and
/// its parameter meanings, so a relayout is a new id, never a mutation of
/// this one (the plan's §3 pin).
pub const LAYOUT: &str = "city-v1";
/// The nominal canvas the city lays out over, in layout units: a
/// view-model needs a plane, and a fixed square keeps the model a pure
/// function of the facts.
const CANVAS: f64 = 1000.0;
/// The days the `90d` window trails the repository's newest commit (the
/// coupling windows' key, no new ones).
const WINDOW_DAYS: i64 = 90;
/// The district of a path with no folder in it: the repository root.
const ROOT: &str = ".";
/// The height channel's name: churn-count, the size proxy this family
/// states (line count is not stored).
const HEIGHT: &str = "churn-count";
/// The color channel's name.
const COLOR: &str = "churn-window";

/// Every commit's every change, with its date: the one statement the
/// family reads, so both channels count the same stored facts.
const CHANGES: &str = "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (ch:Change {repository_id: $repo})-[:MODIFIES]->(f:File {repository_id: $repo}) \
     RETURN c.oid AS commit, c.commit_date AS at, f.path AS file";

/// The statements the family reads (the supernode rule's test): each
/// anchors every labeled node it matches on the `repository_id` key, and
/// none crosses `(:Project)`.
#[cfg(test)]
pub(super) const STATEMENTS: [&str; 1] = [CHANGES];

/// One member of the city: a file with its two channels — `height`, the
/// all-time change count (the size proxy), and `windowed`, the same count
/// inside the window the call named (the color channel).
struct Member {
    district: String,
    path: String,
    height: i64,
    windowed: i64,
}

/// One district: a folder prefix with its members rolled up. `value` is
/// the members' heights summed — the district's share of the canvas.
struct District {
    name: String,
    files: usize,
    value: i64,
    windowed: i64,
}

/// The family's view: the districts and members of the city — bounds,
/// cells and channels, every row citing its path — beside the top-N hot
/// list of the window.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let members = read(graph, repository, params.window)?;

    // The districts: the folder rollup of the members, then the canonical
    // order (value descending, ties by the district's path).
    let mut rollup: BTreeMap<String, District> = BTreeMap::new();
    for member in &members {
        let district = rollup
            .entry(member.district.clone())
            .or_insert_with(|| District {
                name: member.district.clone(),
                files: 0,
                value: 0,
                windowed: 0,
            });
        district.files += 1;
        district.value += member.height;
        district.windowed += member.windowed;
    }
    let mut districts: Vec<District> = rollup.into_values().collect();
    districts.sort_by(|a, b| b.value.cmp(&a.value).then(a.name.cmp(&b.name)));

    // The members of each district, in the canonical order the layout runs
    // on (height descending, ties by path).
    let mut grouped: BTreeMap<&str, Vec<&Member>> = BTreeMap::new();
    for member in &members {
        grouped
            .entry(member.district.as_str())
            .or_default()
            .push(member);
    }
    for district in grouped.values_mut() {
        district.sort_by(|a, b| b.height.cmp(&a.height).then(a.path.cmp(&b.path)));
    }

    // The layout: the districts over the canvas, then each district's
    // members over its bounds. The cells come back in the order given.
    let canvas = Rect {
        x: 0.0,
        y: 0.0,
        w: CANVAS,
        h: CANVAS,
    };
    let district_cells = squarify(
        &districts
            .iter()
            .map(|district| (district.name.clone(), district.value as f64))
            .collect::<Vec<_>>(),
        canvas,
    );

    // The color channel's heat: each member's windowed churn over the
    // hottest member's, so the hottest cell reads 1.0.
    let hottest = members
        .iter()
        .map(|member| member.windowed)
        .max()
        .unwrap_or(0);
    let heat = |windowed: i64| {
        if windowed > 0 && hottest > 0 {
            round(windowed as f64 / hottest as f64)
        } else {
            0.0
        }
    };

    let mut member_rows = Vec::new();
    let mut district_rows = Vec::new();
    let none = Vec::new();
    for (district, (_, bounds)) in districts.iter().zip(&district_cells) {
        let members = grouped.get(district.name.as_str()).unwrap_or(&none);
        let cells = squarify(
            &members
                .iter()
                .map(|member| (member.path.clone(), member.height as f64))
                .collect::<Vec<_>>(),
            *bounds,
        );
        for (member, (_, cell)) in members.iter().zip(&cells) {
            member_rows.push(json!({
                "evidence": member.path.as_str(), "file": member.path.as_str(),
                "district": member.district.as_str(), "height": member.height,
                "churn_window": member.windowed, "heat": heat(member.windowed),
                "cell": cell_json(*cell), "aspect": round(aspect(*cell)),
            }));
        }
        district_rows.push(json!({
            "evidence": district.name.as_str(), "district": district.name.as_str(),
            "files": district.files, "value": district.value,
            "churn_window": district.windowed, "bounds": cell_json(*bounds),
            "aspect": round(aspect(*bounds)),
        }));
    }

    // The hot list beside the map: files by their windowed churn, the rank
    // first and the evidence breaking ties, cut to the call's cap — then
    // the districts the same way.
    let mut hot: Vec<Value> = members
        .iter()
        .map(|member| {
            json!({"evidence": member.path.as_str(), "file": member.path.as_str(),
                "district": member.district.as_str(), "height": member.height,
                "churn_window": member.windowed, "heat": heat(member.windowed)})
        })
        .collect();
    hot.sort_by(|a, b| {
        int(b, "churn_window")
            .cmp(&int(a, "churn_window"))
            .then(evidence(a).cmp(evidence(b)))
    });
    hot.truncate(params.cap);
    let mut rolled: Vec<Value> = districts
        .iter()
        .map(|district| {
            json!({"evidence": district.name.as_str(), "district": district.name.as_str(),
                "files": district.files, "value": district.value,
                "churn_window": district.windowed})
        })
        .collect();
    rolled.sort_by(|a, b| {
        int(b, "churn_window")
            .cmp(&int(a, "churn_window"))
            .then(evidence(a).cmp(evidence(b)))
    });
    rolled.truncate(params.cap);

    let model = json!({
        "layout": LAYOUT,
        "canvas": {"width": CANVAS, "height": CANVAS},
        "window": {"key": params.window.key(), "basis": window_basis(params.window)},
        "channels": {
            "height": {"name": HEIGHT, "basis": format!(
                "all-time change count per file (:Change-[:MODIFIES]->(:File)); \
                 line count is not stored, so {HEIGHT} is the size proxy")},
            "color": {"name": COLOR, "basis":
                "the same count inside the window the call named"}
        },
        "overlay": overlay(params.overlay),
        "districts": district_rows,
        "members": member_rows,
        "notes": [
            "districts are the folder prefixes of the file paths, flat: the stored \
             facts name folders, and no :Community exists to group them another way",
            "members are the files with at least one stored change: a file never \
             changed has no fact to size a cell from",
        ],
    });
    Ok(View {
        sections: vec![
            ("hotspot files by windowed churn", hot),
            ("districts by windowed churn", rolled),
        ],
        model,
        mermaid: None,
    })
}

/// The members, read from the change facts: one row per change, so
/// `height` counts them all and `windowed` the ones inside the window —
/// every change for `all`, the changes of the trailing [`WINDOW_DAYS`] of
/// the repository's newest commit for `90d` (a commit without a parseable
/// date sits outside a time window, as the coupling derivation counts
/// it). The map's order is the store's; nothing relies on it.
fn read(graph: &GraphStore, repository: &str, window: Window) -> Result<Vec<Member>> {
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            CHANGES,
            store_graph::params(&json!({"repo": repository})),
        )
    })?;
    let cutoff = rows
        .iter()
        .filter_map(date)
        .max()
        .map(|newest| newest - Duration::days(WINDOW_DAYS));
    let mut files: BTreeMap<String, Member> = BTreeMap::new();
    for row in &rows {
        let path = store_graph::s(row, "file").to_string();
        let member = files.entry(path.clone()).or_insert_with(|| Member {
            district: district(&path),
            path,
            height: 0,
            windowed: 0,
        });
        member.height += 1;
        let inside = match window {
            Window::All => true,
            Window::NinetyDays => {
                matches!((cutoff, date(row)), (Some(cutoff), Some(at)) if at >= cutoff)
            }
        };
        member.windowed += inside as i64;
    }
    Ok(files.into_values().collect())
}

/// A row's commit date, when it parses (the writer stores rfc3339).
fn date(row: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(store_graph::s(row, "at"))
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// A path's district: its folder prefix — `src` for `src/lib.rs` — the
/// repository root for a path with no folder.
fn district(path: &str) -> String {
    match path.rfind('/') {
        Some(at) if !path[..at].is_empty() => path[..at].to_string(),
        _ => ROOT.to_string(),
    }
}

/// The window's basis, as the model states it.
fn window_basis(window: Window) -> String {
    match window {
        Window::All => "every stored change of the file".to_string(),
        Window::NinetyDays => format!(
            "the file's changes whose commit date falls within the {WINDOW_DAYS} days \
             trailing the repository's newest commit — the history's own clock, never \
             the wall clock, so the view is a pure function of the stored facts"
        ),
    }
}

/// The overlay the call asked for, as the model answers it: this family
/// interprets none of them. Its map carries only its own two channels; an
/// overlay asked for is answered with the honest note, and joining another
/// family's columns onto the map is the consolidation's business, not a
/// half-implemented column here.
fn overlay(asked: Overlay) -> Value {
    match asked {
        Overlay::None => json!({"asked": "none"}),
        _ => json!({"asked": asked.name(), "interpreted": false,
            "note": "the hotspot-city family does not interpret this overlay: its map \
                     carries the height and color channels only, and cross-family \
                     overlay joins are the consolidation's business"}),
    }
}

/// One axis-aligned cell of the layout, in canvas units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) w: f64,
    pub(super) h: f64,
}

/// The squarified treemap (Bruls, Huizing and van Wijk) of `items` —
/// `(key, area)` pairs already in the canonical order, area descending
/// with ties by key — laid out over `bounds`: each key's cell, in the same
/// order. A row grows against the shorter side of the remaining rectangle
/// while adding the next item does not worsen the row's worst aspect
/// ratio, then is placed as a strip spanning that side. Pure arithmetic
/// over the given order: no stored coordinates, and the same inputs
/// always lay out the same cells.
pub(super) fn squarify(items: &[(String, f64)], bounds: Rect) -> Vec<(String, Rect)> {
    let mut cells = Vec::with_capacity(items.len());
    let mut rest = bounds;
    let mut at = 0;
    while at < items.len() {
        // The row is laid against the shorter side of what is left, and the
        // values left are scaled to its area.
        let side = rest.w.min(rest.h);
        let left: f64 = items[at..].iter().map(|(_, area)| *area).sum();
        if side <= 0.0 || left <= 0.0 {
            break;
        }
        let scale = rest.w * rest.h / left;
        // Grow the row while the next item does not worsen its worst aspect.
        let mut row = vec![items[at].1 * scale];
        while at + row.len() < items.len() {
            let mut grown = row.clone();
            grown.push(items[at + row.len()].1 * scale);
            if worst(&grown, side) <= worst(&row, side) {
                row = grown;
            } else {
                break;
            }
        }
        // Place the row as a strip of the thickness its share buys,
        // spanning the shorter side; the members' lengths share the strip.
        let total: f64 = row.iter().sum();
        let thickness = total / side;
        let vertical = rest.w >= rest.h;
        let mut along = 0.0;
        for (offset, area) in row.iter().enumerate() {
            let length = area / thickness;
            let cell = if vertical {
                Rect {
                    x: rest.x,
                    y: rest.y + along,
                    w: thickness,
                    h: length,
                }
            } else {
                Rect {
                    x: rest.x + along,
                    y: rest.y,
                    w: length,
                    h: thickness,
                }
            };
            cells.push((items[at + offset].0.clone(), cell));
            along += length;
        }
        at += row.len();
        rest = if vertical {
            Rect {
                x: rest.x + thickness,
                y: rest.y,
                w: rest.w - thickness,
                h: rest.h,
            }
        } else {
            Rect {
                x: rest.x,
                y: rest.y + thickness,
                w: rest.w,
                h: rest.h - thickness,
            }
        };
    }
    cells
}

/// A row's worst aspect ratio, laid against a side of `side`: the strip's
/// thickness is `total/side`, an item of `area` spans `area/thickness`
/// along the side, so its aspect is the worse of the two quotients.
fn worst(row: &[f64], side: f64) -> f64 {
    let total: f64 = row.iter().sum();
    let thickness = total / side;
    row.iter().fold(0.0, |worst, &area| {
        let ratio = area / (thickness * thickness);
        worst.max(ratio).max(1.0 / ratio)
    })
}

/// A cell's aspect ratio, the worse of its two side quotients (at least
/// 1 for a proper cell).
fn aspect(cell: Rect) -> f64 {
    if cell.w > 0.0 && cell.h > 0.0 {
        (cell.w / cell.h).max(cell.h / cell.w)
    } else {
        0.0
    }
}

/// A coordinate or ratio rounded to the layout's hundredth: the model
/// carries readable numbers, and the rounding is part of the layout id's
/// meaning.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A cell's bounds, as the model carries them.
fn cell_json(cell: Rect) -> Value {
    json!({"x": round(cell.x), "y": round(cell.y), "w": round(cell.w), "h": round(cell.h)})
}

/// A row's integer field (the history pack's `int`).
fn int(row: &Value, key: &str) -> i64 {
    row[key].as_i64().unwrap_or(0)
}

/// A row's evidence (the history pack's tie key).
fn evidence(row: &Value) -> &str {
    row["evidence"].as_str().unwrap_or_default()
}
