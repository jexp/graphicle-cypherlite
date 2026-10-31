//! RETURN-time refresh of entity snapshots (ADR-0170).
//!
//! A returned value may hold node, relationship or path snapshots taken
//! before later writes in the statement; RETURN replaces them with the
//! current state. One copy-on-write walk does it: a list or map with nothing
//! to refresh comes back as the same shared payload, and a container that
//! holds entities is rebuilt with only its changed children replaced. Within
//! one projection each shared payload is walked once, however many rows
//! carry it.

use std::collections::HashMap;
use std::sync::Arc;

use cypherlite_core::{Path, Value};

use super::ExecGraph;

/// Refresh results of one projection, keyed by list/map payload address.
///
/// Each entry holds the original payload, so its address cannot be reused by
/// a new allocation while the cache lives. Entries are valid only while the
/// graph state is fixed: one cache serves one projection, whose expressions
/// never write.
#[derive(Default)]
pub(crate) struct MaterializeCache {
    seen: HashMap<*const (), (Value, Option<Value>)>,
    #[cfg(test)]
    walked: usize,
}

impl MaterializeCache {
    /// `walk`'s result for `payload` (the payload of `v`), walking it only on
    /// first sight. A payload that `v` alone holds cannot be met again, so it
    /// is walked without an entry.
    fn memo<T>(
        &mut self,
        payload: &Arc<T>,
        v: &Value,
        walk: impl FnOnce(&mut Self) -> Option<Value>,
    ) -> Option<Value> {
        let key = Arc::as_ptr(payload).cast::<()>();
        let shared = Arc::strong_count(payload) > 1;
        if shared {
            if let Some((_, out)) = self.seen.get(&key) {
                return out.clone();
            }
        }
        #[cfg(test)]
        {
            self.walked += 1;
        }
        let out = walk(self);
        if shared {
            self.seen.insert(key, (v.clone(), out.clone()));
        }
        out
    }

    /// List and map payloads whose children this cache walked.
    #[cfg(test)]
    pub(super) fn walked(&self) -> usize {
        self.walked
    }
}

impl ExecGraph<'_> {
    /// Refresh the entity snapshots in `v`, walking each shared list or map
    /// payload at most once per `cache`. Lists and maps that hold no entity
    /// come back as the same shared payload (ADR-0170); only containers that
    /// hold an entity are rebuilt.
    pub(crate) fn materialize_cached(&self, v: Value, cache: &mut MaterializeCache) -> Value {
        self.refreshed(&v, cache).unwrap_or(v)
    }

    /// Copy-on-write refresh of `v`: `None` when nothing beneath `v` needs
    /// refreshing, so the caller keeps `v` and its shared payload; otherwise
    /// the rebuilt value.
    fn refreshed(&self, v: &Value, cache: &mut MaterializeCache) -> Option<Value> {
        match v {
            Value::Node(n) => self.node(n.id).map(Value::from_node),
            Value::Relationship(r) => self.rel(r.id).map(Value::from_relationship),
            Value::Path(p) => Some(Value::from_path(Path {
                nodes: p
                    .nodes
                    .iter()
                    .map(|n| self.node(n.id).unwrap_or_else(|| n.clone()))
                    .collect(),
                rels: p
                    .rels
                    .iter()
                    .map(|r| self.rel(r.id).unwrap_or_else(|| r.clone()))
                    .collect(),
            })),
            Value::List(xs) => cache.memo(xs, v, |cache| {
                self.refreshed_values(xs.iter(), cache)
                    .map(|ys| Value::List(ys.into()))
            }),
            Value::Map(m) => cache.memo(m, v, |cache| {
                self.refreshed_values(m.values(), cache)
                    .map(|ys| Value::Map(Arc::new(m.keys().cloned().zip(ys).collect())))
            }),
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Temporal(_)
            | Value::Point(_)
            | Value::Uuid(_)
            | Value::Vector { .. } => None,
        }
    }

    /// Copy-on-write refresh of a container's children: `None` when no child
    /// changed; otherwise every child in order, the changed ones replaced and
    /// the rest cloned (an `Arc` bump for shared payloads).
    fn refreshed_values<'v>(
        &self,
        values: impl ExactSizeIterator<Item = &'v Value> + Clone,
        cache: &mut MaterializeCache,
    ) -> Option<Vec<Value>> {
        let mut out: Option<Vec<Value>> = None;
        for (i, x) in values.clone().enumerate() {
            let fresh = self.refreshed(x, cache);
            if fresh.is_some() && out.is_none() {
                let mut ys = Vec::with_capacity(values.len());
                ys.extend(values.clone().take(i).cloned());
                out = Some(ys);
            }
            if let Some(ys) = out.as_mut() {
                ys.push(fresh.unwrap_or_else(|| x.clone()));
            }
        }
        out
    }
}
