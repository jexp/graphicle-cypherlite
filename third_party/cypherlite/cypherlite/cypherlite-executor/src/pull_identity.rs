//! Forward + reverse pull-identity bimap (ADR-0080).

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use crate::redo::PullKind;

/// Both maps behind one lock: the forward/reverse bimap invariant is
/// maintained atomically — a concurrent reader never observes a put's
/// forward insert before the stale reverse entry is removed.
#[derive(Debug, Default)]
pub(crate) struct PullIdentityIndex {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    fwd: HashMap<(String, PullKind, String), i64>,
    rev: HashMap<(PullKind, i64), HashSet<(String, String)>>,
}

impl PullIdentityIndex {
    pub(crate) fn get(&self, source: &str, kind: PullKind, remote_id: &str) -> Option<i64> {
        self.inner
            .read()
            .unwrap()
            .fwd
            .get(&(source.to_string(), kind, remote_id.to_string()))
            .copied()
    }

    pub(crate) fn put(&self, source: String, kind: PullKind, remote_id: String, local_id: i64) {
        let mut inner = self.inner.write().unwrap();
        let key = (source.clone(), kind, remote_id.clone());
        if let Some(old) = inner.fwd.insert(key, local_id) {
            if old != local_id {
                if let Some(set) = inner.rev.get_mut(&(kind, old)) {
                    set.remove(&(source.clone(), remote_id.clone()));
                }
            }
        }
        inner
            .rev
            .entry((kind, local_id))
            .or_default()
            .insert((source, remote_id));
    }

    pub(crate) fn remove(&self, kind: PullKind, local_id: i64) {
        let mut inner = self.inner.write().unwrap();
        if let Some(keys) = inner.rev.remove(&(kind, local_id)) {
            for (source, remote_id) in keys {
                inner.fwd.remove(&(source, kind, remote_id));
            }
        }
    }
}
