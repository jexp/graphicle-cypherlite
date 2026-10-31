//! Shared commit-test harness: the fault-injecting VFS wrapper and the
//! connection/commit helpers the themed test modules build on.

use super::super::*;
use crate::{Database, OpenOptions};
use cypherlite_storage::{Vfs, VfsFile, VfsMeta, VfsOpen, Wal};
use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct FaultVfs {
    inner: Arc<dyn Vfs>,
    pub(super) short_write: Arc<AtomicBool>,
    pub(super) failed_barrier: Arc<AtomicBool>,
    pub(super) failed_rotate: Arc<AtomicBool>,
}

struct FaultFile {
    inner: Box<dyn VfsFile>,
    short_write: Arc<AtomicBool>,
    failed_barrier: Arc<AtomicBool>,
}

impl VfsFile for FaultFile {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if self.short_write.swap(false, Ordering::SeqCst) {
            self.inner.write_all(&bytes[..2])?;
            return Err(Error::io("short WAL frame write"));
        }
        self.inner.write_all(bytes)
    }
    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.inner.read_exact(bytes)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.inner.sync_all()
    }
    fn durability_barrier(&mut self) -> Result<()> {
        self.inner.durability_barrier()?;
        if self.failed_barrier.swap(false, Ordering::SeqCst) {
            Err(Error::io("barrier failed after publication"))
        } else {
            Ok(())
        }
    }
    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.set_len(len)
    }
}

impl Vfs for FaultVfs {
    fn durable(&self) -> bool {
        self.inner.durable()
    }
    fn create_dir_all(&self, p: &str) -> Result<()> {
        self.inner.create_dir_all(p)
    }
    fn create_dir_all_for_atomic(&self, p: &str) -> Result<()> {
        self.inner.create_dir_all_for_atomic(p)
    }
    fn list_dir(&self, p: &str) -> Result<Vec<String>> {
        self.inner.list_dir(p)
    }
    fn exists(&self, p: &str) -> Result<bool> {
        self.inner.exists(p)
    }
    fn metadata(&self, p: &str) -> Result<VfsMeta> {
        self.inner.metadata(p)
    }
    fn open(&self, p: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        if opts.truncate && self.failed_rotate.swap(false, Ordering::SeqCst) {
            return Err(Error::io("injected new WAL segment failure"));
        }
        let inner = self.inner.open(p, opts)?;
        if opts.append {
            Ok(Box::new(FaultFile {
                inner,
                short_write: Arc::clone(&self.short_write),
                failed_barrier: Arc::clone(&self.failed_barrier),
            }))
        } else {
            Ok(inner)
        }
    }
    fn remove_file(&self, p: &str) -> Result<()> {
        self.inner.remove_file(p)
    }
    fn remove_dir_all(&self, p: &str) -> Result<()> {
        self.inner.remove_dir_all(p)
    }
    fn rename(&self, a: &str, b: &str) -> Result<()> {
        self.inner.rename(a, b)
    }
    fn sync_dir(&self, p: &str) -> Result<()> {
        self.inner.sync_dir(p)
    }
}

/// A `Connection` built by hand for tests that probe internals
/// (`shared` must stay alive while the connection is used).
pub(super) fn raw_connection(shared: &Arc<Shared>) -> crate::Connection {
    crate::connection::fresh(Arc::clone(shared), cypherlite_core::Namespace::Neo4j)
}

/// Spawn `n` threads that all wait on one barrier, then each connects and
/// runs one `CREATE (:Person {name: 'p<i>'})`; joins with panic propagation.
pub(super) fn spawn_person_create_storm(db: &Arc<Database>, n: usize) {
    use std::sync::Barrier;
    let barrier = Arc::new(Barrier::new(n));
    let mut hs = Vec::new();
    for i in 0..n {
        let db = Arc::clone(db);
        let barrier = Arc::clone(&barrier);
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            c.query(&format!("CREATE (n:Person {{name: 'p{i}'}})"))
                .unwrap();
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
}

pub(super) fn install_fault_wal(shared: &Shared) -> Arc<FaultVfs> {
    let fault = Arc::new(FaultVfs {
        inner: shared.store.vfs(),
        short_write: Arc::new(AtomicBool::new(false)),
        failed_barrier: Arc::new(AtomicBool::new(false)),
        failed_rotate: Arc::new(AtomicBool::new(false)),
    });
    let wal = Wal::open(Arc::clone(&fault) as Arc<dyn Vfs>, "wal", 0).unwrap();
    *shared.wal.lock().unwrap() = wal;
    fault
}

pub(super) fn node_op(id: i64) -> RedoOp {
    RedoOp::CreateNode {
        id,
        labels: Arc::new(vec!["N".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }
}

pub(super) fn slot(
    id: i64,
    txn_id: u64,
    snapshot_ts: u64,
    waiter: &Arc<CommitWaiter>,
) -> CommitSlot {
    CommitSlot {
        snapshot_ts,
        txn_id,
        ops: vec![node_op(id)],
        parse_plan_ns: 0,
        execute_ns: 0,
        enqueued: Instant::now(),
        waiter: Arc::clone(waiter),
    }
}

pub(super) fn untimed_file_db(path: &str) -> Database {
    Database::open_with(
        path,
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Phase {
    Sync,
    Apply,
    Rotate,
}
