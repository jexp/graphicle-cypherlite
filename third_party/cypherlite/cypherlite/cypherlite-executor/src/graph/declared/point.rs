//! POINT index grid.

use std::collections::{HashMap, HashSet};

use cypherlite_core::{NodeId, Value};

use super::order::SpatialSeek;
use super::DrainState;

#[derive(Debug, Default)]
pub(crate) struct PointIndex {
    pub(super) pts: HashMap<NodeId, (i32, f64, f64)>,
    pub(super) grid: HashMap<(i32, i32), HashSet<NodeId>>,
    pub(super) dirty: HashSet<NodeId>,
}

impl DrainState for PointIndex {
    fn dirty(&self) -> &HashSet<NodeId> {
        &self.dirty
    }
    fn dirty_mut(&mut self) -> &mut HashSet<NodeId> {
        &mut self.dirty
    }
    fn insert(&mut self, id: NodeId, value: &Value) {
        if let Value::Point(p) = value {
            point_insert(self, id, p.srid, p.x, p.y);
        }
    }
    fn remove_node(&mut self, id: NodeId) {
        point_remove_node(self, id);
    }
}

const CELL: f64 = 1.0;

pub(super) fn cell_of(x: f64, y: f64) -> (i32, i32) {
    fn bucket(v: f64) -> i32 {
        let scaled = (v / CELL).floor();
        if !scaled.is_finite() {
            return if scaled.is_sign_negative() {
                i32::MIN
            } else {
                i32::MAX
            };
        }
        scaled.clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
    (bucket(x), bucket(y))
}

pub(super) fn point_remove_node(st: &mut PointIndex, id: NodeId) {
    if let Some((_, x, y)) = st.pts.remove(&id) {
        let cell = cell_of(x, y);
        if let Some(set) = st.grid.get_mut(&cell) {
            set.remove(&id);
            if set.is_empty() {
                st.grid.remove(&cell);
            }
        }
    }
}

pub(super) fn point_insert(st: &mut PointIndex, id: NodeId, srid: i32, x: f64, y: f64) {
    point_remove_node(st, id);
    st.pts.insert(id, (srid, x, y));
    st.grid.entry(cell_of(x, y)).or_default().insert(id);
}

pub(super) fn spatial_ids(st: &PointIndex, seek: &SpatialSeek) -> HashSet<NodeId> {
    match seek {
        SpatialSeek::BBox { lo, hi } => {
            let (Value::Point(lo), Value::Point(hi)) = (lo, hi) else {
                return st.pts.keys().copied().collect();
            };
            let (minx, maxx) = if lo.x <= hi.x {
                (lo.x, hi.x)
            } else {
                (hi.x, lo.x)
            };
            let (miny, maxy) = if lo.y <= hi.y {
                (lo.y, hi.y)
            } else {
                (hi.y, lo.y)
            };
            cells_in(st, minx, miny, maxx, maxy)
                .into_iter()
                .filter(|id| {
                    st.pts.get(id).is_some_and(|(_, x, y)| {
                        *x >= minx && *x <= maxx && *y >= miny && *y <= maxy
                    })
                })
                .collect()
        }
        SpatialSeek::Distance {
            origin,
            max,
            inclusive: _,
        } => {
            let Value::Point(origin) = origin else {
                return st.pts.keys().copied().collect();
            };
            let (minx, miny, maxx, maxy) = distance_box(origin.srid, origin.x, origin.y, *max);
            // Cell visit is a superset. Residual `point.distance` applies
            // the inclusive/exclusive bound and the z/SRID rules.
            cells_in(st, minx, miny, maxx, maxy)
        }
    }
}

pub(super) fn cells_in(
    st: &PointIndex,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> HashSet<NodeId> {
    let (x0, y0) = cell_of(minx, miny);
    let (x1, y1) = cell_of(maxx, maxy);
    let w = (x1 as i64).saturating_sub(x0 as i64).saturating_add(1);
    let h = (y1 as i64).saturating_sub(y0 as i64).saturating_add(1);
    // A query-controlled rectangle can span the full i32 grid (~2^64 cells).
    // Walking that never finishes and ignores cancellation. When the
    // rectangle is larger than the occupied grid — or inverted — scan the
    // cells that exist. A small ordered rectangle walks `x0..=x1`.
    let area = if w > 0 && h > 0 {
        (w as u128).saturating_mul(h as u128)
    } else {
        u128::MAX
    };
    if area > st.grid.len() as u128 {
        return st
            .grid
            .iter()
            .filter(|(&(x, y), _)| x >= x0 && x <= x1 && y >= y0 && y <= y1)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
    }
    let mut out = HashSet::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            if let Some(set) = st.grid.get(&(x, y)) {
                out.extend(set.iter().copied());
            }
        }
    }
    out
}

pub(crate) fn spatial_holds(v: &Value, seek: &SpatialSeek) -> bool {
    match seek {
        SpatialSeek::BBox { lo, hi } => {
            match crate::spatial::point_within_bbox(&[v.clone(), lo.clone(), hi.clone()]) {
                Ok(Value::Bool(b)) => b,
                Ok(_) => false,
                Err(_) => true,
            }
        }
        SpatialSeek::Distance {
            origin,
            max,
            inclusive,
        } => match crate::spatial::point_distance(&[v.clone(), origin.clone()]) {
            Ok(Value::Float(d)) => {
                if *inclusive {
                    d <= *max
                } else {
                    d < *max
                }
            }
            Ok(_) => false,
            Err(_) => true,
        },
    }
}

pub(super) fn distance_box(srid: i32, x: f64, y: f64, max: f64) -> (f64, f64, f64, f64) {
    let (dx, dy) = if matches!(srid, 4326 | 4979) {
        let lat = y.to_radians();
        let lat_delta = max.abs() / 110_000.0 + CELL;
        let lon_scale = (110_000.0 * lat.cos()).abs().max(1.0);
        (max.abs() / lon_scale + CELL, lat_delta)
    } else {
        (max.abs() + CELL, max.abs() + CELL)
    };
    (x - dx, y - dy, x + dx, y + dy)
}
