use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::sync::{Arc, Mutex};

use cypherlite_core::{Error, Result};

use super::{Vfs, VfsFile, VfsMeta, VfsOpen};

struct MemoryInner {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

pub struct MemoryVfs {
    inner: Arc<MemoryInner>,
}

impl MemoryVfs {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MemoryInner {
                files: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.inner.files.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MemoryVfs {
    fn default() -> Self {
        Self::new()
    }
}

fn is_prefix(files: &BTreeMap<String, Vec<u8>>, path: &str) -> bool {
    let p = format!("{path}/");
    files.keys().any(|k| k.starts_with(&p))
}

impl Vfs for MemoryVfs {
    fn durable(&self) -> bool {
        false
    }

    fn create_dir_all(&self, _path: &str) -> Result<()> {
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let files = self.lock();
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut names = Vec::new();
        for key in files.keys() {
            let rest = if prefix.is_empty() {
                key.as_str()
            } else if let Some(r) = key.strip_prefix(&prefix) {
                r
            } else {
                continue;
            };
            let name = rest.split('/').next().unwrap_or(rest);
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    fn exists(&self, path: &str) -> Result<bool> {
        let files = self.lock();
        Ok(files.contains_key(path) || is_prefix(&files, path))
    }

    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        let files = self.lock();
        if let Some(b) = files.get(path) {
            return Ok(VfsMeta {
                len: b.len() as u64,
                is_dir: false,
            });
        }
        if is_prefix(&files, path) {
            return Ok(VfsMeta {
                len: 0,
                is_dir: true,
            });
        }
        Err(Error::io(format!("missing {path}")))
    }

    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        {
            let mut files = self.lock();
            if opts.create_new {
                if files.contains_key(path) || is_prefix(&files, path) {
                    return Err(Error::io(format!("already exists: {path}")));
                }
                files.insert(path.to_string(), Vec::new());
            } else if opts.create {
                files.entry(path.to_string()).or_default();
            } else if !files.contains_key(path) {
                return Err(Error::io(format!("missing {path}")));
            }
            if opts.truncate {
                files.insert(path.to_string(), Vec::new());
            }
        }
        let pos = if opts.append {
            self.lock().get(path).map(|b| b.len() as u64).unwrap_or(0)
        } else {
            0
        };
        Ok(Box::new(MemoryFile {
            inner: Arc::clone(&self.inner),
            path: path.to_string(),
            pos,
            append: opts.append,
        }))
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.lock().remove(path);
        Ok(())
    }

    fn remove_dir_all(&self, path: &str) -> Result<()> {
        let mut files = self.lock();
        if path.is_empty() {
            files.clear();
            return Ok(());
        }
        let prefix = format!("{path}/");
        files.retain(|k, _| k != path && !k.starts_with(&prefix));
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut files = self.lock();
        let data = files
            .remove(from)
            .ok_or_else(|| Error::io(format!("missing {from}")))?;
        files.insert(to.to_string(), data);
        Ok(())
    }

    fn sync_dir(&self, _path: &str) -> Result<()> {
        // Virtual namespace: no host directory entry to persist.
        Ok(())
    }
}

struct MemoryFile {
    inner: Arc<MemoryInner>,
    path: String,
    pos: u64,
    append: bool,
}

impl MemoryFile {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.inner.files.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl VfsFile for MemoryFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        let mut pos = self.pos;
        {
            let mut files = self.lock();
            let data = files.entry(self.path.clone()).or_default();
            if self.append {
                pos = data.len() as u64;
            }
            let start = pos as usize;
            if start + buf.len() > data.len() {
                data.resize(start + buf.len(), 0);
            }
            data[start..start + buf.len()].copy_from_slice(buf);
            pos += buf.len() as u64;
        }
        self.pos = pos;
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let files = self.lock();
        let data = files
            .get(&self.path)
            .ok_or_else(|| Error::io(format!("missing {}", self.path)))?;
        let pos = self.pos as usize;
        if pos + buf.len() > data.len() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof").into());
        }
        buf.copy_from_slice(&data[pos..pos + buf.len()]);
        drop(files);
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let len = self
            .lock()
            .get(&self.path)
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        let next = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => {
                let base = len as i64;
                base.saturating_add(n).max(0) as u64
            }
            SeekFrom::Current(n) => {
                let base = self.pos as i64;
                base.saturating_add(n).max(0) as u64
            }
        };
        self.pos = next;
        Ok(self.pos)
    }

    fn sync_all(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        let mut files = self.lock();
        let data = files
            .get_mut(&self.path)
            .ok_or_else(|| Error::io(format!("missing {}", self.path)))?;
        data.resize(len as usize, 0);
        drop(files);
        self.pos = self.pos.min(len);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self
            .lock()
            .get(&self.path)
            .map(|b| b.len() as u64)
            .unwrap_or(0))
    }
}
