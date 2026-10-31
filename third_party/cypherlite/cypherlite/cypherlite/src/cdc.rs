//! Optional CDC slots and apply-only replica flag (ADR-0045).

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, Result};
use cypherlite_storage::{atomic_write, read_to_vec, Vfs};
use serde::{Deserialize, Serialize};

const SLOTS_FILE: &str = "cdc/slots.json";

/// A WAL consumer that holds recycle until it acks (ADR-0045).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdcSlot {
    pub name: String,
    pub registered_lsn: u64,
    pub acked_lsn: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CdcFile {
    #[serde(default)]
    apply_only: bool,
    #[serde(default)]
    slots: Vec<CdcSlot>,
}

pub(crate) struct CdcState {
    apply_only: bool,
    slots: BTreeMap<String, CdcSlot>,
    vfs: Arc<dyn Vfs>,
}

impl CdcState {
    pub(crate) fn load(vfs: Arc<dyn Vfs>) -> Result<Self> {
        if !vfs.exists(SLOTS_FILE)? {
            return Ok(Self {
                apply_only: false,
                slots: BTreeMap::new(),
                vfs,
            });
        }
        let bytes = read_to_vec(&*vfs, SLOTS_FILE)?;
        let file: CdcFile =
            serde_json::from_slice(&bytes).map_err(|e| Error::storage(e.to_string()))?;
        let mut slots = BTreeMap::new();
        for s in file.slots {
            slots.insert(s.name.clone(), s);
        }
        Ok(Self {
            apply_only: file.apply_only,
            slots,
            vfs,
        })
    }

    fn persist(&self) -> Result<()> {
        let file = CdcFile {
            apply_only: self.apply_only,
            slots: self.slots.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| Error::storage(e.to_string()))?;
        atomic_write(&*self.vfs, SLOTS_FILE, &bytes)
    }

    pub(crate) fn apply_only(&self) -> bool {
        self.apply_only
    }

    pub(crate) fn set_apply_only(&mut self, on: bool) -> Result<()> {
        self.apply_only = on;
        self.persist()
    }

    pub(crate) fn has_source_slot(&self) -> bool {
        !self.slots.is_empty()
    }

    pub(crate) fn recycle_cut(&self, checkpoint_lsn: u64) -> u64 {
        match self.slots.values().map(|s| s.acked_lsn).min() {
            None => checkpoint_lsn,
            Some(acked) => checkpoint_lsn.min(acked),
        }
    }

    pub(crate) fn list(&self) -> Vec<CdcSlot> {
        self.slots.values().cloned().collect()
    }

    pub(crate) fn register(&mut self, name: &str, wal_lsn: u64) -> Result<CdcSlot> {
        let name = validate_slot_name(name)?;
        if let Some(existing) = self.slots.get(&name) {
            return Ok(existing.clone());
        }
        let slot = CdcSlot {
            name: name.clone(),
            registered_lsn: wal_lsn,
            acked_lsn: wal_lsn,
        };
        self.slots.insert(name, slot.clone());
        self.persist()?;
        Ok(slot)
    }

    pub(crate) fn ack(&mut self, name: &str, lsn: u64, wal_lsn: u64) -> Result<CdcSlot> {
        let slot = self
            .slots
            .get_mut(name)
            .ok_or_else(|| Error::semantic(format!("unknown CDC slot '{name}'")))?;
        if lsn > wal_lsn {
            return Err(Error::semantic(format!(
                "CDC ack LSN {lsn} is ahead of WAL LSN {wal_lsn}"
            )));
        }
        let advanced = lsn > slot.acked_lsn;
        if advanced {
            slot.acked_lsn = lsn;
        }
        let out = slot.clone();
        if advanced {
            self.persist()?;
        }
        Ok(out)
    }

    pub(crate) fn drop_slot(&mut self, name: &str) -> Result<bool> {
        let removed = self.slots.remove(name).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }
}

fn validate_slot_name(name: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(Error::semantic(
            "CDC slot name must be a lowercase DNS label",
        ));
    }
    Ok(name.to_string())
}
