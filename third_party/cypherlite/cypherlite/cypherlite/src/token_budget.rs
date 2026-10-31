//! Opt-in concurrent logical-workspace admission. Off, `reserve` does not
//! count and does not refuse. On, a statement reserves its cached peak, or
//! one quantum when the cache has no cost, against the process pool in
//! logical bytes. That reservation cannot spend the tail left for queries
//! already running. The ledger holds it until drop.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cypherlite_core::tokens::{
    admission_tail, PeakPool, TokenLedger, PEAK_ADMISSION_FLOOR, SCHEDULE_V1,
};
use cypherlite_core::{Error, Result, TokenOutcome, Value};

/// Limits resolved at open. `enabled` false ignores the capacity.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokenLimits {
    pub enabled: bool,
    pub capacity: u64,
}

/// How long an observed peak stays usable.
const PEAK_CACHE_TTL: Duration = Duration::from_secs(60);
/// Bound on remembered statements. The oldest expiry is dropped past this.
const PEAK_CACHE_CAP: usize = 1024;

/// Identity of one statement against one committed graph.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PeakKey {
    commit_ts: u64,
    statement_hash: u64,
    params: u64,
}

impl PeakKey {
    pub(crate) fn new(commit_ts: u64, cypher: &str, params: &BTreeMap<String, Value>) -> Self {
        Self {
            commit_ts,
            statement_hash: hash_statement(cypher),
            params: hash_params(params),
        }
    }
}

struct PeakCache {
    entries: Mutex<HashMap<PeakKey, (u64, Instant)>>,
}

impl PeakCache {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The remembered peak, if it is still inside the TTL. An admissible hit
    /// refreshes the expiry. A peak above the door ceiling does not: retrying
    /// an impossible reservation must not keep its cache entry alive.
    fn peak(&self, key: &PeakKey, admission_ceiling: u64) -> Option<u64> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match entries.get(key) {
            Some((peak, expiry)) if *expiry > now => {
                let peak = *peak;
                if peak <= admission_ceiling {
                    entries.insert(key.clone(), (peak, now + PEAK_CACHE_TTL));
                }
                Some(peak)
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Raise the remembered peak. A zero observation and a lower number
    /// leave a live entry alone.
    fn observe(&self, key: PeakKey, peak: u64) {
        if peak == 0 {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.retain(|_, (_, expiry)| *expiry > now);
        let next = match entries.get(&key) {
            Some((old, expiry)) if *expiry > now => (*old).max(peak),
            _ => peak,
        };
        if entries.len() >= PEAK_CACHE_CAP && !entries.contains_key(&key) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, expiry))| *expiry)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (next, now + PEAK_CACHE_TTL));
    }
}

/// Tokens held in the process pool. `reserved` is the live sum; `capacity`
/// is the configured concurrent peak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenPoolSnapshot {
    pub reserved: u64,
    pub capacity: u64,
}

/// Notified when `reserved` crosses from the admission ceiling into the tail.
type CeilingHook = Arc<dyn Fn(TokenPoolSnapshot) + Send + Sync>;

struct Pool {
    capacity: u64,
    reserved: AtomicU64,
    /// The common path does not lock this.
    ceiling_hook: Mutex<Option<CeilingHook>>,
}

impl Pool {
    fn admission_ceiling(&self) -> u64 {
        self.capacity.saturating_sub(admission_tail(self.capacity))
    }

    /// New work leaves the tail free. Running growth uses [`PeakPool::try_grow`].
    fn try_admit(&self, cost: u64) -> bool {
        if cost == 0 {
            return true;
        }
        let ceiling = self.admission_ceiling();
        let mut current = self.reserved.load(Ordering::Relaxed);
        loop {
            let next = match current.checked_add(cost) {
                Some(next) if next <= ceiling => next,
                _ => return false,
            };
            match self.reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => current = now,
            }
        }
    }

    /// One notification per crossing into the tail. Growth that stays above
    /// the ceiling does not notify again until `reserved` drops to the
    /// ceiling or below and crosses once more.
    fn note_ceiling(&self, previous: u64, reserved: u64) {
        let ceiling = self.admission_ceiling();
        if previous > ceiling || reserved <= ceiling {
            return;
        }
        let hook = self
            .ceiling_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(TokenPoolSnapshot {
                reserved,
                capacity: self.capacity,
            });
        }
    }
}

impl PeakPool for Pool {
    fn try_grow(&self, extra: u64) -> bool {
        if extra == 0 {
            return true;
        }
        let mut current = self.reserved.load(Ordering::Relaxed);
        loop {
            let next = match current.checked_add(extra) {
                Some(next) if next <= self.capacity => next,
                _ => return false,
            };
            match self.reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.note_ceiling(current, next);
                    return true;
                }
                Err(now) => current = now,
            }
        }
    }

    fn release(&self, amount: u64) {
        if amount > 0 {
            self.reserved.fetch_sub(amount, Ordering::AcqRel);
        }
    }
}

pub(crate) struct TokenBudget {
    enabled: bool,
    capacity: u64,
    pool: Arc<Pool>,
    peaks: PeakCache,
}

impl TokenBudget {
    pub(crate) fn new(limits: TokenLimits) -> Arc<Self> {
        Arc::new(Self {
            enabled: limits.enabled,
            capacity: limits.capacity,
            pool: Arc::new(Pool {
                capacity: limits.capacity,
                reserved: AtomicU64::new(0),
                ceiling_hook: Mutex::new(None),
            }),
            peaks: PeakCache::new(),
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Live reservation. `None` when admission is off, so a disabled budget
    /// does not report the dummy pool `reserve` builds for that mode.
    pub(crate) fn snapshot(&self) -> Option<TokenPoolSnapshot> {
        if !self.enabled {
            return None;
        }
        Some(TokenPoolSnapshot {
            reserved: self.pool.reserved.load(Ordering::Relaxed),
            capacity: self.capacity,
        })
    }

    /// Install the tail-crossing callback. A later call replaces it.
    /// Admission off keeps the hook unset because that pool is never charged.
    pub(crate) fn on_ceiling(&self, hook: impl Fn(TokenPoolSnapshot) + Send + Sync + 'static) {
        if !self.enabled {
            return;
        }
        *self
            .pool
            .ceiling_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// Cached peak for this statement, or one token when nothing is remembered.
    pub(crate) fn admission_cost(&self, key: &PeakKey) -> u64 {
        self.peaks
            .peak(key, self.pool.admission_ceiling())
            .unwrap_or(PEAK_ADMISSION_FLOOR)
            .max(PEAK_ADMISSION_FLOOR)
    }

    /// Reserve `cost` against the pool, leaving the tail for running queries.
    /// The returned ledger releases that reservation on drop.
    pub(crate) fn reserve(&self, cost: u64) -> Result<TokenLedger> {
        if !self.enabled {
            return Ok(TokenLedger::new(
                0,
                0,
                Arc::new(Pool {
                    capacity: u64::MAX,
                    reserved: AtomicU64::new(0),
                    ceiling_hook: Mutex::new(None),
                }),
            ));
        }
        let cost = cost.max(PEAK_ADMISSION_FLOOR);
        if !self.pool.try_admit(cost) {
            // A cost above the door can never fit under this configuration.
            // At or below the door, only concurrent reservations blocked it.
            let retryable = cost <= self.pool.admission_ceiling();
            return Err(Error::resource_limit_with_receipt(
                Error::CONCURRENT_LOGICAL_WORKSPACE_LIMIT,
                self.capacity,
                retryable,
                cypherlite_core::ExecutionReceipt {
                    peak_logical_bytes: cost,
                    token_schedule: SCHEDULE_V1,
                    outcome: TokenOutcome::CapacityExceeded,
                },
            ));
        }
        Ok(TokenLedger::new(
            self.capacity,
            cost,
            Arc::clone(&self.pool) as Arc<dyn PeakPool>,
        ))
    }

    /// Remember a peak this statement actually reached. A door rejection
    /// must not call this.
    pub(crate) fn observe(&self, key: PeakKey, peak: u64) {
        if self.enabled {
            self.peaks.observe(key, peak);
        }
    }
}

/// Hash the whitespace-normalized statement without retaining client text.
fn hash_statement(cypher: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut pending_space = false;
    let mut started = false;
    for ch in cypher.chars() {
        if ch.is_whitespace() {
            pending_space = started;
            continue;
        }
        if pending_space {
            mix_bytes(&mut hash, b" ");
            pending_space = false;
        }
        let mut encoded = [0; 4];
        mix_bytes(&mut hash, ch.encode_utf8(&mut encoded).as_bytes());
        started = true;
    }
    finish_mix(&mut hash);
    hash
}

fn hash_params(params: &BTreeMap<String, Value>) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for (name, value) in params {
        mix(&mut hash, name.as_bytes());
        mix_value(&mut hash, value);
    }
    hash
}

fn mix(hash: &mut u64, bytes: &[u8]) {
    mix_bytes(hash, bytes);
    finish_mix(hash);
}

fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn finish_mix(hash: &mut u64) {
    *hash ^= 0xff;
    *hash = hash.wrapping_mul(0x100000001b3);
}

fn mix_value(hash: &mut u64, value: &Value) {
    match value {
        Value::Null => mix(hash, b"null"),
        Value::Bool(v) => mix(hash, if *v { b"true" } else { b"false" }),
        Value::Int(v) => mix(hash, &v.to_le_bytes()),
        Value::Float(v) => mix(hash, &v.to_le_bytes()),
        Value::String(v) => mix(hash, v.as_bytes()),
        Value::Bytes(v) => mix(hash, v),
        Value::Uuid(v) => mix(hash, &v.to_le_bytes()),
        Value::List(values) => {
            mix(hash, b"list");
            for value in values.iter() {
                mix_value(hash, value);
            }
        }
        Value::Map(entries) => {
            mix(hash, b"map");
            for (name, value) in entries.iter() {
                mix(hash, name.as_bytes());
                mix_value(hash, value);
            }
        }
        Value::Node(node) => {
            mix(hash, b"node");
            mix(hash, &node.id.to_le_bytes());
        }
        Value::Relationship(rel) => {
            mix(hash, b"rel");
            mix(hash, &rel.id.to_le_bytes());
        }
        Value::Path(path) => {
            mix(hash, b"path");
            mix(hash, &(path.nodes.len() as u64).to_le_bytes());
            mix(hash, &(path.rels.len() as u64).to_le_bytes());
        }
        Value::Temporal(temporal) => mix(hash, temporal.to_string().as_bytes()),
        Value::Point(point) => mix(hash, &format!("{point:?}").into_bytes()),
        Value::Vector { coord, values } => {
            mix(hash, b"vector");
            mix(hash, &format!("{coord:?}").into_bytes());
            for value in values.iter() {
                mix_value(hash, value);
            }
        }
    }
}

/// Upper bound on the load-test admit hold. A leaked environment variable
/// cannot stall a query for longer than a minute.
const ADMIT_HOLD_CAP_MS: u64 = 60_000;

fn admit_hold_ms() -> u64 {
    static HOLD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *HOLD.get_or_init(|| {
        let Ok(raw) = std::env::var("CYPHERLITE_QUERY_ADMIT_HOLD_MS") else {
            return 0;
        };
        let Ok(ms) = raw.trim().parse::<u64>() else {
            return 0;
        };
        ms.min(ADMIT_HOLD_CAP_MS)
    })
}

/// Test hook for the processing-memory load test. Unset in production, so
/// admission does not sleep. A positive value holds the peak reservation
/// long enough for a second session to overlap it.
pub(crate) fn query_admit_hold() {
    let ms = admit_hold_ms();
    if ms == 0 {
        return;
    }
    eprintln!("query admit hold: CYPHERLITE_QUERY_ADMIT_HOLD_MS={ms}");
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_core::tokens::{LedgerGuard, WorkspaceCharge, CHARGE_QUANTUM_BYTES};

    /// One reservation quantum. Pool sizes in these tests are quanta times Q.
    const Q: u64 = CHARGE_QUANTUM_BYTES;

    fn limits(enabled: bool, capacity: u64) -> Arc<TokenBudget> {
        TokenBudget::new(TokenLimits { enabled, capacity })
    }

    fn key(commit_ts: u64, cypher: &str, max: i64) -> PeakKey {
        let mut params = BTreeMap::new();
        params.insert("max".into(), Value::Int(max));
        PeakKey::new(commit_ts, cypher, &params)
    }

    #[test]
    fn disabled_budget_does_not_refuse_or_count() {
        let budget = limits(false, 1);
        let first = budget.reserve(10).unwrap();
        let second = budget.reserve(10).unwrap();
        drop(first);
        drop(second);
        assert_eq!(budget.pool.reserved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capacity_refusal_is_retryable_and_releases() {
        // Two quanta of pool: one quantum plus its tail fits, a second does not.
        let capacity = 2 * Q;
        let budget = limits(true, capacity);
        let first = budget.reserve(Q).unwrap();
        let err = match budget.reserve(Q) {
            Err(err) => err,
            Ok(_) => panic!("second reservation should be refused"),
        };
        assert!(matches!(
            err,
            Error::ResourceLimit {
                name: "concurrent-logical-workspace-limit",
                limit,
                retryable: true,
                ..
            } if limit == capacity
        ));
        drop(first);
        assert!(budget.reserve(Q).is_ok());
    }

    #[test]
    fn a_cost_at_the_door_is_retryable_under_contention_and_admits_when_empty() {
        let capacity = 16 * Q;
        let ceiling = capacity - admission_tail(capacity);
        let budget = limits(true, capacity);
        let running = budget.reserve(Q).unwrap();
        let err = budget.reserve(ceiling).unwrap_err();
        assert!(matches!(
            err,
            Error::ResourceLimit {
                name: "concurrent-logical-workspace-limit",
                limit: 65536,
                retryable: true,
                ..
            }
        ));
        drop(running);
        assert!(budget.reserve(ceiling).is_ok());
    }

    #[test]
    fn a_cost_above_the_door_is_non_retryable_when_the_pool_is_empty() {
        let capacity = 16 * Q;
        let ceiling = capacity - admission_tail(capacity);
        let budget = limits(true, capacity);
        let err = budget.reserve(ceiling + 1).unwrap_err();
        assert!(matches!(
            err,
            Error::ResourceLimit {
                name: "concurrent-logical-workspace-limit",
                limit: 65536,
                retryable: false,
                ..
            }
        ));
        assert_eq!(budget.pool.reserved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn only_an_admissible_cache_hit_refreshes_its_expiry() {
        let capacity = 80 * Q;
        let ceiling = capacity - admission_tail(capacity);
        let budget = limits(true, capacity);
        let admissible = key(1, "RETURN $max", 1);
        let oversized = key(1, "RETURN $max", 2);
        budget.observe(admissible.clone(), ceiling);
        budget.observe(oversized.clone(), ceiling + 1);

        let old_expiry = Instant::now() + Duration::from_secs(1);
        {
            let mut entries = budget
                .peaks
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            entries.get_mut(&admissible).unwrap().1 = old_expiry;
            entries.get_mut(&oversized).unwrap().1 = old_expiry;
        }

        assert_eq!(budget.admission_cost(&admissible), ceiling);
        assert_eq!(budget.admission_cost(&oversized), ceiling + 1);
        let entries = budget
            .peaks
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(entries[&admissible].1 > old_expiry);
        assert_eq!(entries[&oversized].1, old_expiry);
    }

    #[test]
    fn tail_blocks_a_start_when_only_the_tail_is_free() {
        let capacity = 16 * Q;
        let tail = admission_tail(capacity);
        assert_eq!(tail, 2 * Q);
        let budget = limits(true, capacity);
        let running = budget.reserve(capacity - tail).unwrap();
        let err = match budget.reserve(Q) {
            Err(err) => err,
            Ok(_) => panic!("a start in the tail should be refused"),
        };
        assert!(err
            .to_string()
            .contains("concurrent-logical-workspace-limit"));
        drop(running);
        assert!(budget.reserve(Q).is_ok());
    }

    #[test]
    fn cached_cost_is_reserved_and_a_door_rejection_does_not_lower_it() {
        let budget = limits(true, 80 * Q);
        let statement = key(1, "MATCH (n) RETURN n", 800);
        budget.observe(statement.clone(), 40 * Q);
        assert_eq!(budget.admission_cost(&statement), 40 * Q);
        let held = budget.reserve(budget.admission_cost(&statement)).unwrap();
        assert_eq!(budget.pool.reserved.load(Ordering::Relaxed), 40 * Q);
        let other = key(1, "MATCH (n) RETURN n", 1);
        assert_eq!(budget.admission_cost(&other), Q);
        let err = match budget.reserve(40 * Q) {
            Err(err) => err,
            Ok(_) => panic!("the second reservation should be refused"),
        };
        assert!(err
            .to_string()
            .contains("concurrent-logical-workspace-limit"));
        assert_eq!(budget.admission_cost(&statement), 40 * Q);
        drop(held);
    }

    #[test]
    fn a_higher_peak_raises_the_cache_and_a_commit_misses() {
        let budget = limits(true, 100 * Q);
        let statement = key(1, "MATCH (n) RETURN n", 800);
        budget.observe(statement.clone(), 14 * Q);
        budget.observe(statement.clone(), 22 * Q);
        budget.observe(statement.clone(), 10 * Q);
        assert_eq!(budget.admission_cost(&statement), 22 * Q);
        let later = key(2, "MATCH (n) RETURN n", 800);
        assert_eq!(budget.admission_cost(&later), Q);
        let spaced = key(1, "  MATCH   (n)   RETURN n  ", 800);
        assert_eq!(budget.admission_cost(&spaced), 22 * Q);
    }

    #[test]
    fn a_higher_mid_query_refusal_raises_the_cached_peak() {
        let budget = limits(true, 16 * Q);
        let statement = key(1, "MATCH (n) RETURN n", 800);
        let mut ledger = budget.reserve(Q).unwrap();
        let _guard = LedgerGuard::install(&mut ledger);
        let held = WorkspaceCharge::retain(3 * Q).unwrap();
        let err = WorkspaceCharge::retain(20 * Q).unwrap_err();
        assert!(err
            .to_string()
            .contains("concurrent-logical-workspace-limit"));
        drop(held);
        drop(_guard);
        let peak = ledger
            .receipt(TokenOutcome::CapacityExceeded)
            .peak_logical_bytes;
        assert!(peak >= 3 * Q, "{peak}");
        budget.observe(statement.clone(), peak);
        drop(ledger);
        assert_eq!(budget.admission_cost(&statement), peak);

        let mut further = budget.reserve(peak).unwrap();
        let _guard = LedgerGuard::install(&mut further);
        let held = WorkspaceCharge::retain(3 * Q).unwrap();
        let grown = WorkspaceCharge::retain(2 * Q).unwrap();
        let err = WorkspaceCharge::retain(20 * Q).unwrap_err();
        assert!(err
            .to_string()
            .contains("concurrent-logical-workspace-limit"));
        drop(grown);
        drop(held);
        drop(_guard);
        let higher = further
            .receipt(TokenOutcome::CapacityExceeded)
            .peak_logical_bytes;
        assert!(higher > peak, "raised {peak} further {higher}");
        budget.observe(statement.clone(), higher);
        drop(further);
        assert_eq!(budget.admission_cost(&statement), higher);
    }

    #[test]
    fn growing_into_the_tail_notifies_once_until_it_drops_back() {
        let capacity = 16 * Q;
        let ceiling = capacity - admission_tail(capacity);
        let budget = limits(true, capacity);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        budget.on_ceiling(move |pool| {
            record.lock().unwrap().push(pool.reserved);
        });
        let mut ledger = budget.reserve(Q).unwrap();
        let _guard = LedgerGuard::install(&mut ledger);
        let mut held = WorkspaceCharge::retain(ceiling).unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "the ceiling itself is not the tail"
        );
        held.resize(ceiling + Q).unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), &[ceiling + Q]);
        held.resize(capacity).unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "staying in the tail is one crossing"
        );
        drop(held);
        drop(_guard);
        drop(ledger);
        assert_eq!(budget.snapshot().unwrap().reserved, 0);

        let mut again = budget.reserve(Q).unwrap();
        let _guard = LedgerGuard::install(&mut again);
        let mut held = WorkspaceCharge::retain(ceiling).unwrap();
        held.resize(ceiling + Q).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
